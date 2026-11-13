//! Mini TUI de `pxu update --host` (y de `--nvidia`, «Host Updates»): pasos con estado y tiempo, una línea que dice qué pasa AHORA, barra de descarga real,
//! las últimas líneas de salida y un resumen final. Pantalla alterna: el instalador del driver tarda minutos y escribe cientos de líneas, y así el
//! terminal queda intacto al salir (el resumen en texto se imprime después, y queda en el historial).
//!
//! `Live` es un modelo puro (sin terminal ni reloj): se alimenta con los `Event` del motor y con el tamaño del archivo que se descarga, y `draw` lo pinta;
//! así se prueba con un `TestBackend`. El motor corre en el hilo que llama; la UI vive en otro hilo y recibe todo por un canal sin tope: nunca frena al motor,
//! ni siquiera con el terminal roto (SIGHUP). Sin terminal o con `--json`, `Ui::start` no se usa y sale el texto plano de siempre.
use pxu::share::update::{self, nvidia, Event, Outcome, RunReport, Step};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const TAIL_CAP: usize = 2000;
/// Ventana para la velocidad de descarga: más corta salta demasiado, más larga tarda en notar un corte.
const SPEED_WINDOW: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- modelo

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum St {
    Pending,
    Running,
    Ok,
    Warn,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Prepare,
    Setup,
    Download,
    Verify,
    Build,
    StopCts,
    Install,
    Smi,
    StartCts,
    Ct(u32),
    Cleanup,
    Host(Step),
}

#[derive(Debug, Clone)]
struct Row {
    key: Key,
    label: String,
    st: St,
    since: Duration,
    took: Duration,
    note: String,
}

/// Cuántos bytes van y a qué ritmo (muestras `(instante, bytes)` de los últimos `SPEED_WINDOW`).
#[derive(Debug, Clone, Default)]
pub struct Download {
    pub total: Option<u64>,
    pub got: u64,
    samples: VecDeque<(Duration, u64)>,
}

impl Download {
    pub fn observe(&mut self, at: Duration, bytes: u64) {
        self.got = bytes;
        self.samples.push_back((at, bytes));
        while self.samples.len() > 2 && at.saturating_sub(self.samples[1].0) >= SPEED_WINDOW {
            self.samples.pop_front();
        }
    }
    /// Bytes por segundo entre la muestra más vieja de la ventana y la última; `None` con menos de dos muestras o sin avance de tiempo.
    pub fn speed(&self) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let (t1, b1) = *self.samples.back()?;
        let dt = t1.checked_sub(t0)?.as_secs_f64();
        (dt > 0.0).then(|| b1.saturating_sub(b0) as f64 / dt)
    }
    /// Tiempo que falta al ritmo actual; `None` sin total, sin ritmo o parado.
    pub fn eta(&self) -> Option<Duration> {
        let v = self.speed().filter(|v| *v > 1.0)?;
        let left = self.total?.saturating_sub(self.got);
        Some(Duration::from_secs_f64(left as f64 / v))
    }
    pub fn fraction(&self) -> Option<f64> {
        self.total.filter(|t| *t > 0).map(|t| (self.got as f64 / t as f64).clamp(0.0, 1.0))
    }
}

/// Cómo terminó la corrida, para la pantalla final.
#[derive(Debug, Clone)]
pub struct Summary {
    pub outcome: Outcome,
    pub lines: Vec<String>,
    pub took: Duration,
    pub log: Option<String>,
}

pub fn summarize(rep: &RunReport) -> Summary {
    let mut lines = vec![];
    let mut worst = Outcome::Ok;
    let rank = |o: Outcome| match o {
        Outcome::Failed => 4,
        Outcome::Cancelled => 3,
        Outcome::Warn => 2,
        Outcome::Skipped => 1,
        Outcome::Ok => 0,
    };
    let mut note = |o: Outcome| {
        if rank(o) > rank(worst) {
            worst = o;
        }
    };
    if let Some(n) = &rep.nvidia {
        note(n.outcome);
        match &n.from {
            Some(f) => lines.push(format!("NVIDIA driver {f} → {}: {}", n.to.as_deref().unwrap_or("?"), n.detail)),
            None => lines.push(format!("NVIDIA driver install {}: {}", n.to.as_deref().unwrap_or("?"), n.detail)),
        }
        for c in &n.cts {
            note(c.outcome);
            lines.push(format!("CT {} {}: {:?} — {}", c.vmid, c.name, c.outcome, c.detail));
        }
        if n.reboot_needed {
            lines.push("REBOOT the host (see above) and run the command again if it asks for it, or when nvidia-smi does not confirm the driver yet (pxu never reboots it).".into());
        }
    }
    for r in &rep.results {
        note(r.outcome);
        lines.push(format!("{}: {:?} — {}", if r.vmid == 0 { "host".into() } else { format!("CT {}", r.vmid) }, r.outcome, r.detail));
        if let Some(b) = &r.backup {
            lines.push(format!("backup: {b}"));
        }
        if r.outcome == Outcome::Failed {
            lines.push(format!("recovery (run on the host): {}", pxu::share::update::hostupdate::RECOVERY));
        }
    }
    if let Some(a) = &rep.aborted {
        lines.push(format!("run stopped: {a}"));
    }
    if rep.cancelled {
        worst = Outcome::Cancelled;
    }
    Summary { outcome: worst, lines, took: Duration::from_secs(rep.finished_at.saturating_sub(rep.started_at)), log: rep.log_path.clone() }
}

pub struct Live {
    pub title: String,
    rows: Vec<Row>,
    cur: Option<usize>,
    doing: String,
    tail: VecDeque<String>,
    /// Primera línea visible de `tail` mientras se lee hacia atrás; `None` sigue en vivo.
    top: Option<usize>,
    /// Alto útil del recuadro de salida en el último dibujo (para PgUp/PgDn y los límites).
    view: std::cell::Cell<usize>,
    /// Dónde corre lo que se está viendo: `HOST` o `CT <id>` (lo fija cada orden `$ …`).
    origin: String,
    pub dl: Option<Download>,
    plan: Option<nvidia::Plan>,
    /// Primer `!! motivo` del motor: es lo que se muestra como causa si algo falla.
    why: Option<String>,
    /// Mensaje de Ctrl+C (qué pasó con la petición).
    pub notice: Option<String>,
    pub end: Option<Summary>,
    nv: bool,
}

/// Qué hacer con un Ctrl+C.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlC {
    /// Pedir la cancelación (todavía no se ha tocado nada).
    Cancel,
    /// Ignorarlo: cortar a mitad dejaría el sistema a medias.
    Refuse,
    /// La corrida ya terminó: cerrar la pantalla.
    Close,
}

impl Live {
    fn new(title: &str, nv: bool) -> Live {
        Live { title: title.into(), rows: vec![], cur: None, doing: "Starting…".into(), tail: VecDeque::new(), top: None, view: std::cell::Cell::new(10), origin: "HOST".into(), dl: None, plan: None, why: None, notice: None, end: None, nv }
    }
    /// `pxu update --host --nvidia`: la lista de pasos llega con el `Plan` del motor.
    pub fn nvidia() -> Live {
        Live::new("pxu · Host Updates · With NVIDIA Driver", true)
    }
    /// `pxu update --host`: los pasos son los del motor del apt.
    pub fn apt() -> Live {
        let mut l = Live::new("pxu · Host Updates", false);
        for s in [Step::Preflight, Step::Backup, Step::Refresh, Step::Upgrade, Step::Verify] {
            l.push(Key::Host(s), apt_label(s));
        }
        l.doing = "Starting the host update…".into();
        l
    }

    fn push(&mut self, key: Key, label: impl Into<String>) {
        self.rows.push(Row { key, label: label.into(), st: St::Pending, since: Duration::ZERO, took: Duration::ZERO, note: String::new() });
    }

