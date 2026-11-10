//! Página Storage (DESIGN §5.7): espacio por storage y por guest, backups, snapshots y alertas. Solo lecturas.
//! Fuentes: `pvesh get /nodes/<n>/{lxc,qemu}` (`disk`/`maxdisk`: el uso que muestra la GUI de Proxmox), `pvesh get /nodes/<n>/storage[/<s>/content]`,
//! `lvs` (bloques del thin pool: solo informativo, incluye archivos borrados sin trim), `/etc/pve/storage.cfg` y los `.conf` ya leídos.
//! Lo que no se puede leer queda en `None` (la vista pinta `—`): nunca se inventa un dato.
use crate::share::config::Config;
use crate::share::host::{ExecOpts, Host};
use crate::share::model::{Guest, GuestKind, RunState};
use crate::share::proxmox::conf::{parse_props, parse_size, prop};
use crate::share::update::backup::{is_pve_id, SNAP_PREFIX};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Único `lvs` permitido (sin argumentos variables): LVs con su tamaño y el % de datos de los thin.
pub const LVS: &[&str] = &["lvs", "--reportformat", "json", "--units", "b", "--nosuffix", "-o", "vg_name,lv_name,lv_size,data_percent,pool_lv,lv_attr"];
/// Un backup más viejo que esto (días) genera alerta.
pub const OLD_BACKUP_DAYS: u64 = 30;
/// Un disco thin cuyos bloques superan al uso real por más de estos puntos de % se marca como «sin trim» (solo en el detalle).
pub const UNTRIMMED_GAP_PCT: f64 = 15.0;
/// Disco de un guest con este % usado (o más) genera alerta (siempre sobre el `disk`/`maxdisk` de Proxmox, nunca sobre `lvs`).
pub const FULL_DISK_PCT: u64 = 98;
/// Storage con este % usado (o más) genera alerta (además del overcommit del thin pool).
pub const FULL_STORAGE_PCT: u64 = 90;
const GIB: u64 = 1 << 30;
const DAY: u64 = 86_400;

// ------------------------------------------------------------------ lectura cruda

#[derive(Debug, Clone, PartialEq)]
pub struct StorageRow {
    pub name: String,
    pub kind: String,
    pub content: Vec<String>,
    pub active: bool,
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub avail: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Content {
    pub volid: String,
    pub vmid: Option<u32>,
    /// `backup`, `rootdir`, `images`, `vztmpl`, `iso`…
    pub kind: String,
    pub size: u64,
    pub used: Option<u64>,
    pub ctime: Option<u64>,
    pub notes: String,
    /// Backup protegido (`protected`): Proxmox no deja borrarlo.
    pub protected: bool,
}

/// Una fila de `pvesh get /nodes/<n>/lxc|qemu`: lo que muestra la GUI. Un guest apagado informa `disk = 0`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiGuest {
    pub vmid: u32,
    pub running: bool,
    pub disk: u64,
    pub maxdisk: u64,
}

impl ApiGuest {
    /// Usado real del disco; `None` si Proxmox no lo puede medir (apagado, o una VM sin agente: `disk = 0`).
    pub fn used(&self) -> Option<u64> {
        (self.running && self.disk > 0).then_some(self.disk)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lv {
    pub vg: String,
    pub name: String,
    pub size: u64,
    pub data_pct: Option<f64>,
    pub pool: String,
    pub attr: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CfgEntry {
    pub name: String,
    pub kind: String,
    pub vgname: Option<String>,
    pub thinpool: Option<String>,
}

/// Todo lo leído del servidor, sin interpretar. Un `Err` es el motivo por el que esa parte no se pudo leer.
#[derive(Debug, Clone, Default)]
pub struct Raw {
    pub storages: Option<Result<Vec<StorageRow>, String>>,
    pub content: BTreeMap<String, Result<Vec<Content>, String>>,
    pub lvs: Option<Result<Vec<Lv>, String>>,
    pub api: Option<Result<Vec<ApiGuest>, String>>,
    pub cfg: Vec<CfgEntry>,
}

fn num(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)).or_else(|| v.as_str()?.trim().parse::<f64>().ok().filter(|f| *f >= 0.0).map(|f| f as u64))
}

fn flt(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64().or_else(|| v.as_str()?.trim().parse().ok())
}

pub fn parse_storage_list(json: &str) -> Result<Vec<StorageRow>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unreadable answer: {e}"))?;
    let a = v.as_array().ok_or("unexpected answer (not a list)")?;
    Ok(a
        .iter()
        .filter_map(|r| {
            let name = r.get("storage")?.as_str()?.to_string();
            let flag = |k: &str| r.get(k).map(|x| x.as_u64() != Some(0) && x.as_bool() != Some(false)).unwrap_or(true);
            Some(StorageRow {
                kind: r.get("type").and_then(Value::as_str).unwrap_or("").to_string(),
                content: r.get("content").and_then(Value::as_str).unwrap_or("").split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
                active: flag("active") && flag("enabled"),
                total: num(r.get("total")),
                used: num(r.get("used")),
                avail: num(r.get("avail")),
                name,
            })
        })
        .collect())
}

pub fn parse_content(json: &str) -> Result<Vec<Content>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unreadable answer: {e}"))?;
    let a = v.as_array().ok_or("unexpected answer (not a list)")?;
    Ok(a
        .iter()
        .filter_map(|r| {
            Some(Content {
                volid: r.get("volid")?.as_str()?.to_string(),
                vmid: num(r.get("vmid")).and_then(|n| u32::try_from(n).ok()),
                kind: r.get("content").and_then(Value::as_str).unwrap_or("").to_string(),
                size: num(r.get("size")).unwrap_or(0),
                used: num(r.get("used")),
                ctime: num(r.get("ctime")),
                notes: r.get("notes").and_then(Value::as_str).unwrap_or("").to_string(),
                protected: r.get("protected").is_some_and(|x| x.as_u64() == Some(1) || x.as_bool() == Some(true) || x.as_str() == Some("1")),
            })
        })
        .collect())
}

