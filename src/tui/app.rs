//! Estado + reducer (tecla → estado), sin IO: lo que hace testeable el redibujo.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pxu::share::update::check::{self, CtResult, Report};
use pxu::share::config::Config;
use pxu::share::proxmox::hostcheck::{HostInfo, HostPkg};
use pxu::share::proxmox::guests::Inventory;
use pxu::share::model::{Guest, GuestKind, RunState};
use pxu::share::update::{Candidate, Event as UpdEvent, Outcome, RunReport, Step};
use pxu::share::update::nvidia::Newest;
use super::apprise::{AKey, AppriseModal};
use super::sched::{SchedKey, SchedModal};
use pxu::share::config::AppriseEdit;
use pxu::share::schedule::{CalendarCheck, SchedulePlan, TimerStatus};
use std::collections::{BTreeSet, HashMap, VecDeque};

/// Líneas de log que la TUI conserva en memoria (DESIGN §5.4); el log completo va a /var/log/pxu.
pub const LOG_RING: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Running,
    Stopped,
    Updates,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Vmid,
    Name,
    Updates,
    Ram,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Vmid => "vmid",
            Sort::Name => "name",
            Sort::Updates => "updates",
            Sort::Ram => "ram",
        }
    }
    fn next(self) -> Sort {
        match self {
            Sort::Vmid => Sort::Name,
            Sort::Name => Sort::Updates,
            Sort::Updates => Sort::Ram,
            Sort::Ram => Sort::Vmid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    List,
    Detail(u32),
    Check,
    Update,
    /// `g`: configuración y horario del timer (DESIGN §5.6).
    Config,
    /// `l`: lista de logs de corridas y visor (`LogsState::viewer`).
    Logs,
    /// `d`: espacio por storage y por guest, backups y alertas (DESIGN §5.7).
    Storage,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Reload,
    Quit,
    /// Lanza un chequeo en el worker (`only` vacío = todos los LXC locales).
    Check { refresh: Option<bool>, only: Vec<u32> },
    CancelCheck,
    /// `u`: el bucle calcula los candidatos (necesita host y config) y abre el modal.
    PrepareUpdate { only: Vec<u32> },
    /// Enter en el modal: arranca la corrida (opt-in explícito de escritura).
    Update { only: Vec<u32> },
    /// Enter en el modal del host: instala las actualizaciones del host (opt-in propio; la lista confirmada viaja con la orden).
    HostUpdate { confirmed: Vec<HostPkg> },
    /// Enter en el aviso de dpkg sucio: `dpkg --configure -a` (confirmación aparte).
    HostRepair,
    /// `y`/Enter en el modal de librerías NVIDIA de un CT: reinstala solo las librerías de ese CT con la versión del host (host y otros CT intactos).
    NvidiaCtLibs { vmid: u32 },
    /// Terminar el CT en curso y parar.
    CancelUpdate,
    /// `a`: el bucle lee la config (validada) y abre la ventana de opciones de auto-update (auto-update + driver NVIDIA).
    AskAutoUpdate { vmid: u32 },
    /// Enter en esa ventana: el bucle escribe solo lo que cambió (`auto`: `host_auto`/`[[ct]] auto_update`; `driver`: `nvidia_auto`) y recarga.
    SetAutoUpdate { vmid: u32, auto: Option<bool>, driver: Option<bool> },
    /// `b`: el bucle lee la config (validada) y abre el modal del backup previo al update.
    AskBackup { vmid: u32 },
    /// `y`/Enter en el modal del backup: el bucle escribe `[[ct]] backup` en config.toml (`disabled` = `"none"`; si no, hereda el global) y recarga.
    SetBackup { vmid: u32, disabled: bool },
    /// `g` / `r` en Config: el bucle lee el estado del timer y muestra la vista.
    OpenConfig,
    /// `S`: el bucle lee el timer y abre el diálogo del horario.
    AskSchedule,
    /// Enter en el diálogo: validar el `OnCalendar` con systemd antes de confirmar.
    CheckSchedule { expr: String },
    /// `y`/Enter en la confirmación: instala/escribe/activa y recarga el estado.
    SaveSchedule(SchedulePlan),
    /// `n` en Config: el bucle lee `[apprise]` y abre el diálogo.
    AskApprise,
    /// Enter en el diálogo: validar la config resultante entera (sin escribir) antes de confirmar.
    CheckApprise(AppriseEdit),
    /// `y`/Enter en la confirmación: escribe `[apprise]` (validado, atómico) y recarga.
    SaveApprise(AppriseEdit),
    /// `t` en Config: envía un aviso de prueba (en un hilo; con indicador).
    TestNotify,
    /// Se cerró el selector de columnas con cambios: el bucle los guarda en `[ui] columns` de config.toml.
    SaveColumns(Vec<String>),
    /// `l`: el bucle lee la lista de `run-*.log` y abre la página.
    OpenLogs,
    /// `d` en la lista: abre la página Storage y la lee en un hilo.
    OpenStorage,
    /// `r` en Storage: vuelve a leer sin bloquear.
    RefreshStorage,
    /// `y`/Enter en el modal de borrado de un backup (Storage): el bucle ejecuta `pvesm free <volid>` y recarga los datos.
    DeleteBackup { vmid: u32, item: pxu::share::storage::BackupItem },
    /// Enter / `n` / `p` / doble clic: el bucle lee ese log y lo muestra en el visor.
    OpenLog(std::path::PathBuf),
    /// Enter en el diálogo de retención: guarda `[general] log_retention_days`.
    SaveLogRetention(u32),
}

/// Casilla «Include GPU driver update» de la ventana `a` (solo si la máquina usa la GPU NVIDIA con un driver `.run`).
pub struct DriverBox {
    pub current: String,
    pub newest: Newest,
    /// Casilla marcada; arranca en el valor guardado (`nvidia_auto`).
    pub checked: bool,
    /// Valor guardado al abrir.
    pub initial: bool,
    /// Host: cuántos CT tienen `nvidia_auto` (dejan de recibir el driver si el del host se apaga).
    pub dependents: usize,
}

impl DriverBox {
    /// Líneas informativas, compactas.
    pub fn lines(&self, ct: bool) -> Vec<String> {
        let latest = match &self.newest {
            Newest::Newer(v) => format!("{v} (newer, same {} branch)", self.branch()),
            Newest::UpToDate => format!("{} (up to date)", self.current),
            Newest::Unknown => "unknown (could not reach nvidia.com)".into(),
        };
        let mut l = vec![format!("Installed: {} · Latest: {latest}", self.current)];
        l.push(format!("the run installs the newest {}.x with DKMS after the host update; GPU CTs get its libs", self.branch()));
        if ct {
            l.push("this CT restarts after getting the libs".into());
        }
        l.push("skipped if a host reboot is pending; pxu never reboots the host".into());
        l
    }

    /// Rama mayor del driver instalado (p. ej. "595").
    fn branch(&self) -> String {
        self.current.split('.').next().unwrap_or("").to_string()
    }
}

/// Ventana de opciones del auto-update de una máquina (`a`): casilla «Automatic updates» y, si aplica, «Include GPU driver update».
pub struct AutoModal {
    pub vmid: u32,
    pub name: String,
    /// Casilla «Automatic updates»; arranca en el valor actual.
    pub value: bool,
    pub initial: bool,
    /// Motivo actual del auto-update (`Config::auto_update_why`) y si falta config.toml.
    pub why: String,
    pub missing: bool,
    /// Casilla del driver, o el motivo por el que no se ofrece.
    pub driver: Result<DriverBox, String>,
    /// 0 = auto-update; 1 = driver.
    pub focus: usize,
}

impl AutoModal {
    /// `why`: motivo actual del auto-update; `missing`: aún no existe config.toml; `driver`: oferta del driver o el motivo de que no aplique.
    pub fn new(g: &Guest, why: &str, missing: bool, driver: Result<DriverBox, &str>) -> AutoModal {
        let value = g.auto_update;
        AutoModal { vmid: if g.kind == GuestKind::Host { 0 } else { g.vmid }, name: g.name.clone(), value, initial: value, why: why.into(), missing, driver: driver.map_err(String::from), focus: 0 }
    }

    /// Texto informativo del auto-update (sin las casillas).
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![format!("today: {}", self.why)];
        let cfg = pxu::share::config::CONFIG_PATH;
        if self.vmid == 0 {
            lines.push("the scheduled run (timer) would update the HOST first, then the CTs only if the host ended well".into());
            lines.push("same safeguards as `u`: locks, dpkg --audit, space, simulation, /etc backup, ZFS snapshot; needs a recent check".into());
            lines.push("the host is NEVER rebooted by pxu: a new kernel is reported as reboot required".into());
            if self.missing {
                lines.push(format!("{cfg} does not exist: it will be created with only the settings you change"));
                lines.push("with a config pxu leaves read-only mode (updates become possible)".into());
            }
            return lines;
        }
        if !self.value && self.why.contains("by tag") {
            lines.push("the tag stays on the CT: the [[ct]] override (false) wins over it".into());
        }
        if self.missing {
            lines.push(format!("{cfg} does not exist: it will be created with only the settings you change"));
            lines.push("with a config pxu leaves read-only mode (updates become possible)".into());
        }
        lines
    }

    /// Qué cambió: `(auto, driver)`. Con el auto-update desmarcado el driver de esa máquina se apaga.
    pub fn changes(&self) -> (Option<bool>, Option<bool>) {
        let auto = (self.value != self.initial).then_some(self.value);
        let driver = self.driver.as_ref().ok().and_then(|d| (d.checked != d.initial).then_some(d.checked));
        (auto, driver)
    }

    /// Línea «Saves: …» con exactamente lo que se escribirá.
    pub fn saves(&self) -> String {
        let (auto, driver) = self.changes();
        let mut parts = Vec::new();
        if let Some(v) = auto {
            parts.push(if self.vmid == 0 { format!("[update] host_auto = {v}") } else { format!("[[ct]] {} auto_update = {v}", self.vmid) });
        }
        if let Some(v) = driver {
            parts.push(match (v, self.vmid) {
                (true, 0) => "[update] host_nvidia_auto = true".into(),
                (true, id) => format!("[[ct]] {id} nvidia_auto + [update] host_nvidia_auto"),
                (false, 0) => "[update] host_nvidia_auto = false".into(),
                (false, id) => format!("[[ct]] {id} nvidia_auto = false (host flag untouched)"),
            });
        }
        if parts.is_empty() { "nothing changed: Enter writes nothing".into() } else { format!("Saves: {}", parts.join(" + ")) }
    }

    /// Aviso: apagar el driver del host deja sin él a los CT que lo tienen activo.
    pub fn warning(&self) -> Option<String> {
        let d = self.driver.as_ref().ok()?;
        (self.vmid == 0 && !d.checked && d.dependents > 0).then(|| format!("! {} CT(s) with nvidia_auto get nothing while the host flag is off", d.dependents))
    }

    /// `Space` sobre la casilla enfocada. El driver depende del auto-update: no se marca con este apagado, y apagar este lo desmarca.
    pub fn toggle(&mut self) {
        if self.focus == 0 {
            self.value = !self.value;
            if !self.value {
                if let Ok(d) = &mut self.driver {
                    d.checked = false;
                }
            }
        } else if self.value {
            if let Ok(d) = &mut self.driver {
                d.checked = !d.checked;
            }
        }
    }
}

