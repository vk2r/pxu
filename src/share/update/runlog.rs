//! Un solo log por corrida: `/var/log/pxu/run-AAAAMMDD-HHMM[SS][-N].log` (hora UTC en el nombre).
//! Se crea al empezar la corrida (cabecera mínima: `pxu-run-log 1`, `started`, `node`, línea `---`) y crece mientras corre: una sección por
//! host / NVIDIA / CT con hora local por línea. Al terminar se reescribe entero: cabecera completa (`duration`, `result`), las secciones y al
//! final el mismo resumen del aviso. Un log sin `result` (corte brusco, caída del host) se lista como `⛔ INCOMPLETE`. Retención por días (`[general] log_retention_days`) y lectura para la página Logs de la TUI.
use super::{report, stamp, Event, RunReport, LOG_DIR};
use crate::share::host::Host;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

pub const MAGIC: &str = "pxu-run-log 1";
/// Líneas por sección que se guardan (como el tope en memoria del motor).
const SECTION_CAP: usize = 20_000;
/// Tope de líneas que carga el visor y de logs que lista la página.
pub const VIEW_MAX_LINES: usize = 100_000;
pub const LIST_MAX: usize = 1000;
const HEAD_BYTES: usize = 4096;

// ---------------------------------------------------------------- hora local

/// Segundos que hay que sumar a un epoch UTC para ver la hora local del servidor.
pub fn local_offset(epoch: u64) -> i64 {
    let t = epoch as libc::time_t;
    // SAFETY: `tm` es un struct plano que `localtime_r` rellena; `t` vive durante la llamada.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        tm.tm_gmtoff as i64
    } else {
        0
    }
}

fn local_stamp(epoch: u64) -> String {
    stamp((epoch as i64 + local_offset(epoch)).max(0) as u64)
}

/// `2026-11-10 06:12` (hora local del servidor).
pub fn local_ymd_hm(epoch: u64) -> String {
    let s = local_stamp(epoch);
    format!("{}-{}-{} {}:{}", &s[0..4], &s[4..6], &s[6..8], &s[8..10], &s[10..12])
}

pub fn local_hm(epoch: u64) -> String {
    let s = local_stamp(epoch);
    format!("{}:{}", &s[8..10], &s[10..12])
}

fn local_hms(epoch: u64) -> String {
    let s = local_stamp(epoch);
    format!("{}:{}:{}", &s[8..10], &s[10..12], &s[12..14])
}

// ---------------------------------------------------------------- nombres

fn digits(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_digit())
}