pub fn parse_api_guests(json: &str) -> Result<Vec<ApiGuest>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unreadable answer: {e}"))?;
    let a = v.as_array().ok_or("unexpected answer (not a list)")?;
    Ok(a
        .iter()
        .filter_map(|r| Some(ApiGuest { vmid: u32::try_from(num(r.get("vmid"))?).ok()?, running: r.get("status").and_then(Value::as_str) == Some("running"), disk: num(r.get("disk")).unwrap_or(0), maxdisk: num(r.get("maxdisk")).unwrap_or(0) }))
        .collect())
}

pub fn parse_lvs(json: &str) -> Result<Vec<Lv>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unreadable answer: {e}"))?;
    let rows = v.pointer("/report/0/lv").and_then(Value::as_array).ok_or("unexpected answer (no report)")?;
    let s = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(rows.iter().map(|r| Lv { vg: s(r, "vg_name"), name: s(r, "lv_name"), size: num(r.get("lv_size")).unwrap_or(0), data_pct: flt(r.get("data_percent")), pool: s(r, "pool_lv"), attr: s(r, "lv_attr") }).collect())
}

/// `/etc/pve/storage.cfg`: bloques `tipo: nombre` con líneas `\tclave valor`. Solo importan `vgname` y `thinpool`.
pub fn parse_storage_cfg(text: &str) -> Vec<CfgEntry> {
    let mut out: Vec<CfgEntry> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            if let Some((kind, name)) = line.split_once(':') {
                out.push(CfgEntry { name: name.trim().to_string(), kind: kind.trim().to_string(), ..Default::default() });
            }
        } else if let Some(e) = out.last_mut() {
            let mut it = line.split_whitespace();
            match (it.next(), it.next()) {
                (Some("vgname"), Some(v)) => e.vgname = Some(v.to_string()),
                (Some("thinpool"), Some(v)) => e.thinpool = Some(v.to_string()),
                _ => {}
            }
        }
    }
    out
}

fn pvesh(host: &dyn Host, path: &str) -> Result<String, String> {
    let mut out: Vec<String> = Vec::new();
    let o = ExecOpts { timeout_s: Some(30), ..ExecOpts::default() };
    match host.host_cmd(&["pvesh", "get", path, "--output-format", "json"], &o, &mut out) {
        Ok(h) if h.code == 0 => Ok(out.join("\n")),
        Ok(h) => Err(format!("pvesh exit {}", h.code)),
        Err(e) => Err(e.to_string()),
    }
}

fn joined<T>(h: std::thread::ScopedJoinHandle<'_, Result<T, String>>) -> Result<T, String> {
    h.join().unwrap_or_else(|_| Err("read panicked".into()))
}

/// Fase rápida: lista de storages, `disk`/`maxdisk` de los guests, `lvs` y `storage.cfg` (en paralelo). Cada parte que falla queda como `Err`: la página sigue con el resto.
pub fn read_base(host: &dyn Host, node: &str) -> Raw {
    let mut raw = Raw::default();
    if !is_pve_id(node) {
        raw.storages = Some(Err("unusable node name".into()));
        return raw;
    }
    let lvs = || {
        let mut out: Vec<String> = Vec::new();
        match host.host_cmd(LVS, &ExecOpts { timeout_s: Some(30), ..ExecOpts::default() }, &mut out) {
            Ok(h) if h.code == 0 => parse_lvs(&out.join("\n")),
            Ok(h) => Err(format!("lvs exit {}", h.code)),
            Err(e) => Err(e.to_string()),
        }
    };
    let guests = |kind: &str| pvesh(host, &format!("/nodes/{node}/{kind}")).and_then(|t| parse_api_guests(&t));
    let (st, lv, lxc, qemu) = std::thread::scope(|sc| {
        let a = sc.spawn(|| pvesh(host, &format!("/nodes/{node}/storage")).and_then(|t| parse_storage_list(&t)));
        let b = sc.spawn(lvs);
        let c = sc.spawn(|| guests("lxc"));
        let d = sc.spawn(|| guests("qemu"));
        (joined(a), joined(b), joined(c), joined(d))
    });
    raw.storages = Some(st);
    raw.lvs = Some(lv);
    // los LXC mandan; si solo falla `qemu` (p. ej. sin VMs) no se pierde nada de los CT
    raw.api = Some(match (lxc, qemu) {
        (Ok(mut a), Ok(b)) => {
            a.extend(b);
            Ok(a)
        }
        (Ok(a), Err(_)) => Ok(a),
        (Err(e), _) => Err(e),
    });
    raw.cfg = crate::share::host::read_string(host, "/etc/pve/storage.cfg").map(|t| parse_storage_cfg(&t)).unwrap_or_default();
    raw
}