/// Confirmación de `u` sobre un CT con GPU: reinstalar solo sus librerías NVIDIA con la versión del host.
pub struct NvidiaLibsModal {
    pub vmid: u32,
    pub name: String,
    pub version: Option<String>,
}

/// Modal de confirmación del backup previo al update de un CT (`b`).
pub struct BackupModal {
    pub vmid: u32,
    pub name: String,
    /// Lo que se guardará: `true` = `backup = "none"`; `false` = quitar el override y heredar `[backup] mode`.
    pub disabled: bool,
    pub lines: Vec<String>,
}

impl BackupModal {
    /// `why`: modo actual y su origen (`Config::backup_why`); `now`: modo efectivo; `global`: `[backup] mode`; `missing`: aún no existe config.toml.
    pub fn new(g: &Guest, why: &str, global: &str, missing: bool) -> BackupModal {
        let disabled = g.backup != "none";
        let mut lines = vec![format!("today: {why}")];
        if disabled {
            if g.backup != global {
                lines.push(format!("the [[ct]] override ({}) is replaced by \"none\"", g.backup));
            }
            lines.push("WARNING: no backup before the update and no automatic rollback if it fails".into());
            lines.push("the CT is updated in place; a failed update can only be fixed by hand".into());
            lines.push(format!("saved as [[ct]] vmid = {}, backup = \"none\" in {}", g.vmid, pxu::share::config::CONFIG_PATH));
            if missing {
                lines.push(format!("{} does not exist: it will be created with only that block", pxu::share::config::CONFIG_PATH));
                lines.push("with a config pxu leaves read-only mode (updates become possible)".into());
            }
        } else if global == "none" {
            lines.push("the global [backup] mode is none: this CT gets backup = \"auto\"".into());
            lines.push("temporary backup before the update (snapshot, else vzdump) and auto-rollback on failure".into());
            lines.push(format!("saved as [[ct]] vmid = {}, backup = \"auto\" in {}; the rest is kept as it is", g.vmid, pxu::share::config::CONFIG_PATH));
        } else {
            lines.push(format!("the backup key of the [[ct]] block is removed: the CT follows [backup] mode = \"{global}\""));
            lines.push("temporary backup before the update (snapshot, else vzdump) and auto-rollback on failure".into());
            lines.push(format!("the rest of {} is kept as it is", pxu::share::config::CONFIG_PATH));
        }
        BackupModal { vmid: g.vmid, name: g.name.clone(), disabled, lines }
    }
}

/// Confirmación del borrado de un backup (`x` en el detalle de Storage).
pub struct BackupDelModal {
    pub vmid: u32,
    pub item: pxu::share::storage::BackupItem,
    pub lines: Vec<String>,
}

impl BackupDelModal {
    /// `only`: es el único backup del guest; `pbs`: el storage es un Proxmox Backup Server; `now`: reloj del servidor.
    pub fn new(vmid: u32, item: pxu::share::storage::BackupItem, only: bool, pbs: bool, now: u64) -> BackupDelModal {
        use pxu::share::storage::{fmt_gb, when};
        let mut lines = vec![
            format!("volume : {}", item.volid),
            format!("storage: {}{}", item.storage, if pbs { " (Proxmox Backup Server)" } else { "" }),
            format!("date   : {}   size: {}{}", when(item.ctime, now), fmt_gb(item.size), if item.pxu { "   (pxu pre-update)" } else { "" }),
        ];
        if only {
            lines.push(format!("WARNING: this is the only backup of CT {vmid}"));
        }
        if pbs {
            lines.push("space is freed after garbage collection on the PBS".into());
        }
        lines.push(format!("pxu runs: pvesm free {}", item.volid));
        BackupDelModal { vmid, item, lines }
    }
}