/// `run-AAAAMMDD-HHMM[SS][-N].log`: devuelve el epoch UTC de su fecha. Nada más que ese formato.
fn run_name_epoch(name: &str) -> Option<u64> {
    let r = name.strip_prefix("run-")?.strip_suffix(".log")?;
    let mut it = r.splitn(3, '-');
    let (d, t) = (it.next()?, it.next()?);
    let n = it.next();
    if !digits(d, 8) || !(digits(t, 4) || digits(t, 6)) || n.is_some_and(|n| n.is_empty() || n.len() > 3 || !n.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let t = if t.len() == 4 { format!("{t}00") } else { t.to_string() };
    epoch_of(&format!("{d}{t}"))
}

/// Epoch UTC de un `YYYYMMDDHHMMSS` (inverso de `stamp`).
fn epoch_of(s: &str) -> Option<u64> {
    if !digits(s, 14) {
        return None;
    }
    let n = |a: usize, b: usize| s[a..b].parse::<i64>().ok();
    let (y, m, d, hh, mm, ss) = (n(0, 4)?, n(4, 6)?, n(6, 8)?, n(8, 10)?, n(10, 12)?, n(12, 14)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let y = y - (m <= 2) as i64;
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

/// Epoch de un log de pxu del formato viejo (`host-14d.log`, `nvidia-14d.log`) o del actual.
fn file_epoch(name: &str) -> Option<u64> {
    if let Some(e) = run_name_epoch(name) {
        return Some(e);
    }
    for pre in ["host-", "nvidia-"] {
        if let Some(t) = name.strip_prefix(pre).and_then(|r| r.strip_suffix(".log")) {
            return epoch_of(t);
        }
    }
    None
}

/// Nombre `<epoch>.log` de un log por CT del formato viejo (`/var/log/pxu/<vmid>/<epoch>.log`).
fn legacy_ct_epoch(name: &str) -> Option<u64> {
    let t = name.strip_suffix(".log")?;
    (!t.is_empty() && t.len() <= 11 && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok()).flatten()
}

fn plain_path(p: &Path, dir: &Path) -> bool {
    p.parent() == Some(dir) && !p.components().any(|c| c == Component::ParentDir)
}

/// `/var/log/pxu/run-….log` con el formato exacto de `run_name_epoch`.
pub fn is_run_log_path(p: &Path) -> bool {
    plain_path(p, Path::new(LOG_DIR)) && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| run_name_epoch(n).is_some())
}

/// Cualquier archivo que pxu escribe en `/var/log/pxu` (actual o viejo): lo único que la retención puede borrar.
pub fn is_pxu_log_path(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if p.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    if p.parent() == Some(Path::new(LOG_DIR)) {
        return file_epoch(name).is_some();
    }
    p.parent().and_then(|d| d.parent()) == Some(Path::new(LOG_DIR)) && p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()).is_some_and(vmid_dir) && legacy_ct_epoch(name).is_some()
}

/// `/var/log/pxu/<vmid>`: la carpeta por CT del formato viejo.
pub fn is_legacy_ct_dir(p: &Path) -> bool {
    p.parent() == Some(Path::new(LOG_DIR)) && p.file_name().and_then(|n| n.to_str()).is_some_and(vmid_dir)
}

fn vmid_dir(n: &str) -> bool {
    !n.is_empty() && n.len() <= 10 && n.bytes().all(|b| b.is_ascii_digit())
}

/// Ruta libre para esta corrida: `run-AAAAMMDD-HHMM.log`; si existe, con segundos; si aún existe, `-2`, `-3`…
fn unique_path(host: &dyn Host, dir: &Path, started_at: u64) -> PathBuf {
    let s = stamp(started_at);
    let mut p = dir.join(format!("run-{}-{}.log", &s[..8], &s[8..12]));
    if host.exists(&p) {
        p = dir.join(format!("run-{}-{}.log", &s[..8], &s[8..14]));
    }
    let mut n = 2;
    while host.exists(&p) && n < 100 {
        p = dir.join(format!("run-{}-{}-{n}.log", &s[..8], &s[8..14]));
        n += 1;
    }
    p
}

// ---------------------------------------------------------------- grabación

struct Section {
    title: String,
    lines: Vec<(u64, String)>,
    dropped: usize,
    /// Ya salió su `── título ──` en el texto en vivo.
    shown: bool,
}

/// Recoge los eventos de la corrida (cada línea que dice el motor, ya saneada) en secciones: host, NVIDIA, un CT cada una.
#[derive(Default)]
pub struct Recorder {
    names: HashMap<u32, String>,
    sections: Vec<Section>,
    /// Texto en vivo todavía sin añadir al archivo (`take_pending`).
    pending: String,
}

impl Recorder {
    pub fn begin(&mut self, title: &str) {
        self.sections.push(Section { title: title.into(), lines: Vec::new(), dropped: 0, shown: false });
    }

    pub fn feed(&mut self, now: u64, e: &Event) {
        match e {
            Event::Queue(q) => self.names.extend(q.iter().cloned()),
            Event::Start(0) => self.begin("host"),
            Event::Start(v) => {
                let t = match self.names.get(v) {
                    Some(n) => format!("CT {v} {n}"),
                    None => format!("CT {v}"),
                };
                self.begin(&t);
            }
            Event::Line(_, l) => {
                if self.sections.is_empty() {
                    self.begin("run");
                }
                let s = self.sections.last_mut().expect("section exists");
                if s.lines.len() >= SECTION_CAP {
                    s.dropped += 1;
                } else {
                    s.lines.push((now, l.clone()));
                    if !s.shown {
                        s.shown = true;
                        self.pending += &format!("\n── {} ──\n", s.title);
                    }
                    self.pending += &format!("{}  {}\n", local_hms(now), l);
                }
            }
            Event::Step(..) | Event::Done(_) => {}
        }
    }
}

impl Recorder {
    /// Lo que se dijo desde la última vez, listo para añadir al archivo en vivo.
    pub fn take_pending(&mut self) -> String {
        std::mem::take(&mut self.pending)
    }
}

/// Cabecera mínima del log en vivo: sin `result`, así que si la corrida se corta queda como INCOMPLETE.
fn live_header(started: u64, node: &str) -> String {
    format!("{MAGIC}\nstarted: {started}\nnode: {}\n---\n", node.replace(['\n', '\r'], " "))
}

/// Crea el log de la corrida con su cabecera mínima y devuelve su ruta (`None` si no se puede escribir: la corrida sigue sin log).
pub fn begin_live(host: &dyn Host, started_at: u64, node: &str) -> Option<PathBuf> {
    let path = unique_path(host, Path::new(LOG_DIR), started_at);
    host.write_file(&path, live_header(started_at, node).as_bytes()).ok()?;
    Some(path)
}

/// Alimenta el recorder con un evento y añade al archivo en vivo lo nuevo; si añadir falla, el log en vivo se abandona (se escribirá entero al final).
pub fn feed_live(host: &dyn Host, rec: &std::cell::RefCell<Recorder>, live: &std::cell::RefCell<Option<PathBuf>>, now: u64, e: &Event) {
    let text = {
        let mut r = rec.borrow_mut();
        r.feed(now, e);
        r.take_pending()
    };
    if !text.is_empty() {
        let mut l = live.borrow_mut();
        if l.as_ref().is_some_and(|p| host.append_file(p, text.as_bytes()).is_err()) {
            *l = None;
        }
    }
}

/// Texto completo del log: cabecera, secciones y, al final, el resumen (lo mismo que el aviso).
pub fn render(rep: &RunReport, rec: &Recorder) -> String {
    let mut out = format!(
        "{MAGIC}\nstarted: {}\nduration: {}\nresult: {}\nnode: {}\n---\n",
        rep.started_at,
        rep.finished_at.saturating_sub(rep.started_at),
        report::result_line(rep),
        rep.node.replace(['\n', '\r'], " "),
    );
    for s in rec.sections.iter().filter(|s| !s.lines.is_empty()) {
        out += &format!("\n── {} ──\n", s.title);
        for (t, l) in &s.lines {
            out += &format!("{}  {}\n", local_hms(*t), l);
        }
        if s.dropped > 0 {
            out += &format!("[{} more lines dropped: cap {SECTION_CAP}]\n", s.dropped);
        }
    }
    out += &format!("\n── summary ──\n{}\n\n{}\n", report::title(rep), report::body(rep));
    out
}

/// Cierra la corrida: reescribe el log en vivo (`live`) con la cabecera y el resumen finales (también si se detuvo o se canceló),
/// anota su ruta en el informe y aplica la retención. Sin log en vivo, crea uno nuevo.
pub fn finalize(host: &dyn Host, retention_days: u32, live: Option<PathBuf>, rec: &Recorder, rep: &mut RunReport) {
    let path = live.unwrap_or_else(|| unique_path(host, Path::new(LOG_DIR), rep.started_at));
    rep.log_path = Some(path.to_string_lossy().into_owned());
    let text = render(rep, rec);
    if host.write_file(&path, text.as_bytes()).is_err() {
        rep.log_path = None;
    }
    let lp = rep.log_path.clone();
    for r in &mut rep.results {
        r.log_path = lp.clone();
    }
    if let Some(n) = rep.nvidia.as_mut() {
        n.log_path = lp;
    }
    let finished = host.now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(rep.finished_at);
    prune(host, Path::new(LOG_DIR), retention_days, finished);
}

// ---------------------------------------------------------------- retención

/// Borra los logs de pxu con más de `days` días (fecha del nombre, UTC); `0` = no borra nada. Incluye el formato viejo
/// (`host-…`, `nvidia-…`, `<vmid>/<epoch>.log`) y las carpetas `<vmid>` que queden vacías. Solo archivos con nombre de pxu dentro de `dir`
/// (sin seguir symlinks, sin salir de él). Devuelve cuántos archivos borró.
pub fn prune(host: &dyn Host, dir: &Path, days: u32, now: u64) -> usize {
    prune_in(host, dir, days, now)
}

/// Lo mismo con el directorio a elegir (las pruebas usan una carpeta temporal con el host real).
pub fn prune_in(host: &dyn Host, dir: &Path, days: u32, now: u64) -> usize {
    if days == 0 {
        return 0;
    }
    let cutoff = now.saturating_sub(days as u64 * 86400);
    let regular = |p: &Path| host.file_meta(p).is_ok_and(|m| !m.is_dir);
    let mut n = 0;
    for p in host.read_dir(dir).unwrap_or_default() {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let Ok(meta) = host.file_meta(&p) else { continue }; // symlinks y lo ilegible: no se tocan
        if !meta.is_dir {
            if file_epoch(&name).is_some_and(|e| e < cutoff) && host.remove_file(&p).is_ok() {
                n += 1;
            }
        } else if vmid_dir(&name) {
            for f in host.read_dir(&p).unwrap_or_default() {
                let fname = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if legacy_ct_epoch(fname).is_some_and(|e| e < cutoff) && regular(&f) && host.remove_file(&f).is_ok() {
                    n += 1;
                }
            }
            if host.read_dir(&p).is_ok_and(|v| v.is_empty()) {
                let _ = host.remove_empty_dir(&p);
            }
        }
    }
    n
}

// ---------------------------------------------------------------- lectura (página Logs)

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogMeta {
    pub started: u64,
    pub duration: u64,
    pub result: String,
    pub node: String,
    /// `false`: la cabecera no tiene `result` (la corrida no llegó a cerrar el log).
    pub complete: bool,
}

/// Resultado que se muestra de un log que nunca se cerró.
pub const INCOMPLETE: &str = "⛔ INCOMPLETE";

/// Lee solo la cabecera (hasta la línea `---`). `None` si no es un log de pxu o está truncado. Sin `result`: INCOMPLETE.
pub fn parse_header(text: &str) -> Option<LogMeta> {
    let mut lines = text.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    let (mut started, mut duration, mut result, mut node) = (None, None::<u64>, None, String::new());
    for l in lines {
        if l == "---" {
            return Some(match result {
                Some(result) => LogMeta { started: started?, duration: duration?, result, node, complete: true },
                None => LogMeta { started: started?, duration: 0, result: INCOMPLETE.into(), node, complete: false },
            });
        }
        match l.split_once(": ") {
            Some(("started", v)) => started = v.parse().ok(),
            Some(("duration", v)) => duration = v.parse().ok(),
            Some(("result", v)) => result = Some(crate::share::fmt::clean(v)),
            Some(("node", v)) => node = crate::share::fmt::clean(v),
            _ => {}
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    /// `None`: la cabecera falta o está dañada (se lista igual, con la fecha del nombre).
    pub meta: Option<LogMeta>,
    pub started: u64,
}

/// Los `run-*.log` del directorio, del más nuevo al más viejo. Solo se lee la cabecera de cada uno.
pub fn list(host: &dyn Host, dir: &Path) -> Vec<LogEntry> {
    let mut v: Vec<LogEntry> = host
        .read_dir(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_string();
            let from_name = run_name_epoch(&name)?;
            let (head, size) = host.read_head(&p, HEAD_BYTES).ok()?;
            let meta = parse_header(&String::from_utf8_lossy(&head));
            let started = meta.as_ref().map_or(from_name, |m| m.started);
            Some(LogEntry { path: p, name, size, meta, started })
        })
        .collect();
    v.sort_by(|a, b| b.started.cmp(&a.started).then_with(|| b.name.cmp(&a.name)));
    v.truncate(LIST_MAX);
    v
}

/// Contenido de un log para el visor: líneas saneadas, con tope (`VIEW_MAX_LINES`). Solo rutas de `run-*.log`.
pub fn read_lines(host: &dyn Host, path: &Path) -> Result<Vec<String>, String> {
    if !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| run_name_epoch(n).is_some()) || path.components().any(|c| c == Component::ParentDir) {
        return Err(format!("{}: not a pxu run log", path.display()));
    }
    let bytes = host.read_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    // La cabecera de máquina (`pxu-run-log 1`, `started: <epoch>`…) no se muestra: el visor empieza en la primera sección.
    let mut all = text.lines();
    if text.lines().next() == Some(MAGIC) {
        for l in all.by_ref() {
            if l == "---" {
                break;
            }
        }
    }
    let all: Vec<&str> = all.skip_while(|l| l.trim().is_empty()).collect();
    let mut out: Vec<String> = all.iter().take(VIEW_MAX_LINES).map(|l| crate::share::fmt::clean(l)).collect();
    if out.is_empty() {
        out.push("(no output yet)".into());
    }
    if all.len() > VIEW_MAX_LINES {
        out.push(format!("[log truncated: only the first {VIEW_MAX_LINES} lines are shown]"));
    }
    Ok(out)
}

pub fn size_text(b: u64) -> String {
    if b >= 1 << 20 {
        format!("{:.1} MB", b as f64 / (1u64 << 20) as f64)
    } else {
        format!("{} KB", b.div_ceil(1024).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::host::RealHost;
    use std::os::unix::fs::symlink;

    /// 2026-11-10 12:00:00 UTC.
    const NOW: u64 = 1_794_312_000;
    const DAY: u64 = 86400;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pxu-runlog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x").unwrap();
    }
    fn name_days_ago(pre: &str, n: u64) -> String {
        let s = stamp(NOW - n * DAY);
        match pre {
            "run" => format!("run-{}-{}.log", &s[..8], &s[8..12]),
            _ => format!("{pre}-{s}.log"),
        }
    }

    #[test]
    fn epoch_round_trips_with_stamp() {
        for e in [0u64, 86399, 951_782_400, NOW, 4_102_444_799] {
            assert_eq!(epoch_of(&stamp(e)), Some(e));
        }
        assert_eq!(epoch_of("20261340000000"), None);
        assert_eq!(run_name_epoch("run-20261110-1200.log"), Some(NOW));
        assert_eq!(run_name_epoch("run-20261110-120000-2.log"), Some(NOW));
        for bad in ["run-20261110.log", "run-2026111-1200.log", "run-20261110-12.log", "run-20261110-1200-.log", "run-20261110-1200.txt", "xrun-20261110-1200.log", "run-20261110-1200-x.log"] {
            assert_eq!(run_name_epoch(bad), None, "{bad}");
        }
    }

    #[test]
    fn path_checks_accept_only_pxu_names_directly_inside_the_log_dir() {
        assert!(is_run_log_path(Path::new("/var/log/pxu/run-20261110-1200.log")));
        assert!(!is_run_log_path(Path::new("/var/log/pxu/sub/run-20261110-1200.log")));
        assert!(!is_run_log_path(Path::new("/var/log/pxu/../run-20261110-1200.log")));
        assert!(!is_run_log_path(Path::new("/tmp/run-20261110-1200.log")));
        assert!(is_pxu_log_path(Path::new("/var/log/pxu/host-20261110120000.log")));
        assert!(is_pxu_log_path(Path::new("/var/log/pxu/nvidia-20261110120000.log")));
        assert!(is_pxu_log_path(Path::new("/var/log/pxu/401/1794312000.log")));
        assert!(!is_pxu_log_path(Path::new("/var/log/pxu/other.log")));
        assert!(!is_pxu_log_path(Path::new("/var/log/pxu/401/notes.txt")));
        assert!(!is_pxu_log_path(Path::new("/var/log/pxu/abc/1794312000.log")));
        assert!(!is_pxu_log_path(Path::new("/var/log/pxu/401/../../../etc/1794312000.log")));
        assert!(!is_pxu_log_path(Path::new("/etc/pxu/host-20261110120000.log")));
    }

    #[test]
    fn retention_deletes_only_old_pxu_logs_including_the_legacy_formats() {
        let d = tmp("old");
        let h = RealHost::new();
        for (pre, n) in [("run", 40), ("run", 29), ("run", 0), ("host", 45), ("host", 3), ("nvidia", 31), ("nvidia", 1)] {
            touch(&d.join(name_days_ago(pre, n)));
        }
        touch(&d.join("401").join(format!("{}.log", NOW - 60 * DAY)));
        touch(&d.join("401").join(format!("{}.log", NOW - 2 * DAY)));
        touch(&d.join("402").join(format!("{}.log", NOW - 90 * DAY)));
        touch(&d.join("notes.txt"));
        touch(&d.join("other").join("run-20200101-0000.log"));
        let gone = prune_in(&h, &d, 30, NOW);
        assert_eq!(gone, 5, "run-40, host-45, nvidia-31, 401/-60d, 402/-90d");
        let left = |n: String| d.join(n).exists();
        assert!(!left(name_days_ago("run", 40)) && left(name_days_ago("run", 29)) && left(name_days_ago("run", 0)));
        assert!(!left(name_days_ago("host", 45)) && left(name_days_ago("host", 3)));
        assert!(!left(name_days_ago("nvidia", 31)) && left(name_days_ago("nvidia", 1)));
        assert!(!d.join("401").join(format!("{}.log", NOW - 60 * DAY)).exists() && d.join("401").join(format!("{}.log", NOW - 2 * DAY)).exists());
        assert!(!d.join("402").exists(), "emptied <vmid> dir removed");
        assert!(d.join("401").exists(), "non-empty <vmid> dir kept");
        assert!(d.join("notes.txt").exists(), "not a pxu name");
        assert!(d.join("other").join("run-20200101-0000.log").exists(), "not a numeric dir: untouched");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn retention_zero_never_deletes() {
        let d = tmp("zero");
        let h = RealHost::new();
        touch(&d.join("run-20150101-0000.log"));
        touch(&d.join("host-20150101000000.log"));
        touch(&d.join("401").join("1400000000.log"));
        assert_eq!(prune_in(&h, &d, 0, NOW), 0);
        assert!(d.join("run-20150101-0000.log").exists() && d.join("host-20150101000000.log").exists() && d.join("401").join("1400000000.log").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn retention_never_follows_symlinks_or_leaves_the_directory() {
        let d = tmp("sym");
        let outside = tmp("sym-outside");
        let h = RealHost::new();
        // un archivo fuera con nombre de pxu y muy viejo, y un symlink dentro que apunta a él
        touch(&outside.join("run-20150101-0000.log"));
        symlink(outside.join("run-20150101-0000.log"), d.join("run-20150102-0000.log")).unwrap();
        // un symlink a un directorio con nombre <vmid> y archivos viejos dentro
        touch(&outside.join("ct").join("1400000000.log"));
        symlink(outside.join("ct"), d.join("777")).unwrap();
        // un symlink con nombre de log dentro de un <vmid> real
        touch(&d.join("401").join("keep.txt"));
        symlink(outside.join("run-20150101-0000.log"), d.join("401").join("1400000000.log")).unwrap();
        assert_eq!(prune_in(&h, &d, 30, NOW), 0);
        assert!(outside.join("run-20150101-0000.log").exists(), "target outside untouched");
        assert!(outside.join("ct").join("1400000000.log").exists());
        assert!(d.join("run-20150102-0000.log").symlink_metadata().is_ok(), "the symlink itself is left alone");
        assert!(d.join("777").symlink_metadata().is_ok());
        assert!(d.join("401").join("1400000000.log").symlink_metadata().is_ok());
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn retention_on_a_missing_dir_is_a_no_op() {
        assert_eq!(prune_in(&RealHost::new(), Path::new("/nonexistent-pxu-dir"), 30, NOW), 0);
    }

    #[test]
    fn header_parses_and_rejects_foreign_or_truncated_files() {
        let t = "pxu-run-log 1\nstarted: 1794312000\nduration: 1122\nresult: ❌ 3 ok · 1 warn · 2 failed\nnode: server\n---\ntitle\n";
        let m = parse_header(t).unwrap();
        assert_eq!((m.started, m.duration, m.result.as_str(), m.node.as_str()), (1794312000, 1122, "❌ 3 ok · 1 warn · 2 failed", "server"));
        assert_eq!(parse_header("hello\nworld\n"), None);
        assert_eq!(parse_header("pxu-run-log 1\nstarted: 5\nduration: 1\n"), None, "no --- line: truncated");
        let inc = parse_header("pxu-run-log 1\nstarted: 5\nnode: n\n---\n").unwrap();
        assert!(!inc.complete && inc.result == INCOMPLETE, "no result: INCOMPLETE");
        assert_eq!(parse_header("pxu-run-log 1\nstarted: abc\nduration: 1\nresult: x\n---\n"), None);
        assert_eq!(parse_header("pxu-run-log 2\nstarted: 5\nduration: 1\nresult: x\n---\n"), None, "unknown version");
        assert_eq!(parse_header(""), None);
        // la cabecera no se lee más allá de la primera línea «---»: lo de abajo no cuenta
        let m = parse_header("pxu-run-log 1\nstarted: 5\nduration: 1\nresult: ok\n---\nresult: evil\n").unwrap();
        assert_eq!(m.result, "ok");
    }

    fn sample_report() -> RunReport {
        use crate::share::update::{CtRun, Step};
        let r = CtRun {
            vmid: 113, name: "cap".into(), outcome: Outcome::Ok, step: Step::Done, detail: String::new(), packages_before: 3, packages_after: Some(0), security: 1, reboot_required: false,
            new_failed_units: vec![], backup: None, rollback: None, revert: None, log_path: None, took_s: 30,
        };
        RunReport { node: "server".into(), started_at: NOW, finished_at: NOW + 95, results: vec![r], aborted: None, cancelled: false, nvidia: None, log_path: None }
    }
    use crate::share::update::Outcome;

    #[test]
    fn render_writes_a_parsable_header_the_summary_and_timestamped_sections() {
        let mut rec = Recorder::default();
        rec.feed(NOW, &Event::Queue(vec![(113, "cap".into())]));
        rec.feed(NOW, &Event::Start(113));
        rec.feed(NOW + 5, &Event::Line(113, "== backup".into()));
        rec.feed(NOW + 6, &Event::Line(113, "$ pct exec".into()));
        let text = render(&sample_report(), &rec);
        let m = parse_header(&text).unwrap();
        assert_eq!((m.started, m.duration, m.node.as_str()), (NOW, 95, "server"));
        assert!(m.result.starts_with("✅ 1 ok"), "{}", m.result);
        assert!(text.contains("pxu: update run on server — 1 ok") && text.contains("## Update run"), "{text}");
        assert!(text.find("── CT 113 cap ──").unwrap() < text.find("── summary ──").unwrap(), "summary goes last");
        assert!(text.contains("\n── CT 113 cap ──\n"), "{text}");
        let line = text.lines().find(|l| l.ends_with("  $ pct exec")).unwrap();
        assert_eq!(line.len(), "00:00:00  $ pct exec".len(), "HH:MM:SS prefix: {line}");
    }

    #[test]
    fn read_lines_hides_the_machine_header_and_starts_at_the_first_section() {
        use crate::share::mock::scenario::World;
        let h = World::host_gpu().build();
        let p = Path::new("/var/log/pxu/run-20261110-1200.log");
        let mut rec = Recorder::default();
        rec.feed(NOW, &Event::Start(0));
        rec.feed(NOW, &Event::Line(0, "apt update … ok".into()));
        h.write_file(p, render(&sample_report(), &rec).as_bytes()).unwrap();
        let v = read_lines(&h, p).unwrap();
        assert_eq!(v[0], "── host ──", "{v:?}");
        assert!(!v.iter().any(|l| l.starts_with("pxu-run-log") || l.starts_with("started:")), "{v:?}");
        h.write_file(p, live_header(NOW, "n").as_bytes()).unwrap();
        assert_eq!(read_lines(&h, p).unwrap(), ["(no output yet)"]);
    }

    #[test]
    fn the_live_log_grows_while_the_run_goes_and_an_interrupted_one_is_listed_as_incomplete() {
        use crate::share::mock::scenario::World;
        use std::cell::RefCell;
        let h = World::host_gpu().build();
        let (rec, live) = (RefCell::new(Recorder::default()), RefCell::new(begin_live(&h, NOW, "server")));
        let p = live.borrow().clone().unwrap();
        assert!(is_run_log_path(&p));
        feed_live(&h, &rec, &live, NOW, &Event::Queue(vec![(113, "cap".into())]));
        feed_live(&h, &rec, &live, NOW + 1, &Event::Start(113));
        feed_live(&h, &rec, &live, NOW + 2, &Event::Line(113, "== backup".into()));
        feed_live(&h, &rec, &live, NOW + 3, &Event::Line(113, "$ pct exec".into()));
        // el proceso muere aquí (SIGKILL): lo que hay en el archivo es lo único que queda
        let text = String::from_utf8(h.read_file(&p).unwrap()).unwrap();
        assert!(text.contains("\n── CT 113 cap ──\n") && text.contains("  == backup\n") && text.ends_with("  $ pct exec\n"), "{text}");
        assert_eq!(text.matches("── CT 113 cap ──").count(), 1);
        let m = parse_header(&text).unwrap();
        assert!(!m.complete && m.result == "⛔ INCOMPLETE" && m.started == NOW, "{m:?}");
        let v = list(&h, Path::new(LOG_DIR));
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].meta.as_ref().map(|m| m.result.as_str()), Some("⛔ INCOMPLETE"));
        // si la corrida llega al final, el mismo archivo se reescribe con cabecera completa, secciones y resumen
        let mut rep = sample_report();
        finalize(&h, 30, live.borrow_mut().take(), &rec.borrow(), &mut rep);
        assert_eq!(rep.log_path.as_deref(), Some(p.to_str().unwrap()), "same file, no second log");
        let text = String::from_utf8(h.read_file(&p).unwrap()).unwrap();
        let m = parse_header(&text).unwrap();
        assert!(m.complete && m.result.starts_with("✅ 1 ok"), "{m:?}");
        assert!(text.contains("  $ pct exec\n") && text.contains("── summary ──"), "{text}");
        assert_eq!(list(&h, Path::new(LOG_DIR)).len(), 1);
    }

    #[test]
    fn recorder_skips_empty_sections_and_caps_long_ones() {
        let mut rec = Recorder::default();
        rec.feed(NOW, &Event::Start(5));
        rec.feed(NOW, &Event::Start(6));
        for i in 0..SECTION_CAP + 3 {
            rec.feed(NOW, &Event::Line(6, format!("l{i}")));
        }
        let text = render(&sample_report(), &rec);
        assert!(!text.contains("CT 5 ") && !text.contains("── CT 5"), "empty section skipped");
        assert!(text.contains("── CT 6 ──") && text.contains("[3 more lines dropped"), "cap noted");
    }

    #[test]
    fn finalize_writes_one_log_names_it_without_collisions_and_prunes() {
        use crate::share::mock::scenario::World;
        let h = World::host_gpu().build();
        let rec = Recorder::default();
        let mut a = sample_report();
        let mut b = sample_report();
        finalize(&h, 30, None, &rec, &mut a);
        finalize(&h, 30, None, &rec, &mut b);
        let (pa, pb) = (a.log_path.clone().unwrap(), b.log_path.clone().unwrap());
        assert_eq!(pa, "/var/log/pxu/run-20261110-1200.log");
        assert_ne!(pa, pb, "same minute: second one gets seconds");
        assert!(pb.starts_with("/var/log/pxu/run-20261110-") && is_run_log_path(Path::new(&pb)), "{pb}");
        assert_eq!(a.results[0].log_path.as_deref(), Some(pa.as_str()));
        assert!(h.exists(Path::new(&pa)) && h.exists(Path::new(&pb)));
        let text = String::from_utf8(h.read_file(Path::new(&pa)).unwrap()).unwrap();
        assert!(text.contains(&format!("📄 Full report: {pa}")), "the notice names its own log");
    }

    #[test]
    fn list_orders_newest_first_and_tolerates_a_damaged_header() {
        let d = tmp("list");
        let h = RealHost::new();
        let log = |name: &str, started: u64, res: &str| {
            std::fs::write(d.join(name), format!("pxu-run-log 1\nstarted: {started}\nduration: 60\nresult: {res}\nnode: n\n---\nbody\n")).unwrap();
        };
        log("run-20261108-0300.log", 1_794_000_000 - 3 * DAY, "✅ 2 ok");
        log("run-20261110-0300.log", NOW - 9 * 3600, "❌ 1 ok · 1 failed");
        std::fs::write(d.join("run-20261109-0300.log"), "garbage").unwrap();
        std::fs::write(d.join("README"), "x").unwrap();
        let v = list(&h, &d);
        assert_eq!(v.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["run-20261110-0300.log", "run-20261109-0300.log", "run-20261108-0300.log"]);
        assert_eq!(v[0].meta.as_ref().map(|m| m.duration), Some(60));
        assert!(v[1].meta.is_none(), "damaged header still listed");
        assert_eq!(v[2].size, std::fs::metadata(d.join("run-20261108-0300.log")).unwrap().len());
        assert!(list(&h, Path::new("/nonexistent-pxu-dir")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn read_lines_refuses_other_files_cleans_control_chars_and_caps_big_logs() {
        use crate::share::mock::scenario::World;
        let h = World::host_gpu().build();
        assert!(read_lines(&h, Path::new("/etc/passwd")).is_err());
        assert!(read_lines(&h, Path::new("/var/log/pxu/../../etc/run-20261110-1200.log")).is_err());
        assert!(read_lines(&h, Path::new("/var/log/pxu/run-20261110-1200.log")).is_err(), "missing file");
        let mut big = String::from("a\u{1b}[31mred\u{7}\n");
        for i in 0..VIEW_MAX_LINES + 10 {
            big += &format!("line {i}\n");
        }
        h.write_file(Path::new("/var/log/pxu/run-20261110-1200.log"), big.as_bytes()).unwrap();
        let v = read_lines(&h, Path::new("/var/log/pxu/run-20261110-1200.log")).unwrap();
        assert_eq!(v.len(), VIEW_MAX_LINES + 1);
        assert!(!v[0].contains('\u{1b}') && !v[0].contains('\u{7}'), "{:?}", v[0]);
        assert!(v.last().unwrap().contains("truncated"));
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size_text(0), "1 KB");
        assert_eq!(size_text(1500), "2 KB");
        assert_eq!(size_text(3 << 20), "3.0 MB");
    }
}
