//! Backups temporales de `pxu update` (ARCHITECTURE §3.3): nombres, registro en `/var/lib/pxu/backups.json` y poda.
//! Solo se borra lo que pxu creó: snapshots `pxu-<YYYYMMDDHHMMSS>[-N]` o volúmenes vzdump anotados en el registro.
use crate::share::host::Host;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const REGISTRY_PATH: &str = "/var/lib/pxu/backups.json";
pub const SNAP_PREFIX: &str = "pxu-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Snapshot,
    Vzdump,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Creado y con el CT en curso: si pxu murió a mitad, queda así y la siguiente corrida lo pasa a `Held`.
    Active,
    /// Red de salida pendiente de decisión humana (rollback fallido, timeout o corrida interrumpida): nunca se borra solo.
    Held,
    /// Conservado por `keep`: candidato a poda.
    Kept,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub vmid: u32,
    pub kind: Kind,
    /// Snapshot: `pxu-<stamp>`. Vzdump: volid `<storage>:backup/vzdump-lxc-<vmid>-….tar.zst`.
    pub name: String,
    pub stamp: String,
    pub status: Status,
    /// Vzdump: storage del rootfs del CT al hacer el backup; el rollback lo pasa a `pct restore --storage` (sin él PVE restaura en `local`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
}

