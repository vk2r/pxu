//! `/etc/pxu/config.toml` (ARCHITECTURE §5): listado, chequeo, update, backup y `[apprise]`.
use crate::share::host::Host;
use serde::Deserialize;
use std::path::Path;

pub const CONFIG_PATH: &str = "/etc/pxu/config.toml";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralCfg,
    #[serde(default)]
    pub check: CheckCfg,
    #[serde(default)]
    pub apprise: crate::share::notify::AppriseCfg,
    #[serde(default)]
    pub update: UpdateCfg,
    #[serde(default)]
    pub backup: BackupCfg,
    #[serde(default)]
    pub ct: Vec<CtOverride>,
    #[serde(default)]
    pub ui: UiCfg,
}

/// Columnas opcionales de la lista (ID, NAME y STATE son fijas), en el orden en que se muestran en la TUI.
pub const OPTIONAL_COLUMNS: &[&str] = &["type", "os", "cpu", "ram", "disk", "tags", "gpu", "upd", "backup", "auto"];

/// `[ui]`: preferencias de la TUI. `columns` ausente ⇒ todas las opcionales salvo `tags`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UiCfg {
    #[serde(default)]
    pub columns: Option<Vec<String>>,
}

impl UiCfg {
    /// Columnas opcionales visibles, en minúsculas y sin repetir; los nombres desconocidos se ignoran.
    pub fn visible_columns(&self) -> Vec<String> {
        match &self.columns {
            None => OPTIONAL_COLUMNS.iter().filter(|c| **c != "tags").map(|c| c.to_string()).collect(),
            Some(v) => OPTIONAL_COLUMNS.iter().filter(|c| v.iter().any(|x| x.eq_ignore_ascii_case(c))).map(|c| c.to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GeneralCfg {
    #[serde(default = "d_nice")]
    pub nice: i32,
    /// "idle" | "best-effort"
    #[serde(default = "d_io")]
    pub io_class: String,
    /// Días que se conservan los logs de `/var/log/pxu`; 0 = no borrar nunca. (`logs_per_ct` de configs antiguas se ignora.)
    #[serde(default = "d_log_days")]
    pub log_retention_days: u32,
}
fn d_io() -> String {
    "idle".into()
}
fn d_log_days() -> u32 {
    30
}
/// Tope de `log_retention_days` (10 años): un valor mayor casi seguro es una errata.
pub const MAX_LOG_RETENTION_DAYS: u32 = 3650;
fn d_nice() -> i32 {
    10
}
impl Default for GeneralCfg {
    fn default() -> Self {
        GeneralCfg { nice: d_nice(), io_class: d_io(), log_retention_days: d_log_days() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckCfg {
    /// Por defecto SÍ se refrescan los índices (`refresh = false` lo apaga: chequeo solo lectura).
    #[serde(default = "d_refresh")]
    pub refresh: bool,
    #[serde(default = "d_min_age")]
    pub refresh_min_age_hours: u64,
    #[serde(default = "d_timeout")]
    pub timeout_s: u64,
    /// Un índice más viejo que esto se marca `stale` en el resultado.
    #[serde(default = "d_stale_days")]
    pub index_stale_days: u64,
}
fn d_refresh() -> bool {
    true
}
fn d_min_age() -> u64 {
    12
}
fn d_timeout() -> u64 {
    60
}
fn d_stale_days() -> u64 {
    7
}
impl Default for CheckCfg {
    fn default() -> Self {
        CheckCfg { refresh: d_refresh(), refresh_min_age_hours: d_min_age(), timeout_s: d_timeout(), index_stale_days: d_stale_days() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateCfg {
    #[serde(default)]
    pub auto_update_tags: Vec<String>,
    /// "skip" | "start_and_restore"
    #[serde(default = "d_skip")]
    pub stopped: String,
    #[serde(default = "d_fresh")]
    pub require_fresh_check_minutes: u64,
    #[serde(default = "d_free")]
    pub min_free_mb: u64,
    /// "never" | "if-required"
    #[serde(default = "d_never")]
    pub reboot: String,
    /// "dist-upgrade" | "upgrade"
    #[serde(default = "d_dist")]
    pub apt_mode: String,
    /// "continue" | "stop"
    #[serde(default = "d_continue")]
    pub on_failure: String,
    #[serde(default = "d_maxfail")]
    pub max_consecutive_failures: u32,
    #[serde(default = "d_upd_timeout")]
    pub timeout_min: u64,
    /// `true` ⇒ la corrida programada actualiza también el HOST Proxmox, antes que los CT. Apagado por defecto (opt-in explícito).
    #[serde(default)]
    pub host_auto: bool,
    /// `true` ⇒ la corrida programada actualiza el driver NVIDIA del host (instalado con el `.run`) tras el update del host. Apagado por defecto.
    #[serde(default)]
    pub host_nvidia_auto: bool,
}
fn d_skip() -> String {
    "skip".into()
}
fn d_fresh() -> u64 {
    60
}
fn d_free() -> u64 {
    512
}
fn d_never() -> String {
    "never".into()
}
fn d_dist() -> String {
    "dist-upgrade".into()
}
fn d_continue() -> String {
    "continue".into()
}
fn d_maxfail() -> u32 {
    2
}
fn d_upd_timeout() -> u64 {
    30
}
impl Default for UpdateCfg {
    fn default() -> Self {
        UpdateCfg {
            auto_update_tags: vec![],
            stopped: d_skip(),
            require_fresh_check_minutes: d_fresh(),
            min_free_mb: d_free(),
            reboot: d_never(),
            apt_mode: d_dist(),
            on_failure: d_continue(),
            max_consecutive_failures: d_maxfail(),
            timeout_min: d_upd_timeout(),
            host_auto: false,
            host_nvidia_auto: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackupCfg {
    /// "auto" (snapshot y, si el almacenamiento no lo soporta, vzdump) | "snapshot" | "vzdump" | "none" (sin red de seguridad).
    #[serde(default = "d_auto")]
    pub mode: String,
    /// Storage de PVE para los vzdump.
    #[serde(default = "d_storage")]
    pub storage: String,
    /// Backups de pxu que se conservan por CT tras un update correcto (0 = ninguno: el backup es temporal).
    pub keep: Option<usize>,
    /// Obsoleto: se lee como `keep` si éste falta (y `config check` avisa).
    pub keep_snapshots: Option<usize>,
    /// Tamaño estimado del vzdump = espacio usado del CT × `space_ratio` (zstd: 1.0 = sin compresión, el caso seguro).
    #[serde(default = "d_ratio")]
    pub space_ratio: f64,
    /// Espacio libre mínimo (GB) que debe quedar en el storage tras el vzdump (y como mínimo el 10 % de su tamaño).
    #[serde(default = "d_min_free_gb")]
    pub min_free_gb: u64,
}
fn d_ratio() -> f64 {
    1.0
}
fn d_min_free_gb() -> u64 {
    5
}
fn d_auto() -> String {
    "auto".into()
}
fn d_storage() -> String {
    "local".into()
}
impl BackupCfg {
    pub fn keep(&self) -> usize {
        self.keep.or(self.keep_snapshots).unwrap_or(0)
    }
}
impl Default for BackupCfg {
    fn default() -> Self {
        BackupCfg { mode: d_auto(), storage: d_storage(), keep: None, keep_snapshots: None, space_ratio: d_ratio(), min_free_gb: d_min_free_gb() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CtOverride {
    pub vmid: u32,
    pub auto_update: Option<bool>,
    /// Este CT recibe las librerías NVIDIA de la versión del driver del host (corrida programada). Apagado por defecto.
    #[serde(default)]
    pub nvidia_auto: Option<bool>,
    /// Este CT refresca el índice al chequear (override de `[check] refresh`).
    pub refresh: Option<bool>,
    /// Override de `[backup] mode` para este CT.
    pub backup: Option<String>,
    /// Override de `[backup] keep` para este CT.
    pub keep: Option<usize>,
}

impl Config {
    /// Sin archivo ⇒ configuración vacía (solo lectura, nada se autoactualiza).
    /// Con archivo: se niega si no es de root o es accesible por grupo/otros (ARCHITECTURE §5).
    pub fn load(host: &dyn Host) -> Result<Config, String> {
        let p = Path::new(CONFIG_PATH);
        if !host.exists(p) {
            return Ok(Config::default());
        }
        if let Some(e) = perm_problem(host) {
            return Err(format!("{CONFIG_PATH}: {e}"));
        }
        let bytes = host.read_file(p).map_err(|e| format!("{CONFIG_PATH}: {e}"))?;
        toml::from_str(&String::from_utf8_lossy(&bytes)).map_err(|e| format!("{CONFIG_PATH}: {e}"))
    }

    /// Carga para usar la config al ESCRIBIR (`update`, TUI): además de `load`, pasa por la misma validación que `config check`.
    /// Errores (valores fuera de rango, `on_failure = "Stop"`…) y claves mal escritas se rechazan; los avisos no bloquean y se devuelven.
    pub fn load_checked(host: &dyn Host) -> Result<(Config, Vec<String>), String> {
        let cfg = Config::load(host)?;
        if !host.exists(Path::new(CONFIG_PATH)) {
            return Ok((cfg, vec![]));
        }
        let v = validate(host);
        let bad: Vec<&String> = v.errors.iter().chain(v.typos.iter()).collect();
        if !bad.is_empty() {
            return Err(format!("{CONFIG_PATH} is invalid: {} (see `pxu config check`)", bad.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ")));
        }
        Ok((cfg, v.warnings))
    }

    /// Por qué `auto_update` vale lo que vale para este CT (para el modal de la TUI).
    pub fn auto_update_why(&self, vmid: u32, tags: &[String]) -> String {
        if vmid == 0 {
            return format!("{} by [update] host_auto", if self.update.host_auto { "yes" } else { "no" });
        }
        if let Some(a) = self.ct.iter().find(|c| c.vmid == vmid).and_then(|c| c.auto_update) {
            return format!("{} by [[ct]] override", if a { "yes" } else { "no" });
        }
        match tags.iter().find(|t| self.update.auto_update_tags.contains(t)) {
            Some(t) => format!("yes by tag {t}"),
            None => "no (no override, no matching tag)".into(),
        }
    }

    pub fn backup_mode(&self, vmid: u32) -> &str {
        self.ct.iter().find(|c| c.vmid == vmid).and_then(|c| c.backup.as_deref()).unwrap_or(&self.backup.mode)
    }

    /// De dónde viene el modo de backup de un CT: `[[ct]] backup` (override) o `[backup] mode` (global).
    pub fn backup_why(&self, vmid: u32) -> String {
        match self.ct.iter().find(|c| c.vmid == vmid).and_then(|c| c.backup.as_deref()) {
            Some(m) => format!("{m} by [[ct]] override"),
            None => format!("{} by [backup] mode (global)", self.backup.mode),
        }
    }

    pub fn keep_for(&self, vmid: u32) -> usize {
        self.ct.iter().find(|c| c.vmid == vmid).and_then(|c| c.keep).unwrap_or_else(|| self.backup.keep())
    }

    /// ¿Recibe este CT las librerías NVIDIA del driver del host en la corrida programada? (`[[ct]] nvidia_auto`; el host usa `[update] host_nvidia_auto`.)
    pub fn nvidia_auto(&self, vmid: u32) -> bool {
        if vmid == 0 {
            return self.update.host_nvidia_auto;
        }
        self.ct.iter().find(|c| c.vmid == vmid).and_then(|c| c.nvidia_auto).unwrap_or(false)
    }

    /// Prioridad: `[[ct]]` > tag > nada (ARCHITECTURE §5).
    pub fn auto_update(&self, vmid: u32, tags: &[String]) -> bool {
        // vmid 0 = el host Proxmox: no hay override por CT ni tags; solo `[update] host_auto` (opt-in aparte) lo incluye en la corrida programada
        if vmid == 0 {
            return false;
        }
        if let Some(o) = self.ct.iter().find(|c| c.vmid == vmid) {
            if let Some(a) = o.auto_update {
                return a;
            }
        }
        tags.iter().any(|t| self.update.auto_update_tags.contains(t))
    }
}

/// Problema de permisos del archivo de configuración (contiene secretos y define qué CT se tocan).
pub fn perm_problem(host: &dyn Host) -> Option<String> {
    // Falla en cerrado: si no se puede ni hacer `stat` (o es un symlink), la config no se usa.
    let m = match host.file_meta(Path::new(CONFIG_PATH)) {
        Ok(m) => m,
        Err(e) => return Some(format!("cannot stat: {e}")),
    };
    if m.uid != 0 {
        return Some(format!("owner is uid {}, must be root", m.uid));
    }
    if m.mode & 0o077 != 0 {
        return Some(format!("mode is {:04o}, must be 0600 (it holds secrets and selects what gets updated)", m.mode & 0o777));
    }
    None
}

const KEYS: &[(&str, &[&str])] = &[
    ("general", &["nice", "io_class", "log_retention_days", "logs_per_ct"]),
    ("check", &["refresh", "refresh_min_age_hours", "timeout_s", "index_stale_days"]),
    ("update", &["auto_update_tags", "stopped", "require_fresh_check_minutes", "min_free_mb", "reboot", "apt_mode", "on_failure", "max_consecutive_failures", "timeout_min", "host_auto", "host_nvidia_auto"]),
    ("backup", &["mode", "storage", "keep", "space_ratio", "min_free_gb"]),
    ("ui", &["columns"]),
    ("apprise", &["transport", "api_url", "api_key", "urls", "tag", "on", "timeout_s", "retry_delays_ms"]),
];
const CT_KEYS: &[&str] = &["vmid", "auto_update", "nvidia_auto", "refresh", "backup", "keep"];
/// Claves retiradas: se avisa en claro en vez de ignorarlas en silencio.
const DEPRECATED: &[(&str, &str, &str)] = &[
    ("general", "logs_per_ct", "removed: logs are kept `log_retention_days` days (default 30, 0 = never delete); it is ignored"),
    ("backup", "keep_snapshots", "renamed to `keep` (default 0); it is still read as `keep` for now"),
    ("backup", "fallback", "removed: `mode = \"auto\"` already falls back from snapshot to vzdump"),
];

/// Resultado de `pxu config check`: errores (exit 2) y avisos.
#[derive(Debug, Default)]
pub struct Validation {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub notes: Vec<String>,
    /// Claves desconocidas de secciones conocidas (también están en `warnings`): una errata como `on_falure` se ignoraría en silencio,
    /// así que `load_checked` (la carga para escribir) las rechaza.
    pub typos: Vec<String>,
}

const MODES: &[&str] = &["auto", "snapshot", "vzdump", "none"];

fn oneof(v: &mut Validation, key: &str, val: &str, ok: &[&str]) {
    if !ok.contains(&val) {
        v.errors.push(format!("{key} = \"{val}\": expected one of {}", ok.join(" | ")));
    }
}

/// Valida el TOML (claves desconocidas, valores, permisos, transporte de Apprise). Nunca imprime secretos.
pub fn validate(host: &dyn Host) -> Validation {
    let mut v = Validation::default();
    let p = Path::new(CONFIG_PATH);
    if !host.exists(p) {
        v.notes.push(format!("{CONFIG_PATH} not found: defaults apply (read-only; nothing is auto-updated)"));
        return v;
    }
    match host.file_meta(p) {
        Ok(m) => {
            if m.uid != 0 {
                v.errors.push(format!("owner is uid {}, must be root", m.uid));
            }
            if m.mode & 0o077 != 0 {
                v.errors.push(format!("mode is {:04o}, must be 0600", m.mode & 0o777));
            }
            if m.uid == 0 && m.mode & 0o077 == 0 {
                v.notes.push(format!("permissions ok: {:04o} root", m.mode & 0o777));
            }
        }
        Err(e) => v.errors.push(format!("cannot stat: {e}")),
    }
    let text = match host.read_file(p) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => {
            v.errors.push(format!("cannot read: {e}"));
            return v;
        }
    };
    validate_text_into(host, &text, &mut v);
    v
}

/// Valida solo el contenido TOML (sin permisos): para comprobar un texto ANTES de escribirlo.
pub fn validate_text(host: &dyn Host, text: &str) -> Validation {
    let mut v = Validation::default();
    validate_text_into(host, text, &mut v);
    v
}

fn validate_text_into(host: &dyn Host, text: &str, v: &mut Validation) {
    let raw: toml::Value = match toml::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            v.errors.push(format!("TOML syntax: {e}"));
            return;
        }
    };
    if let Some(t) = raw.as_table() {
        for (sec, val) in t {
            if sec == "ct" {
                for (i, c) in val.as_array().into_iter().flatten().enumerate() {
                    for k in c.as_table().into_iter().flat_map(|t| t.keys()).filter(|k| !CT_KEYS.contains(&k.as_str())) {
                        v.warnings.push(format!("[[ct]] #{}: unknown key `{k}`", i + 1));
                        v.typos.push(format!("[[ct]] #{}: unknown key `{k}`", i + 1));
                    }
                }
                continue;
            }
            match KEYS.iter().find(|(n, _)| n == sec) {
                None => v.warnings.push(format!("unknown section [{sec}]")),
                Some((_, keys)) => {
                    for k in val.as_table().into_iter().flat_map(|t| t.keys()).filter(|k| !keys.contains(&k.as_str())) {
                        match DEPRECATED.iter().find(|(s, d, _)| s == sec && d == k) {
                            Some((_, _, why)) => v.warnings.push(format!("[{sec}] `{k}` is deprecated: {why}")),
                            None => {
                                v.warnings.push(format!("[{sec}] unknown key `{k}` (ignored)"));
                                v.typos.push(format!("[{sec}] unknown key `{k}`"));
                            }
                        }
                    }
                }
            }
        }
    }
    let cfg: Config = match toml::from_str(text) {
        Ok(c) => c,
        Err(e) => {
            v.errors.push(format!("invalid value: {e}"));
            return;
        }
    };
    if !(0..=19).contains(&cfg.general.nice) {
        v.errors.push(format!("[general] nice = {}: expected 0..19", cfg.general.nice));
    }
    oneof(v, "[general] io_class", &cfg.general.io_class, &["idle", "best-effort"]);
    if cfg.general.log_retention_days > MAX_LOG_RETENTION_DAYS {
        v.errors.push(format!("[general] log_retention_days = {}: expected 0..{MAX_LOG_RETENTION_DAYS} (0 = never delete)", cfg.general.log_retention_days));
    }
    oneof(v, "[update] stopped", &cfg.update.stopped, &["skip", "start_and_restore"]);
    oneof(v, "[update] reboot", &cfg.update.reboot, &["never", "if-required"]);
    oneof(v, "[update] apt_mode", &cfg.update.apt_mode, &["dist-upgrade", "upgrade"]);
    oneof(v, "[update] on_failure", &cfg.update.on_failure, &["continue", "stop"]);
    oneof(v, "[backup] mode", &cfg.backup.mode, MODES);
    if !(0.1..=4.0).contains(&cfg.backup.space_ratio) {
        v.errors.push(format!("[backup] space_ratio = {}: expected 0.1..4.0", cfg.backup.space_ratio));
    }
    oneof(v, "[apprise] transport", &cfg.apprise.transport, &["auto", "api", "cli", "none"]);
    if cfg.update.max_consecutive_failures == 0 {
        v.errors.push("[update] max_consecutive_failures must be >= 1".into());
    }
    if cfg.ct.iter().any(|c| c.vmid == 0 && c.auto_update == Some(true)) {
        v.errors.push("[[ct]] vmid = 0 is the Proxmox host: auto_update is not allowed there (use `[update] host_auto = true` for the scheduled run, or `pxu update --host` by hand)".into());
    }
    if cfg.ct.iter().any(|c| c.vmid == 0 && c.nvidia_auto == Some(true)) {
        v.errors.push("[[ct]] vmid = 0 is the Proxmox host: nvidia_auto is not allowed there (use `[update] host_nvidia_auto = true`)".into());
    }
    for c in &cfg.ct {
        if let Some(b) = &c.backup {
            oneof(v, &format!("[[ct]] {} backup", c.vmid), b, MODES);
        }
    }
    let none: Vec<String> = std::iter::once((String::from("[backup] mode"), cfg.backup.mode.as_str()))
        .chain(cfg.ct.iter().filter_map(|c| Some((format!("[[ct]] {} backup", c.vmid), c.backup.as_deref()?))))
        .filter(|(_, m)| *m == "none")
        .map(|(k, _)| k)
        .collect();
    if !none.is_empty() {
        v.warnings.push(format!("backup mode \"none\" ({}): those CTs are updated WITHOUT a backup and without automatic rollback", none.join(", ")));
    }
    v.notes.push(format!("backups are temporary: keep = {} per CT (default 0), mode = {}", cfg.backup.keep(), cfg.backup.mode));
    let t = crate::share::notify::resolve(host, &cfg.apprise);
    v.notes.push(format!("apprise transport: {}{}", t.label(), if cfg.apprise.api_key.is_empty() { "" } else { " (api_key set)" }));
    if t == crate::share::notify::Transport::None {
        v.warnings.push("no Apprise transport usable: notifications are disabled (set [apprise] api_url, or urls with the apprise CLI installed)".into());
    }
    for c in cfg.ui.columns.iter().flatten().filter(|c| !OPTIONAL_COLUMNS.iter().any(|o| o.eq_ignore_ascii_case(c))) {
        v.warnings.push(format!("[ui] columns: unknown column `{c}` (ignored; valid: {})", OPTIONAL_COLUMNS.join(", ")));
    }
    for e in &cfg.apprise.on {
        if !["updates_found", "update_failed", "run_summary", "check_error"].contains(&e.as_str()) {
            v.warnings.push(format!("[apprise] on: unknown event `{e}`"));
        }
    }
    if cfg.update.host_auto {
        v.notes.push("[update] host_auto = true: the scheduled run updates the Proxmox HOST first (full-upgrade, same safeguards as `pxu update --host`), and touches the CTs only if the host finished well; it never reboots the host".into());
    }
    v.notes.push(format!("{} [[ct]] override(s), auto_update_tags = {:?}", cfg.ct.len(), cfg.update.auto_update_tags));
}

/// Pone `auto_update = value` en el bloque `[[ct]]` de `vmid` (o añade uno al final), conservando comentarios y orden.
pub fn set_auto_update(text: &str, vmid: u32, value: bool) -> Result<String, String> {
    use toml_edit::{value as val, ArrayOfTables, DocumentMut, Item, Table};
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("TOML syntax: {e}"))?;
    let item = doc.entry("ct").or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let cts = item.as_array_of_tables_mut().ok_or("`ct` is not an array of tables ([[ct]])")?;
    let at = cts.iter().position(|t| t.get("vmid").and_then(|v| v.as_integer()) == Some(vmid as i64));
    match at.and_then(|i| cts.get_mut(i)) {
        Some(t) => {
            t["auto_update"] = val(value);
        }
        None => {
            let mut t = Table::new();
            t["vmid"] = val(vmid as i64);
            t["auto_update"] = val(value);
            cts.push(t);
        }
    }
    Ok(doc.to_string())
}

/// Pone `backup = "none"` en el bloque `[[ct]]` de `vmid` (`disabled`; añade el bloque si falta) o quita la clave `backup`
/// para heredar el `[backup] mode` global (`!disabled`; el bloque se conserva aunque quede con solo `vmid`). Si el global es "none", heredar no
/// activaría nada: entonces `!disabled` escribe `backup = "auto"` en el bloque (lo crea si falta). Conserva comentarios y orden.
pub fn set_backup(text: &str, vmid: u32, disabled: bool) -> Result<String, String> {
    use toml_edit::{value as val, ArrayOfTables, DocumentMut, Item, Table};
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("TOML syntax: {e}"))?;
    let global_none = doc.get("backup").and_then(|b| b.get("mode")).and_then(|m| m.as_str()) == Some("none");
    if !disabled && !global_none && doc.get("ct").is_none() {
        return Ok(doc.to_string());
    }
    let item = doc.entry("ct").or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let cts = item.as_array_of_tables_mut().ok_or("`ct` is not an array of tables ([[ct]])")?;
    let at = cts.iter().position(|t| t.get("vmid").and_then(|v| v.as_integer()) == Some(vmid as i64));
    match (at.and_then(|i| cts.get_mut(i)), disabled) {
        (Some(t), true) => t["backup"] = val("none"),
        (Some(t), false) if global_none => t["backup"] = val("auto"),
        (Some(t), false) => {
            t.remove("backup");
        }
        (None, _) if !disabled && !global_none => {}
        (None, _) => {
            let mut t = Table::new();
            t["vmid"] = val(vmid as i64);
            t["backup"] = val(if disabled { "none" } else { "auto" });
            cts.push(t);
        }
    }
    Ok(doc.to_string())
}

/// Activa/desactiva el backup previo al update de un CT en `/etc/pxu/config.toml`. Valida el resultado ANTES de escribir (atómico, como `write_auto_update`).
pub fn write_backup(host: &dyn Host, vmid: u32, disabled: bool) -> Result<(), String> {
    let old = read_config_text(host)?;
    let new = set_backup(&old, vmid, disabled)?;
    let v = validate_text(host, &new);
    if let Some(e) = v.errors.iter().chain(v.typos.iter()).next() {
        return Err(format!("the resulting config would be invalid ({e}): nothing was written"));
    }
    host.write_config(new.as_bytes()).map_err(|e| format!("{CONFIG_PATH}: {e}"))
}

/// Pone `[update] host_auto = value`, conservando el resto del archivo.
pub fn set_host_auto(text: &str, value: bool) -> Result<String, String> {
    use toml_edit::{Item, Table};
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("TOML syntax: {e}"))?;
    let up = doc.entry("update").or_insert_with(|| Item::Table(Table::new())).as_table_mut().ok_or("`update` is not a table ([update])")?;
    up["host_auto"] = toml_edit::value(value);
    Ok(doc.to_string())
}

/// Activa o desactiva la actualización programada del host en `[update] host_auto` (crea el archivo si falta, como `write_auto_update`).
pub fn write_host_auto(host: &dyn Host, value: bool) -> Result<(), String> {
    let old = read_config_text(host)?;
    let new = set_host_auto(&old, value)?;
    let v = validate_text(host, &new);
    if let Some(e) = v.errors.iter().chain(v.typos.iter()).next() {
        return Err(format!("the resulting config would be invalid ({e}): nothing was written"));
    }
    host.write_config(new.as_bytes()).map_err(|e| format!("{CONFIG_PATH}: {e}"))
}

/// Pone `nvidia_auto = value` en el bloque `[[ct]]` de `vmid` (o `[update] host_nvidia_auto` si es el host), conservando comentarios y orden.
pub fn set_nvidia_auto(text: &str, vmid: u32, value: bool) -> Result<String, String> {
    use toml_edit::{value as val, ArrayOfTables, DocumentMut, Item, Table};
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("TOML syntax: {e}"))?;
    if vmid == 0 {
        let up = doc.entry("update").or_insert_with(|| Item::Table(Table::new())).as_table_mut().ok_or("`update` is not a table ([update])")?;
        up["host_nvidia_auto"] = val(value);
        return Ok(doc.to_string());
    }
    let item = doc.entry("ct").or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let cts = item.as_array_of_tables_mut().ok_or("`ct` is not an array of tables ([[ct]])")?;
    let at = cts.iter().position(|t| t.get("vmid").and_then(|v| v.as_integer()) == Some(vmid as i64));
    match at.and_then(|i| cts.get_mut(i)) {
        Some(t) => t["nvidia_auto"] = val(value),
        None => {
            let mut t = Table::new();
            t["vmid"] = val(vmid as i64);
            t["nvidia_auto"] = val(value);
            cts.push(t);
        }
    }
    Ok(doc.to_string())
}

/// Activa o desactiva la actualización programada del driver NVIDIA de un CT o del host (vmid 0). Valida el resultado ANTES de escribir (atómico, como `write_auto_update`).
pub fn write_nvidia_auto(host: &dyn Host, vmid: u32, value: bool) -> Result<(), String> {
    let old = read_config_text(host)?;
    let new = set_nvidia_auto(&old, vmid, value)?;
    let v = validate_text(host, &new);
    if let Some(e) = v.errors.iter().chain(v.typos.iter()).next() {
        return Err(format!("the resulting config would be invalid ({e}): nothing was written"));
    }
    host.write_config(new.as_bytes()).map_err(|e| format!("{CONFIG_PATH}: {e}"))
}

/// Pone `[general] log_retention_days = days`, conservando el resto del archivo.
pub fn set_log_retention(text: &str, days: u32) -> Result<String, String> {
    use toml_edit::{Item, Table};
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("TOML syntax: {e}"))?;
    let g = doc.entry("general").or_insert_with(|| Item::Table(Table::new())).as_table_mut().ok_or("`general` is not a table ([general])")?;
    g["log_retention_days"] = toml_edit::value(days as i64);
    Ok(doc.to_string())
}

/// Guarda `[general] log_retention_days` (valida el resultado ANTES de escribir; atómico, como `write_auto_update`).
pub fn write_log_retention(host: &dyn Host, days: u32) -> Result<(), String> {
    let old = read_config_text(host)?;
    let new = set_log_retention(&old, days)?;
    let v = validate_text(host, &new);
    if let Some(e) = v.errors.iter().chain(v.typos.iter()).next() {
        return Err(format!("the resulting config would be invalid ({e}): nothing was written"));
    }
    host.write_config(new.as_bytes()).map_err(|e| format!("{CONFIG_PATH}: {e}"))
}

/// Pone `[ui] columns = [...]`, conservando el resto del archivo.
pub fn set_columns(text: &str, cols: &[String]) -> Result<String, String> {
    use toml_edit::{Array, DocumentMut, Item, Table};
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("TOML syntax: {e}"))?;
    let ui = doc.entry("ui").or_insert_with(|| Item::Table(Table::new())).as_table_mut().ok_or("`ui` is not a table ([ui])")?;
    ui["columns"] = toml_edit::value(cols.iter().map(|c| c.as_str()).collect::<Array>());
    Ok(doc.to_string())
}

/// Guarda las columnas visibles en `[ui]` de config.toml. Solo si el archivo existe y es válido: crear la config desde aquí
/// sacaría a la TUI del modo solo lectura como efecto lateral.
pub fn write_columns(host: &dyn Host, cols: &[String]) -> Result<(), String> {
    if !host.exists(Path::new(CONFIG_PATH)) {
        return Err(format!("{CONFIG_PATH} does not exist"));
    }
    let old = read_config_text(host)?;
    let new = set_columns(&old, cols)?;
    let v = validate_text(host, &new);
    if let Some(e) = v.errors.iter().chain(v.typos.iter()).next() {
        return Err(format!("the resulting config would be invalid ({e}): nothing was written"));
    }
    host.write_config(new.as_bytes()).map_err(|e| format!("{CONFIG_PATH}: {e}"))
}

/// Texto actual de config.toml ("" si falta), tras comprobar dueño y permisos.
fn read_config_text(host: &dyn Host) -> Result<String, String> {
    let p = Path::new(CONFIG_PATH);
    if !host.exists(p) {
        return Ok(String::new());
    }
    if let Some(e) = perm_problem(host) {
        return Err(format!("{CONFIG_PATH}: {e}"));
    }
    Ok(String::from_utf8_lossy(&host.read_file(p).map_err(|e| format!("{CONFIG_PATH}: {e}"))?).into_owned())
}

// ---------------------------------------------------------------- [apprise] desde la TUI

pub use crate::share::notify::EVENTS as APPRISE_EVENTS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppriseMode {
    /// Config guardada en Apprise API (`/notify/<api_key>`).
    Key,
    /// URLs de destino directas.
    Urls,
}

/// Lo que el diálogo de la TUI quiere dejar en `[apprise]`. `secret` vacío = conservar el guardado (api_key o urls según `mode`).
#[derive(Clone, PartialEq, Eq)]
pub struct AppriseEdit {
    pub api_url: String,
    pub mode: AppriseMode,
    /// api_key, o URLs separadas por coma/espacio. Nunca se imprime (ver `Debug`).
    pub secret: String,
    pub on: Vec<String>,
}

impl std::fmt::Debug for AppriseEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppriseEdit").field("api_url", &mask_api_url(&self.api_url)).field("mode", &self.mode).field("secret", &"•••").field("on", &self.on).finish()
    }
}

/// `tgram://…`: solo el esquema (en las URLs de destino el host suele ser el token).
pub fn mask_target(u: &str) -> String {
    match u.split_once("://") {
        Some((s, _)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) => format!("{s}://…"),
        _ => "•••".into(),
    }
}

/// Dirección de la API: esquema y host (sin usuario:clave ni ruta).
pub fn mask_api_url(u: &str) -> String {
    let u = u.trim();
    match u.split_once("://") {
        Some((s, rest)) => {
            let auth = rest.split(['/', '?', '#']).next().unwrap_or("");
            format!("{s}://{}", auth.rsplit('@').next().unwrap_or(""))
        }
        None if u.is_empty() => String::new(),
        None => "•••".into(),
    }
}

/// Una línea con el estado de `[apprise]`, sin secretos (para el «hoy → nuevo»).
pub fn apprise_line(c: &crate::share::notify::AppriseCfg) -> String {
    let mut p = vec![format!("api_url {}", if c.api_url.is_empty() { "—".to_string() } else { mask_api_url(&c.api_url) })];
    if !c.api_key.is_empty() {
        p.push("api_key •••".into());
    }
    if !c.urls.is_empty() {
        p.push(format!("urls {}", c.urls.iter().map(|u| mask_target(u)).collect::<Vec<_>>().join(" ")));
    }
    if c.api_key.is_empty() && c.urls.is_empty() {
        p.push("no key or urls".into());
    }
    let all = APPRISE_EVENTS.iter().all(|e| c.on.iter().any(|x| x == e));
    p.push(format!("on {}", if all { "all events".to_string() } else if c.on.is_empty() { "none".to_string() } else { c.on.join(",") }));
    p.join(" · ")
}

/// Líneas de la sección «Notifications» de la vista Config (secretos enmascarados).
pub fn apprise_summary(host: &dyn Host, c: &crate::share::notify::AppriseCfg) -> Vec<String> {
    let t = crate::share::notify::resolve(host, c);
    let mut v = vec![if t == crate::share::notify::Transport::None { "status          : not configured".to_string() } else { format!("status          : configured, sends via {}", t.label()) }];
    v.push(format!("api_url         : {}", if c.api_url.is_empty() { "—".to_string() } else { mask_api_url(&c.api_url) }));
    if !c.api_key.is_empty() {
        v.push("api_key         : ••• (set)".into());
    }
    if !c.urls.is_empty() {
        v.push(format!("urls            : {} ({})", c.urls.iter().map(|u| mask_target(u)).collect::<Vec<_>>().join(", "), c.urls.len()));
    }
    if c.api_key.is_empty() && c.urls.is_empty() {
        v.push("api_key / urls   : —".into());
    }
    v.push(format!("events          : {}", if c.on.is_empty() { "none".to_string() } else { c.on.join(", ") }));
    v
}

fn split_urls(s: &str) -> Vec<String> {
    s.split([',', ' ', '\n']).map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

/// Aplica `edit` sobre `[apprise]` conservando el resto del texto, comentarios y orden. Un secreto vacío conserva el guardado;
/// el del otro modo se elimina (`api_key` y `urls` se excluyen).
pub fn set_apprise(text: &str, edit: &AppriseEdit) -> Result<String, String> {
    use toml_edit::{value, Array, DocumentMut, Item, Table};
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("TOML syntax: {e}"))?;
    let t = doc.entry("apprise").or_insert_with(|| Item::Table(Table::new())).as_table_mut().ok_or("`apprise` is not a table ([apprise])")?;
    let cur = |t: &Table, k: &str| t.get(k).and_then(|v| v.as_str()).map(String::from);
    let api = edit.api_url.trim();
    if api.is_empty() {
        t.remove("api_url");
    } else if cur(t, "api_url").as_deref() != Some(api) {
        t["api_url"] = value(api);
    }
    match edit.mode {
        AppriseMode::Key => {
            let key = Some(edit.secret.trim().to_string()).filter(|s| !s.is_empty()).or_else(|| cur(t, "api_key")).filter(|s| !s.is_empty());
            let key = key.ok_or("api_key is empty and there is none stored: type the key")?;
            if cur(t, "api_key").as_deref() != Some(&key) {
                t["api_key"] = value(key);
            }
            t.remove("urls");
        }
        AppriseMode::Urls => {
            let typed = split_urls(&edit.secret);
            let urls = if typed.is_empty() { t.get("urls").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() } else { typed };
            if urls.is_empty() {
                return Err("no urls: type at least one (the stored ones are kept only when there are some)".into());
            }
            if let Some(bad) = urls.iter().find(|u| !u.contains("://")) {
                return Err(format!("`{}` is not an Apprise URL (expected scheme://…)", mask_target(bad)));
            }
            let mut a = Array::new();
            urls.iter().for_each(|u| a.push(u.as_str()));
            t["urls"] = value(a);
            t.remove("api_key");
        }
    }
    let mut on = Array::new();
    APPRISE_EVENTS.iter().filter(|e| edit.on.iter().any(|x| x == *e)).for_each(|e| on.push(*e));
    t["on"] = value(on);
    Ok(doc.to_string())
}

/// Resultado de `plan_apprise`: el texto que se escribiría y las líneas de la confirmación.
pub struct ApprisePlan {
    pub text: String,
    pub lines: Vec<String>,
}

/// Calcula (sin escribir) la config resultante y la valida entera con el código de `config check`:
/// errores y erratas (también las que ya hubiera) o ningún transporte utilizable ⇒ `Err`.
pub fn plan_apprise(host: &dyn Host, edit: &AppriseEdit) -> Result<ApprisePlan, String> {
    let old_text = read_config_text(host)?;
    let old: Config = toml::from_str(&old_text).map_err(|e| format!("{CONFIG_PATH}: {e}"))?;
    let text = set_apprise(&old_text, edit)?;
    let v = validate_text(host, &text);
    if let Some(e) = v.errors.iter().chain(v.typos.iter()).next() {
        return Err(format!("the resulting config would be invalid ({e}): nothing was written"));
    }
    let new: Config = toml::from_str(&text).map_err(|e| format!("the resulting config would be invalid ({e}): nothing was written"))?;
    if crate::share::notify::resolve(host, &new.apprise) == crate::share::notify::Transport::None {
        return Err("no usable Apprise transport with these values (api_url is needed for an api_key; urls without api_url need the apprise CLI installed): nothing was written".into());
    }
    let mut lines = vec![format!("today: {}", apprise_line(&old.apprise)), format!("new  : {}", apprise_line(&new.apprise))];
    if !old.apprise.urls.is_empty() && new.apprise.urls.is_empty() {
        lines.push("the stored urls are removed (the key and the urls exclude each other)".into());
    }
    if !old.apprise.api_key.is_empty() && new.apprise.api_key.is_empty() {
        lines.push("the stored api_key is removed (the key and the urls exclude each other)".into());
    }
    if old_text.is_empty() {
        lines.push(format!("{CONFIG_PATH} does not exist: it will be created (root, 0600)"));
    }
    lines.push(format!("the rest of {CONFIG_PATH} is kept as it is"));
    Ok(ApprisePlan { text, lines })
}

/// Escribe `[apprise]` en `/etc/pxu/config.toml` (crea el archivo si falta): valida la config resultante ANTES y la escritura es atómica (host).
pub fn write_apprise(host: &dyn Host, edit: &AppriseEdit) -> Result<(), String> {
    let plan = plan_apprise(host, edit)?;
    host.write_config(plan.text.as_bytes()).map_err(|e| format!("{CONFIG_PATH}: {e}"))
}

/// Cambia el auto-update de un CT en `/etc/pxu/config.toml` (crea el archivo si falta). Valida el TOML resultante ANTES de escribir.
/// El host decide si la escritura está permitida (`ReadOnlyHost`) y la hace atómica (0600 root).
pub fn write_auto_update(host: &dyn Host, vmid: u32, value: bool) -> Result<(), String> {
    let old = read_config_text(host)?;
    let new = set_auto_update(&old, vmid, value)?;
    let v = validate_text(host, &new);
    if let Some(e) = v.errors.iter().chain(v.typos.iter()).next() {
        return Err(format!("the resulting config would be invalid ({e}): nothing was written"));
    }
    host.write_config(new.as_bytes()).map_err(|e| format!("{CONFIG_PATH}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::host::ReadOnlyHost;
    use crate::share::mock::scenario::World;
    use crate::share::mock::MockHost;

    fn host(cfg: &str) -> ReadOnlyHost<MockHost> {
        ReadOnlyHost::new(World::demo().config_raw(cfg).build())
    }
    #[test]
    fn check_refreshes_by_default_and_can_be_turned_off() {
        let d = CheckCfg::default();
        assert!(d.refresh && d.refresh_min_age_hours == 12);
        let c: Config = toml::from_str("[check]\ntimeout_s = 30\n").unwrap();
        assert!(c.check.refresh && c.check.refresh_min_age_hours == 12, "missing keys take the new defaults");
        let c: Config = toml::from_str("[check]\nrefresh = false\n").unwrap();
        assert!(!c.check.refresh);
    }

    fn text(h: &dyn Host) -> String {
        String::from_utf8(h.read_file(Path::new(CONFIG_PATH)).unwrap()).unwrap()
    }

    #[test]
    fn load_checked_rejects_wrong_case_values_and_misspelled_keys() {
        // `Stop` no es `stop`: sin validar, update seguiría tras los fallos
        let e = Config::load_checked(&host("[update]\non_failure = \"Stop\"\n")).unwrap_err();
        assert!(e.contains("on_failure") && e.contains("Stop"), "{e}");
        // `Upgrade` no es `upgrade`: terminaría en dist-upgrade
        let e = Config::load_checked(&host("[update]\napt_mode = \"Upgrade\"\n")).unwrap_err();
        assert!(e.contains("apt_mode"), "{e}");
        // una errata se ignoraría en silencio
        let e = Config::load_checked(&host("[update]\non_falure = \"stop\"\n")).unwrap_err();
        assert!(e.contains("on_falure"), "{e}");
        let e = Config::load_checked(&host("[[ct]]\nvmid = 1\nauto_updat = true\n")).unwrap_err();
        assert!(e.contains("auto_updat"), "{e}");
        // `Config::load` sigue siendo permisiva (listar/chequear no escriben)
        assert!(Config::load(&host("[update]\non_failure = \"Stop\"\n")).is_ok());
    }

    #[test]
    fn load_checked_lets_warnings_through_and_accepts_a_missing_config() {
        let (c, w) = Config::load_checked(&host("[update]\nstopped = \"skip\"\n[nope]\nx = 1\n")).unwrap();
        assert_eq!(c.update.stopped, "skip");
        assert!(w.iter().any(|w| w.contains("[nope]")), "{w:?}");
        let h = host("");
        h.inner().remove_file(Path::new(CONFIG_PATH)).unwrap();
        let (c, w) = Config::load_checked(&h).unwrap();
        assert!(c.ct.is_empty() && w.is_empty());
    }

    #[test]
    fn set_auto_update_changes_only_that_line_and_keeps_comments_and_order() {
        let src = "# mi config\n[update]\nauto_update_tags = [\"autoupdate\"] # tag\n\n# CT de prueba\n[[ct]]\nvmid = 105 # ojo\nauto_update = true\nbackup = \"none\"\n\n[backup]\nmode = \"auto\"\n";
        let out = set_auto_update(src, 105, false).unwrap();
        assert_eq!(out, src.replace("auto_update = true", "auto_update = false"));
        // bloque sin la clave: se añade dentro del bloque, el resto intacto
        let src2 = "# c\n[[ct]]\nvmid = 7\nkeep = 1\n\n[backup]\nmode = \"auto\"\n";
        let out2 = set_auto_update(src2, 7, true).unwrap();
        assert!(out2.starts_with("# c\n[[ct]]\nvmid = 7\nkeep = 1\nauto_update = true\n"), "{out2}");
        assert!(out2.ends_with("[backup]\nmode = \"auto\"\n"), "{out2}");
    }

    #[test]
    fn set_backup_adds_none_removes_the_override_and_keeps_the_rest() {
        let src = "# mi config\n[[ct]]\nvmid = 105 # web\nauto_update = true\n\n[[ct]]\nvmid = 7\nbackup = \"vzdump\"\n";
        let out = set_backup(src, 105, true).unwrap();
        assert!(out.starts_with("# mi config\n") && out.contains("vmid = 105 # web") && out.contains("auto_update = true") && out.contains("backup = \"none\""), "{out}");
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(c.backup_mode(105), "none");
        assert_eq!(c.backup_mode(7), "vzdump", "the other CT is untouched");
        // volver a activar: la clave desaparece, el resto (y el bloque) se queda
        let back = set_backup(&out, 105, false).unwrap();
        assert_eq!(back, src, "round trip");
        // el bloque con solo vmid se conserva
        let only = set_backup("[[ct]]\nvmid = 9\nbackup = \"none\"\n", 9, false).unwrap();
        assert_eq!(only, "[[ct]]\nvmid = 9\n");
        // override snapshot/vzdump → none
        assert!(set_backup(src, 7, true).unwrap().contains("vmid = 7\nbackup = \"none\""));
    }

    #[test]
    fn set_backup_re_enable_with_global_none_writes_auto() {
        let src = "[backup]\nmode = \"none\"\n\n[[ct]]\nvmid = 4\nauto_update = true\n";
        let out = set_backup(src, 4, false).unwrap();
        assert!(out.contains("auto_update = true\nbackup = \"auto\""), "{out}");
        let c: Config = toml::from_str(&out).unwrap();
        assert_eq!(c.backup_mode(4), "auto");
        assert_eq!(c.backup_mode(5), "none", "the others keep the global none");
        // sin bloque: se crea
        let out = set_backup("[backup]\nmode = \"none\"\n", 6, false).unwrap();
        assert!(out.ends_with("[[ct]]\nvmid = 6\nbackup = \"auto\"\n"), "{out}");
        // volver a desactivar el CT: "none" otra vez
        assert_eq!(toml::from_str::<Config>(&set_backup(&out, 6, true).unwrap()).unwrap().backup_mode(6), "none");
    }

    #[test]
    fn set_backup_creates_a_block_when_missing_and_does_nothing_to_re_enable_an_absent_one() {
        let out = set_backup("[update]\nhost_auto = false\n", 9, true).unwrap();
        assert!(out.ends_with("[[ct]]\nvmid = 9\nbackup = \"none\"\n") && out.starts_with("[update]"), "{out}");
        assert_eq!(set_backup("[update]\n", 9, false).unwrap(), "[update]\n");
        assert_eq!(set_backup("", 9, false).unwrap(), "");
        assert_eq!(set_backup("", 5, true).unwrap(), "[[ct]]\nvmid = 5\nbackup = \"none\"\n");
        assert!(set_backup("[update\n", 5, true).is_err());
        let c: Config = toml::from_str("[backup]\nmode = \"snapshot\"\n[[ct]]\nvmid = 3\nbackup = \"none\"\n").unwrap();
        assert_eq!(c.backup_why(3), "none by [[ct]] override");
        assert_eq!(c.backup_why(4), "snapshot by [backup] mode (global)");
    }

    #[test]
    fn set_auto_update_appends_a_block_at_the_end_or_creates_the_file_from_scratch() {
        let src = "# c\n[update]\nauto_update_tags = [\"a\"]\n\n[[ct]]\nvmid = 1\nauto_update = true\n";
        let out = set_auto_update(src, 9, false).unwrap();
        assert!(out.starts_with(src), "the original text is untouched:\n{out}");
        assert!(out.trim_end().ends_with("[[ct]]\nvmid = 9\nauto_update = false"), "{out}");
        let new = set_auto_update("", 5, true).unwrap();
        assert_eq!(new.trim(), "[[ct]]\nvmid = 5\nauto_update = true");
        assert!(set_auto_update("[update\n", 5, true).is_err());
    }

    #[test]
    fn write_auto_update_is_refused_without_opt_in_and_writes_atomically_with_it() {
        let src = "# keep me\n[update]\nauto_update_tags = [\"auto\"]\n";
        let h = host(src);
        let e = write_auto_update(&h, 116, false).unwrap_err();
        assert!(e.contains("read-only"), "{e}");
        assert_eq!(text(&h), src, "refused: the file is unchanged");
        h.set_config_edit(true);
        write_auto_update(&h, 116, false).unwrap();
        h.set_config_edit(false);
        let now = text(&h);
        assert!(now.starts_with(src) && now.contains("vmid = 116") && now.contains("auto_update = false"), "{now}");
        let (cfg, _) = Config::load_checked(&h).unwrap();
        assert!(!cfg.auto_update(116, &["auto".to_string()]), "the override wins over the tag");
        assert!(cfg.auto_update(117, &["auto".to_string()]));
    }

    #[test]
    fn write_auto_update_creates_config_from_scratch_and_refuses_one_that_would_not_validate() {
        let h = host("");
        h.inner().remove_file(Path::new(CONFIG_PATH)).unwrap();
        h.set_config_edit(true);
        write_auto_update(&h, 105, true).unwrap();
        assert_eq!(text(&h).trim(), "[[ct]]\nvmid = 105\nauto_update = true");
        assert!(validate(&h).errors.is_empty());
        // una config previa inválida: el resultado tampoco valida, no se escribe nada
        let bad = "[update]\non_failure = \"Stop\"\n";
        let h = host(bad);
        h.set_config_edit(true);
        let e = write_auto_update(&h, 105, true).unwrap_err();
        assert!(e.contains("nothing was written") && e.contains("on_failure"), "{e}");
        assert_eq!(text(&h), bad);
    }

    #[test]
    fn auto_update_why_names_override_or_tag() {
        let (c, _) = Config::load_checked(&host("[update]\nauto_update_tags = [\"auto\"]\n[[ct]]\nvmid = 2\nauto_update = false\n")).unwrap();
        let t = |s: &str| vec![s.to_string()];
        assert_eq!(c.auto_update_why(1, &t("auto")), "yes by tag auto");
        assert_eq!(c.auto_update_why(2, &t("auto")), "no by [[ct]] override");
        assert!(c.auto_update_why(3, &[]).starts_with("no"));
    }

    #[test]
    fn nvidia_auto_fields_are_backward_compatible_and_validated() {
        let c: Config = toml::from_str("[update]\nstopped = \"skip\"\n[[ct]]\nvmid = 134\nauto_update = true\n").unwrap();
        assert!(!c.update.host_nvidia_auto && !c.nvidia_auto(134) && !c.nvidia_auto(0));
        let h = host("[update]\nhost_nvidia_auto = true\n[[ct]]\nvmid = 134\nnvidia_auto = true\n");
        let (c, _) = Config::load_checked(&h).unwrap();
        assert!(c.nvidia_auto(0) && c.nvidia_auto(134) && !c.nvidia_auto(135));
        let e = Config::load_checked(&host("[[ct]]\nvmid = 0\nnvidia_auto = true\n")).unwrap_err();
        assert!(e.contains("vmid = 0") && e.contains("nvidia_auto"), "{e}");
        // misspelled key is rejected
        assert!(Config::load_checked(&host("[update]\nhost_nvidia_aut = true\n")).is_err());
    }

    #[test]
    fn writing_nvidia_auto_edits_in_place_and_respects_read_only() {
        let t = set_nvidia_auto("# mi config\n[[ct]]\nvmid = 134\nauto_update = true\n", 134, true).unwrap();
        assert!(t.starts_with("# mi config\n") && t.contains("nvidia_auto = true"), "{t}");
        let t = set_nvidia_auto(&t, 134, false).unwrap();
        assert!(t.contains("nvidia_auto = false"), "{t}");
        assert!(set_nvidia_auto("", 140, true).unwrap().contains("vmid = 140"));
        assert!(set_nvidia_auto("", 0, true).unwrap().contains("host_nvidia_auto = true"));
        // sin la opción de edición de config el escritor falla y no cambia nada
        let h = host("[update]\nstopped = \"skip\"\n");
        let before = text(&h);
        assert!(write_nvidia_auto(&h, 0, true).is_err());
        assert_eq!(text(&h), before);
        // con ella escribe
        {
            let _g = crate::share::host::ConfigEditOptIn::new(&h);
            write_nvidia_auto(&h, 0, true).unwrap();
        }
        assert!(text(&h).contains("host_nvidia_auto = true"));
    }
}