    fn set_plan(&mut self, p: nvidia::Plan) {
        self.title = if p.host {
            "pxu · Host Updates · With NVIDIA Driver".to_string()
        } else {
            format!("pxu · NVIDIA driver {} libraries in CT", p.to)
        };
        self.rows.clear();
        self.push(Key::Prepare, "Check disk space and NVIDIA's checksum");
        if p.host && p.fresh {
            self.push(Key::Setup, "Install kernel headers and block nouveau");
        }
        self.push(Key::Download, format!("Download driver {}", p.to));
        self.push(Key::Verify, "Verify the download (SHA-256 and installer check)");
        if p.host {
            self.push(Key::Build, "Extract and test-build the kernel module");
            if !p.stop.is_empty() {
                self.push(Key::StopCts, format!("Stop GPU CTs ({})", ids(&p.stop)));
            }
            self.push(Key::Install, "Unload the old module and install the new driver");
            self.push(Key::Smi, "Confirm the version with nvidia-smi");
            if p.stop.iter().any(|(id, _)| !p.libs.iter().any(|(l, _)| l == id)) {
                self.push(Key::StartCts, "Start the stopped CTs again");
            }
        }
        for (id, name) in &p.libs {
            self.push(Key::Ct(*id), format!("CT {id}{}: update the driver libraries", if name.is_empty() { String::new() } else { format!(" ({name})") }));
        }
        self.push(Key::Cleanup, "Remove temporary files");
        self.plan = Some(p);
    }

    fn row_of(&self, key: Key) -> Option<usize> {
        self.rows.iter().position(|r| r.key == key)
    }

    /// Pasa al paso `i`: el que estaba en curso termina bien (si no falló) y los pendientes que se saltó quedan «saltados».
    fn enter(&mut self, at: Duration, i: usize) {
        if self.cur == Some(i) {
            return;
        }
        for j in 0..self.rows.len() {
            let r = &mut self.rows[j];
            if j < i && r.st == St::Running {
                r.st = St::Ok;
                r.took = at.saturating_sub(r.since);
            } else if j < i && r.st == St::Pending {
                r.st = St::Skipped;
            }
        }
        let r = &mut self.rows[i];
        if r.st == St::Pending {
            r.st = St::Running;
            r.since = at;
        }
        self.cur = Some(i);
        let doing = self.describe(self.rows[i].key);
        self.doing = doing;
    }

    fn describe(&self, key: Key) -> String {
        let to = self.plan.as_ref().map_or("?".into(), |p| p.to.clone());
        match key {
            Key::Prepare => format!("Checking that {} has enough free space and fetching NVIDIA's SHA-256 checksum for {}…", nvidia::WORK_DIR, nvidia::run_file(&to)),
            Key::Setup => "Installing the kernel headers the driver build needs (if missing) and blocking nouveau for the next boot. If nouveau is loaded now, it stops here and asks you to reboot.".into(),
            Key::Download => format!("Downloading the NVIDIA driver {to} from download.nvidia.com{}. Nothing on the system has been touched yet.", self.dl.as_ref().and_then(|d| d.total).map_or(String::new(), |t| format!(" ({})", fmt_bytes(t)))),
            Key::Verify => "Verifying the download: its SHA-256 against NVIDIA's checksum, then the installer's own integrity check.".into(),
            Key::Build => "Unpacking the installer and test-compiling the kernel module for the running kernel. The current driver is still untouched.".into(),
            Key::StopCts => format!("Stopping the CTs that use the GPU ({}) so the old module can be unloaded.", self.plan.as_ref().map_or(String::new(), |p| ids(&p.stop))),
            Key::Install => "Unloading the old NVIDIA module and running the installer (DKMS build). This takes several minutes and must not be interrupted.".into(),
            Key::Smi => "Asking nvidia-smi which driver version is loaded now.".into(),
            Key::StartCts => "Starting the CTs that were stopped for the driver change.".into(),
            Key::Ct(id) => format!("CT {id}: installing the driver libraries inside the container."),
            Key::Cleanup => format!("Removing the temporary files in {}.", nvidia::WORK_DIR),
            Key::Host(s) => apt_doing(s).into(),
        }
    }

    pub fn event(&mut self, at: Duration, e: &Event) {
        match e {
            Event::Nv(nvidia::Note::Plan(p)) => self.set_plan(p.clone()),
            Event::Nv(nvidia::Note::Size(n)) => {
                self.dl.get_or_insert_with(Download::default).total = Some(*n);
                if self.cur.is_some_and(|i| self.rows[i].key == Key::Download) {
                    self.doing = self.describe(Key::Download);
                }
            }
            Event::Nv(nvidia::Note::Ct { vmid, outcome, detail }) => {
                if let Some(i) = self.row_of(Key::Ct(*vmid)) {
                    let r = &mut self.rows[i];
                    r.st = match outcome {
                        Outcome::Ok => St::Ok,
                        Outcome::Warn => St::Warn,
                        Outcome::Skipped => St::Skipped,
                        _ => St::Failed,
                    };
                    r.took = at.saturating_sub(r.since);
                    r.note = detail.clone();
                }
            }
            Event::Step(_, s) => {
                let key = Key::Host(*s);
                let i = match self.row_of(key) {
                    Some(i) => Some(i),
                    None if self.nv || matches!(s, Step::Lock | Step::Pending | Step::Done) => None,
                    None => {
                        self.push(key, apt_label(*s));
                        Some(self.rows.len() - 1)
                    }
                };
                if let Some(i) = i {
                    self.enter(at, i);
                }
            }
            Event::Line(vmid, l) => self.line(at, *vmid, l),
            _ => {}
        }
    }

    fn line(&mut self, at: Duration, vmid: u32, l: &str) {
        let mut tag = self.origin.clone();
        let mut shown = l;
        if let Some(cmd) = l.strip_prefix("$ ") {
            let (o, t, body) = origin_of(cmd);
            self.origin = o;
            tag = t;
            shown = body;
            if self.nv {
                if let Some(k) = self.classify(vmid, cmd) {
                    if let Some(i) = self.row_of(k) {
                        // un paso que ya pasó no vuelve a abrirse (p. ej. `pct start` de la recuperación)
                        if self.rows[i].st == St::Pending || self.cur == Some(i) {
                            self.enter(at, i);
                        }
                    }
                }
                if let Some(Key::Ct(id)) = self.cur.map(|i| self.rows[i].key) {
                    if let Some(p) = ct_phase(cmd) {
                        self.doing = format!("CT {id}: {p}.");
                    }
                }
            }
        } else if let Some(why) = l.strip_prefix("!! ") {
            if self.why.is_none() {
                self.why = Some(why.to_string());
                if let Some(i) = self.cur {
                    let r = &mut self.rows[i];
                    r.st = St::Failed;
                    r.took = at.saturating_sub(r.since);
                    r.note = why.to_string();
                }
            }
        }
        if !l.starts_with("== ") {
            if self.tail.len() >= TAIL_CAP {
                self.tail.pop_front();
                // la vista sigue anclada a lo que se lee aunque el buffer descarte la línea más vieja
                self.top = self.top.map(|t| t.saturating_sub(1));
            }
            let shown = if l.starts_with("$ ") { format!("$ {shown}") } else { shown.to_string() };
            self.tail.push_back(format!("{tag:<14}│ {shown}"));
        }
    }

    /// Mueve la vista de "Live output". Devuelve `true` si la tecla era de desplazamiento.
    fn scroll(&mut self, code: crossterm::event::KeyCode) -> bool {
        use crossterm::event::KeyCode::*;
        let view = self.view.get().max(1);
        let max_top = self.tail.len().saturating_sub(view);
        let cur = self.top.unwrap_or(max_top).min(max_top);
        let to = match code {
            Up => cur.saturating_sub(1),
            Down => cur + 1,
            PageUp => cur.saturating_sub(view),
            PageDown => cur + view,
            Home => 0,
            End => max_top,
            _ => return false,
        };
        // llegar al fondo (o End) vuelve a seguir en vivo
        self.top = if to >= max_top || code == End { None } else { Some(to) };
        true
    }

