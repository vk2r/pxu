//! Trait `Host` (ARCHITECTURE §8.1): único punto de contacto con el sistema.
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

mod readonly;
pub use readonly::ReadOnlyHost;
pub(crate) use readonly::{pxu_snapshot, vzdump_volid};
mod real;
pub use real::RealHost;

/// Prioridad de E/S de los hijos (`ioprio_set`); se hereda por fork/exec (ARCHITECTURE §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoPrio {
    Idle,
    BestEffort7,
}

/// Directorio de locks de ProxMenux (compartido, 1777): pxu solo lee sus locks `proxmenux-lxc-*`, nunca crea nada ahí (A-08).
pub const PROXMENUX_LOCK_DIR: &str = "/run/lock";
/// Directorio de los locks propios de pxu (root:root 0700, validado antes de abrir cada lock).
pub const PXU_LOCK_DIR: &str = "/run/pxu";

/// Descriptor de un lock `flock`: se libera al soltarlo (o si el proceso muere).
pub type LockGuard = Box<dyn std::any::Any + Send>;

/// Modo y dueño de un archivo (para validar `/etc/pxu/config.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    pub mode: u32,
    pub uid: u32,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ExecOpts<'a> {
    pub nice: Option<i32>,
    pub ionice: Option<IoPrio>,
    pub timeout_s: Option<u64>,
    /// `true`: al vencer `timeout_s` NO se mata nada (apt-get/dpkg no se interrumpen a medias); se espera a que
    /// el proceso termine y entonces se devuelve `TimedOut`.
    pub no_kill_on_timeout: bool,
    /// Con `no_kill_on_timeout`: margen (s) tras `timeout_s` antes de rendirse (SIGTERM al proceso; SIGKILL a su grupo si sigue vivo `min(300, margen)` s después).
    /// `None` = `NO_KILL_GRACE_S`. Así ninguna espera es infinita.
    pub no_kill_grace_s: Option<u64>,
    /// Bytes que se escriben en el stdin del proceso (secretos: nunca van en argv).
    pub stdin: Option<Vec<u8>>,
    pub env: Vec<(String, String)>,
    /// Si pasa a `true` mientras corre, el host real manda SIGTERM al proceso (solo a él; sus hijos los limpia él mismo) y devuelve
    /// `Interrupted`. Solo para órdenes que se pueden abortar limpiamente (vzdump): nunca para apt/dpkg.
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// Con `cancel`: SIGTERM a TODO el grupo de procesos (no solo al hijo) y SIGKILL al grupo si sigue vivo 1 s después. Para órdenes sin
    /// estado que limpiar (curl, sha256sum, el `.run`, `make -jN`, que tiene hijos): la cancelación corta en menos de 2 s y no deja procesos.
    pub cancel_group: bool,
    /// `true`: stderr va aparte (a `LineSink::err_line`) en vez de mezclarse con stdout. Para órdenes cuya stdout es un dato
    /// (systemd-creds), donde un aviso en stderr lo corrompería.
    pub split_stderr: bool,
}

/// Límites de la salida de un comando (A-04): un CT comprometido no debe poder agotar la memoria del host.
/// Pasado el tope se sigue drenando (el hijo no se bloquea) pero se descarta, con una única marca.
pub const MAX_LINE_BYTES: usize = 64 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_OUTPUT_LINES: usize = 100_000;
/// Marca que se entrega una sola vez a `LineSink::line` cuando se recortó la salida.
pub const TRUNCATED_MARK: &str = "[pxu: output truncated]";

/// Margen por omisión de `ExecOpts::no_kill_grace_s`.
pub const NO_KILL_GRACE_S: u64 = 1800;

/// Texto del error de `ExecOpts::cancel` (el motor lo reconoce por el tipo `Interrupted`).
pub const CANCELLED_MSG: &str = "interrupted: cancelled by user (SIGTERM sent)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: i32,
}

/// Lee la respuesta JSON de `pvesh` entre las líneas capturadas. `pvesh` sale con 0 aunque OTRO storage falle y antes del JSON
/// imprime avisos (p. ej. «remote-backup-3: error fetching datastores…»): se ignora todo lo anterior al primer `{`/`[` que
/// da un JSON válido y esas líneas se devuelven como avisos. Sin JSON válido → `Err` (falla cerrado).
pub fn json_answer(lines: &[String]) -> Result<(serde_json::Value, Vec<String>), String> {
    let whole = lines.join("\n");
    if let Ok(v) = serde_json::from_str(&whole) {
        return Ok((v, Vec::new()));
    }
    for (i, l) in lines.iter().enumerate() {
        if !l.trim_start().starts_with(['{', '[']) {
            continue;
        }
        let rest = lines[i..].join("\n");
        // el primer valor del texto; lo que venga detrás (avisos tardíos) se ignora
        if let Some(Ok(v)) = serde_json::Deserializer::from_str(&rest).into_iter::<serde_json::Value>().next() {
            if v.is_object() || v.is_array() {
                return Ok((v, lines[..i].iter().filter(|w| !w.trim().is_empty()).cloned().collect()));
            }
        }
    }
    let first = whole.lines().next().unwrap_or("").chars().take(80).collect::<String>();
    Err(format!("no JSON in the pvesh answer (first line: {first:?})"))
}