/// Fase lenta: el `content` de cada storage activo, uno por hilo (un PBS lento no retrasa a los demás).
pub fn read_content(host: &dyn Host, node: &str, raw: &mut Raw) {
    let Some(Ok(list)) = &raw.storages else { return };
    let names: Vec<&str> = list.iter().filter(|s| s.active && is_pve_id(&s.name) && s.content.iter().any(|c| matches!(c.as_str(), "backup" | "images" | "rootdir" | "vztmpl" | "iso"))).map(|s| s.name.as_str()).collect();
    let res: Vec<(String, Result<Vec<Content>, String>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = names.iter().map(|n| sc.spawn(move || (n.to_string(), pvesh(host, &format!("/nodes/{node}/storage/{n}/content")).and_then(|t| parse_content(&t))))).collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    raw.content.extend(res);
}

/// Lee todo (fase rápida + lenta).
pub fn read(host: &dyn Host, node: &str) -> Raw {
    let mut raw = read_base(host, node);
    read_content(host, node, &mut raw);
    raw
}

// ------------------------------------------------------------------ resultado

#[derive(Debug, Clone, PartialEq)]
pub struct StorageInfo {
    pub name: String,
    pub kind: String,
    pub active: bool,
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub avail: Option<u64>,
    /// Solo lvmthin: suma de los tamaños de los discos thin del pool.
    pub allocated: Option<u64>,
    /// vztmpl + iso (bytes).
    pub templates: u64,
    /// Backups del storage: cuántos y su tamaño lógico.
    pub backups: usize,
    pub logical: u64,
    /// Solo pbs: `logical / used`.
    pub dedup: Option<f64>,
    /// Destino de los backups previos de pxu (`[backup] storage`).
    pub pxu_target: bool,
    pub has_backups: bool,
    pub error: Option<String>,
}

impl StorageInfo {
    pub fn pct(&self) -> Option<u64> {
        pct(self.used?, self.total?)
    }
    pub fn alloc_pct(&self) -> Option<u64> {
        pct(self.allocated?, self.total?)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiskUse {
    /// `rootfs`, `mp0`, `scsi0`…
    pub slot: String,
    pub volid: String,
    pub storage: String,
    pub mount: Option<String>,
    pub alloc: Option<u64>,
    /// Uso real: solo el rootfs (de `disk` en Proxmox); en los demás, el del contenido del storage si lo da. Nunca los bloques del thin.
    pub used: Option<u64>,
    /// Solo discos thin: bloques que ocupa en el pool (`lvs` Data%) y su tamaño. Cuentan archivos borrados sin trim.
    pub thin_pct: Option<f64>,
    pub thin_used: Option<u64>,
}

impl DiskUse {
    pub fn pct(&self) -> Option<u64> {
        pct(self.used?, self.alloc?)
    }
    /// Los bloques del thin superan al uso real por más de `UNTRIMMED_GAP_PCT` puntos (solo con el uso real conocido).
    pub fn untrimmed(&self) -> bool {
        match (self.thin_pct, self.used.zip(self.alloc)) {
            (Some(t), Some((u, a))) if a > 0 => t - u as f64 * 100.0 / a as f64 > UNTRIMMED_GAP_PCT,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupItem {
    pub volid: String,
    pub storage: String,
    pub ctime: Option<u64>,
    pub size: u64,
    /// Sobra de un update de pxu (`pxu pre-update …`): no cuenta como backup programado.
    pub pxu: bool,
    /// Protegido en Proxmox: pxu no ofrece borrarlo.
    pub protected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    NoBackup,
    /// Días desde el último backup.
    OldBackup(u64),
    /// % usado del disco (`disk`/`maxdisk` de Proxmox).
    DiskFull(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct GuestUse {
    pub vmid: u32,
    pub name: String,
    pub kind: GuestKind,
    pub state: RunState,
    pub disks: Vec<DiskUse>,
    pub alloc: Option<u64>,
    pub used: Option<u64>,
    pub snaps: Vec<String>,
    pub pxu_snaps: usize,
    /// Más reciente primero.
    pub backups: Vec<BackupItem>,
    /// Modo de backup previo al update y su origen (`auto by [backup] mode (global)`).
    pub pre_update: String,
    pub pre_update_why: String,
    pub alerts: Vec<Alert>,
}

impl GuestUse {
    pub fn pct(&self) -> Option<u64> {
        pct(self.used?, self.alloc?)
    }
    /// Último backup programado (sin los restos de pxu).
    pub fn last_backup(&self) -> Option<&BackupItem> {
        self.backups.iter().find(|b| !b.pxu)
    }
    /// Discos thin con bloques sin liberar (aviso solo del detalle; no es una alerta).
    pub fn untrimmed(&self) -> Vec<&DiskUse> {
        self.disks.iter().filter(|k| k.untrimmed()).collect()
    }
    pub fn has_alert(&self) -> bool {
        !self.alerts.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StorageData {
    pub node: String,
    pub now: u64,
    pub storages: Vec<StorageInfo>,
    pub guests: Vec<GuestUse>,
    /// Alertas de storage (overcommit, casi lleno, destino de pxu sin espacio).
    pub storage_alerts: Vec<String>,
    /// Lo que no se pudo leer.
    pub errors: Vec<String>,
    /// Aún faltan los `content` (backups y uso de discos no thin): la vista pinta `…`.
    pub partial: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    Used,
    Alloc,
    Pct,
    LastBackup,
    Id,
}

impl SortBy {
    pub fn label(self) -> &'static str {
        match self {
            SortBy::Used => "used ↓",
            SortBy::Alloc => "alloc ↓",
            SortBy::Pct => "% ↓",
            SortBy::LastBackup => "last backup (oldest first)",
            SortBy::Id => "id ↑",
        }
    }
    pub fn next(self) -> SortBy {
        match self {
            SortBy::Used => SortBy::Alloc,
            SortBy::Alloc => SortBy::Pct,
            SortBy::Pct => SortBy::LastBackup,
            SortBy::LastBackup => SortBy::Id,
            SortBy::Id => SortBy::Used,
        }
    }
}

impl StorageData {
    /// Guests en el orden pedido; con `only_alerts`, solo los que tienen alguna alerta. Los que no tienen el dato van al final.
    pub fn view(&self, sort: SortBy, only_alerts: bool) -> Vec<&GuestUse> {
        let mut v: Vec<&GuestUse> = self.guests.iter().filter(|g| !only_alerts || g.has_alert()).collect();
        let desc = |a: Option<u64>, b: Option<u64>| match (a, b) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        v.sort_by(|a, b| {
            let o = match sort {
                SortBy::Used => desc(a.used, b.used),
                SortBy::Alloc => desc(a.alloc, b.alloc),
                SortBy::Pct => desc(a.pct(), b.pct()),
                // sin backup (None) primero, luego del más viejo al más nuevo
                SortBy::LastBackup => a.last_backup().and_then(|x| x.ctime).cmp(&b.last_backup().and_then(|x| x.ctime)),
                SortBy::Id => std::cmp::Ordering::Equal,
            };
            o.then(a.vmid.cmp(&b.vmid))
        });
        v
    }

    pub fn count(&self, f: impl Fn(&Alert) -> bool) -> usize {
        self.guests.iter().filter(|g| g.alerts.iter().any(&f)).count()
    }
    pub fn ids(&self, f: impl Fn(&Alert) -> bool) -> Vec<u32> {
        self.guests.iter().filter(|g| g.alerts.iter().any(&f)).map(|g| g.vmid).collect()
    }
    pub fn any_alert(&self) -> bool {
        !self.storage_alerts.is_empty() || self.guests.iter().any(GuestUse::has_alert)
    }
}

fn pct(used: u64, total: u64) -> Option<u64> {
    (total > 0).then(|| (used as f64 * 100.0 / total as f64).round() as u64)
}

type ConfDisk = (String, String, String, Option<String>, Option<u64>);
type ConfDiskKeyed = (u32, String, String, String, Option<String>, Option<u64>);

/// Volumen de un disco del `.conf`: `(slot, volid, storage, mount, alloc)`. Los bind mounts (`/ruta`) y los CD-ROM no ocupan un storage.
fn conf_disks(g: &Guest) -> Vec<ConfDisk> {
    let keys: &[&str] = match g.kind {
        GuestKind::Lxc => &["rootfs", "mp"],
        GuestKind::Vm => &["scsi", "virtio", "sata", "ide", "efidisk", "tpmstate"],
        GuestKind::Host => &[],
    };
    let mut v: Vec<ConfDiskKeyed> = Vec::new();
    for (k, val) in &g.conf.main {
        let base = k.trim_end_matches(|c: char| c.is_ascii_digit());
        if !keys.contains(&base) {
            continue;
        }
        let (vol, props) = parse_props(val);
        let Some(vol) = vol else { continue };
        if vol.starts_with('/') || prop(&props, "media") == Some("cdrom") {
            continue;
        }
        let Some((st, _)) = vol.split_once(':') else { continue };
        if !is_pve_id(st) {
            continue;
        }
        let idx: u32 = k[base.len()..].parse().unwrap_or(0);
        v.push((if k == "rootfs" { 0 } else { idx + 1 }, k.clone(), vol.to_string(), st.to_string(), prop(&props, "mp").map(String::from), prop(&props, "size").and_then(parse_size)));
    }
    v.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    v.into_iter().map(|(_, k, vol, st, mp, sz)| (k, vol, st, mp, sz)).collect()
}

/// ¿Va este mp en el vzdump? (no los bind mounts ni `backup=0`; los demás sí). Misma regla que el chequeo previo al vzdump.
fn mp_in_backup(g: &Guest, slot: &str) -> bool {
    g.conf.get(slot).map(parse_props).is_some_and(|(vol, p)| vol.is_some_and(|v| !v.starts_with('/')) && prop(&p, "backup") != Some("0"))
}

/// Lo que pesará el vzdump de este CT (usado del rootfs + mp incluidos; el asignado si no se conoce el usado) × `space_ratio`.
fn vzdump_need(g: &GuestUse, guest: &Guest, ratio: f64) -> u64 {
    let sum: u64 = g.disks.iter().filter(|d| d.slot == "rootfs" || mp_in_backup(guest, &d.slot)).map(|d| if d.slot == "rootfs" { d.used.or(d.alloc).unwrap_or(0) } else { d.alloc.or(d.used).unwrap_or(0) }).sum();
    (sum as f64 * ratio) as u64
}

/// Tamaño compacto para tablas: `127M`, `3.0G`, `250G`, `1.9T`.
pub fn fmt_gb(b: u64) -> String {
    let g = b as f64 / GIB as f64;
    if g >= 1024.0 {
        format!("{:.1}T", g / 1024.0)
    } else if g >= 100.0 {
        format!("{g:.0}G")
    } else if g >= 1.0 {
        format!("{g:.1}G")
    } else if b >= 1 << 20 {
        format!("{}M", b >> 20)
    } else {
        format!("{}K", b >> 10)
    }
}

/// ¿Es `volid` un backup de ESTE guest? `<storage>:backup/vzdump-<lxc|qemu>-<vmid>-…` o, en un PBS, `<storage>:backup/<ct|vm>/<vmid>/<fecha>`.
/// Solo caracteres de nombre de archivo / fecha PBS (sin espacios, `;`, `..`): el argv va sin shell, pero se valida igual.
pub fn backup_volid_ok(volid: &str, vmid: u32) -> bool {
    let Some((st, rest)) = volid.split_once(':') else { return false };
    let Some(path) = rest.strip_prefix("backup/") else { return false };
    let safe = path.len() <= 160 && !path.contains("..") && path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b));
    let mine = match path.split('/').collect::<Vec<_>>().as_slice() {
        [file] => ["vzdump-lxc-", "vzdump-qemu-"].iter().any(|p| file.strip_prefix(p).and_then(|r| r.split_once('-')).is_some_and(|(id, tail)| id == vmid.to_string() && !tail.is_empty())),
        ["ct" | "vm", id, stamp] => *id == vmid.to_string() && !stamp.is_empty(),
        _ => false,
    };
    is_pve_id(st) && safe && mine
}

/// El único comando que borra un backup: `pvesm free <volid>` (argumentos separados, sin shell).
pub fn free_argv(volid: &str) -> [&str; 3] {
    ["pvesm", "free", volid]
}

/// Borra el backup `b` del guest `vmid` (`pvesm free`), tras validar que es de ese guest y no está protegido; lo quita del registro de pxu si estaba.
/// `Ok` = mensaje de la barra de estado; `Err` = el motivo (el error de `pvesm` tal cual).
pub fn delete_backup(host: &dyn Host, vmid: u32, b: &BackupItem) -> Result<String, String> {
    use crate::share::host::BackupDeleteOptIn;
    if b.protected {
        return Err(format!("{} is protected: remove the protection in Proxmox first", b.volid));
    }
    if !backup_volid_ok(&b.volid, vmid) {
        return Err(format!("refused: {} is not a backup of {vmid}", b.volid));
    }
    let mut out: Vec<String> = Vec::new();
    let o = ExecOpts { timeout_s: Some(120), ..ExecOpts::default() };
    let res = {
        let _optin = BackupDeleteOptIn::new(host, &b.volid);
        host.host_cmd(&free_argv(&b.volid), &o, &mut out)
    };
    match res {
        Ok(h) if h.code == 0 => {}
        Ok(h) => return Err(format!("pvesm free failed (exit {}){}", h.code, out.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).fold(String::new(), |a, l| a + ": " + l))),
        Err(e) => return Err(e.to_string()),
    }
    // si era un backup temporal de pxu, el registro ya no debe apuntar a él (si no se puede guardar, el borrado igualmente ocurrió)
    let mut reg = crate::share::update::backup::Registry::load(host);
    if !reg.tainted && reg.entries.iter().any(|e| e.vmid == vmid && e.name == b.volid) {
        reg.entries.retain(|e| !(e.vmid == vmid && e.name == b.volid));
        let _ = reg.save(host);
    }
    Ok(format!("deleted {}", b.volid))
}

/// Une lo leído con el inventario y la config: cálculos, alertas y orden de los storages.
pub fn build(raw: &Raw, guests: &[Guest], node: &str, cfg: &Config, now: u64) -> StorageData {
    let mut d = StorageData { node: node.to_string(), now, ..Default::default() };
    let rows: Vec<StorageRow> = match &raw.storages {
        Some(Ok(r)) => r.clone(),
        Some(Err(e)) => {
            d.errors.push(format!("storages: {e}"));
            vec![]
        }
        None => vec![],
    };
    let lvs: &[Lv] = match &raw.lvs {
        Some(Ok(l)) => l,
        Some(Err(e)) => {
            d.errors.push(format!("lvs: {e} (thin disk usage unknown)"));
            &[]
        }
        None => &[],
    };
    let kind_of = |st: &str| rows.iter().find(|r| r.name == st).map(|r| r.kind.clone()).or_else(|| raw.cfg.iter().find(|c| c.name == st).map(|c| c.kind.clone())).unwrap_or_default();
    let cfg_of = |st: &str| raw.cfg.iter().find(|c| c.name == st);
    let target = cfg.backup.storage.as_str();

    // backups por vmid de todos los storages con contenido `backup`; si uno de esos no se pudo leer, «sin backup» no se puede afirmar
    let mut backups: BTreeMap<u32, Vec<BackupItem>> = BTreeMap::new();
    let mut backup_unknown = false;
    for r in rows.iter().filter(|r| r.active && r.content.iter().any(|c| c == "backup")) {
        match raw.content.get(&r.name) {
            Some(Ok(items)) => {
                for c in items.iter().filter(|c| c.kind == "backup") {
                    if let Some(id) = c.vmid {
                        backups.entry(id).or_default().push(BackupItem { volid: c.volid.clone(), storage: r.name.clone(), ctime: c.ctime, size: c.size, pxu: c.notes.starts_with("pxu pre-update"), protected: c.protected });
                    }
                }
            }
            Some(Err(e)) => {
                backup_unknown = true;
                d.errors.push(format!("{}: {e}", r.name));
            }
            None => backup_unknown = true,
        }
    }
    for v in backups.values_mut() {
        v.sort_by(|a, b| b.ctime.cmp(&a.ctime).then(a.volid.cmp(&b.volid)));
    }

    // lo asignado y el uso real de cada volumen, por storage
    let content_of = |st: &str, volid: &str| raw.content.get(st).and_then(|r| r.as_ref().ok()).and_then(|v| v.iter().find(|c| c.volid == volid));
    let thin_lv = |st: &str, volid: &str| -> Option<&Lv> {
        let c = cfg_of(st).filter(|c| c.kind == "lvmthin")?;
        let name = volid.split_once(':')?.1;
        lvs.iter().find(|l| Some(&l.vg) == c.vgname.as_ref() && l.name == name && !l.pool.is_empty())
    };

    let api_of = |id: u32| raw.api.as_ref().and_then(|r| r.as_ref().ok()).and_then(|v| v.iter().find(|a| a.vmid == id));
    if let Some(Err(e)) = &raw.api {
        d.errors.push(format!("guests: {e} (disk usage unknown)"));
    }
    let local: Vec<&Guest> = guests.iter().filter(|g| g.kind != GuestKind::Host && g.node == node).collect();
    let mut max_need = (0u64, 0u32);
    for g in &local {
        let disks: Vec<DiskUse> = conf_disks(g)
            .into_iter()
            .map(|(slot, volid, storage, mount, conf_size)| {
                let lv = thin_lv(&storage, &volid);
                let c = content_of(&storage, &volid);
                // rootfs: el asignado es el `maxdisk` de Proxmox (tamaño del sistema de archivos), el mismo que el encabezado y la lista; el size del .conf es el del volumen
                let api_max = if slot == "rootfs" { api_of(g.vmid).map(|a| a.maxdisk).filter(|m| *m > 0) } else { None };
                let alloc = api_max.or(conf_size).or(lv.map(|l| l.size)).or(c.map(|c| c.size));
                // uso real = lo que muestra Proxmox (rootfs); los bloques del thin son otro dato y no lo sustituyen
                let used = if slot == "rootfs" { api_of(g.vmid).and_then(ApiGuest::used) } else if lv.is_some() { None } else { c.and_then(|c| c.used) };
                let thin_pct = lv.and_then(|l| l.data_pct);
                DiskUse { slot, volid, storage, mount, alloc, used, thin_pct, thin_used: lv.zip(thin_pct).map(|(l, p)| (l.size as f64 * p / 100.0) as u64) }
            })
            .collect();
        // como la GUI: USED/% del guest = `disk`/`maxdisk` (el rootfs); sin ese dato, el asignado sale de los discos del .conf y el uso queda en `—`
        let api = api_of(g.vmid);
        let alloc = api.map(|a| a.maxdisk).filter(|m| *m > 0).or_else(|| disks.iter().map(|x| x.alloc).try_fold(0u64, |a, x| x.map(|x| a + x)).filter(|_| !disks.is_empty()));
        let used = api.and_then(ApiGuest::used);
        let snaps = g.conf.snapshots.clone();
        let pxu_snaps = snaps.iter().filter(|s| s.starts_with(SNAP_PREFIX)).count();
        let mut u = GuestUse {
            vmid: g.vmid,
            name: g.name.clone(),
            kind: g.kind,
            state: g.state,
            disks,
            alloc,
            used,
            snaps,
            pxu_snaps,
            backups: backups.remove(&g.vmid).unwrap_or_default(),
            pre_update: g.backup.clone(),
            pre_update_why: if g.kind == GuestKind::Lxc { cfg.backup_why(g.vmid) } else { String::new() },
            alerts: vec![],
        };
        if !g.template {
            match u.last_backup() {
                None if !backup_unknown => u.alerts.push(Alert::NoBackup),
                Some(b) => {
                    if let Some(days) = b.ctime.map(|t| now.saturating_sub(t) / DAY).filter(|d| *d > OLD_BACKUP_DAYS) {
                        u.alerts.push(Alert::OldBackup(days));
                    }
                }
                None => {}
            }
            if let Some(p) = u.pct().filter(|p| *p >= FULL_DISK_PCT) {
                u.alerts.push(Alert::DiskFull(p));
            }
            if g.kind == GuestKind::Lxc && u.pre_update != "none" && u.pre_update != "snapshot" {
                let need = vzdump_need(&u, g, cfg.backup.space_ratio);
                if need > max_need.0 {
                    max_need = (need, g.vmid);
                }
            }
        }
        d.guests.push(u);
    }

    // storages
    for r in &rows {
        let content = raw.content.get(&r.name);
        let items: &[Content] = content.and_then(|c| c.as_ref().ok()).map_or(&[], |v| v.as_slice());
        let bk: Vec<&Content> = items.iter().filter(|c| c.kind == "backup").collect();
        let logical: u64 = bk.iter().map(|c| c.size).sum();
        let allocated = cfg_of(&r.name).filter(|c| c.kind == "lvmthin" && r.kind == "lvmthin" && !lvs.is_empty()).map(|c| {
            let rows = lvs.iter().filter(|l| Some(&l.vg) == c.vgname.as_ref() && Some(&l.pool) == c.thinpool.as_ref());
            rows.map(|l| l.size).sum::<u64>()
        });
        let has_backups = r.content.iter().any(|c| c == "backup");
        d.storages.push(StorageInfo {
            name: r.name.clone(),
            kind: if r.kind.is_empty() { kind_of(&r.name) } else { r.kind.clone() },
            active: r.active,
            total: r.total.filter(|t| *t > 0),
            used: r.used,
            avail: r.avail,
            allocated,
            templates: items.iter().filter(|c| matches!(c.kind.as_str(), "vztmpl" | "iso")).map(|c| c.size).sum(),
            backups: bk.len(),
            logical,
            dedup: (r.kind == "pbs" && r.used.is_some_and(|u| u > 0) && logical > 0).then(|| logical as f64 / r.used.unwrap_or(1) as f64),
            pxu_target: r.name == target,
            has_backups,
            error: content.and_then(|c| c.as_ref().err().cloned()),
        });
    }

    // alertas de storage
    for s in d.storages.iter().filter(|s| s.active) {
        if s.kind == "lvmthin" && s.allocated.zip(s.total).is_some_and(|(a, t)| a > t) {
            d.storage_alerts.push(format!("{} thin pool overcommitted: {} allocated of {}", s.name, fmt_gb(s.allocated.unwrap_or(0)), fmt_gb(s.total.unwrap_or(0))));
        }
        if let Some(p) = s.pct().filter(|p| *p >= FULL_STORAGE_PCT) {
            d.storage_alerts.push(format!("{} is {p}% full", s.name));
        }
    }
    if let Some(s) = d.storages.iter().find(|s| s.pxu_target && s.active) {
        if let (Some(avail), total) = (s.avail, s.total.unwrap_or(0)) {
            let spare = (cfg.backup.min_free_gb * GIB).max(total / 10);
            if max_need.0 > 0 && avail < max_need.0.saturating_add(spare) {
                d.storage_alerts.push(format!("pxu backup target {}: {} free, CT {} needs ~{} + {} spare", s.name, fmt_gb(avail), max_need.1, fmt_gb(max_need.0), fmt_gb(spare)));
            }
        }
    } else if !rows.is_empty() && !d.storages.iter().any(|s| s.pxu_target) {
        d.storage_alerts.push(format!("pxu backup target {target} not found on this node"));
    }
    d
}

/// Lee y calcula en un paso (tests, mock).
pub fn load(host: &dyn Host, guests: &[Guest], node: &str, cfg: &Config) -> StorageData {
    load_progressive(host, guests, node, cfg, |_| {})
}

/// Como `load`, pero entrega antes (`first`, con `partial = true`) lo que no depende de los `content`.
pub fn load_progressive(host: &dyn Host, guests: &[Guest], node: &str, cfg: &Config, first: impl FnOnce(StorageData)) -> StorageData {
    let now = host.now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut raw = read_base(host, node);
    let mut d = build(&raw, guests, node, cfg, now);
    d.partial = true;
    first(d);
    read_content(host, node, &mut raw);
    build(&raw, guests, node, cfg, now)
}

/// `2026-11-07` (hora local) y la edad entre paréntesis si pasa de un mes: `2026-03-28 (7 mo)`.
pub fn when(ts: Option<u64>, now: u64) -> String {
    let Some(t) = ts else { return "—".into() };
    let d = crate::share::update::runlog::local_ymd_hm(t)[..10].to_string();
    let days = now.saturating_sub(t) / DAY;
    if days > OLD_BACKUP_DAYS {
        format!("{d} ({} mo)", days / 30)
    } else {
        d
    }
}

pub fn exists(host: &dyn Host, p: &str) -> bool {
    host.exists(Path::new(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::host::ReadOnlyHost;
    use crate::share::mock::scenario::World;
    use crate::share::proxmox::guests;

    fn demo() -> StorageData {
        let h = ReadOnlyHost::new(World::demo().build());
        let inv = guests::load(&h);
        let cfg = Config::load(&h).unwrap();
        load(&h, &inv.guests, "server", &cfg)
    }

    fn g(d: &StorageData, id: u32) -> &GuestUse {
        d.guests.iter().find(|g| g.vmid == id).unwrap()
    }

    #[test]
    fn parses_pvesh_lists_and_tolerates_junk() {
        let l = parse_storage_list(r#"[{"storage":"a","type":"dir","content":"iso,backup","active":1,"total":100,"used":40,"avail":60},{"storage":"b","type":"nfs","active":0},{"nope":1}]"#).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!((l[0].total, l[0].used, l[0].content.len(), l[0].active), (Some(100), Some(40), 2, true));
        assert!(!l[1].active && l[1].total.is_none());
        assert!(parse_storage_list("not json").is_err());
        assert!(parse_storage_list("{}").is_err());
        assert_eq!(parse_storage_list("[]").unwrap().len(), 0);
        let c = parse_content(r#"[{"volid":"s:backup/x","vmid":"101","content":"backup","size":"5","ctime":7},{"x":1}]"#).unwrap();
        assert_eq!((c.len(), c[0].vmid, c[0].size, c[0].ctime), (1, Some(101), 5, Some(7)));
        let v = parse_lvs(r#"{"report":[{"lv":[{"vg_name":"pve","lv_name":"data","lv_size":"100","data_percent":"12.50","pool_lv":"","lv_attr":"twi"},{"vg_name":"pve","lv_name":"root","lv_size":"5","data_percent":"","pool_lv":"","lv_attr":"-wi"}]}]}"#).unwrap();
        assert_eq!((v[0].data_pct, v[1].data_pct), (Some(12.5), None));
        assert!(parse_lvs("{}").is_err());
        let cfg = parse_storage_cfg("lvmthin: local-lvm\n\tthinpool data\n\tvgname pve\n\ndir: local\n\tpath /x\n");
        assert_eq!((cfg[0].vgname.as_deref(), cfg[0].thinpool.as_deref(), cfg[1].kind.as_str()), (Some("pve"), Some("data"), "dir"));
    }

    #[test]
    fn demo_has_the_four_storages_with_their_numbers() {
        let d = demo();
        let s = |n: &str| d.storages.iter().find(|s| s.name == n).unwrap();
        assert_eq!(d.storages.len(), 4);
        assert!(d.errors.is_empty(), "{:?}", d.errors);
        // lvmthin: asignado = suma de los thin LV del pool (CT 130..136)
        assert_eq!(s("local-lvm").allocated, Some(25 << 30));
        assert!(s("local-lvm").alloc_pct().is_some());
        assert_eq!(s("local-zfs").allocated, None);
        // pbs: dedup = lógico / usado
        let p = s("remote-backup-3");
        assert!(p.backups > 20 && p.logical > p.used.unwrap());
        assert!((p.dedup.unwrap() - 4.2).abs() < 0.1, "{:?}", p.dedup);
        assert!(s("local").templates > 0 && s("local").pxu_target && !s("remote-backup-3").pxu_target);
    }

    #[test]
    fn demo_alerts_and_per_guest_numbers() {
        let d = demo();
        assert_eq!(g(&d, 100).alerts, vec![Alert::NoBackup]);
        assert_eq!(g(&d, 101).alerts, vec![Alert::OldBackup(199)]);
        assert_eq!(g(&d, 133).alerts, vec![Alert::OldBackup(44), Alert::DiskFull(99)]);
        // caso arr: bloques del thin al 99.8% pero disco real al 66% → ninguna alerta de disco lleno
        assert!(g(&d, 131).alerts.is_empty(), "{:?}", g(&d, 131).alerts);
        assert!(g(&d, 102).alerts.is_empty());
        assert_eq!(g(&d, 106).pxu_snaps, 1);
        // 103: dos snapshots ajenos + dos discos (rootfs y mp0); el resto de pxu de local no cuenta como último backup
        assert_eq!(g(&d, 103).disks.len(), 2);
        assert_eq!(g(&d, 103).pxu_snaps, 0);
        assert_eq!(g(&d, 103).backups.len(), 2);
        assert_eq!(g(&d, 103).last_backup().unwrap().storage, "remote-backup-3");
        assert!(g(&d, 103).backups.iter().any(|b| b.pxu && b.storage == "local"));
        // el uso sale de `disk`/`maxdisk` de Proxmox; los bloques del thin son un dato aparte
        let r = &g(&d, 131).disks[0];
        // maxdisk (2.9G) ≠ size del .conf (3.0G): la fila usa el de Proxmox, igual que el guest
        assert_eq!(r.alloc, Some((3u64 << 30) / 100 * 97));
        assert_eq!(r.alloc, g(&d, 131).alloc);
        assert_eq!(r.used, g(&d, 131).used);
        assert_eq!((g(&d, 131).pct(), r.pct()), (Some(66), Some(66)));
        assert_eq!(r.thin_pct, Some(99.8));
        assert!(r.untrimmed() && g(&d, 131).untrimmed().len() == 1);
        // thin == real en el resto: sin aviso de trim
        assert!(d.guests.iter().filter(|x| x.vmid != 131).all(|x| x.untrimmed().is_empty()));
        assert!(g(&d, 100).used.is_some() && g(&d, 100).pct().is_some());
        assert_eq!(d.count(|a| matches!(a, Alert::NoBackup)), 1);
        assert_eq!(d.ids(|a| matches!(a, Alert::DiskFull(_))), vec![133]);
        // apagado: Proxmox da disk = 0 → `—`, sin alerta ni 0%
        let st = g(&d, 117);
        assert_eq!(st.state, RunState::Stopped);
        assert_eq!((st.used, st.pct()), (None, None));
        assert!(st.alloc.is_some() && !st.alerts.iter().any(|a| matches!(a, Alert::DiskFull(_))));
    }

    #[test]
    fn progressive_load_sends_a_partial_first_then_the_full_data() {
        let h = ReadOnlyHost::new(World::demo().build());
        let guests = guests::load(&h).guests;
        let cfg = Config::load(&h).unwrap();
        let mut first = None;
        let full = load_progressive(&h, &guests, "server", &cfg, |d| first = Some(d));
        let first = first.expect("partial delivered first");
        assert!(first.partial && !full.partial);
        assert!(!first.storages.is_empty() && first.guests.iter().all(|g| g.alerts.iter().all(|a| *a != Alert::NoBackup)), "no «no backup» claims without the content");
        assert!(full.guests.iter().any(|g| g.alerts.contains(&Alert::NoBackup)));
    }

    #[test]
    fn sort_and_filter() {
        let d = demo();
        let ids = |s, f| d.view(s, f).iter().map(|g| g.vmid).collect::<Vec<_>>();
        assert_eq!(ids(SortBy::Id, true), vec![100, 101, 133]);
        assert_eq!(ids(SortBy::Id, false).len(), 37);
        assert_eq!(ids(SortBy::Id, false)[..3], [100, 101, 102]);
        let by_used = d.view(SortBy::Used, false);
        assert!(by_used.windows(2).all(|w| w[0].used >= w[1].used));
        let by_pct = d.view(SortBy::Pct, false);
        assert!(by_pct.windows(2).all(|w| w[0].pct() >= w[1].pct()));
        assert_eq!(by_pct[0].vmid, 133, "the real % decides, not the thin blocks of 131");
        // sin backup primero, luego el más viejo
        let lb = ids(SortBy::LastBackup, false);
        assert_eq!(lb[..2], [100, 101]);
        assert_eq!(SortBy::Id.next(), SortBy::Used);
    }

    #[test]
    fn full_disk_alert_ignores_thin_blocks_and_stopped_guests() {
        let h = ReadOnlyHost::new(World::demo().build());
        let inv = guests::load(&h);
        let cfg = Config::load(&h).unwrap();
        let mut raw = read(&h, "server");
        // lvs dice 100% en todos los thin, pero Proxmox ve el disco al 50%
        if let Some(Ok(l)) = raw.lvs.as_mut() {
            l.iter_mut().filter(|l| !l.pool.is_empty()).for_each(|l| l.data_pct = Some(100.0));
        }
        if let Some(Ok(a)) = raw.api.as_mut() {
            a.iter_mut().filter(|a| a.running).for_each(|a| a.disk = a.maxdisk / 2);
            a.iter_mut().filter(|a| !a.running).for_each(|a| a.disk = 0);
        }
        let d = build(&raw, &inv.guests, "server", &cfg, 1_794_107_760);
        assert_eq!(d.count(|a| matches!(a, Alert::DiskFull(_))), 0);
        assert!(g(&d, 130).untrimmed().len() == 1, "thin 100% vs real 50% → trim notice");
        // ahora el disco real sí está lleno
        if let Some(Ok(a)) = raw.api.as_mut() {
            let x = a.iter_mut().find(|a| a.vmid == 130).unwrap();
            x.disk = x.maxdisk * 99 / 100;
        }
        let d = build(&raw, &inv.guests, "server", &cfg, 1_794_107_760);
        assert_eq!(d.ids(|a| matches!(a, Alert::DiskFull(_))), vec![130]);
        assert_eq!(g(&d, 130).pct(), Some(99));
    }

    #[test]
    fn parses_api_guests() {
        let a = parse_api_guests(r#"[{"vmid":130,"status":"running","disk":13875617792,"maxdisk":21026242560},{"vmid":"101","status":"stopped","disk":0,"maxdisk":1000},{"x":1}]"#).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!((a[0].used(), a[0].maxdisk), (Some(13_875_617_792), 21_026_242_560));
        assert_eq!((a[1].vmid, a[1].used()), (101, None));
        assert!(parse_api_guests("{}").is_err());
        // VM en marcha sin agente: disk = 0 → no se inventa el uso
        assert_eq!(ApiGuest { vmid: 1, running: true, disk: 0, maxdisk: 5 }.used(), None);
    }

    #[test]
    fn unreadable_data_shows_none_not_zero() {
        let h = ReadOnlyHost::new(World::demo().build());
        let inv = guests::load(&h);
        let cfg = Config::load(&h).unwrap();
        let raw = Raw { storages: Some(Err("boom".into())), lvs: Some(Err("no lvs".into())), ..Raw::default() };
        let d = build(&raw, &inv.guests, "server", &cfg, 1_794_107_760);
        assert_eq!(d.errors.len(), 2);
        assert!(d.storages.is_empty());
        // sin leer los storages no se puede afirmar «sin backup»... y tampoco hay uso
        let u = g(&d, 131);
        assert_eq!((u.used, u.pct()), (None, None));
        assert!(!u.has_alert() || u.alerts == vec![Alert::NoBackup]);
    }

    #[test]
    fn empty_world_and_missing_reads_do_not_panic() {
        let h = ReadOnlyHost::new(World::new().build());
        let cfg = Config::load(&h).unwrap();
        let d = load(&h, &[], "server", &cfg);
        assert!(d.guests.is_empty() && d.storages.is_empty());
        assert!(!d.errors.is_empty(), "unreadable reads are reported");
        assert!(d.view(SortBy::Used, true).is_empty());
    }

    #[test]
    fn target_without_space_is_a_storage_alert() {
        let h = ReadOnlyHost::new(World::demo().build());
        let inv = guests::load(&h);
        let mut cfg = Config::load(&h).unwrap();
        let mut raw = read(&h, "server");
        // `local` casi lleno
        if let Some(Ok(l)) = raw.storages.as_mut() {
            let s = l.iter_mut().find(|s| s.name == "local").unwrap();
            s.used = Some(99 << 30);
            s.avail = Some(1 << 30);
        }
        cfg.backup.mode = "vzdump".into();
        let d = build(&raw, &inv.guests, "server", &cfg, 1_794_107_760);
        assert!(d.storage_alerts.iter().any(|a| a.contains("local") && a.contains("is 99% full")), "{:?}", d.storage_alerts);
        assert!(d.storage_alerts.iter().any(|a| a.starts_with("pxu backup target local")), "{:?}", d.storage_alerts);
    }

    #[test]
    fn when_formats_old_dates() {
        let now = 1_794_107_760;
        assert_eq!(when(None, now), "—");
        assert!(when(Some(now - 3 * DAY), now).len() == 10);
        assert!(when(Some(now - 200 * DAY), now).ends_with("(6 mo)"));
    }
}