/// Confirmación de la instalación en el host (`u` sobre la fila del host), o el aviso de `dpkg --audit` sucio.
pub struct HostModal {
    pub pkgs: Vec<HostPkg>,
    /// Motivo por el que se vuelve a pedir confirmación («la lista cambió…»).
    pub note: Option<String>,
    /// `Some(líneas)`: `dpkg --audit` sucio; el modal ofrece `dpkg --configure -a` en vez de instalar.
    pub dpkg: Option<Vec<String>>,
    /// Kernel más nuevo para el que no hay módulo NVIDIA (aviso informativo).
    pub nvidia: Option<String>,
    pub scroll: usize,
}

impl HostModal {
    pub fn confirm(pkgs: Vec<HostPkg>, info: Option<&HostInfo>, note: Option<String>) -> HostModal {
        HostModal { pkgs, note, dpkg: None, nvidia: info.and_then(|i| i.nvidia_missing_for.clone()), scroll: 0 }
    }
    pub fn dirty(lines: Vec<String>) -> HostModal {
        HostModal { pkgs: vec![], note: None, dpkg: Some(lines), nvidia: None, scroll: 0 }
    }
}

/// Por qué la corrida del host volvió a pedir algo antes de instalar.
pub enum HostStop {
    Reconfirm(Vec<HostPkg>),
    Dirty(Vec<String>),
    Repaired,
}

/// Modal de confirmación (DESIGN §5.5).
pub struct UpdateModal {
    pub cands: Vec<Candidate>,
    pub excluded: BTreeSet<u32>,
    pub cursor: usize,
    pub head: Vec<String>,
    pub foot: Vec<String>,
}