pub trait LineSink {
    fn line(&mut self, line: &str);
    /// Línea de stderr cuando `ExecOpts::split_stderr`; por omisión se descarta.
    fn err_line(&mut self, _line: &str) {}
}

impl LineSink for Vec<String> {
    fn line(&mut self, line: &str) {
        self.push(line.to_string());
    }
}

pub trait Host: Sync {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>>;
    fn exists(&self, path: &Path) -> bool;
    fn guest_exec(&self, vmid: u32, argv: &[&str], opts: &ExecOpts, sink: &mut dyn LineSink) -> io::Result<ExitInfo>;
    fn host_cmd(&self, argv: &[&str], opts: &ExecOpts, sink: &mut dyn LineSink) -> io::Result<ExitInfo>;
    /// Escritura atómica (tmp+rename) de estado propio de pxu. Crea los directorios.
    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()>;
    /// Escribe `/etc/pxu/config.toml` (root, 0600) de forma atómica (tmp en el mismo directorio + rename). Solo la TUI lo usa, tras confirmar.
    fn write_config(&self, _data: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "this host cannot write the config"))
    }
    /// Escribe la credencial cifrada del aviso de fallo (`FAILURE_CRED_PATH`, root, 0600, atómica). Solo la TUI lo usa, junto con la config de Apprise.
    fn write_failure_notice(&self, _data: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "this host cannot write the failure-notice credential"))
    }
    /// Borra la credencial cifrada del aviso de fallo (`FAILURE_CRED_PATH`; que no exista no es error). Al desactivar Apprise no debe quedar una vieja.
    fn remove_failure_notice(&self) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "this host cannot remove the failure-notice credential"))
    }
    /// Estado real del timer `pxu-update` (solo lectura).
    fn timer_status(&self) -> io::Result<crate::share::schedule::TimerStatus> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "this host cannot query systemd"))
    }
    /// Valida un `OnCalendar` (`systemd-analyze calendar`) y calcula la próxima ejecución (solo lectura).
    fn validate_calendar(&self, _expr: &str) -> io::Result<crate::share::schedule::CalendarCheck> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "this host cannot validate schedules"))
    }
    /// Instala las unidades (si faltan), escribe el drop-in del horario (root, 0644, atómico), `daemon-reload` y `enable --now` / `disable --now`. Solo la TUI lo usa, tras confirmar.
    fn apply_schedule(&self, _plan: &crate::share::schedule::SchedulePlan) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "this host cannot change the schedule"))
    }
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    /// Borra un directorio SOLO si está vacío (la retención de logs retira las carpetas `<vmid>` viejas). No sigue symlinks.
    fn remove_empty_dir(&self, _path: &Path) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "this host cannot remove directories"))
    }
    /// Primeros `max` bytes de un archivo y su tamaño total (la lista de logs lee solo la cabecera). No sigue symlinks en el host real.
    fn read_head(&self, path: &Path, max: usize) -> io::Result<(Vec<u8>, u64)> {
        let mut b = self.read_file(path)?;
        let n = b.len() as u64;
        b.truncate(max);
        Ok((b, n))
    }
    /// Añade `data` al final de un log propio (crea el archivo 0600; no sigue symlinks). Lo usa el log en vivo de `update --host`.
    fn append_file(&self, _path: &Path, _data: &[u8]) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "this host cannot append to files"))
    }
    fn lock_dir(&self) -> &Path;
    /// `flock(LOCK_EX|LOCK_NB)` sobre `path`. `Ok(None)` = ya tomado. Sin `create`, un archivo ausente es `NotFound`.
    fn try_lock(&self, path: &Path, create: bool) -> io::Result<Option<LockGuard>>;
    fn file_meta(&self, path: &Path) -> io::Result<FileMeta>;
    /// Opt-in explícito de `update`: habilita las escrituras de la lista blanca de update (no-op fuera de `ReadOnlyHost`).
    fn set_update_enabled(&self, _on: bool) {}
    /// CT en curso de `update`: las escrituras de la lista blanca solo valen para él (no-op fuera de `ReadOnlyHost`).
    fn set_update_target(&self, _vmid: Option<u32>) {}
    /// Opt-in explícito de `update --host` (instalar las actualizaciones del host Proxmox): abre SOLO la lista blanca de `hostupdate` (no-op fuera de `ReadOnlyHost`).
    fn set_host_update_enabled(&self, _on: bool) {}
    /// Opt-in explícito del paso NVIDIA de la corrida programada: abre SOLO los comandos exactos del permiso (no-op fuera de `ReadOnlyHost`).
    fn set_nvidia_update(&self, _g: Option<crate::share::update::nvidia::Grant>) {}
    /// Opt-in explícito de la TUI (tecla `a` tras confirmar): habilita `write_config` (no-op fuera de `ReadOnlyHost`).
    fn set_config_edit(&self, _on: bool) {}
    /// Opt-in del borrado manual de UN backup (`pvesm free <volid>`, página Storage): solo ese volid exacto.
    fn set_backup_delete(&self, _volid: Option<String>) {}
    /// Opt-in explícito de la TUI (tecla `S` tras confirmar): habilita `apply_schedule` (no-op fuera de `ReadOnlyHost`).
    fn set_schedule_edit(&self, _on: bool) {}
    fn now(&self) -> SystemTime;
    /// ¿Es un host de pruebas? (la UI lo anuncia en el encabezado)
    fn is_mock(&self) -> bool {
        false
    }
}