    /// Qué paso corresponde a una orden `$ …` (las del motor: `Grant` y `ct_*`).
    fn classify(&self, vmid: u32, cmd: &str) -> Option<Key> {
        let t: Vec<&str> = cmd.split_whitespace().collect();
        let has = |s: &str| t.iter().any(|x| *x == s || x.starts_with(s));
        match t.first().copied()? {
            _ if cmd.starts_with("rm -rf -- /var/lib/pxu-nvidia") && !cmd.starts_with("rm -rf -- /var/lib/pxu-nvidia.") => Some(Key::Cleanup),
            "env" if vmid == 0 && has("apt-get") => Some(Key::Setup),
            "update-initramfs" if vmid == 0 => Some(Key::Setup),
            "install" | "df" if vmid == 0 => Some(Key::Prepare),
            "curl" if has("-o") => Some(Key::Download),
            "curl" if has("-fsSIL") => Some(Key::Download),
            "curl" => Some(Key::Prepare),
            "sha256sum" if vmid == 0 => Some(Key::Verify),
            "sh" if has("--check") => Some(Key::Verify),
            "sh" if has("--extract-only") => Some(Key::Build),
            "make" => Some(Key::Build),
            "systemctl" | "modprobe" if vmid == 0 && has("-r") => Some(Key::Install),
            "systemctl" if has("stop") => Some(Key::Install),
            "sh" if has("--dkms") => Some(Key::Install),
            "nvidia-smi" if vmid == 0 => Some(Key::Smi),
            "pct" if vmid == 0 && (has("shutdown") || has("stop")) => Some(Key::StopCts),
            "pct" if vmid == 0 && has("start") && self.cur.is_some_and(|i| self.rows[i].key == Key::Smi) => Some(Key::StartCts),
            _ if vmid != 0 => Some(Key::Ct(vmid)),
            _ => None,
        }
    }

    /// Bytes que lleva la descarga y de dónde leerlos (solo mientras ese paso corre).
    pub fn download_file(&self) -> Option<String> {
        let i = self.cur?;
        if self.rows[i].key == Key::Download && self.end.is_none() { self.plan.as_ref().map(|p| nvidia::run_path(&p.to)) } else { None }
    }

    pub fn observe_download(&mut self, at: Duration, bytes: u64) {
        let had = self.dl.is_some();
        self.dl.get_or_insert_with(Download::default).observe(at, bytes);
        if !had {
            let d = self.describe(Key::Download);
            self.doing = d;
        }
    }

    /// Hasta aquí cancelar corta sin dejar nada a medias; desde parar los CT / instalar, no.
    pub fn critical(&self) -> bool {
        self.end.is_none()
            && self.cur.is_some_and(|i| match self.rows[i].key {
                Key::Host(s) => !s.before_install(),
                Key::Prepare | Key::Setup | Key::Download | Key::Verify | Key::Build => false,
                _ => true,
            })
    }

    /// El aviso inmediato de una petición de cancelar (Ctrl+C o una señal): qué llegó y qué va a pasar.
    pub fn cancel_notice(&mut self, sig: i32) {
        self.notice = Some(update::cancel_requested(sig, self.critical(), self.nv));
    }

    pub fn ctrl_c(&mut self) -> CtrlC {
        if self.end.is_some() {
            return CtrlC::Close;
        }
        let critical = self.critical();
        self.cancel_notice(libc::SIGINT);
        if critical {
            CtrlC::Refuse
        } else {
            CtrlC::Cancel
        }
    }

    pub fn finish(&mut self, at: Duration, s: Summary) {
        // si un paso ya falló, el que seguía en curso (p. ej. la limpieza) terminó bien
        let failed_before = self.rows.iter().any(|r| r.st == St::Failed);
        for r in &mut self.rows {
            match r.st {
                St::Running => {
                    r.took = at.saturating_sub(r.since);
                    if !failed_before && matches!(s.outcome, Outcome::Failed | Outcome::Cancelled) {
                        r.st = St::Failed;
                        if r.note.is_empty() {
                            r.note = s.lines.first().cloned().unwrap_or_default();
                        }
                    } else {
                        r.st = St::Ok;
                    }
                }
                St::Pending => r.st = St::Skipped,
                St::Failed if s.outcome == Outcome::Warn => r.st = St::Warn,
                _ => {}
            }
        }
        self.end = Some(s);
    }

    #[cfg(test)]
    fn states(&self) -> Vec<St> {
        self.rows.iter().map(|r| r.st).collect()
    }
}

/// De una orden `$ …` del motor: dónde corre (`HOST` o `CT <id>`), la etiqueta de su línea y la orden sin el prefijo `[CT n]`.
/// `pct shutdown|start|stop <id>` corre en el HOST pero actúa sobre el CT: la etiqueta lo dice (`HOST → CT 129`) y lo que sigue sigue siendo HOST.
fn origin_of(cmd: &str) -> (String, String, &str) {
    if let Some(rest) = cmd.strip_prefix("[CT ") {
        if let Some((id, body)) = rest.split_once("] ") {
            if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                let o = format!("CT {id}");
                return (o.clone(), o, body);
            }
        }
    }
    let mut t = cmd.split_whitespace();
    let target = (t.next() == Some("pct")).then(|| t.skip(1).find(|x| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit()))).flatten();
    let tag = target.map_or("HOST".to_string(), |id| format!("HOST → CT {id}"));
    ("HOST".into(), tag, cmd)
}

fn ids(v: &[(u32, String)]) -> String {
    v.iter().map(|(i, _)| i.to_string()).collect::<Vec<_>>().join(", ")
}

fn apt_label(s: Step) -> &'static str {
    match s {
        Step::Preflight => "Safety checks (dpkg, disk, layout)",
        Step::Backup => "Back up /etc (and ZFS snapshot)",
        Step::Refresh => "Refresh package lists and simulate",
        Step::Upgrade => "apt full-upgrade",
        Step::Verify => "Verify the result",
        Step::Rollback => "Roll back",
        Step::Restore => "Restore",
        _ => "…",
    }
}

fn apt_doing(s: Step) -> &'static str {
    match s {
        Step::Preflight => "Running safety checks: dpkg state, free disk space and the root filesystem.",
        Step::Backup => "Backing up /etc and, if the root is ZFS, taking a snapshot you can roll back to.",
        Step::Refresh => "Refreshing the package lists and simulating the upgrade to be sure nothing important would be removed.",
        Step::Upgrade => "Running apt full-upgrade: packages are being downloaded, unpacked and configured. This must not be interrupted.",
        Step::Verify => "Checking the result: installed versions, failed units and whether a reboot is needed.",
        Step::Rollback | Step::Restore => "Something failed: putting things back the way they were.",
        _ => "Working…",
    }
}

/// Qué hace una orden dentro de un CT, en lenguaje llano.
fn ct_phase(cmd: &str) -> Option<&'static str> {
    let c = cmd.strip_prefix("[CT ").map_or(cmd, |r| r.split_once("] ").map_or(r, |(_, x)| x));
    Some(if c.starts_with("df") {
        "checking free space in the container"
    } else if c.starts_with("mktemp") {
        "creating a private temporary directory"
    } else if c.starts_with("pct push") {
        "copying the installer into the container"
    } else if c.starts_with("pct start") {
        "starting the container"
    } else if c.starts_with("pct reboot") {
        "restarting the container"
    } else if c.starts_with("sha256sum") {
        "verifying the copied installer"
    } else if c.contains("--no-kernel-modules") {
        "running the libraries installer (a few minutes)"
    } else if c.starts_with("nvidia-smi") {
        "checking the driver with nvidia-smi inside the container"
    } else if c.starts_with("rm") {
        "removing the temporary directory"
    } else {
        return None;
    })
}