impl UpdateModal {
    pub fn new(cands: Vec<Candidate>, cfg: &Config) -> UpdateModal {
        let u = &cfg.update;
        let mut head = vec![
            format!("order vmid ↑ · backup: {} (keep {}) · stopped: {}", cfg.backup.mode, cfg.backup.keep(), if u.stopped == "start_and_restore" { "start + restore" } else { "skip" }),
            format!("on failure: {} (circuit-breaker after {} in a row) · reboot: {}", u.on_failure, if u.on_failure == "stop" { 1 } else { u.max_consecutive_failures }, u.reboot),
        ];
        let no_backup: Vec<String> = cands.iter().filter(|c| c.skip.is_none() && cfg.backup_mode(c.vmid) == "none").map(|c| c.vmid.to_string()).collect();
        if !no_backup.is_empty() {
            head.push(format!("WARNING: backup \"none\" for CT {}: updated with NO backup and NO rollback", no_backup.join(", ")));
        }
        head.push("WARNING: backup + rollback cover only each CT's rootfs; bind mounts / mp with backup=0 (e.g. /mnt/pve/...) are NOT saved or reverted".into());
        let foot = vec![
            format!("per guest: pre-checks (check ≤ {} m, free ≥ {} MB) → lock → backup → refresh + simulate", u.require_fresh_check_minutes, u.min_free_mb),
            format!("→ upgrade (nice {}, io {}) → verify → delete backup; on failure: automatic rollback", cfg.general.nice, cfg.general.io_class),
        ];
        UpdateModal { cands, excluded: BTreeSet::new(), cursor: 0, head, foot }
    }
    pub fn chosen(&self) -> Vec<u32> {
        self.cands.iter().filter(|c| c.skip.is_none() && !self.excluded.contains(&c.vmid)).map(|c| c.vmid).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QState {
    Pending,
    Active(Step),
    Done(Outcome),
}

#[derive(Debug, Clone)]
pub struct QItem {
    pub vmid: u32,
    pub name: String,
    pub state: QState,
    pub detail: String,
    pub started: u64,
    pub took_s: u64,
}

/// Corrida de update mostrada en la vista `Update` (DESIGN §5.4).
#[derive(Default)]
pub struct UpdateState {
    pub queue: Vec<QItem>,
    /// (hora, vmid, línea): ring de `LOG_RING` líneas.
    pub log: VecDeque<(String, u32, String)>,
    pub dropped: usize,
    pub running: bool,
    pub stop_requested: bool,
    pub confirm_cancel: bool,
    pub current: Option<u32>,
    /// Líneas hacia atrás desde el final (0 = seguir).
    pub scroll: usize,
    pub started: String,
    pub report: Option<RunReport>,
    pub notes: Vec<String>,
    pub failed: Option<String>,
}

/// Última corrida de chequeo mostrada en la vista `Check` (DESIGN §5.3).
#[derive(Default)]
pub struct CheckState {
    pub results: Vec<CtResult>,
    pub total: usize,
    pub running: bool,
    pub cancelling: bool,
    pub refresh: bool,
    pub cursor: usize,
    pub took_s: u64,
    pub finished: String,
    pub report: Option<Report>,
}

pub struct App {
    pub inv: Inventory,
    pub filter: Filter,
    pub sort: Sort,
    pub search: String,
    pub searching: bool,
    pub cursor: usize,
    pub view: View,
    pub detail_scroll: usize,
    pub help: bool,
    /// Desplazamiento de la ayuda; la vista lo recorta al máximo real.
    pub help_scroll: std::cell::Cell<usize>,
    pub read_only: bool,
    /// Solo lectura forzado (`--read-only`): ni siquiera `a` puede tocar la config.
    pub forced_ro: bool,
    /// Por qué la config no es utilizable (inválida, permisos…); se muestra en la lista. `None` si falta o es válida.
    pub ro_reason: Option<String>,
    pub mock: bool,
    /// Solo capturas: oculta `[MOCK]` (PXU_MOCK_NO_BADGE=1, leída únicamente en modo mock).
    pub hide_mock_badge: bool,
    pub status: String,
    pub clock: String,
    pub now_epoch: u64,
    /// CPU % por vmid (de los cores asignados al CT, tope 100); solo existe tras dos muestras (tick automático o `r`).
    pub cpu_pct: HashMap<u32, f32>,
    /// Última muestra por vmid: (cpu µs acumulados, reloj µs).
    pub cpu_prev: HashMap<u32, (u64, u64)>,
    /// Se espera la segunda muestra: los LXC en marcha sin dato muestran "…" en vez de "—".
    pub cpu_waiting: bool,
    pub check: CheckState,
    /// `C`/`R` pidió refrescar índices: espera `y` (DESIGN §5.3: separado de `c` y con confirmación).
    /// `[check] refresh` de la config: lo que hace `c` fuera de `[RO]` (default true).
    pub refresh_default: bool,
    pub update: UpdateState,
    pub modal: Option<UpdateModal>,
    pub auto_modal: Option<AutoModal>,
    pub backup_modal: Option<BackupModal>,
    pub nvidia_libs: Option<NvidiaLibsModal>,
    pub backup_del: Option<BackupDelModal>,
    pub host_modal: Option<HostModal>,
    pub sched: Option<SchedModal>,
    pub apprise_modal: Option<AppriseModal>,
    /// Columnas opcionales visibles de la lista (`config::OPTIONAL_COLUMNS`, en minúsculas).
    pub columns: Vec<String>,
    /// Selector de columnas (`o`): posición del cursor y si algo cambió desde que se abrió.
    pub col_modal: Option<(usize, bool)>,
    /// Sección «Notifications» de la vista Config (ya enmascarada) o el motivo por el que no se pudo leer.
    pub apprise: Option<Result<Vec<String>, String>>,
    /// Hay un aviso de prueba en vuelo (`t`).
    pub testing: bool,
    /// Último estado del timer leído para la vista Config (o el motivo por el que no se pudo leer).
    pub timer: Option<Result<TimerStatus, String>>,
    pub logs: super::logs::LogsState,
    pub storage: super::storage::StorageState,
    /// `log_retention_days` leído para la vista Config.
    pub log_days: Option<u32>,
    /// Diálogo de retención (`d` en Config): los dígitos escritos.
    pub log_modal: Option<String>,
}

impl App {
    pub fn new(inv: Inventory, read_only: bool, mock: bool) -> App {
        App {
            inv,
            filter: Filter::All,
            sort: Sort::Vmid,
            search: String::new(),
            searching: false,
            cursor: 0,
            view: View::List,
            detail_scroll: 0,
            help: false,
            help_scroll: std::cell::Cell::new(0),
            read_only,
            forced_ro: false,
            ro_reason: None,
            mock,
            hide_mock_badge: mock && std::env::var_os("PXU_MOCK_NO_BADGE").is_some_and(|v| v == "1"),
            status: String::new(),
            clock: String::new(),
            now_epoch: 0,
            cpu_pct: HashMap::new(),
            cpu_prev: HashMap::new(),
            cpu_waiting: false,
            check: CheckState::default(),
            refresh_default: true,
            update: UpdateState::default(),
            modal: None,
            auto_modal: None,
            backup_modal: None,
            nvidia_libs: None,
            backup_del: None,
            host_modal: None,
            sched: None,
            apprise_modal: None,
            columns: pxu::share::config::UiCfg::default().visible_columns(),
            col_modal: None,
            apprise: None,
            testing: false,
            timer: None,
            logs: Default::default(),
            storage: Default::default(),
            log_days: None,
            log_modal: None,
        }
    }

    pub fn count(&self, f: Filter) -> usize {
        self.inv.real().filter(|g| Self::matches_filter(f, g)).count()
    }

    fn matches_filter(f: Filter, g: &Guest) -> bool {
        match f {
            Filter::All => true,
            Filter::Running => g.is_running() && g.kind != GuestKind::Host,
            Filter::Stopped => g.state == RunState::Stopped,
            Filter::Updates => g.updates() > 0,
        }
    }

    pub fn visible(&self) -> Vec<&Guest> {
        let q = self.search.to_lowercase();
        let mut v: Vec<&Guest> = self
            .inv
            .guests
            .iter()
            .filter(|g| Self::matches_filter(self.filter, g))
            .filter(|g| {
                q.is_empty()
                    || g.name.to_lowercase().contains(&q)
                    || g.vmid.to_string().contains(&q)
                    || g.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect();
        match self.sort {
            Sort::Vmid => {}
            Sort::Name => v.sort_by_key(|g| g.name.to_lowercase()),
            Sort::Updates => v.sort_by_key(|g| (std::cmp::Reverse(g.updates()), g.vmid)),
            Sort::Ram => v.sort_by_key(|g| (std::cmp::Reverse(g.mem_used_bytes.unwrap_or(0)), g.vmid)),
        }
        v
    }

    pub fn selected(&self) -> Option<&Guest> {
        self.visible().get(self.cursor).copied()
    }

    pub fn guest(&self, vmid: u32) -> Option<&Guest> {
        self.inv.guests.iter().find(|g| g.vmid == vmid)
    }

    fn clamp(&mut self) {
        let n = self.visible().len();
        self.cursor = if n == 0 { 0 } else { self.cursor.min(n - 1) };
    }

    /// Primera muestra de CPU (al arrancar) a partir del inventario; hasta la segunda se muestra "…".
    pub fn cpu_seed(&mut self, now_us: u64) {
        let samples = self.inv.guests.iter().filter_map(|g| g.cpu_usec.map(|u| (g.vmid, u))).collect();
        self.cpu_sample(samples, now_us);
        self.cpu_waiting = true;
    }

    /// Incorpora una muestra de CPU acumulado (µs) por vmid tomada en `now_us` (reloj inyectado):
    /// % = delta CPU / delta reloj / cores del CT (1 si no declara `cores`), con tope 100.
    pub fn cpu_sample(&mut self, samples: HashMap<u32, u64>, now_us: u64) {
        self.cpu_pct.clear();
        for g in &self.inv.guests {
            let Some(&usec) = samples.get(&g.vmid) else { continue };
            if let Some(&(pu, pw)) = self.cpu_prev.get(&g.vmid) {
                let wall = now_us.saturating_sub(pw) as f64;
                if wall > 0.0 {
                    let cores = g.cores.filter(|c| *c > 0).unwrap_or(1) as f64;
                    let pct = (usec.saturating_sub(pu) as f64 / wall / cores * 100.0).min(100.0);
                    self.cpu_pct.insert(g.vmid, pct as f32);
                }
            }
        }
        self.cpu_prev = samples.into_iter().map(|(v, u)| (v, (u, now_us))).collect();
        self.cpu_waiting = false;
    }

    /// Sustituye el inventario (recarga); con `Some(ahora_µs)` toma además una muestra de CPU de él (`r`).
    pub fn set_inventory(&mut self, inv: Inventory, cpu_now_us: Option<u64>) {
        self.inv = inv;
        if let Some(now) = cpu_now_us {
            let samples = self.inv.guests.iter().filter_map(|g| g.cpu_usec.map(|u| (g.vmid, u))).collect();
            self.cpu_sample(samples, now);
        }
        self.clamp();
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Action {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            if self.update.running {
                self.update.confirm_cancel = true; // Ctrl-C con corrida activa = diálogo de cancelación (DESIGN §7)
                return Action::None;
            }
            return Action::Quit;
        }
        if self.modal.is_some() {
            return self.modal_key(k);
        }
        if self.col_modal.is_some() {
            return self.col_modal_key(k);
        }
        if self.host_modal.is_some() {
            return self.host_modal_key(k);
        }
        if self.auto_modal.is_some() {
            return self.auto_modal_key(k);
        }
        if self.nvidia_libs.is_some() {
            return self.nvidia_libs_key(k);
        }
        if self.backup_modal.is_some() {
            return self.backup_modal_key(k);
        }
        if self.backup_del.is_some() {
            return self.backup_del_key(k);
        }
        if self.sched.is_some() {
            return self.sched_key(k);
        }
        if self.apprise_modal.is_some() {
            return self.apprise_key(k);
        }
        if self.log_modal.is_some() {
            return self.log_modal_key(k);
        }
        if self.help {
            // El diálogo captura todo salvo q (ARCHITECTURE/DESIGN §10).
            let sc = self.help_scroll.get();
            return match k.code {
                KeyCode::Char('q') => Action::Quit,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll.set(sc.saturating_sub(1));
                    Action::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll.set(sc + 1);
                    Action::None
                }
                KeyCode::PageUp => {
                    self.help_scroll.set(sc.saturating_sub(10));
                    Action::None
                }
                KeyCode::PageDown => {
                    self.help_scroll.set(sc + 10);
                    Action::None
                }
                _ => {
                    self.help = false;
                    self.help_scroll.set(0);
                    Action::None
                }
            };
        }
        if self.searching {
            match k.code {
                KeyCode::Esc => {
                    self.search.clear();
                    self.searching = false;
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Char(c) => self.search.push(c),
                _ => {}
            }
            self.clamp();
            return Action::None;
        }
        self.status.clear();
        match self.view {
            View::List => self.list_key(k),
            View::Detail(_) => self.detail_key(k),
            View::Check => self.check_key(k),
            View::Update => self.update_key(k),
            View::Config => self.config_key(k),
            View::Logs => self.logs_key(k),
            View::Storage => self.storage_key(k),
        }
    }

    fn storage_key(&mut self, k: KeyEvent) -> Action {
        use super::storage::StorageKey;
        match self.storage.on_key(k) {
            StorageKey::None => Action::None,
            StorageKey::Back => {
                self.view = View::List;
                Action::None
            }
            StorageKey::Refresh => Action::RefreshStorage,
            StorageKey::DeleteBackup => self.ask_delete_backup(),
            StorageKey::Help => {
                self.help = true;
                Action::None
            }
        }
    }

    /// `x` en el detalle de Storage: abre la confirmación del borrado del backup seleccionado (nada en solo lectura ni si está protegido).
    fn ask_delete_backup(&mut self) -> Action {
        if self.forced_ro {
            self.status = "read-only mode (--read-only): backups cannot be deleted".into();
            return Action::None;
        }
        if self.read_only {
            self.status = "no usable config: backups cannot be deleted".into();
            return Action::None;
        }
        let st = &self.storage;
        let (Some(id), Some(d), Some(b)) = (st.detail, st.data.as_ref(), st.selected_backup()) else {
            self.status = "no backup selected".into();
            return Action::None;
        };
        if b.protected {
            self.status = format!("protected: {} cannot be deleted (remove the protection in Proxmox first)", b.volid);
            return Action::None;
        }
        let only = st.backups().len() == 1;
        let pbs = d.storages.iter().any(|s| s.name == b.storage && s.kind == "pbs");
        self.backup_del = Some(BackupDelModal::new(id, b.clone(), only, pbs, d.now));
        Action::None
    }

    fn backup_del_key(&mut self, k: KeyEvent) -> Action {
        let Some(m) = self.backup_del.take() else { return Action::None };
        match k.code {
            KeyCode::Char('y') | KeyCode::Enter => return Action::DeleteBackup { vmid: m.vmid, item: m.item },
            KeyCode::Esc | KeyCode::Char('n' | 'q') => self.status = "backup kept".into(),
            _ => self.backup_del = Some(m),
        }
        Action::None
    }

    /// `d` en la lista: abre la página al momento (con lo que haya) y el bucle lee los datos en un hilo.
    fn open_storage(&mut self) -> Action {
        self.view = View::Storage;
        self.storage.detail = None;
        Action::OpenStorage
    }

    fn logs_key(&mut self, k: KeyEvent) -> Action {
        let r = self.logs.on_key(k);
        self.logs_result(r)
    }

    pub fn on_mouse(&mut self, m: crossterm::event::MouseEvent, now: std::time::Instant) -> Action {
        if self.view != View::Logs || self.help || self.log_modal.is_some() {
            return Action::None;
        }
        let r = self.logs.on_mouse(m, now);
        self.logs_result(r)
    }

    fn logs_result(&mut self, r: super::logs::LogsKey) -> Action {
        use super::logs::LogsKey;
        match r {
            LogsKey::None => Action::None,
            LogsKey::Quit => Action::Quit,
            LogsKey::Back => {
                self.view = View::List;
                Action::None
            }
            LogsKey::Open(p) => Action::OpenLog(p),
        }
    }

    /// El bucle leyó la lista de logs: abre la página.
    pub fn open_logs(&mut self, entries: Vec<pxu::share::update::runlog::LogEntry>) {
        self.logs = super::logs::LogsState::new(entries);
        self.view = View::Logs;
    }

    /// El bucle leyó un log: lo muestra arriba del todo (o deja el error en la línea de estado).
    pub fn log_loaded(&mut self, r: Result<Vec<String>, String>) {
        match r {
            Ok(lines) => {
                let title = self.logs.entries.get(self.logs.sel).map(|e| e.name.clone()).unwrap_or_default();
                self.logs.viewer = Some(super::logs::Viewer { title, lines, top: 0 });
            }
            Err(e) => self.status = format!("log not opened: {e}"),
        }
    }

    /// `d` en Config (alias `L`): pide el número de días (editar config.toml: no en `--read-only`).
    fn ask_log_retention(&mut self) -> Action {
        if self.forced_ro {
            self.status = "read-only mode (--read-only): the log retention cannot be changed".into();
        } else if self.read_only {
            self.status = "no usable config: the log retention cannot be changed".into();
        } else {
            self.log_modal = Some(self.log_days.map(|d| d.to_string()).unwrap_or_default());
        }
        Action::None
    }

    fn log_modal_key(&mut self, k: KeyEvent) -> Action {
        let Some(t) = self.log_modal.as_mut() else { return Action::None };
        match k.code {
            KeyCode::Esc => {
                self.log_modal = None;
                self.status = "log retention unchanged".into();
            }
            KeyCode::Backspace => {
                t.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() && t.len() < 5 => t.push(c),
            KeyCode::Enter => match t.parse::<u32>() {
                Ok(d) if d <= pxu::share::config::MAX_LOG_RETENTION_DAYS => {
                    self.log_modal = None;
                    return Action::SaveLogRetention(d);
                }
                _ => self.status = format!("days must be 0 (never delete) to {}", pxu::share::config::MAX_LOG_RETENTION_DAYS),
            },
            _ => {}
        }
        Action::None
    }

    /// `S`: el bucle abre el diálogo con el estado real del timer.
    fn ask_schedule(&mut self) -> Action {
        if self.forced_ro {
            self.status = "read-only mode (--read-only): the schedule cannot be changed".into();
            return Action::None;
        }
        if self.update.running {
            self.status = "an update is running: change the schedule when it finishes".into();
            return Action::None;
        }
        Action::AskSchedule
    }

    pub fn open_sched(&mut self, m: SchedModal) {
        self.sched = Some(m);
    }

    pub fn sched_checked(&mut self, r: Result<CalendarCheck, String>) {
        if let Some(m) = self.sched.as_mut() {
            m.checked(r);
        }
    }

    fn sched_key(&mut self, k: KeyEvent) -> Action {
        let Some(m) = self.sched.as_mut() else { return Action::None };
        match m.on_key(k) {
            SchedKey::None => Action::None,
            SchedKey::Close => {
                self.sched = None;
                self.status = "schedule unchanged".into();
                Action::None
            }
            SchedKey::Check(expr) => Action::CheckSchedule { expr },
            SchedKey::Save(p) => {
                self.sched = None;
                Action::SaveSchedule(p)
            }
        }
    }

    fn config_key(&mut self, k: KeyEvent) -> Action {
        match k.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc | KeyCode::Backspace => self.view = View::List,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => return Action::OpenConfig,
            KeyCode::Char('s' | 'S') => return self.ask_schedule(),
            KeyCode::Char('n' | 'N') => return self.ask_apprise(),
            KeyCode::Char('t') => return self.ask_test(),
            KeyCode::Char('d' | 'L') => return self.ask_log_retention(),
            KeyCode::Char('l') => return Action::OpenLogs,
            _ => {}
        }
        Action::None
    }

    /// `n` (alias `N`): el bucle abre el diálogo de `[apprise]` (edita config.toml: no en `--read-only`).
    fn ask_apprise(&mut self) -> Action {
        if self.forced_ro {
            self.status = "read-only mode (--read-only): notifications cannot be changed".into();
            return Action::None;
        }
        if self.update.running {
            self.status = "an update is running: change the notifications when it finishes".into();
            return Action::None;
        }
        Action::AskApprise
    }

    /// `t`: la prueba envía por red, así que cuenta como acción (no como lectura).
    fn ask_test(&mut self) -> Action {
        if self.forced_ro {
            self.status = "read-only mode (--read-only): the test sends a notice over the network".into();
            return Action::None;
        }
        if self.testing {
            self.status = "a test notice is already being sent".into();
            return Action::None;
        }
        self.testing = true;
        self.status = "sending test notice…".into();
        Action::TestNotify
    }

    pub fn open_apprise(&mut self, m: AppriseModal) {
        self.apprise_modal = Some(m);
    }

    pub fn apprise_checked(&mut self, r: Result<Vec<String>, String>) {
        if let Some(m) = self.apprise_modal.as_mut() {
            m.checked(r);
        }
    }

    /// Fin del aviso de prueba: OK o el error (ya sin secretos).
    pub fn test_done(&mut self, r: Result<String, String>) {
        self.testing = false;
        self.status = r.unwrap_or_else(|e| e);
    }

    fn apprise_key(&mut self, k: KeyEvent) -> Action {
        let Some(m) = self.apprise_modal.as_mut() else { return Action::None };
        match m.on_key(k) {
            AKey::None => Action::None,
            AKey::Close => {
                self.apprise_modal = None;
                self.status = "notifications unchanged".into();
                Action::None
            }
            AKey::Check(e) => Action::CheckApprise(e),
            AKey::Save(e) => {
                self.apprise_modal = None;
                Action::SaveApprise(e)
            }
        }
    }

    /// `u`: CTs con chequeo positivo dentro de `from` (lo visible, el detalle o el resultado del chequeo).
    fn ask_update(&mut self, from: Vec<u32>) -> Action {
        let on_host = if self.view == View::List { self.selected().is_some_and(|g| g.kind == GuestKind::Host) } else { from == [0] };
        if self.read_only {
            self.status = "updates disabled in read-only mode (no valid config)".into();
            return Action::None;
        }
        if self.check.running || self.update.running {
            self.status = "a run is already in progress".into();
            return Action::None;
        }
        if on_host {
            // La fila del host nunca entra en una corrida de CT: tiene su propia confirmación y su propio opt-in.
            match self.inv.host.clone() {
                None => self.status = "host not reviewed yet: press c".into(),
                Some(i) if i.error.is_some() => self.status = format!("host review failed: {}", i.error.unwrap_or_default()),
                Some(i) if i.blocked.is_some() => self.status = format!("host update blocked: {}", i.blocked.unwrap_or_default()),
                Some(i) if i.packages.is_empty() => self.status = "host: nothing to update".into(),
                Some(i) => self.host_modal = Some(HostModal::confirm(i.packages.clone(), Some(&i), None)),
            }
            return Action::None;
        }
        let gpu: Vec<u32> = from.iter().copied().filter(|v| self.guest(*v).is_some_and(|g| g.kind == GuestKind::Lxc && g.gpu.nvidia)).collect();
        let only: Vec<u32> = from.into_iter().filter(|v| self.guest(*v).is_some_and(|g| g.kind == GuestKind::Lxc && g.updates() > 0)).collect();
        if only.is_empty() && gpu.len() == 1 {
            return self.ask_nvidia_libs(gpu[0]);
        }
        if only.is_empty() {
            self.status = "no CT with a positive check here: run [c] first".into();
            return Action::None;
        }
        Action::PrepareUpdate { only }
    }

    /// Abre la confirmación de las librerías NVIDIA de un CT con GPU (necesita el driver del host cargado).
    #[cfg(test)]
    pub fn open_nvidia_libs_for_test(&mut self, vmid: u32) {
        self.ask_nvidia_libs(vmid);
    }

    fn ask_nvidia_libs(&mut self, vmid: u32) -> Action {
        let Some(g) = self.guest(vmid).filter(|g| g.kind == GuestKind::Lxc && g.gpu.nvidia) else { return Action::None };
        let version = self.inv.host.as_ref().and_then(|h| h.nvidia_driver.clone());
        self.nvidia_libs = Some(NvidiaLibsModal { vmid, name: g.name.clone(), version });
        Action::None
    }

    fn nvidia_libs_key(&mut self, k: KeyEvent) -> Action {
        let Some(m) = self.nvidia_libs.as_ref() else { return Action::None };
        match k.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let vmid = m.vmid;
                self.nvidia_libs = None;
                self.update = UpdateState { running: true, started: self.clock.clone(), ..Default::default() };
                self.view = View::Update;
                return Action::NvidiaCtLibs { vmid };
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                self.nvidia_libs = None;
                self.status = "NVIDIA libs unchanged".into();
            }
            _ => {}
        }
        Action::None
    }

    /// `a`: alterna el auto-update del CT (el bucle abre el modal con la config leída).
    fn ask_auto(&mut self, vmid: Option<u32>) -> Action {
        let Some(vmid) = vmid else { return Action::None };
        if self.forced_ro {
            self.status = "read-only mode (--read-only): the config cannot be changed".into();
            return Action::None;
        }
        if self.check.running || self.update.running {
            self.status = "a run is in progress: change auto-update when it finishes".into();
            return Action::None;
        }
        if !self.guest(vmid).is_some_and(|g| (g.kind == GuestKind::Lxc && g.state != RunState::Remote) || g.kind == GuestKind::Host) {
            self.status = "auto-update applies to local LXC and the host only".into();
            return Action::None;
        }
        Action::AskAutoUpdate { vmid }
    }

    /// `b`: alterna el backup previo al update del CT (el bucle abre el modal con la config leída).
    fn ask_backup(&mut self, vmid: Option<u32>) -> Action {
        let Some(vmid) = vmid else { return Action::None };
        if self.forced_ro {
            self.status = "read-only mode (--read-only): the config cannot be changed".into();
            return Action::None;
        }
        if self.check.running || self.update.running {
            self.status = "a run is in progress: change the backup when it finishes".into();
            return Action::None;
        }
        match self.guest(vmid) {
            Some(g) if g.kind == GuestKind::Host => self.status = "the host uses its own backup (/etc, ZFS snapshot): it is not changed here".into(),
            Some(g) if g.kind == GuestKind::Lxc && g.state != RunState::Remote => return Action::AskBackup { vmid },
            _ => self.status = "backup applies to local LXC only".into(),
        }
        Action::None
    }

    pub fn open_backup_modal(&mut self, m: BackupModal) {
        self.backup_modal = Some(m);
    }

    fn backup_modal_key(&mut self, k: KeyEvent) -> Action {
        let Some(m) = self.backup_modal.as_ref() else { return Action::None };
        match k.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                let a = Action::SetBackup { vmid: m.vmid, disabled: m.disabled };
                self.backup_modal = None;
                return a;
            }
            KeyCode::Esc | KeyCode::Char('n' | 'q') => {
                self.backup_modal = None;
                self.status = "backup unchanged".into();
            }
            _ => {}
        }
        Action::None
    }