impl Entry {
    /// Texto para logs y avisos.
    pub fn describe(&self) -> String {
        match self.kind {
            Kind::Snapshot => format!("snapshot {}", self.name),
            Kind::Vzdump => format!("vzdump {}", self.name),
        }
    }
    /// Comando manual para volver atrás (solo se muestra, el rollback automático usa el motor).
    pub fn manual_revert(&self) -> String {
        match self.kind {
            Kind::Snapshot => format!("pct status {id} | grep -q running && pct stop {id}; pct rollback {id} {}", self.name, id = self.vmid),
            Kind::Vzdump => match &self.storage {
                Some(st) => format!("pct status {id} | grep -q running && pct stop {id}; pct restore {id} {} --force --storage {st}", self.name, id = self.vmid),
                None => format!("pct status {id} | grep -q running && pct stop {id}; pct restore {id} {} --force --storage <rootfs storage>", self.name, id = self.vmid),
            },
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// Avisos de esta carga (registro corrupto, entradas descartadas). No se guarda.
    #[serde(skip)]
    pub warnings: Vec<String>,
    /// `true` si el fichero estaba corrupto o traía entradas que no cuadran: en esta corrida no se poda nada.
    #[serde(skip)]
    pub tainted: bool,
}

/// Una entrada solo es de pxu si su nombre cuadra con su vmid: snapshot `pxu-<sello>[-N]` o volid `vzdump-lxc-<vmid>-…` de ese mismo CT.
pub fn entry_check(e: &Entry) -> Result<(), String> {
    if e.vmid == 0 {
        return Err("vmid 0".into());
    }
    match e.kind {
        Kind::Snapshot if crate::share::host::pxu_snapshot(&e.name) && snapshot_stamp(&e.name).is_some() => {}
        Kind::Vzdump if crate::share::host::vzdump_volid(&e.name, Some(&e.vmid.to_string())) => {}
        _ => return Err(format!("name {:?} does not belong to a pxu backup of CT {}", e.name, e.vmid)),
    }
    if e.storage.as_deref().is_some_and(|s| !is_pve_id(s)) {
        return Err("invalid storage".into());
    }
    if e.stamp.len() > 14 || !e.stamp.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid stamp".into());
    }
    Ok(())
}

impl Registry {
    /// Ausente ⇒ vacío. Corrupto ⇒ se conserva una copia `backups.json.corrupt-<sello>` (el contenido original no se pierde
    /// al guardar), se avisa y `tainted` impide podar. Cada entrada se valida; la que no cuadre se descarta con aviso.
    pub fn load(host: &dyn Host) -> Registry {
        let path = Path::new(REGISTRY_PATH);
        let b = match host.read_file(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Registry::default(),
            Err(e) => return Registry { warnings: vec![format!("cannot read {REGISTRY_PATH}: {e}; nothing will be pruned in this run")], tainted: true, ..Default::default() },
        };
        let quarantine = |why: String, mut r: Registry| {
            let stamp = host.now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let copy = format!("{REGISTRY_PATH}.corrupt-{stamp}");
            let saved = host.write_file(Path::new(&copy), &b).map(|_| format!("original kept as {copy}")).unwrap_or_else(|e| format!("could not keep a copy ({e})"));
            r.warnings.push(format!("WARNING: {why}; {saved}; nothing will be pruned in this run"));
            r.tainted = true;
            r
        };
        let mut r: Registry = match serde_json::from_slice(&b) {
            Ok(r) => r,
            Err(e) => return quarantine(format!("{REGISTRY_PATH} is corrupt ({e})"), Registry::default()),
        };
        let mut bad = vec![];
        r.entries.retain(|e| match entry_check(e) {
            Ok(()) => true,
            Err(why) => {
                bad.push(format!("{} of CT {}: {why}", e.name, e.vmid));
                false
            }
        });
        if bad.is_empty() {
            r
        } else {
            quarantine(format!("{} registry entr{} dropped as invalid ({})", bad.len(), if bad.len() == 1 { "y" } else { "ies" }, bad.join("; ")), r)
        }
    }
    pub fn save(&self, host: &dyn Host) -> std::io::Result<()> {
        let mut r = self.clone();
        r.schema = 1;
        r.warnings.clear();
        let json = serde_json::to_vec_pretty(&r).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        host.write_file(Path::new(REGISTRY_PATH), &json)
    }
    pub fn remove(&mut self, e: &Entry) {
        self.entries.retain(|x| !(x.vmid == e.vmid && x.kind == e.kind && x.name == e.name));
    }
    pub fn set_status(&mut self, e: &Entry, s: Status) {
        for x in self.entries.iter_mut().filter(|x| x.vmid == e.vmid && x.kind == e.kind && x.name == e.name) {
            x.status = s;
        }
    }
    /// Entradas `Active` de este CT (restos de una corrida interrumpida): pasan a `Held`. Devuelve cuántas.
    pub fn hold_orphans(&mut self, vmid: u32) -> usize {
        let mut n = 0;
        for x in self.entries.iter_mut().filter(|x| x.vmid == vmid && x.status == Status::Active) {
            x.status = Status::Held;
            n += 1;
        }
        n
    }
    /// Qué borrar para dejar como mucho `keep` backups de pxu de este CT: los más nuevos se conservan.
    /// Candidatos = entradas `Kept` + snapshots `pxu-<14 dígitos>[-N]` que ya están en el `.conf` y no constan en el registro.
    /// Nunca `Held`/`Active`, nunca snapshots con otro nombre.
    pub fn prune_plan(&self, vmid: u32, conf_snapshots: &[String], keep: usize) -> Vec<Entry> {
        if self.tainted {
            return vec![];
        }
        let mut c: Vec<Entry> = self.entries.iter().filter(|e| e.vmid == vmid && e.status == Status::Kept).cloned().collect();
        for s in conf_snapshots {
            let known = self.entries.iter().any(|e| e.vmid == vmid && e.kind == Kind::Snapshot && &e.name == s);
            if let (false, Some(stamp)) = (known, snapshot_stamp(s)) {
                c.push(Entry { vmid, kind: Kind::Snapshot, name: s.clone(), stamp, status: Status::Kept, storage: None });
            }
        }
        c.sort_by(|a, b| b.stamp.cmp(&a.stamp).then_with(|| b.name.cmp(&a.name)));
        c.into_iter().skip(keep).collect()
    }
}

pub fn snapshot_name(stamp: &str) -> String {
    format!("{SNAP_PREFIX}{stamp}")
}

/// `pxu-<YYYYMMDDHHMMSS>` (o 12 dígitos, formato antiguo) con sufijo opcional `-N`: devuelve el sello. Otro nombre ⇒ no es de pxu.
pub fn snapshot_stamp(name: &str) -> Option<String> {
    let rest = name.strip_prefix(SNAP_PREFIX)?;
    let (digits, suffix) = match rest.split_once('-') {
        Some((d, n)) => (d, Some(n)),
        None => (rest, None),
    };
    let ok = (digits.len() == 12 || digits.len() == 14) && digits.bytes().all(|b| b.is_ascii_digit()) && suffix.is_none_or(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()));
    ok.then(|| digits.to_string())
}