// ---------------------------------------------------------------- formato

pub fn fmt_bytes(b: u64) -> String {
    let f = b as f64;
    if f >= 1e9 {
        format!("{:.2} GB", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1} MB", f / 1e6)
    } else {
        format!("{:.0} kB", f / 1e3)
    }
}

pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

// ---------------------------------------------------------------- dibujo

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn badge(st: St, at: Duration) -> (String, Style) {
    match st {
        St::Pending => ("○".into(), Style::new().fg(Color::DarkGray)),
        St::Running => (SPIN[(at.as_millis() / 100 % 10) as usize].into(), Style::new().fg(Color::Cyan)),
        St::Ok => ("✓".into(), Style::new().fg(Color::Green)),
        St::Warn => ("!".into(), Style::new().fg(Color::Yellow)),
        St::Failed => ("✘".into(), Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
        St::Skipped => ("–".into(), Style::new().fg(Color::DarkGray)),
    }
}

pub fn draw(f: &mut Frame, m: &Live, at: Duration) {
    let area = f.area();
    let show_dl = m.dl.is_some() && m.rows.iter().any(|r| r.key == Key::Download);
    let steps_h = (m.rows.len() as u16 + 2).min(area.height.saturating_sub(10).max(4));
    let now_h = if m.end.is_some() { (m.end.as_ref().map_or(0, |s| s.lines.len()) as u16 + 4).min(10) } else { 4 };
    // el aviso de cancelación es largo: el pie crece con las líneas que ocupa (nunca se corta lo que va a pasar)
    let foot_h = match (&m.notice, &m.end) {
        (Some(n), None) => (((n.chars().count() + 2) as u16).div_ceil(area.width.max(1)) + 1).clamp(2, 8), // +1: el ajuste por palabras gasta algo más que la división
        _ => 1,
    };
    let [head, steps, now, dl, out, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(steps_h),
        Constraint::Length(now_h),
        Constraint::Length(if show_dl { 3 } else { 0 }),
        Constraint::Min(3),
        Constraint::Length(foot_h),
    ])
    .areas(area);

    let total = m.end.as_ref().map_or(at, |s| s.took);
    f.render_widget(Paragraph::new(Line::from(Span::styled(format!(" {}", m.title), Style::new().add_modifier(Modifier::BOLD)))), head);
    // el tiempo transcurrido va en la esquina derecha, con el mismo margen de 1 columna que el título
    f.render_widget(Paragraph::new(Line::from(format!("elapsed {} ", fmt_dur(total))).right_aligned()), head);

    // pasos
    let items: Vec<ListItem> = m
        .rows
        .iter()
        .map(|r| {
            let (b, style) = badge(r.st, at);
            let time = match r.st {
                St::Running => fmt_dur(at.saturating_sub(r.since)),
                St::Ok | St::Warn | St::Failed => fmt_dur(r.took),
                _ => String::new(),
            };
            // la hora ocupa siempre 8 columnas al final; el nombre se recorta con "…" para dejarle sitio
            let max = steps.width.saturating_sub(2 + 3 + 8) as usize;
            let mut text = r.label.clone();
            if !r.note.is_empty() && matches!(r.st, St::Failed | St::Warn) {
                text = format!("{} — {}", r.label, r.note);
            }
            if text.chars().count() > max {
                text = text.chars().take(max.saturating_sub(1)).collect::<String>() + "…";
            }
            let text = format!("{text:<max$}");
            let label_style = match r.st {
                St::Running => Style::new().add_modifier(Modifier::BOLD),
                St::Pending | St::Skipped => Style::new().fg(Color::DarkGray),
                St::Failed => Style::new().fg(Color::Red),
                _ => Style::new(),
            };
            ListItem::new(Line::from(vec![Span::styled(format!(" {b} "), style), Span::styled(text, label_style), Span::styled(format!(" {time:>7}"), Style::new().fg(Color::DarkGray))]))
        })
        .collect();
    f.render_widget(List::new(items).block(Block::default().borders(Borders::ALL).title(" Steps ")), steps);

    // ahora / resumen
    match &m.end {
        None => f.render_widget(Paragraph::new(m.doing.as_str()).wrap(Wrap { trim: true }).block(Block::default().borders(Borders::ALL).title(" Right now ")), now),
        Some(s) => {
            let (head, color) = match s.outcome {
                Outcome::Ok => (format!("✓ Finished OK in {}", fmt_dur(s.took)), Color::Green),
                Outcome::Warn => (format!("! Finished with warnings in {}", fmt_dur(s.took)), Color::Yellow),
                Outcome::Cancelled => (format!("– Cancelled after {}", fmt_dur(s.took)), Color::Yellow),
                Outcome::Skipped => (format!("– Nothing was done ({})", fmt_dur(s.took)), Color::Gray),
                Outcome::Failed => {
                    let at_step = m.rows.iter().find(|r| r.st == St::Failed).map(|r| r.label.as_str()).unwrap_or("?");
                    (format!("✘ FAILED at \"{at_step}\" after {}", fmt_dur(s.took)), Color::Red)
                }
            };
            let mut lines = vec![Line::styled(head, Style::new().fg(color).add_modifier(Modifier::BOLD))];
            if s.outcome == Outcome::Failed {
                if let Some(w) = &m.why {
                    lines.push(Line::raw(format!("Why: {w}")));
                }
            }
            lines.extend(s.lines.iter().map(|l| Line::raw(l.clone())));
            lines.push(Line::raw(format!("Log: {}", s.log.as_deref().unwrap_or("(none)"))));
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(Block::default().borders(Borders::ALL).title(" Result ")), now);
        }
    }

    // descarga
    if show_dl {
        if let Some(d) = &m.dl {
            let speed = d.speed().map_or("– MB/s".into(), |v| format!("{}/s", fmt_bytes(v as u64)));
            let eta = d.eta().map_or("ETA –".into(), |e| format!("ETA {}", fmt_dur(e)));
            let done = m.end.is_some() || m.cur.is_some_and(|i| m.rows[i].key != Key::Download);
            let got = if done { d.total.unwrap_or(d.got) } else { d.got };
            let of = d.total.map_or(fmt_bytes(got), |t| format!("{} / {}", fmt_bytes(got), fmt_bytes(t)));
            let pct = d.fraction().map_or(String::new(), |x| format!("{:>3.0}% · ", x * 100.0));
            let label = if done { format!("{of} · downloaded") } else { format!("{pct}{of} · {speed} · {eta}") };
            let frame = Block::default().borders(Borders::ALL).title(" Download ");
            let inner = frame.inner(dl).inner(ratatui::layout::Margin { horizontal: 1, vertical: 0 });
            f.render_widget(frame, dl);
            // sin glifos de bloque: cada celda es un espacio con color de fondo, así la barra mide lo mismo en toda su longitud
            let ratio = if done { 1.0 } else { d.fraction().unwrap_or(0.0) }.clamp(0.0, 1.0);
            let w = inner.width as usize;
            let filled = (w as f64 * ratio).round() as usize;
            let chars: Vec<char> = label.chars().take(w).collect();
            let start = (w - chars.len()) / 2;
            let spans: Vec<Span> = (0..w)
                .map(|i| {
                    let ch = if i >= start && i < start + chars.len() { chars[i - start] } else { ' ' };
                    let style = if i < filled { Style::new().fg(Color::Black).bg(Color::Cyan) } else { Style::new().fg(Color::White).bg(Color::DarkGray) };
                    Span::styled(ch.to_string(), style.add_modifier(Modifier::BOLD))
                })
                .collect();
            f.render_widget(Paragraph::new(Line::from(spans)), inner);
        }
    }

    // salida en vivo
    let inner = out.height.saturating_sub(2) as usize;
    let w = out.width.saturating_sub(2) as usize;
    m.view.set(inner);
    let max_top = m.tail.len().saturating_sub(inner);
    let skip = m.top.map_or(max_top, |t| t.min(max_top));
    let title = if m.top.is_some() { format!(" Live output — line {}/{} · End to follow ", (skip + inner).min(m.tail.len()), m.tail.len()) } else { " Live output ".to_string() };
    let lines: Vec<Line> = m.tail.iter().skip(skip).take(inner).map(|l| Line::raw(l.chars().take(w).collect::<String>())).collect();
    f.render_widget(Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title).title_bottom(" ↑↓ PgUp/PgDn Home/End: scroll ")), out);

    // pie
    let hint = match (&m.notice, &m.end) {
        (_, Some(_)) => " Press Enter, q or Ctrl+C to close — the summary stays in your terminal.".to_string(),
        (Some(n), None) => format!(" {n}"),
        (None, None) if m.critical() => " Ctrl+C is ignored from here on: the system is being changed.".into(),
        (None, None) => " Ctrl+C cancels safely: nothing has been changed yet.".into(),
    };
    let style = if m.notice.is_some() && m.end.is_none() { Style::new().fg(Color::Yellow) } else { Style::new().fg(Color::DarkGray) };
    f.render_widget(Clear, foot);
    f.render_widget(Paragraph::new(hint).style(style).wrap(Wrap { trim: true }), foot);
}