    pub fn open_auto_modal(&mut self, m: AutoModal) {
        self.auto_modal = Some(m);
    }

    fn auto_modal_key(&mut self, k: KeyEvent) -> Action {
        let Some(m) = self.auto_modal.as_mut() else { return Action::None };
        match k.code {
            KeyCode::Down | KeyCode::Up | KeyCode::Char('j' | 'k') if m.driver.is_ok() => m.focus = 1 - m.focus,
            KeyCode::Char(' ') => {
                if m.focus == 1 && !m.value {
                    self.status = "the GPU driver update needs Automatic updates checked".into();
                } else {
                    m.toggle();
                }
            }
            KeyCode::Enter => {
                let (auto, driver) = m.changes();
                let vmid = m.vmid;
                self.auto_modal = None;
                if auto.is_none() && driver.is_none() {
                    self.status = "auto-update options unchanged: nothing written".into();
                    return Action::None;
                }
                return Action::SetAutoUpdate { vmid, auto, driver };
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.auto_modal = None;
                self.status = "auto-update options unchanged".into();
            }
            _ => {}
        }
        Action::None
    }

    fn host_modal_key(&mut self, k: KeyEvent) -> Action {
        let Some(m) = self.host_modal.as_mut() else { return Action::None };
        match k.code {
            KeyCode::Esc | KeyCode::Char('q' | 'n') => {
                self.host_modal = None;
                self.status = "host update cancelled: nothing was changed".into();
            }
            KeyCode::Down | KeyCode::Char('j') => m.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => m.scroll = m.scroll.saturating_sub(1),
            KeyCode::Enter => {
                let Some(m) = self.host_modal.take() else { return Action::None };
                self.update = UpdateState { running: true, started: self.clock.clone(), ..Default::default() };
                self.view = View::Update;
                return if m.dpkg.is_some() { Action::HostRepair } else { Action::HostUpdate { confirmed: m.pkgs } };
            }
            _ => {}
        }
        Action::None
    }