/// `pxu-<stamp>`; si ya existe (en el `.conf` o en el registro) → `pxu-<stamp>-2`, `-3`… (como mucho 99).
pub fn unique_snapshot_name(stamp: &str, taken: &[String]) -> Option<String> {
    let base = snapshot_name(stamp);
    std::iter::once(base.clone()).chain((2..100).map(|n| format!("{base}-{n}"))).find(|c| !taken.contains(c))
}

/// Identificador de storage/nodo de PVE: empieza por letra, solo `[A-Za-z0-9._-]`, termina alfanumérico.
pub fn is_pve_id(s: &str) -> bool {
    s.len() <= 64 && s.starts_with(|c: char| c.is_ascii_alphabetic()) && s.ends_with(|c: char| c.is_ascii_alphanumeric()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Storage del `rootfs:` de la sección principal del `.conf` (`rootfs: local-lvm:vm-121-disk-0,size=8G` → `local-lvm`). Las secciones `[snap]` no cuentan.
pub fn rootfs_storage(conf: &crate::share::proxmox::conf::Conf) -> Option<String> {
    let v = &conf.main.iter().find(|(k, _)| k == "rootfs")?.1;
    let st = v.split(',').next()?.split_once(':')?.0.trim();
    is_pve_id(st).then(|| st.to_string())
}

/// Volid `<storage>:backup/<archivo>` a partir de la ruta que imprime vzdump. Solo `vzdump-lxc-<vmid>-…` de este CT.
pub fn volid(storage: &str, archive: &str, vmid: u32) -> Option<String> {
    let file = archive.rsplit('/').next()?;
    let ok = file.starts_with(&format!("vzdump-lxc-{vmid}-")) && file.len() <= 128 && file.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    ok.then(|| format!("{storage}:backup/{file}"))
}

/// `stamp` de un archivo vzdump (`2026_11_08-03_16_00` → `20261108031600`) para ordenar junto a los snapshots.
pub fn vzdump_stamp(volid: &str) -> String {
    let file = volid.rsplit('/').next().unwrap_or("");
    let digits: String = file.strip_prefix("vzdump-lxc-").unwrap_or("").split('-').skip(1).collect::<Vec<_>>().join("").chars().filter(char::is_ascii_digit).collect();
    digits.chars().take(14).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: Kind, name: &str, stamp: &str, status: Status) -> Entry {
        Entry { vmid: 7, kind, name: name.into(), stamp: stamp.into(), status, storage: None }
    }

    #[test]
    fn volid_only_accepts_this_cts_vzdump_archives() {
        assert_eq!(volid("local", "/var/lib/vz/dump/vzdump-lxc-402-2026_11_08-03_16_00.tar.zst", 402).as_deref(), Some("local:backup/vzdump-lxc-402-2026_11_08-03_16_00.tar.zst"));
        assert!(volid("local", "/var/lib/vz/dump/vzdump-lxc-403-x.tar.zst", 402).is_none());
        assert!(volid("local", "/etc/passwd", 402).is_none());
        assert!(volid("local", "/x/vzdump-lxc-402-a b.tar", 402).is_none());
        assert_eq!(vzdump_stamp("local:backup/vzdump-lxc-402-2026_11_08-03_16_00.tar.zst"), "20261108031600");
    }

    #[test]
    fn prune_plan_keeps_newest_n_and_only_touches_pxu_backups() {
        let mut r = Registry::default();
        r.entries.push(e(Kind::Vzdump, "local:backup/vzdump-lxc-7-a", "202611050300", Status::Kept));
        r.entries.push(e(Kind::Snapshot, "pxu-202611060300", "202611060300", Status::Kept));
        r.entries.push(e(Kind::Snapshot, "pxu-202611010300", "202611010300", Status::Held)); // rescate pendiente
        r.entries.push(e(Kind::Snapshot, "pxu-202611090300", "202611090300", Status::Active)); // en curso
        r.entries.push(Entry { vmid: 8, ..e(Kind::Snapshot, "pxu-202611040300", "202611040300", Status::Kept) }); // otro CT
        let conf: Vec<String> = ["pxu-202611070300", "pxu-custom", "manual-before-x", "pxu-2026110703", "zfs_auto_1"].map(String::from).to_vec();
        let plan: Vec<String> = r.prune_plan(7, &conf, 2).into_iter().map(|e| e.name).collect();
        // candidatos: 20261107 (conf, sin registro), 20261106, 20261105 → con keep=2 solo cae el más viejo
        assert_eq!(plan, ["local:backup/vzdump-lxc-7-a"]);
        let all: Vec<String> = r.prune_plan(7, &conf, 0).into_iter().map(|e| e.name).collect();
        assert_eq!(all, ["pxu-202611070300", "pxu-202611060300", "local:backup/vzdump-lxc-7-a"]);
        for never in ["pxu-202611010300", "pxu-202611090300", "pxu-custom", "manual-before-x", "pxu-2026110703", "zfs_auto_1", "pxu-202611040300"] {
            assert!(!all.iter().any(|n| n == never), "{never} must never be pruned");
        }
    }

    #[test]
    fn interrupted_runs_become_held() {
        let mut r = Registry::default();
        r.entries.push(e(Kind::Snapshot, "pxu-1", "202611090300", Status::Active));
        assert_eq!(r.hold_orphans(7), 1);
        assert_eq!(r.entries[0].status, Status::Held);
        assert_eq!(r.hold_orphans(7), 0);
    }

    #[test]
    fn snapshot_names_carry_seconds_and_never_collide() {
        assert_eq!(unique_snapshot_name("20261108031605", &[]).as_deref(), Some("pxu-20261108031605"));
        let taken: Vec<String> = ["pxu-20261108031605", "pxu-20261108031605-2"].map(String::from).to_vec();
        assert_eq!(unique_snapshot_name("20261108031605", &taken).as_deref(), Some("pxu-20261108031605-3"));
        let full: Vec<String> = std::iter::once("pxu-20261108031605".to_string()).chain((2..100).map(|n| format!("pxu-20261108031605-{n}"))).collect();
        assert_eq!(unique_snapshot_name("20261108031605", &full), None, "fails cleanly after 99");
        assert_eq!(snapshot_stamp("pxu-20261108031605-2").as_deref(), Some("20261108031605"));
        for no in ["pxu-custom", "pxu-2026110803", "pxu-20261108031605-x", "pxu-20261108031605-", "manual-20261108031605"] {
            assert!(snapshot_stamp(no).is_none(), "{no}");
        }
    }

    #[test]
    fn rootfs_storage_comes_from_the_main_section_only() {
        use crate::share::proxmox::conf::Conf;
        let c = Conf::parse("arch: amd64\nrootfs: local-lvm:vm-121-disk-0,size=8G\nmp0: /mnt/pve/x,mp=/data\n\n[snap1]\nrootfs: evil:vm-1-disk-0,size=1G\n");
        assert_eq!(rootfs_storage(&c).as_deref(), Some("local-lvm"));
        assert_eq!(rootfs_storage(&Conf::parse("arch: amd64\n")), None);
        assert_eq!(rootfs_storage(&Conf::parse("rootfs: /dev/x,size=1G\n")), None);
        assert_eq!(rootfs_storage(&Conf::parse("rootfs: a;b:vm-1,size=1G\n")), None);
        assert!(is_pve_id("local-lvm") && is_pve_id("drive-14a") && !is_pve_id("-x") && !is_pve_id("x y") && !is_pve_id("1abc") && !is_pve_id(""));
    }
}