pub fn read_string(h: &dyn Host, path: &str) -> io::Result<String> {
    Ok(String::from_utf8_lossy(&h.read_file(Path::new(path))?).into_owned())
}

/// Opt-in de escritura de la config con RAII: se retira en toda salida.
pub struct ConfigEditOptIn<'a>(&'a dyn Host);

impl<'a> ConfigEditOptIn<'a> {
    pub fn new(host: &'a dyn Host) -> Self {
        host.set_config_edit(true);
        ConfigEditOptIn(host)
    }
}

impl Drop for ConfigEditOptIn<'_> {
    fn drop(&mut self) {
        self.0.set_config_edit(false);
    }
}

/// Opt-in de cambio del horario con RAII: se retira en toda salida.
pub struct ScheduleEditOptIn<'a>(&'a dyn Host);

impl<'a> ScheduleEditOptIn<'a> {
    pub fn new(host: &'a dyn Host) -> Self {
        host.set_schedule_edit(true);
        ScheduleEditOptIn(host)
    }
}

impl Drop for ScheduleEditOptIn<'_> {
    fn drop(&mut self) {
        self.0.set_schedule_edit(false);
    }
}

/// Opt-in de escritura de `update` con RAII: se retira en toda salida (return temprano, pánico con unwind).
pub struct UpdateOptIn<'a>(&'a dyn Host);

impl<'a> UpdateOptIn<'a> {
    pub fn new(host: &'a dyn Host) -> Self {
        host.set_update_enabled(true);
        UpdateOptIn(host)
    }
}

impl Drop for UpdateOptIn<'_> {
    fn drop(&mut self) {
        self.0.set_update_enabled(false);
    }
}

/// Opt-in de la instalación en el host con RAII: se retira en toda salida. Va APARTE de `UpdateOptIn`: un update de CT jamás ejecuta apt en el host.
pub struct HostUpdateOptIn<'a>(&'a dyn Host);

impl<'a> HostUpdateOptIn<'a> {
    pub fn new(host: &'a dyn Host) -> Self {
        host.set_host_update_enabled(true);
        HostUpdateOptIn(host)
    }
}

impl Drop for HostUpdateOptIn<'_> {
    fn drop(&mut self) {
        self.0.set_host_update_enabled(false);
    }
}

/// Opt-in del borrado de un backup (`pvesm free <volid>`) con RAII: se retira en toda salida. Vale para ese volid y nada más.
pub struct BackupDeleteOptIn<'a>(&'a dyn Host);

impl<'a> BackupDeleteOptIn<'a> {
    pub fn new(host: &'a dyn Host, volid: &str) -> Self {
        host.set_backup_delete(Some(volid.to_string()));
        BackupDeleteOptIn(host)
    }
}

impl Drop for BackupDeleteOptIn<'_> {
    fn drop(&mut self) {
        self.0.set_backup_delete(None);
    }
}

/// Opt-in del paso NVIDIA con RAII: se retira en toda salida. Va APARTE de los de CT y host.
pub struct NvidiaOptIn<'a>(&'a dyn Host);

impl<'a> NvidiaOptIn<'a> {
    pub fn new(host: &'a dyn Host, grant: crate::share::update::nvidia::Grant) -> Self {
        host.set_nvidia_update(Some(grant));
        NvidiaOptIn(host)
    }
}

impl Drop for NvidiaOptIn<'_> {
    fn drop(&mut self) {
        self.0.set_nvidia_update(None);
    }
}

#[cfg(test)]
mod json_answer_tests {
    use super::json_answer;

    fn v(l: &[&str]) -> Vec<String> {
        l.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn plain_json_has_no_warnings() {
        let (j, w) = json_answer(&v(&["{\"avail\":5}"])).unwrap();
        assert_eq!(j["avail"], 5);
        assert!(w.is_empty());
    }

    #[test]
    fn warning_before_json_is_returned_aside() {
        let (j, w) = json_answer(&v(&["remote-backup-3: error fetching datastores - fingerprint 'F3' not verified, abort!", "{\"avail\":74015944704}"])).unwrap();
        assert_eq!(j["avail"], 74015944704u64);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn warning_before_pretty_array_and_trailing_text() {
        let (j, w) = json_answer(&v(&["[warn] x", "[", "  {\"a\":1}", "]", "late warning"])).unwrap();
        assert_eq!(j[0]["a"], 1);
        assert_eq!(w, v(&["[warn] x"]));
    }

    #[test]
    fn no_json_fails_closed() {
        assert!(json_answer(&v(&["remote-backup-3: error", "ok"])).is_err());
        assert!(json_answer(&[]).is_err());
        assert!(json_answer(&v(&["warning", "42"])).is_err());
    }
}