    /// La corrida del host paró antes de instalar y pide algo: vuelve a la lista y abre el modal que corresponda.
    pub fn host_stop(&mut self, why: HostStop) {
        self.update.running = false;
        self.view = View::List;
        let info = self.inv.host.clone();
        match why {
            HostStop::Reconfirm(l) => {
                self.status = "the pending host list changed: confirm again".into();
                self.host_modal = Some(HostModal::confirm(l, info.as_ref(), Some("the pending list changed since you confirmed it: nothing was installed".into())));
            }
            HostStop::Dirty(d) => self.host_modal = Some(HostModal::dirty(d)),
            HostStop::Repaired => {
                self.status = "dpkg repaired: review the list and confirm again".into();
                match info.filter(|i| !i.packages.is_empty()) {
                    Some(i) => self.host_modal = Some(HostModal::confirm(i.packages.clone(), Some(&i), Some("dpkg was repaired: confirm the list again".into()))),
                    None => self.status = "dpkg repaired: press c to review the host again".into(),
                }
            }
        }
    }

    pub fn open_update_modal(&mut self, m: UpdateModal) {
        self.modal = Some(m);
    }

    fn modal_key(&mut self, k: KeyEvent) -> Action {
        let Some(m) = self.modal.as_mut() else { return Action::None };
        let n = m.cands.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.modal = None;
                self.status = "update cancelled: nothing was changed".into();
            }
            KeyCode::Down | KeyCode::Char('j') => m.cursor = (m.cursor + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => m.cursor = m.cursor.saturating_sub(1),
            KeyCode::Char('n') => {
                if let Some(v) = m.cands.get(m.cursor).map(|c| c.vmid).filter(|v| self.guest(*v).is_some_and(|g| g.gpu.nvidia)) {
                    self.modal = None;
                    return self.ask_nvidia_libs(v);
                }
                self.status = "n: the selected CT has no NVIDIA GPU".into();
            }
            KeyCode::Char(' ') => {
                if let Some(c) = m.cands.get(m.cursor).filter(|c| c.skip.is_none()) {
                    if !m.excluded.remove(&c.vmid) {
                        m.excluded.insert(c.vmid);
                    }
                }
            }
            KeyCode::Enter => {
                let only = m.chosen();
                if only.is_empty() {
                    self.status = "nothing selected".into();
                    self.modal = None;
                    return Action::None;
                }
                self.modal = None;
                self.update = UpdateState { running: true, started: self.clock.clone(), ..Default::default() };
                self.view = View::Update;
                return Action::Update { only };
            }
            _ => {}
        }
        Action::None
    }

    fn update_key(&mut self, k: KeyEvent) -> Action {
        let u = &mut self.update;
        if u.confirm_cancel {
            u.confirm_cancel = false;
            if k.code == KeyCode::Enter && u.running && !u.stop_requested {
                u.stop_requested = true;
                return Action::CancelUpdate;
            }
            return Action::None;
        }
        match k.code {
            KeyCode::Char('x' | 'q') | KeyCode::Esc if u.running => u.confirm_cancel = true,
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc | KeyCode::Backspace => self.view = View::List,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Up | KeyCode::Char('k') => u.scroll = (u.scroll + 1).min(u.log.len()),
            KeyCode::Down | KeyCode::Char('j') => u.scroll = u.scroll.saturating_sub(1),
            KeyCode::PageUp => u.scroll = (u.scroll + 10).min(u.log.len()),
            KeyCode::PageDown => u.scroll = u.scroll.saturating_sub(10),
            KeyCode::Char('f') | KeyCode::End => u.scroll = 0,
            KeyCode::Home => u.scroll = u.log.len(),
            KeyCode::Char('u') => self.status = "u disabled during run".into(),
            _ => {}
        }
        Action::None
    }

    /// Evento del motor; `stamp` = hora de llegada (HH:MM:SS) y `now` = epoch del host.
    pub fn update_event(&mut self, e: UpdEvent, stamp: &str, now: u64) {
        let u = &mut self.update;
        match e {
            UpdEvent::Queue(v) => u.queue = v.into_iter().map(|(vmid, name)| QItem { vmid, name, state: QState::Pending, detail: String::new(), started: 0, took_s: 0 }).collect(),
            UpdEvent::Start(v) => {
                u.current = Some(v);
                if let Some(i) = u.queue.iter_mut().find(|i| i.vmid == v) {
                    i.state = QState::Active(Step::Preflight);
                    i.started = now;
                }
            }
            UpdEvent::Step(v, s) => {
                if let Some(i) = u.queue.iter_mut().find(|i| i.vmid == v) {
                    i.state = QState::Active(s);
                }
            }
            UpdEvent::Line(v, l) => {
                if u.log.len() >= LOG_RING {
                    u.log.pop_front();
                    u.dropped += 1;
                }
                u.log.push_back((stamp.to_string(), v, l));
                if u.scroll > 0 {
                    u.scroll = (u.scroll + 1).min(u.log.len()); // quien lee hacia atrás no salta
                }
            }
            UpdEvent::Done(r) => {
                if let Some(i) = u.queue.iter_mut().find(|i| i.vmid == r.vmid) {
                    i.state = QState::Done(r.outcome);
                    i.detail = r.detail.clone();
                    i.took_s = r.took_s;
                }
                if u.current == Some(r.vmid) {
                    u.current = None;
                }
            }
            // solo para la mini TUI de `pxu update --host`
            UpdEvent::Nv(_) => {}
        }
    }

    pub fn update_done(&mut self, rep: RunReport, notes: Vec<String>) {
        self.update.running = false;
        self.update.stop_requested = false;
        self.update.confirm_cancel = false;
        self.update.current = None;
        self.update.report = Some(rep);
        self.update.notes = notes;
    }

    pub fn update_failed(&mut self, msg: String) {
        self.update.running = false;
        self.update.confirm_cancel = false;
        self.update.failed = Some(msg.clone());
        self.status = msg;
    }

    fn list_key(&mut self, k: KeyEvent) -> Action {
        let n = self.visible().len();
        match k.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc => {
                self.search.clear();
                self.clamp();
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('1') => self.set_filter(Filter::All),
            KeyCode::Char('2') => self.set_filter(Filter::Running),
            KeyCode::Char('3') => self.set_filter(Filter::Stopped),
            KeyCode::Char('4') => self.set_filter(Filter::Updates),
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.cursor = 0;
            }
            KeyCode::Char('r') => return Action::Reload,
            KeyCode::Char('g') => return Action::OpenConfig,
            KeyCode::Char('l') => return Action::OpenLogs,
            KeyCode::Char('d') => return self.open_storage(),
            KeyCode::Char('S') => return self.ask_schedule(),
            KeyCode::Char('a') => {
                let v = self.selected().map(|g| g.vmid);
                return self.ask_auto(v);
            }
            KeyCode::Char('b') => {
                let v = self.selected().map(|g| g.vmid);
                return self.ask_backup(v);
            }
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::PageDown => self.cursor = (self.cursor + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(10),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = n.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(g) = self.selected() {
                    self.view = View::Detail(g.vmid);
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Char('c') => return self.start_check(None),
            KeyCode::Char('C') => return self.start_check(Some(false)),
            KeyCode::Char('o') => self.col_modal = Some((0, false)),
            KeyCode::Char('v') if self.check.report.is_some() || self.check.running => self.view = View::Check,
            KeyCode::Char('u') => {
                let from: Vec<u32> = self.visible().iter().map(|g| g.vmid).collect();
                return self.ask_update(from);
            }
            _ => {}
        }
        Action::None
    }

    pub fn col_shown(&self, name: &str) -> bool {
        self.columns.iter().any(|c| c == name)
    }

    /// Selector de columnas: Espacio marca/desmarca y se aplica al momento; Enter/Esc cierran y, si hubo cambios, piden guardarlos.
    fn col_modal_key(&mut self, k: KeyEvent) -> Action {
        let Some((cur, changed)) = self.col_modal else { return Action::None };
        let n = pxu::share::config::OPTIONAL_COLUMNS.len();
        match k.code {
            KeyCode::Down | KeyCode::Char('j') => self.col_modal = Some(((cur + 1).min(n - 1), changed)),
            KeyCode::Up | KeyCode::Char('k') => self.col_modal = Some((cur.saturating_sub(1), changed)),
            KeyCode::Char(' ') => {
                let name = pxu::share::config::OPTIONAL_COLUMNS[cur];
                self.columns = pxu::share::config::OPTIONAL_COLUMNS.iter().filter(|c| (**c == name) != self.col_shown(c)).map(|c| c.to_string()).collect();
                self.col_modal = Some((cur, true));
            }
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('o') | KeyCode::Char('q') => {
                self.col_modal = None;
                if changed {
                    return Action::SaveColumns(self.columns.clone());
                }
            }
            _ => {}
        }
        Action::None
    }

    /// Un update en curso o un chequeo ya corriendo bloquean otro chequeo (el refresco escribe en los CT).
    fn check_blocked(&mut self) -> bool {
        if self.update.running {
            // el sondeo con refresco escribe en los CT: durante un update solo trabaja el motor de update
            self.status = "an update is running: checks (c/C/R) are disabled until it finishes".into();
            return true;
        }
        if self.check.running {
            self.status = "a check is already running".into();
            return true;
        }
        false
    }

    /// Desde la lista se chequea lo filtrado/buscado; sin filtro, todos los LXC.
    /// `refresh`: `None` = refresca según `[check] refresh` (`c`); `Some(false)` = solo lectura (`C`). En `[RO]`, `c` no refresca.
    fn start_check(&mut self, refresh: Option<bool>) -> Action {
        if self.check_blocked() {
            return Action::None;
        }
        let ro_note = self.read_only && refresh.is_none() && self.refresh_default;
        let refresh = if self.read_only { Some(false) } else { refresh };
        let only: Vec<u32> = if self.view == View::List && (self.filter != Filter::All || !self.search.is_empty()) {
            self.visible().iter().map(|g| g.vmid).collect()
        } else {
            Vec::new()
        };
        let total = check::targets(&self.inv, &only).len();
        if total == 0 {
            self.status = "no LXC to check in this selection".into();
            return Action::None;
        }
        let shown = refresh.unwrap_or(self.refresh_default);
        self.check = CheckState { total, running: true, refresh: shown, ..Default::default() };
        if ro_note {
            self.status = "read-only mode: checking without refreshing the indexes".into();
        }
        self.view = View::Check;
        Action::Check { refresh, only }
    }

    fn check_key(&mut self, k: KeyEvent) -> Action {
        let n = self.check.results.len();
        match k.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc | KeyCode::Backspace => {
                if self.check.running {
                    self.check.cancelling = true;
                    return Action::CancelCheck;
                }
                self.view = View::List;
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('c') => return self.start_check(None),
            KeyCode::Char('C' | 'R') => return self.start_check(Some(false)),
            KeyCode::Down | KeyCode::Char('j') => self.check.cursor = (self.check.cursor + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => self.check.cursor = self.check.cursor.saturating_sub(1),
            KeyCode::PageDown => self.check.cursor = (self.check.cursor + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => self.check.cursor = self.check.cursor.saturating_sub(10),
            KeyCode::Home => self.check.cursor = 0,
            KeyCode::End => self.check.cursor = n.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(r) = self.check.results.get(self.check.cursor) {
                    self.view = View::Detail(r.vmid);
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Char('u') => {
                let from: Vec<u32> = self.check.results.iter().filter(|r| r.updates() > 0).map(|r| r.vmid).collect();
                return self.ask_update(from);
            }
            _ => {}
        }
        Action::None
    }

    pub fn check_progress(&mut self, r: CtResult) {
        self.check.results.push(r);
    }

    pub fn check_done(&mut self, rep: Report) {
        self.check.running = false;
        self.check.took_s = rep.finished_at.saturating_sub(rep.started_at);
        self.check.finished = super::hhmm(rep.finished_at);
        self.check.results = rep.results.clone();
        self.check.report = Some(rep);
    }

    pub fn check_failed(&mut self, msg: String) {
        self.check.running = false;
        self.status = msg;
    }

    fn set_filter(&mut self, f: Filter) {
        self.filter = f;
        self.cursor = 0;
    }

    fn detail_key(&mut self, k: KeyEvent) -> Action {
        match k.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Esc | KeyCode::Backspace => self.view = View::List,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => return Action::Reload,
            KeyCode::Char('c') => return self.start_check(None),
            KeyCode::Char('g') => return Action::OpenConfig,
            KeyCode::Char('S') => return self.ask_schedule(),
            KeyCode::Char('a') => {
                if let View::Detail(id) = self.view {
                    return self.ask_auto(Some(id));
                }
            }
            KeyCode::Char('b') => {
                if let View::Detail(id) = self.view {
                    return self.ask_backup(Some(id));
                }
            }
            KeyCode::Char('u') => {
                if let View::Detail(id) = self.view {
                    return self.ask_update(vec![id]);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.detail_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => self.detail_scroll = self.detail_scroll.saturating_sub(1),
            KeyCode::PageDown => self.detail_scroll += 10,
            KeyCode::PageUp => self.detail_scroll = self.detail_scroll.saturating_sub(10),
            KeyCode::Home => self.detail_scroll = 0,
            _ => {}
        }
        Action::None
    }
}