// ---------------------------------------------------------------- hilo de la UI

enum Msg {
    Ev(Event),
    Finish(Summary),
    Close,
}

pub struct Ui {
    tx: Sender<Msg>,
    th: std::thread::JoinHandle<()>,
}

/// ¿Se levanta la mini TUI? Solo con terminal en la entrada Y en la salida, sin `--json` y con un TERM que sepa pintar.
pub fn wanted(stdin_tty: bool, stdout_tty: bool, json: bool, term: Option<&str>) -> bool {
    stdin_tty && stdout_tty && !json && term.is_some_and(|t| !t.is_empty() && t != "dumb")
}

impl Ui {
    /// `None` si no se pudo preparar el terminal: quien llama usa entonces la salida plana.
    pub fn start(mut m: Live, cancel: &'static AtomicBool) -> Option<Ui> {
        let term = ratatui::try_init().ok()?;
        let (tx, rx) = channel();
        let th = std::thread::spawn(move || {
            let mut term = term;
            // Pase lo que pase (también un pánico) el terminal se deja como estaba.
            struct Restore;
            impl Drop for Restore {
                fn drop(&mut self) {
                    // sin `ratatui::restore()`: con el terminal ya cerrado (SIGHUP) imprimiría el error y podría hacer pánico
                    let _ = ratatui::try_restore();
                }
            }
            let _r = Restore;
            ui_loop(&mut term, &mut m, rx, cancel);
        });
        Some(Ui { tx, th })
    }

    /// El `on_event` del motor. Nunca falla ni bloquea: si la UI ya cerró, el evento se tira.
    pub fn sink(&self) -> impl FnMut(Event) {
        let tx = self.tx.clone();
        move |e| {
            let _ = tx.send(Msg::Ev(e));
        }
    }

    /// Con resumen: la pantalla se queda hasta que se pulsa una tecla. Sin él (la corrida no llegó a empezar) se cierra al instante.
    pub fn finish(self, s: Option<Summary>) {
        let _ = self.tx.send(match s {
            Some(s) => Msg::Finish(s),
            None => Msg::Close,
        });
        let _ = self.th.join();
    }
}

/// Espera hasta `ms` a que el terminal tenga algo que leer. Con el terminal cerrado `event::poll` de crossterm gira sin respetar el plazo (HUP permanente en el epoll), así que la espera la hace poll(2) y el HUP se ve aquí.
fn wait_tty(ms: i32) -> std::io::Result<bool> {
    let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
    // SAFETY: `p` es un único pollfd válido que vive durante la llamada.
    let r = unsafe { libc::poll(&mut p, 1, ms) };
    if r < 0 {
        let e = std::io::Error::last_os_error();
        return if e.kind() == std::io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
    }
    if r == 0 {
        return Ok(false);
    }
    if p.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
    }
    Ok(true)
}

fn ui_loop(term: &mut ratatui::DefaultTerminal, m: &mut Live, rx: Receiver<Msg>, cancel: &AtomicBool) {
    use crossterm::event::{self, Event as Ct, KeyCode, KeyEventKind, KeyModifiers};
    let t0 = Instant::now();
    let mut closed = false;
    let mut told = false;
    loop {
        let at = t0.elapsed();
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Ev(e) => m.event(at, &e),
                Msg::Finish(s) => m.finish(at, s),
                Msg::Close => closed = true,
            }
        }
        if closed {
            return;
        }
        let sig = update::CANCEL_SIGNAL.load(Ordering::SeqCst);
        // una señal que no vino del teclado (SSH cortado, kill): el aviso sale igual en la pantalla
        if cancel.load(Ordering::SeqCst) && !told && m.end.is_none() {
            told = true;
            if m.notice.is_none() {
                m.cancel_notice(sig);
            }
        }
        // SIGHUP: el terminal ya no existe; no se espera ninguna tecla para cerrar
        if sig == libc::SIGHUP && m.end.is_some() {
            return;
        }
        if let Some(p) = m.download_file() {
            let fake = pxu::share::mock::MOCK_DL.load(Ordering::SeqCst);
            if fake > 0 {
                m.observe_download(at, fake);
            } else if let Ok(md) = std::fs::symlink_metadata(&p) {
                m.observe_download(at, md.len());
            }
        }
        // un terminal roto no frena nada: el dibujo falla y se sigue
        let _ = term.draw(|f| draw(f, m, at));
        // sondeo corto: da el giro del spinner y deja pasar los eventos del motor
        let polled = match wait_tty(100) {
            Ok(true) => event::poll(Duration::ZERO),
            other => other,
        };
        match polled {
            Ok(true) => match event::read() {
                Ok(Ct::Key(k)) => {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl_c {
                        match m.ctrl_c() {
                            CtrlC::Cancel | CtrlC::Refuse => {
                                // también en la parte crítica la petición queda anotada: el motor la ignora hasta salir de ella
                                update::note_signal(libc::SIGINT);
                                told = true;
                                cancel.store(true, Ordering::SeqCst);
                            }
                            CtrlC::Close => return,
                        }
                    } else if m.scroll(k.code) {
                    } else if m.end.is_some() && matches!(k.code, KeyCode::Enter | KeyCode::Char('q') | KeyCode::Esc) {
                        return;
                    }
                }
                Ok(_) => {}
                // poll dijo que había evento y leerlo falla (terminal cerrado): igual que un poll fallido
                Err(_) => {
                    if m.end.is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            },
            Ok(false) => {}
            // el terminal falló (cerrado): sin tecla que esperar si la corrida ya acabó; si no, no girar en vacío
            Err(_) => {
                if m.end.is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pxu::share::update::nvidia::{Note, Plan};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }
    fn ev(m: &mut Live, at: u64, e: Event) {
        m.event(s(at), &e);
    }
    fn line(m: &mut Live, at: u64, vmid: u32, l: &str) {
        ev(m, at, Event::Line(vmid, l.into()));
    }

    fn render(m: &Live, at: u64) -> String {
        render_w(m, at, 100)
    }

    fn render_w(m: &Live, at: u64, w: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, 32)).unwrap();
        t.draw(|f| draw(f, m, s(at))).unwrap();
        let b = t.backend().buffer().clone();
        (0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string()).collect::<Vec<_>>().join("\n")
    }

    fn plan() -> Plan {
        Plan { from: "595.58.03".into(), to: "595.112.07".into(), host: true, stop: vec![(101, "plex".into()), (102, "ollama".into())], libs: vec![(101, "plex".into()), (103, "comfy".into())], fresh: false }
    }

    /// El motor, tal como habla en `nvidia::body` (mismos `$ …`, `== …` y avisos), hasta el inicio de la descarga.
    fn start_download(m: &mut Live) {
        ev(m, 0, Event::Nv(Note::Plan(plan())));
        line(m, 0, 0, "pxu NVIDIA driver update · 20261112 · host 595.58.03 (open) → 595.112.07");
        ev(m, 0, Event::Step(0, Step::Preflight));
        line(m, 1, 0, "$ install -d -m 0700 -o root -g root /var/lib/pxu-nvidia");
        line(m, 1, 0, "$ df -Pk /var/lib/pxu-nvidia");
        line(m, 2, 0, "$ curl -fsS --proto =https --max-time 30 https://download.nvidia.com/XFree86/Linux-x86_64/595.112.07/NVIDIA-Linux-x86_64-595.112.07.run.sha256sum");
        ev(m, 3, Event::Step(0, Step::Refresh));
        line(m, 3, 0, "$ curl -fsSIL --proto =https --max-time 30 https://download.nvidia.com/XFree86/Linux-x86_64/595.112.07/NVIDIA-Linux-x86_64-595.112.07.run");
        ev(m, 3, Event::Nv(Note::Size(423_000_000)));
        line(m, 3, 0, "download size: 423000000 bytes");
        line(m, 4, 0, "$ curl -fsSL --proto =https --retry 3 --max-time 3600 -o /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run https://download.nvidia.com/…");
    }

    fn fill(m: &mut Live, n: usize) {
        for i in 0..n {
            line(m, 5, 0, &format!("row-{i:04}"));
        }
    }

    #[test]
    fn scrolling_up_and_down_stays_within_the_buffer() {
        use crossterm::event::KeyCode::*;
        let mut m = Live::new("t", false);
        fill(&mut m, 50);
        m.view.set(10);
        assert!(!m.scroll(Char('x')));
        assert!(m.scroll(Up));
        assert_eq!(m.top, Some(39));
        assert!(m.scroll(PageUp));
        assert_eq!(m.top, Some(29));
        m.scroll(Home);
        m.scroll(Up);
        assert_eq!(m.top, Some(0), "no se pasa por arriba");
        m.scroll(PageDown);
        assert_eq!(m.top, Some(10));
        m.scroll(Down);
        assert_eq!(m.top, Some(11));
        m.scroll(End);
        assert_eq!(m.top, None);
        m.scroll(Down);
        assert_eq!(m.top, None, "no se pasa por abajo");
        m.scroll(Up);
        m.scroll(PageDown);
        assert_eq!(m.top, None, "bajar hasta el fondo vuelve a seguir en vivo");
    }

    #[test]
    fn the_view_stays_put_when_new_lines_arrive() {
        use crossterm::event::KeyCode::*;
        let mut m = Live::new("t", false);
        fill(&mut m, 50);
        render(&m, 5);
        m.scroll(Home);
        m.scroll(PageDown);
        let before = render(&m, 5);
        assert!(before.contains("End to follow"), "{before}");
        fill(&mut m, 20);
        assert_eq!(render(&m, 5), before.replace("/50", "/70"));
        m.scroll(End);
        assert!(render(&m, 7).contains("row-0019"));
    }

    #[test]
    fn a_full_buffer_keeps_the_anchor() {
        use crossterm::event::KeyCode::*;
        let mut m = Live::new("t", false);
        fill(&mut m, TAIL_CAP);
        m.view.set(10);
        m.scroll(Home);
        m.scroll(PageDown);
        let top = m.tail[m.top.unwrap()].clone();
        fill(&mut m, 5);
        assert_eq!(m.tail[m.top.unwrap()], top);
        fill(&mut m, TAIL_CAP);
        assert_eq!(m.top, Some(0), "la línea anclada ya se descartó: se queda en lo más viejo");
    }

    #[test]
    fn speed_and_eta_come_from_the_recent_samples() {
        let mut d = Download { total: Some(100_000_000), ..Default::default() };
        assert_eq!((d.speed(), d.eta()), (None, None), "no samples yet");
        d.observe(s(0), 0);
        assert_eq!(d.speed(), None, "one sample is not a rate");
        d.observe(s(5), 10_000_000);
        assert_eq!(d.speed(), Some(2_000_000.0));
        assert_eq!(d.eta(), Some(s(45)), "90 MB left at 2 MB/s");
        assert_eq!(d.fraction(), Some(0.1));
        // pasada la ventana solo cuentan las muestras recientes: ahora va a 10 MB/s
        d.observe(s(10), 20_000_000);
        d.observe(s(15), 70_000_000);
        assert_eq!(d.speed(), Some(10_000_000.0), "{:?}", d.samples);
        assert_eq!(d.eta(), Some(s(3)));
        // parada: sin ETA en vez de «infinito»
        let mut z = Download { total: Some(10), ..Default::default() };
        z.observe(s(0), 5);
        z.observe(s(3), 5);
        assert_eq!((z.speed(), z.eta()), (Some(0.0), None));
        // sin total no hay porcentaje ni ETA, pero sí velocidad
        let mut n = Download::default();
        n.observe(s(0), 0);
        n.observe(s(2), 4_000_000);
        assert_eq!((n.fraction(), n.eta()), (None, None));
        assert_eq!(n.speed(), Some(2_000_000.0));
    }

    #[test]
    fn formats_bytes_and_durations() {
        assert_eq!(fmt_bytes(423_000_000), "423.0 MB");
        assert_eq!(fmt_bytes(1_500_000_000), "1.50 GB");
        assert_eq!(fmt_bytes(12_000), "12 kB");
        assert_eq!((fmt_dur(s(75)), fmt_dur(s(3725))), ("1:15".into(), "1:02:05".into()));
    }

    #[test]
    fn mid_download_shows_the_bar_the_speed_the_eta_and_what_it_is_doing() {
        let mut m = Live::nvidia();
        start_download(&mut m);
        m.observe_download(s(4), 0);
        m.observe_download(s(9), 10_000_000);
        m.observe_download(s(14), 20_000_000);
        assert_eq!(m.download_file().as_deref(), Some("/var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run"));
        let t = render(&m, 14);
        println!("--- mid-download\n{t}");
        for want in ["Host Updates · With NVIDIA Driver", "Download driver 595.112.07", "20.0 MB / 423.0 MB", "2.0 MB/s", "ETA 3:21", "  5%", "Downloading the NVIDIA driver 595.112.07 from download.nvidia.com (423.0 MB)", "Ctrl+C cancels safely", "$ curl -fsSL"] {
            assert!(t.contains(want), "missing {want:?} in:\n{t}");
        }
        assert_eq!(m.states()[..3], [St::Ok, St::Running, St::Pending]);
        assert!(!m.critical());
    }

    #[test]
    fn the_steps_advance_with_the_engines_commands_and_stopping_the_system_is_not_cancellable() {
        let mut m = Live::nvidia();
        start_download(&mut m);
        line(&mut m, 200, 0, "$ sha256sum /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run");
        line(&mut m, 210, 0, "$ sh /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run --check");
        line(&mut m, 215, 0, "$ sh /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run --extract-only --target /var/lib/pxu-nvidia/extract-595.112.07");
        line(&mut m, 230, 0, "$ make -C /var/lib/pxu-nvidia/extract-595.112.07/kernel-open -j4 SYSSRC=/lib/modules/6.17/build modules");
        assert!(!m.critical(), "the test build still leaves the system untouched");
        assert_eq!(m.ctrl_c(), CtrlC::Cancel);
        m.notice = None;
        ev(&mut m, 250, Event::Step(0, Step::Upgrade));
        line(&mut m, 250, 0, "$ pct shutdown 101 --timeout 60");
        assert!(m.critical());
        assert_eq!(m.ctrl_c(), CtrlC::Refuse);
        assert!(m.notice.as_deref().unwrap().contains("waits until the whole change finishes"), "{:?}", m.notice);
        line(&mut m, 270, 0, "$ modprobe -r nvidia_uvm nvidia_drm nvidia_modeset nvidia");
        line(&mut m, 280, 0, "$ sh /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run --no-questions --ui=none --no-nouveau-check --dkms --kernel-module-type=open");
        line(&mut m, 281, 0, "Installing NVIDIA driver version 595.112.07.");
        let t = render(&m, 300);
        println!("--- installing\n{t}");
        assert!(t.contains("Unloading the old NVIDIA module and running the installer"), "{t}");
        assert!(t.contains("Installing NVIDIA driver version 595.112.07."), "{t}");
        assert_eq!(m.states()[..6], [St::Ok, St::Ok, St::Ok, St::Ok, St::Ok, St::Running]);
    }

    #[test]
    fn inside_a_ct_it_says_which_ct_and_what_it_is_doing_there() {
        let mut m = Live::nvidia();
        start_download(&mut m);
        for (at, c) in [(10, "sha256sum x"), (11, "sh x --check"), (12, "sh x --extract-only"), (13, "make -C x modules"), (20, "pct shutdown 101 --timeout 60"), (30, "modprobe -r nvidia"), (31, "sh x --dkms"), (500, "nvidia-smi --query-gpu=driver_version --format=csv,noheader")] {
            line(&mut m, at, 0, &format!("$ {c}"));
        }
        line(&mut m, 520, 0, "nvidia-smi: driver 595.112.07 OK");
        // `pct start 102` (CT sin librerías) cierra el paso nvidia-smi y abre «arrancar de nuevo»
        line(&mut m, 521, 0, "$ pct start 102");
        line(&mut m, 540, 101, "$ pct start 101");
        line(&mut m, 545, 101, "$ [CT 101] df -Pk /var/lib");
        line(&mut m, 560, 101, "$ pct push 101 /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run /var/lib/pxu-nvidia.AbCd1234/nvidia.run --user 0 --group 0 --perms 0700");
        line(&mut m, 580, 101, "$ [CT 101] sh /var/lib/pxu-nvidia.AbCd1234/nvidia.run --no-kernel-modules --no-questions");
        line(&mut m, 581, 101, "Installing 'NVIDIA Accelerated Graphics Driver for Linux-x86_64' (595.112.07)");
        let t = render(&m, 600);
        println!("--- inside CT 101\n{t}");
        assert!(t.contains("CT 101: running the libraries installer"), "{t}");
        assert!(t.contains("CT 101 (plex): update the driver libraries"), "{t}");
        assert!(t.contains("CT 103 (comfy)"), "{t}");
        assert!(t.contains("Stop GPU CTs (101, 102)") && t.contains("Start the stopped CTs again"), "{t}");
        let st = m.states();
        assert_eq!(st[st.len() - 4..], [St::Ok, St::Running, St::Pending, St::Pending][..], "{st:?}");
        // fin de CT 101 y paso a 103
        ev(&mut m, 590, Event::Nv(Note::Ct { vmid: 101, outcome: Outcome::Ok, detail: "libs 595.112.07 installed, CT restarted".into() }));
        line(&mut m, 600, 103, "$ [CT 103] df -Pk /var/lib");
        assert!(render(&m, 601).contains("CT 103: checking free space in the container."));
    }

    #[test]
    fn every_block_says_whether_it_runs_on_the_host_or_in_a_ct() {
        let mut m = Live::nvidia();
        line(&mut m, 1, 0, "$ pct shutdown 129 --timeout 60");
        line(&mut m, 2, 0, "$ sh /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run --no-questions --dkms");
        line(&mut m, 3, 0, "Welcome to the NVIDIA Software Installer for Unix/Linux");
        line(&mut m, 4, 129, "$ [CT 129] sh /var/lib/x/nvidia.run --no-kernel-modules");
        line(&mut m, 5, 129, "Installing driver");
        line(&mut m, 6, 129, "$ pct start 129");
        line(&mut m, 7, 129, "started");
        let t: Vec<&str> = m.tail.iter().map(String::as_str).collect();
        assert!(t[0].starts_with("HOST → CT 129") && t[0].ends_with("$ pct shutdown 129 --timeout 60"), "{t:?}");
        assert!(t[1].starts_with("HOST ") && t[1].contains("$ sh /var/lib/pxu-nvidia/"), "{t:?}");
        assert!(t[2].starts_with("HOST ") && t[2].ends_with("Welcome to the NVIDIA Software Installer for Unix/Linux"), "{t:?}");
        assert!(t[3].starts_with("CT 129 ") && t[3].ends_with("$ sh /var/lib/x/nvidia.run --no-kernel-modules") && !t[3].contains("[CT 129]"), "{t:?}");
        assert!(t[4].starts_with("CT 129 ") && t[4].ends_with("Installing driver"), "{t:?}");
        assert!(t[5].starts_with("HOST → CT 129"), "{t:?}");
        assert!(t[6].starts_with("HOST ") && !t[6].starts_with("HOST →"), "{t:?}");
        // la │ cae en la misma columna (en caracteres) para las tres etiquetas
        let cols: Vec<usize> = t.iter().map(|l| l.chars().position(|c| c == '│').unwrap()).collect();
        assert!(cols.iter().all(|&c| c == cols[0]), "{cols:?}\n{t:?}");
    }

    #[test]
    fn the_separator_column_fits_a_four_digit_ct_id() {
        let mut m = Live::nvidia();
        line(&mut m, 1, 0, "$ pct shutdown 1234");
        line(&mut m, 2, 0, "$ modprobe -r nvidia");
        line(&mut m, 3, 1234, "$ [CT 1234] nvidia-smi");
        let cols: Vec<usize> = m.tail.iter().map(|l| l.chars().position(|c| c == '│').unwrap()).collect();
        assert!(m.tail[0].starts_with("HOST → CT 1234│"), "{:?}", m.tail);
        assert!(cols.iter().all(|&c| c == cols[0]), "{cols:?}");
    }

    #[test]
    fn the_skipped_ct_line_and_a_redrawn_progress_bar_reach_the_panel_clean() {
        let mut m = Live::nvidia();
        line(&mut m, 1, 0, "CT 129 (llama-swap): no libraries (already at the target version)");
        // lo que llega ya pasó por el motor: solo el último estado de la barra
        let bar = pxu::share::fmt::last_redraw("[                              ]   0%\r[#                             ]   0%\r[##############################] 100%\r");
        line(&mut m, 2, 0, bar);
        let t = m.tail.iter().cloned().collect::<Vec<_>>().join("\n");
        assert!(t.contains("CT 129 (llama-swap): no libraries (already at the target version)"), "{t}");
        assert!(t.ends_with("│ [##############################] 100%") && !t.contains("  0%"), "{t}");
    }

    fn ok_summary() -> Summary {
        Summary { outcome: Outcome::Ok, lines: vec!["NVIDIA driver 595.58.03 → 595.112.07: host driver 595.58.03 → 595.112.07; CT updated: 129, 130".into()], took: s(342), log: Some("/var/log/pxu/run-20261112-224500.log".into()) }
    }

    #[test]
    fn a_successful_run_ends_with_a_summary_the_duration_and_the_log() {
        let mut m = Live::nvidia();
        start_download(&mut m);
        line(&mut m, 340, 0, "$ rm -rf -- /var/lib/pxu-nvidia");
        m.finish(s(342), ok_summary());
        let t = render(&m, 400);
        println!("--- finished ok\n{t}");
        assert!(t.contains("✓ Finished OK in 5:42"), "{t}");
        assert!(t.contains("Log: /var/log/pxu/run-20261112-224500.log"), "{t}");
        assert!(t.contains("Press Enter, q or Ctrl+C to close"), "{t}");
        assert!(m.states().iter().all(|s| matches!(s, St::Ok | St::Skipped)), "{:?}", m.states());
        assert_eq!(m.ctrl_c(), CtrlC::Close);
    }

    #[test]
    fn a_failure_keeps_the_failed_step_the_reason_and_the_last_output() {
        let mut m = Live::nvidia();
        start_download(&mut m);
        line(&mut m, 200, 0, "$ sha256sum /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run");
        line(&mut m, 201, 0, "!! SHA-256 mismatch for NVIDIA-Linux-x86_64-595.112.07.run: the download does not match NVIDIA's checksum; nothing was installed");
        line(&mut m, 202, 0, "$ rm -rf -- /var/lib/pxu-nvidia");
        m.finish(s(203), Summary { outcome: Outcome::Failed, lines: vec!["NVIDIA driver 595.58.03 → 595.112.07: SHA-256 mismatch for NVIDIA-Linux-x86_64-595.112.07.run".into()], took: s(203), log: Some("/var/log/pxu/run-x.log".into()) });
        let t = render(&m, 300);
        println!("--- failed\n{t}");
        assert!(t.contains("✘ FAILED at \"Verify the download (SHA-256 and installer check)\" after 3:23"), "{t}");
        assert!(t.contains("Why: SHA-256 mismatch"), "{t}");
        assert!(t.contains("$ sha256sum"), "the last output stays visible:\n{t}");
        let st = m.states();
        assert_eq!(st[2], St::Failed);
        assert_eq!(*st.last().unwrap(), St::Ok, "cleanup still ran");
        assert!(st[3..st.len() - 1].iter().all(|s| *s == St::Skipped), "{st:?}");
    }

    #[test]
    fn a_failed_row_keeps_its_full_time_column_at_80_columns() {
        let mut m = Live::nvidia();
        start_download(&mut m);
        line(&mut m, 200, 0, "$ sha256sum /var/lib/pxu-nvidia/NVIDIA-Linux-x86_64-595.112.07.run");
        line(&mut m, 201, 0, "!! SHA-256 mismatch for NVIDIA-Linux-x86_64-595.112.07.run: the download does not match NVIDIA's checksum; nothing was installed");
        m.finish(s(203), Summary { outcome: Outcome::Failed, lines: vec!["SHA-256 mismatch".into()], took: s(203), log: None });
        for w in [80u16, 60] {
            let t = render_w(&m, 300, w);
            println!("--- failed {w} cols\n{t}");
            let row = |mark: char| t.lines().find(|l| l.contains(mark) && l.starts_with('│')).unwrap().to_string();
            let (bad, ok) = (row('✘'), row('✓'));
            assert!(bad.contains('…'), "the step name is truncated:\n{t}");
            assert!(bad.ends_with("0:01│"), "the time stays complete:\n{t}");
            assert_eq!(bad.chars().count(), ok.chars().count(), "aligned with the other rows:\n{t}");
        }
    }

    #[test]
    fn a_reboot_warning_is_shown_as_a_warning_not_a_failure() {
        let mut m = Live::nvidia();
        start_download(&mut m);
        for c in ["sha256sum x", "sh x --check", "make -C x modules", "pct shutdown 101", "sh x --dkms", "nvidia-smi --query"] {
            line(&mut m, 100, 0, &format!("$ {c}"));
        }
        line(&mut m, 110, 0, "!! the installer finished but nvidia-smi reports nothing instead of 595.112.07: REBOOT the host");
        m.finish(s(120), Summary { outcome: Outcome::Warn, lines: vec!["REBOOT the host".into()], took: s(120), log: None });
        assert!(m.states().contains(&St::Warn) && !m.states().contains(&St::Failed));
        assert!(render(&m, 120).contains("! Finished with warnings"));
    }

    #[test]
    fn the_apt_update_uses_the_engines_steps_and_refuses_ctrl_c_once_it_installs() {
        let mut m = Live::apt();
        ev(&mut m, 0, Event::Step(0, Step::Lock));
        ev(&mut m, 1, Event::Step(0, Step::Preflight));
        ev(&mut m, 5, Event::Step(0, Step::Backup));
        ev(&mut m, 9, Event::Step(0, Step::Refresh));
        assert_eq!(m.ctrl_c(), CtrlC::Cancel);
        ev(&mut m, 30, Event::Step(0, Step::Upgrade));
        line(&mut m, 31, 0, "$ apt-get -y dist-upgrade");
        line(&mut m, 40, 0, "Unpacking pve-kernel-6.17 (6.17.13-2) ...");
        assert_eq!(m.ctrl_c(), CtrlC::Refuse);
        let t = render(&m, 50);
        println!("--- apt upgrade\n{t}");
        assert!(t.contains("Unpacking pve-kernel-6.17") && t.contains("Running apt full-upgrade"), "{t}");
        assert_eq!(m.states()[..4], [St::Ok, St::Ok, St::Ok, St::Running]);
        assert!(!t.contains("Download ─"), "no download bar for apt");
    }

    #[test]
    fn the_cancel_notice_names_the_signal_and_says_what_will_happen() {
        // antes de instalar: corta ya
        let mut m = Live::nvidia();
        start_download(&mut m);
        line(&mut m, 60, 0, "$ make -C /var/lib/pxu-nvidia/extract-595.112.07/kernel-open -j4 SYSSRC=/lib/modules/6.17/build modules");
        assert_eq!(m.ctrl_c(), CtrlC::Cancel);
        let t = render(&m, 61);
        println!("--- cancel requested during the test build (Ctrl+C)\n{t}");
        assert!(t.contains("cancel requested (SIGINT: Ctrl+C): stopping now; nothing on the system has been changed."), "{t}");
        // una señal de fuera (SSH cortado) da el mismo aviso con su nombre
        let mut m = Live::nvidia();
        start_download(&mut m);
        m.cancel_notice(libc::SIGHUP);
        let t = render(&m, 61);
        assert!(t.contains("cancel requested (SIGHUP: the terminal or SSH session closed)"), "{t}");
        // instalando: se espera y se dice hasta dónde
        let mut m = Live::nvidia();
        start_download(&mut m);
        ev(&mut m, 250, Event::Step(0, Step::Upgrade));
        line(&mut m, 250, 0, "$ pct shutdown 101 --timeout 60");
        assert_eq!(m.ctrl_c(), CtrlC::Refuse);
        let t = render(&m, 260);
        println!("--- cancel requested while the system is being changed\n{t}");
        assert!(t.contains("cancel requested (SIGINT: Ctrl+C): the system is being changed") && t.contains("pxu waits until the whole change"), "{t}");
        // ancho estrecho: el aviso se parte en líneas, no se corta
        let t = render_w(&m, 260, 60);
        assert!(t.contains("recorded") && t.contains("SIGINT"), "{t}");
    }

    #[test]
    fn plain_output_is_kept_without_a_terminal_or_with_json() {
        assert!(wanted(true, true, false, Some("xterm-256color")));
        assert!(!wanted(false, true, false, Some("xterm")), "stdin is a pipe");
        assert!(!wanted(true, false, false, Some("xterm")), "stdout is a pipe");
        assert!(!wanted(true, true, true, Some("xterm")), "--json");
        assert!(!wanted(true, true, false, Some("dumb")) && !wanted(true, true, false, None));
    }

    #[test]
    fn a_tiny_terminal_does_not_panic() {
        let mut m = Live::nvidia();
        start_download(&mut m);
        for (w, h) in [(20, 5), (60, 12), (200, 60), (1, 1)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &m, s(1))).unwrap();
        }
    }
}
