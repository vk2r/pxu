//! Constructor de servidores simulados: `World` arma en memoria el host (`/etc/pve`, cgroups, estado de pxu, salida de los
//! sondeos) que lee `MockHost`. Nada versionado en disco: cada test declara su caso con CT genéricos (`ct-101`, `ct-102`…).
//! Del lado Proxmox todos los CT se comportan igual; solo cambian la distro (gestor de paquetes), el estado
//! encendido/apagado, el storage (con o sin snapshot) y las fallas que se inyectan.
use super::{FailSpec, LockSpec, MockHost, Scenario};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 2026-11-08T03:16:00Z: reloj fijo de todos los casos.
pub const NOW: u64 = 1_794_107_760;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Debian,
    Ubuntu,
    Alpine,
    Arch,
    Fedora,
    Tumbleweed,
    /// Sin gestor de paquetes conocido (`pm = none`).
    NixOs,
}

impl Distro {
    /// `(os id, versión, gestor de paquetes)` como los informa el sondeo.
    pub fn info(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Distro::Debian => ("debian", "12", "apt"),
            Distro::Ubuntu => ("ubuntu", "24.04", "apt"),
            Distro::Alpine => ("alpine", "3.20", "apk"),
            Distro::Arch => ("archlinux", "rolling", "pacman"),
            Distro::Fedora => ("fedora", "41", "dnf"),
            Distro::Tumbleweed => ("opensuse", "tumbleweed", "zypper"),
            Distro::NixOs => ("nixos", "24.11", "none"),
        }
    }
}

/// Storage del rootfs. Los tres admiten snapshot en PVE; un storage que no lo admite se modela con `Ct::no_snapshots`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    Zfs,
    LvmThin,
    Dir,
}

impl Storage {
    fn rootfs(self, id: u32, size_gb: u32) -> String {
        match self {
            Storage::Zfs => format!("local-zfs:subvol-{id}-disk-0,size={size_gb}G"),
            Storage::LvmThin => format!("local-lvm:vm-{id}-disk-0,size={size_gb}G"),
            Storage::Dir => format!("local:{id}/vm-{id}-disk-0.raw,size={size_gb}G"),
        }
    }
}

/// Paso del flujo de update en el que se inyecta una falla.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailAt {
    Probe,
    /// Falla `pct snapshot` y `vzdump`.
    Backup,
    /// Solo `pct snapshot` (p. ej. bind mounts que no admiten snapshots).
    Snapshot,
    /// Solo `vzdump`.
    Vzdump,
    Upgrade,
    Rollback,
}

impl FailAt {
    fn name(self) -> &'static str {
        match self {
            FailAt::Probe => "probe",
            FailAt::Backup => "backup",
            FailAt::Snapshot => "snapshot",
            FailAt::Vzdump => "vzdump",
            FailAt::Upgrade => "upgrade",
            FailAt::Rollback => "rollback",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Pkg {
    name: String,
    cur: String,
    new: String,
    sec: bool,
}

impl Pkg {
    pub fn new(name: &str, cur: &str, new: &str) -> Pkg {
        Pkg { name: name.into(), cur: cur.into(), new: new.into(), sec: false }
    }
    pub fn security(mut self) -> Pkg {
        self.sec = true;
        self
    }
}

/// Tres paquetes pendientes (uno de seguridad): el set de actualizaciones por defecto de los casos de update.
pub fn std_updates() -> Vec<Pkg> {
    vec![Pkg::new("curl", "8.5", "8.6").security(), Pkg::new("libssl3", "3.0.1", "3.0.2"), Pkg::new("bash", "5.2", "5.2.1")]
}

#[derive(Debug, Clone)]
pub struct Ct {
    id: u32,
    node: Option<String>,
    distro: Distro,
    running: bool,
    storage: Storage,
    mem: u32,
    cores: u32,
    size_gb: u32,
    tags: String,
    desc: String,
    ip: String,
    extra: Vec<String>,
    snaps: Vec<String>,
    lock: Option<String>,
    template: bool,
    pkgs: Vec<Pkg>,
    held: Vec<String>,
    reboot: bool,
    index_age_h: u64,
    checked_min_ago: u64,
    checked: bool,
    mem_used_mb: Option<u32>,
    probe_broken: bool,
    no_snapshots: bool,
}

impl Ct {
    pub fn new(id: u32) -> Ct {
        Ct {
            id,
            node: None,
            distro: Distro::Debian,
            running: true,
            storage: Storage::LvmThin,
            mem: 512,
            cores: 1,
            size_gb: 4,
            tags: String::new(),
            desc: String::new(),
            ip: "ip=dhcp".into(),
            extra: vec![],
            snaps: vec![],
            lock: None,
            template: false,
            pkgs: vec![],
            held: vec![],
            reboot: false,
            index_age_h: 3,
            checked_min_ago: 1,
            checked: true,
            mem_used_mb: None,
            probe_broken: false,
            no_snapshots: false,
        }
    }
    pub fn distro(mut self, d: Distro) -> Ct {
        self.distro = d;
        self
    }
    pub fn stopped(mut self) -> Ct {
        self.running = false;
        self
    }
    /// CT de otro nodo del cluster: aparece como `Remote` y no tiene cgroup local.
    pub fn on(mut self, node: &str) -> Ct {
        self.node = Some(node.into());
        self
    }
    pub fn storage(mut self, s: Storage) -> Ct {
        self.storage = s;
        self
    }
    pub fn mem(mut self, mb: u32) -> Ct {
        self.mem = mb;
        self
    }
    pub fn cores(mut self, n: u32) -> Ct {
        self.cores = n;
        self
    }
    pub fn size(mut self, gb: u32) -> Ct {
        self.size_gb = gb;
        self
    }
    pub fn tags(mut self, t: &str) -> Ct {
        self.tags = t.into();
        self
    }
    pub fn desc(mut self, d: &str) -> Ct {
        self.desc = d.into();
        self
    }
    pub fn ip(mut self, ip: &str) -> Ct {
        self.ip = format!("ip={ip}");
        self
    }
    /// Línea extra del `.conf` (p. ej. un `mp0:`).
    pub fn conf(mut self, line: &str) -> Ct {
        self.extra.push(line.into());
        self
    }
    /// Un bind mount de `/mnt/pve/...`: PVE no puede hacer snapshot de él.
    pub fn bind_mount(self, n: u32) -> Ct {
        let l = format!("mp{n}: /mnt/pve/store/ct-{}-{n},mp=/data", self.id);
        self.conf(&l).no_snapshots()
    }
    pub fn snapshots(mut self, names: &[&str]) -> Ct {
        self.snaps = names.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn pve_lock(mut self, l: &str) -> Ct {
        self.lock = Some(l.into());
        self
    }
    pub fn template(mut self) -> Ct {
        self.template = true;
        self
    }
    /// Paquetes pendientes que verá el sondeo (y el resultado de `state.json`).
    pub fn updates(mut self, p: Vec<Pkg>) -> Ct {
        self.pkgs = p;
        self
    }
    pub fn std_updates(self) -> Ct {
        self.updates(std_updates())
    }
    pub fn held(mut self, name: &str) -> Ct {
        self.held.push(name.into());
        self
    }
    pub fn reboot_required(mut self) -> Ct {
        self.reboot = true;
        self
    }
    pub fn index_age_hours(mut self, h: u64) -> Ct {
        self.index_age_h = h;
        self
    }
    pub fn checked_minutes_ago(mut self, m: u64) -> Ct {
        self.checked_min_ago = m;
        self
    }
    /// Sin resultado previo en `state.json` ni salida de sondeo.
    pub fn never_checked(mut self) -> Ct {
        self.checked = false;
        self
    }
    pub fn mem_used(mut self, mb: u32) -> Ct {
        self.mem_used_mb = Some(mb);
        self
    }
    /// El sondeo se corta sin `@@end` (el `sh` murió a medias) y sale con 100.
    pub fn probe_broken(mut self) -> Ct {
        self.probe_broken = true;
        self
    }
    /// El storage no admite `pct snapshot`: `auto` cae a vzdump.
    pub fn no_snapshots(mut self) -> Ct {
        self.no_snapshots = true;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Vm {
    id: u32,
    node: Option<String>,
    ostype: String,
    mem: u32,
    cores: u32,
    disks: Vec<String>,
    tags: String,
    running: bool,
    extra: Vec<String>,
    pending: Vec<String>,
}

impl Vm {
    pub fn new(id: u32) -> Vm {
        Vm { id, node: None, ostype: "l26".into(), mem: 2048, cores: 2, disks: vec![format!("scsi0: local-lvm:vm-{id}-disk-0,size=32G")], tags: String::new(), running: true, extra: vec![], pending: vec![] }
    }
    pub fn stopped(mut self) -> Vm {
        self.running = false;
        self
    }
    pub fn on(mut self, node: &str) -> Vm {
        self.node = Some(node.into());
        self
    }
    pub fn ostype(mut self, o: &str) -> Vm {
        self.ostype = o.into();
        self
    }
    pub fn mem(mut self, mb: u32) -> Vm {
        self.mem = mb;
        self
    }
    pub fn cores(mut self, n: u32) -> Vm {
        self.cores = n;
        self
    }
    pub fn disk(mut self, line: &str) -> Vm {
        self.disks.push(line.into());
        self
    }
    pub fn tags(mut self, t: &str) -> Vm {
        self.tags = t.into();
        self
    }
    pub fn conf(mut self, line: &str) -> Vm {
        self.extra.push(line.into());
        self
    }
    /// Cambios pendientes (`[PENDING]`) del `.conf`.
    pub fn pending(mut self, lines: &[&str]) -> Vm {
        self.pending = lines.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// Salida de `apt list --upgradable` del caso `host` (kernel, firmware, PVE, dos de seguridad y uno normal).
pub const HOST_UPGRADABLE: &str = "Listing...\npve-manager/stable 9.1.4 all [upgradable from: 9.1.1]\nproxmox-kernel-6.17/stable 6.17.13-2 amd64 [upgradable from: 6.17.9-1]\nproxmox-default-kernel/stable 2.0.2 all [upgradable from: 2.0.1]\npve-firmware/stable 3.17-2 all [upgradable from: 3.16-1]\nlibc6/stable-security 2.36-9+deb12u10 amd64 [upgradable from: 2.36-9+deb12u9]\nopenssl/stable-security,stable 3.0.17-1 amd64 [upgradable from: 3.0.16-1]\nhtop/stable 3.4.1-1 amd64 [upgradable from: 3.4.0-1]\nWARNING: apt does not have a stable CLI interface. Use with caution in scripts.\n";

#[derive(Debug, Clone)]
pub struct World {
    node: String,
    cts: Vec<Ct>,
    vms: Vec<Vm>,
    files: BTreeMap<PathBuf, Vec<u8>>,
    scenario: Scenario,
    // config.toml
    tags: Vec<String>,
    stopped_policy: Option<String>,
    require_fresh: Option<u32>,
    backup_mode: Option<String>,
    backup_storage: String,
    keep: Option<u32>,
    cts_cfg: Vec<(u32, Vec<String>)>,
    apprise_url: String,
    config_raw: Option<String>,
    strays: Vec<String>,
    /// `host` del state.json (revisión del host ya guardada).
    host_state: Option<serde_json::Value>,
    /// `[update] host_auto = true`: la corrida programada actualiza también el host.
    host_auto: bool,
    /// `[update] host_nvidia_auto = true`.
    host_nvidia_auto: bool,
    /// Datos de la página Storage generados a partir de los discos de los CT (`storage_demo`).
    storage_demo: bool,
}

impl Default for World {
    fn default() -> Self {
        World::new()
    }
}

impl World {
    pub fn new() -> World {
        World {
            node: "server".into(),
            cts: vec![],
            vms: vec![],
            files: BTreeMap::new(),
            scenario: Scenario { now_epoch: NOW, line_delay_ms: 15, ..Scenario::default() },
            tags: vec!["auto".into()],
            stopped_policy: None,
            require_fresh: None,
            backup_mode: None,
            backup_storage: "local".into(),
            keep: None,
            cts_cfg: vec![],
            apprise_url: "http://apprise.example:8000".into(),
            config_raw: None,
            strays: vec![],
            host_state: None,
            host_auto: false,
            host_nvidia_auto: false,
            storage_demo: false,
        }
    }
    /// Storages `local` (dir), `local-lvm` (lvmthin), `local-zfs` y `remote-backup-3` con backups, coherentes con los discos de los CT.
    pub fn storage_demo(mut self) -> World {
        self.storage_demo = true;
        self
    }
    pub fn node(mut self, n: &str) -> World {
        self.node = n.into();
        self
    }
    pub fn ct(mut self, c: Ct) -> World {
        self.cts.push(c);
        self
    }
    pub fn vm(mut self, v: Vm) -> World {
        self.vms.push(v);
        self
    }
    /// Falla un paso de un CT (`exit` 0 = el código por defecto del paso).
    pub fn fail(mut self, vmid: u32, step: FailAt, exit: i32) -> World {
        self.scenario.fail.push(FailSpec { vmid, step: step.name().into(), exit, hang: false });
        self
    }
    /// El paso se cuelga: el upgrade no se mata y devuelve `TimedOut`; el sondeo devuelve `TimedOut` de inmediato.
    pub fn hang(mut self, vmid: u32, step: FailAt) -> World {
        self.scenario.fail.push(FailSpec { vmid, step: step.name().into(), exit: 0, hang: true });
        self
    }
    /// ProxMenux tiene el lock de `flock` del CT.
    pub fn proxmenux_flock(mut self, vmid: u32) -> World {
        self.scenario.locks_held.push(LockSpec { vmid, by: "proxmenux".into() });
        self
    }
    /// Reserva de ProxMenux (archivo `proxmenux-lxc-update-<id>.lock` válido).
    pub fn proxmenux_reservation(self, vmid: u32) -> World {
        let body = format!("{{\"node\": \"{}\", \"task\": \"UPID:{}:0001:0002:0003:vzstop:{vmid}:root@pam:\"}}\n", self.node, self.node);
        self.file(&format!("/run/lock/proxmenux-lxc-update-{vmid}.lock"), body.as_bytes())
    }
    /// Marcador de ProxMenux ilegible: pxu lo trata como inválido y salta el CT.
    pub fn proxmenux_invalid_marker(self, vmid: u32) -> World {
        self.file(&format!("/run/lock/proxmenux-lxc-check-invalid-{vmid}"), b"")
    }
    /// Modo y dueño que `file_meta` informa para cualquier archivo (por defecto 0600 root).
    pub fn config_perms(mut self, mode: u32, uid: u32) -> World {
        self.scenario.config_mode = Some(mode);
        self.scenario.config_uid = Some(uid);
        self
    }
    pub fn line_delay_ms(mut self, ms: u64) -> World {
        self.scenario.line_delay_ms = ms;
        self
    }
    /// Copias de seguridad de un `.conf` (`117.conf.save`…): no son guests.
    pub fn stray_conf(mut self, name: &str) -> World {
        self.strays.push(name.into());
        self
    }
    /// Salida simulada de un comando del host (`apt-list`, `apt-sim`, `lspci`, `dkms`; con `.exit` = su código de salida).
    pub fn host_out(self, name: &str, text: &str) -> World {
        self.file(&format!("/exec/0/{name}.out"), text.as_bytes())
    }
    /// Revisión del host ya guardada en `state.json` (aparece la fila del host sin haber chequeado).
    pub fn host_state(mut self, v: serde_json::Value) -> World {
        self.host_state = Some(v);
        self
    }
    /// Archivo arbitrario del host simulado (ruta absoluta).
    pub fn file(mut self, path: &str, data: &[u8]) -> World {
        self.files.insert(PathBuf::from(path), data.to_vec());
        self
    }

    // ---- /etc/pxu/config.toml ----
    pub fn auto_tags(mut self, tags: &[&str]) -> World {
        self.tags = tags.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn host_auto(mut self) -> World {
        self.host_auto = true;
        self
    }
    pub fn host_nvidia_auto(mut self) -> World {
        self.host_nvidia_auto = true;
        self
    }
    pub fn stopped_policy(mut self, p: &str) -> World {
        self.stopped_policy = Some(p.into());
        self
    }
    pub fn require_fresh_minutes(mut self, m: u32) -> World {
        self.require_fresh = Some(m);
        self
    }
    /// Espacio libre y total (GB) del storage de backups, tal como lo informa `pvesh get …/storage/<s>/status`.
    pub fn storage_gb(mut self, avail: f64, total: f64) -> World {
        self.scenario.storage_avail_gb = Some(avail);
        self.scenario.storage_total_gb = Some(total);
        self
    }
    /// Sección `[backup]` con `mode` (`auto`/`snapshot`/`vzdump`/`none`).
    pub fn backup_mode(mut self, m: &str) -> World {
        self.backup_mode = Some(m.into());
        self
    }
    pub fn keep(mut self, n: u32) -> World {
        self.keep = Some(n);
        self
    }
    /// Sección `[[ct]]` con líneas TOML propias para ese CT.
    pub fn ct_config(mut self, vmid: u32, lines: &[&str]) -> World {
        let new = lines.iter().map(|s| s.to_string());
        match self.cts_cfg.iter_mut().find(|(v, _)| *v == vmid) {
            Some((_, l)) => {
                for n in new {
                    if !l.contains(&n) {
                        l.push(n);
                    }
                }
            }
            None => self.cts_cfg.push((vmid, new.collect())),
        }
        self
    }
    pub fn apprise_url(mut self, u: &str) -> World {
        self.apprise_url = u.into();
        self
    }
    /// Estado inicial del timer `pxu-update` simulado.
    pub fn timer(mut self, installed: bool, enabled: bool, calendar: &str) -> World {
        self.scenario.timer = crate::share::mock::MockTimer { installed, enabled, calendar: calendar.into(), ..Default::default() };
        self
    }
    /// Reemplaza todo el `config.toml` (casos de validación).
    pub fn config_raw(mut self, toml: &str) -> World {
        self.config_raw = Some(toml.into());
        self
    }

    fn render_config(&self) -> String {
        if let Some(r) = &self.config_raw {
            return r.clone();
        }
        let tags = self.tags.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(", ");
        let mut s = format!("[update]\nauto_update_tags = [{tags}]\n");
        if self.host_auto {
            s += "host_auto = true\n";
        }
        if self.host_nvidia_auto {
            s += "host_nvidia_auto = true\n";
        }
        if let Some(p) = &self.stopped_policy {
            s += &format!("stopped = \"{p}\"\n");
        }
        if let Some(m) = self.require_fresh {
            s += &format!("require_fresh_check_minutes = {m}\n");
        }
        if let Some(m) = &self.backup_mode {
            s += &format!("\n[backup]\nmode = \"{m}\"\nstorage = \"{}\"\n", self.backup_storage);
            if let Some(k) = self.keep {
                s += &format!("keep = {k}\n");
            }
        }
        for (id, lines) in &self.cts_cfg {
            s += &format!("\n[[ct]]\nvmid = {id}\n{}\n", lines.join("\n"));
        }
        s + &format!("\n[apprise]\ntransport = \"auto\"\napi_url = \"{}\"\napi_key_enc = \"mockenc707875\"\nretry_delays_ms = []\n", self.apprise_url)
    }

    /// Materializa el host simulado (todo en memoria).
    pub fn build(mut self) -> MockHost {
        let node = self.node.clone();
        self.scenario.node = node.clone();
        if self.storage_demo {
            let mut vols = vec![];
            for c in &self.cts {
                let root = c.storage.rootfs(c.id, c.size_gb);
                vols.push(super::storage::Vol { id: c.id, volid: root.split(',').next().unwrap_or("").to_string(), size: (c.size_gb as u64) << 30, running: c.running });
                for l in c.extra.iter().filter(|l| l.starts_with("mp")) {
                    let spec = l.split_once(": ").map_or("", |x| x.1);
                    let gb = spec.split(',').find_map(|p| p.strip_prefix("size=")).and_then(|v| v.trim_end_matches('G').parse::<u64>().ok());
                    if let (Some(v), Some(gb)) = (spec.split(',').next(), gb) {
                        vols.push(super::storage::Vol { id: c.id, volid: v.to_string(), size: gb << 30, running: c.running });
                    }
                }
            }
            let fx = super::storage::demo(&vols, self.scenario.now_epoch);
            self.files.insert(PathBuf::from("/etc/pve/storage.cfg"), fx.cfg.clone().into_bytes());
            self.scenario.storage = Some(fx);
        }
        let mut f = std::mem::take(&mut self.files);
        let mut put = |p: String, d: String| {
            f.insert(PathBuf::from(p), d.into_bytes());
        };
        put("/etc/hostname".into(), format!("{node}\n"));
        put("/etc/pxu/config.toml".into(), self.render_config());
        let mut ids = serde_json::Map::new();
        let mut checks = serde_json::Map::new();
        for c in &self.cts {
            let n = c.node.clone().unwrap_or_else(|| node.clone());
            let id = c.id;
            let (os_id, ver, pm) = c.distro.info();
            let mut l: Vec<String> = c.desc.lines().map(|d| format!("#{d}")).collect();
            l.extend([
                "arch: amd64".into(),
                format!("cores: {}", c.cores),
                format!("hostname: {}", guest_name(id)),
                if id % 3 == 0 { "features: nesting=1".into() } else { "features: keyctl=1,nesting=1".into() },
                format!("memory: {}", c.mem),
                format!("net0: name=eth0,bridge=vmbr0,firewall=1,hwaddr=BC:24:11:2A:{:02X}:{:02X},{},type=veth", id % 256, (id * 7) % 256, c.ip),
                "onboot: 1".into(),
                format!("ostype: {os_id}"),
                format!("rootfs: {}", c.storage.rootfs(id, c.size_gb)),
                "swap: 512".into(),
            ]);
            if !c.tags.is_empty() {
                l.push(format!("tags: {}", c.tags));
            }
            if c.template {
                l.push("template: 1".into());
            }
            if let Some(k) = &c.lock {
                l.push(format!("lock: {k}"));
            }
            l.push("unprivileged: 1".into());
            l.extend(c.extra.iter().cloned());
            for s in &c.snaps {
                l.extend([String::new(), format!("[{s}]"), "arch: amd64".into(), format!("hostname: {}", guest_name(id)), format!("memory: {}", c.mem), "snaptime: 1762500000".into()]);
            }
            put(format!("/etc/pve/nodes/{n}/lxc/{id}.conf"), l.join("\n") + "\n");
            ids.insert(id.to_string(), serde_json::json!({"node": n, "type": "lxc", "version": 1}));
            if c.no_snapshots {
                self.scenario.fail.push(FailSpec { vmid: id, step: "snapshot".into(), exit: 1, hang: false });
            }
            if c.running && n == node {
                // como PVE 9 real: cgroup.procs vacío, el estado va en cgroup.events
                put(format!("/sys/fs/cgroup/lxc/{id}/cgroup.procs"), String::new());
                put(format!("/sys/fs/cgroup/lxc/{id}/cgroup.events"), "populated 1\nfrozen 0\n".into());
                let used = c.mem_used_mb.map_or(c.mem as u64 * 35 / 100, |m| m as u64);
                put(format!("/sys/fs/cgroup/lxc/{id}/memory.current"), format!("{}\n", used * 1024 * 1024));
                put(format!("/sys/fs/cgroup/lxc/{id}/cpu.stat"), format!("usage_usec {}\nuser_usec {}\nsystem_usec {}\n", id as u64 * 1_234_567, id as u64 * 800_000, id as u64 * 434_567));
            }
            if !c.checked {
                continue;
            }
            // salida del sondeo (protocolo @@) y resultado previo de state.json
            let mut p = vec!["@@v 1".to_string(), format!("@@os {os_id} {ver} \"\""), format!("@@pm {pm}")];
            if pm != "none" {
                p.push(format!("@@cache_age {}", c.index_age_h * 3600));
                p.push("@@refresh skipped".into());
                p.extend(c.pkgs.iter().map(|k| format!("@@pkg {} {} {} {}", k.name, k.cur, k.new, k.sec as u8)));
                p.extend(c.held.iter().map(|h| format!("@@held {h}")));
                p.push(format!("@@reboot_required {}", c.reboot as u8));
            }
            p.push("@@end 0".into());
            let out = p.join("\n") + "\n";
            put(format!("/exec/{id}/probe-refresh.out"), out.replace("@@refresh skipped", "@@refresh ok"));
            if c.probe_broken {
                put(format!("/exec/{id}/probe-norefresh.out"), format!("@@v 1\n@@os {os_id} {ver} \"\"\n@@pm {pm}\nE: Could not get lock\n"));
                put(format!("/exec/{id}/probe-norefresh.exit"), "100\n".into());
            } else {
                put(format!("/exec/{id}/probe-norefresh.out"), out);
            }
            if c.node.is_none() {
                checks.insert(
                    id.to_string(),
                    serde_json::json!({
                        "checked_at": NOW - c.checked_min_ago * 60, "pm": pm, "os": format!("{os_id} {ver}"), "index_age_s": c.index_age_h * 3600,
                        "reboot_required": c.reboot, "held": c.held,
                        "packages": c.pkgs.iter().map(|k| serde_json::json!({"name": k.name, "cur": k.cur, "new": k.new, "sec": k.sec})).collect::<Vec<_>>(),
                    }),
                );
            }
        }
        for v in &self.vms {
            let n = v.node.clone().unwrap_or_else(|| node.clone());
            let id = v.id;
            let mut l = vec![
                format!("#VM vm-{id}"), "agent: 1".into(), "balloon: 0".into(), "bios: ovmf".into(), "boot: order=scsi0;net0".into(),
                format!("cores: {}", v.cores), "cpu: host".into(), "machine: q35".into(), format!("memory: {}", v.mem), format!("name: vm-{id}"),
                format!("net0: virtio=BC:24:11:2B:{:02X}:03,bridge=vmbr0,firewall=1", id % 256),
                "numa: 0".into(), "onboot: 1".into(), format!("ostype: {}", v.ostype), "scsihw: virtio-scsi-single".into(), "sockets: 1".into(),
            ];
            l.extend(v.disks.iter().cloned());
            if !v.tags.is_empty() {
                l.push(format!("tags: {}", v.tags));
            }
            l.extend(v.extra.iter().cloned());
            if !v.pending.is_empty() {
                l.push(String::new());
                l.push("[PENDING]".into());
                l.extend(v.pending.iter().cloned());
            }
            put(format!("/etc/pve/nodes/{n}/qemu-server/{id}.conf"), l.join("\n") + "\n");
            ids.insert(id.to_string(), serde_json::json!({"node": n, "type": "qemu", "version": 1}));
            if v.running && n == node {
                put(format!("/var/run/qemu-server/{id}.pid"), format!("{}\n", 8000 + id));
                put(format!("/proc/{}/.keep", 8000 + id), String::new());
            }
        }
        for s in &self.strays {
            put(format!("/etc/pve/nodes/{node}/lxc/{s}"), "hostname: stale\nmemory: 1\n".into());
        }
        put("/etc/pve/.vmlist".into(), serde_json::json!({"version": 1, "ids": ids}).to_string() + "\n");
        put("/var/lib/pxu/state.json".into(), serde_json::json!({"schema": 1, "checks": checks, "host": self.host_state}).to_string() + "\n");
        MockHost::new(f, self.scenario)
    }

    // ---- servidores predefinidos (demo del binario `--mock` y tests que necesitan "un servidor poblado") ----

    pub fn preset_names() -> &'static [&'static str] {
        &["demo", "update-ok", "update-fail", "distros", "cluster", "failures", "vms", "host"]
    }

    pub fn preset(name: &str) -> Option<World> {
        Some(match name {
            "demo" => World::demo(),
            "host" => World::host_gpu(),
            "update-ok" => World::new()
                .ct(Ct::new(401).tags("auto").std_updates().checked_minutes_ago(5))
                .ct(Ct::new(402).tags("auto").distro(Distro::Alpine).std_updates().checked_minutes_ago(5))
                .ct(Ct::new(403).tags("auto").stopped().std_updates().checked_minutes_ago(5))
                .backup_mode("auto")
                .stopped_policy("start_and_restore")
                .require_fresh_minutes(60)
                .ct_config(402, &["backup = \"vzdump\""]),
            "update-fail" => World::new()
                .ct(Ct::new(421).tags("auto").std_updates().checked_minutes_ago(5))
                .ct(Ct::new(422).tags("auto").std_updates().checked_minutes_ago(5))
                .ct(Ct::new(423).tags("auto").std_updates().checked_minutes_ago(5))
                .ct(Ct::new(424).tags("auto").std_updates().checked_minutes_ago(5))
                .backup_mode("auto")
                .require_fresh_minutes(60)
                .fail(421, FailAt::Upgrade, 100)
                .fail(422, FailAt::Backup, 255),
            "distros" => World::distros(),
            // VMs de distintos tipos (encendidas, con passthrough y NAS, apagada con cambios pendientes)
            "vms" => World::new()
                .vm(Vm::new(200).tags("home"))
                .vm(Vm::new(201).ostype("win11").mem(16384).cores(8).disk("tpmstate0: local-lvm:vm-201-disk-2,size=4M,version=v2.0").conf("hostpci0: 0000:01:00,pcie=1,x-vga=1"))
                .vm(Vm::new(202).tags("nas").disk("sata0: /dev/disk/by-id/ata-DISK1,size=4000G").disk("sata1: /dev/disk/by-id/ata-DISK2,size=4000G"))
                .vm(Vm::new(203).tags("infra").ostype("other").conf("hostpci0: 0000:00:1f.6,pcie=1").conf("usb0: host=0bda:8153").conf("protection: 1").conf("startup: up=1"))
                .vm(Vm::new(204).stopped().pending(&["memory: 4096", "cores: 4"])),
            // 104 upgrade falla · 105 reservado por ProxMenux · 106 backup falla · 107 sondeo colgado · 108 lock de PVE · 109 apagado · 110 sondeo cortado
            "failures" => World::new()
                .ct(Ct::new(104).std_updates())
                .ct(Ct::new(105).tags("auto").std_updates())
                .ct(Ct::new(106).tags("auto").std_updates())
                .ct(Ct::new(107).never_checked())
                .ct(Ct::new(108).pve_lock("backup").never_checked())
                .ct(Ct::new(109).stopped().never_checked())
                .ct(Ct::new(110).std_updates().probe_broken())
                .apprise_url("http://down.invalid:8000")
                .fail(104, FailAt::Upgrade, 100)
                .fail(106, FailAt::Backup, 255)
                .hang(107, FailAt::Probe)
                .proxmenux_flock(105),
            "cluster" => World::new()
                .node("pve1")
                .ct(Ct::new(101).tags("auto").std_updates())
                .ct(Ct::new(102).distro(Distro::Alpine))
                .ct(Ct::new(201).on("pve2").tags("auto"))
                .ct(Ct::new(202).on("pve2").distro(Distro::Ubuntu))
                .vm(Vm::new(250).on("pve2")),
            _ => return None,
        })
    }

    /// Host con A4000 (NVIDIA) + Phoenix3 (AMD): kernel 6.17.9-1 en marcha y 6.17.13-2 instalado (reinicio pendiente), DKMS al día.
    /// CT134 nvidia+dri (= host), CT131 dri+kfd, CT135 nvidia apagado sin datos, CT140 sin GPU, VM200 con hostpci0, CT150 OCI.
    pub fn host_gpu() -> World {
        World::new()
            .ct(Ct::new(131).conf("lxc.mount.entry: /dev/dri dev/dri none bind,optional,create=dir").conf("lxc.mount.entry: /dev/kfd dev/kfd none bind,optional,create=file"))
            .ct(Ct::new(134).conf("dev0: /dev/dri/renderD128,gid=104").conf("lxc.mount.entry: /dev/nvidia0 dev/nvidia0 none bind,optional,create=file").updates(vec![Pkg::new("libc6", "1", "2")]))
            .ct(Ct::new(135).stopped().conf("lxc.mount.entry: /dev/nvidia0 dev/nvidia0 none bind,optional,create=file"))
            .ct(Ct::new(140))
            .ct(Ct::new(150).conf("entrypoint: /bin/app").conf("dev0: /dev/dri/renderD128"))
            .vm(Vm::new(200).conf("hostpci0: 0000:01:00,pcie=1"))
            .host_out("apt-list", HOST_UPGRADABLE)
            .host_out("apt-sim", "Inst pve-manager [9.1.1] (9.1.4 Proxmox:stable [all])\n")
            .host_out("lspci", "01:00.0 VGA compatible controller [0300]: NVIDIA Corporation GA104GL [RTX A4000] [10de:24b0] (rev a1)\n01:00.1 Audio device [0403]: NVIDIA Corporation GA104 High Definition Audio Controller [10de:228b] (rev a1)\nc5:00.0 Display controller [0380]: Advanced Micro Devices, Inc. [AMD/ATI] Phoenix3 [1002:1900] (rev c1)\n")
            .host_out("dkms", "nvidia/595.112.07, 6.17.13-2-pve, x86_64: installed\n")
            .host_out("pve-version", "9.0.11\n")
            .file("/proc/sys/kernel/osrelease", b"6.17.9-1-pve\n")
            .file("/proc/meminfo", b"MemTotal:       65536000 kB\nMemFree:         2000000 kB\nMemAvailable:   40000000 kB\nBuffers:          500000 kB\n")
            .file("/proc/stat", b"cpu  400000 1000 200000 7000000 5000 0 3000 0 0 0\ncpu0 100000 250 50000 1750000 1250 0 750 0 0 0\ncpu1 100000 250 50000 1750000 1250 0 750 0 0 0\ncpu2 100000 250 50000 1750000 1250 0 750 0 0 0\ncpu3 100000 250 50000 1750000 1250 0 750 0 0 0\nintr 1\n")
            .file("/lib/modules/6.17.9-1-pve/.keep", b"")
            .file("/lib/modules/6.17.13-2-pve/.keep", b"")
            .file("/proc/driver/nvidia/version", b"NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.112.07  Tue Oct 21 2025\n")
            .file("/etc/apt/sources.list.d/pve.sources", b"Types: deb\nURIs: http://download.proxmox.com/debian/pve\nSuites: trixie\nComponents: pve-no-subscription\n")
            .file("/exec/134/nvidia-smi.out", b"595.112.07\n")
    }

    /// Driver NVIDIA instalado con el `.run` (`nvidia-uninstall`) en un host GPU, con el índice oficial simulado (`versions`). Sin `index` no hay red.
    pub fn nvidia_run_driver(self, versions: &[&str]) -> World {
        let html: String = std::iter::once("<html><body><a href='../'>../</a>\n".to_string()).chain(versions.iter().map(|v| format!("      <span class='dir'><a href='{v}/'>{v}/</a></span>\n"))).chain(std::iter::once("<a href='latest.txt'>latest.txt</a></body></html>\n".to_string())).collect();
        self.file("/usr/bin/nvidia-uninstall", b"#!/bin/sh\n").host_out("nvidia-index", &html)
    }

    /// Un CT por cada gestor de paquetes soportado, más uno sin gestor conocido.
    pub fn distros() -> World {
        let mut w = World::new();
        for (i, d) in [Distro::Debian, Distro::Ubuntu, Distro::Alpine, Distro::Arch, Distro::Fedora, Distro::Tumbleweed, Distro::NixOs].into_iter().enumerate() {
            let mut c = Ct::new(300 + i as u32).tags("distro");
            if d != Distro::NixOs {
                c = c.updates(vec![Pkg::new("zlib", "1.3", "1.3.1").security(), Pkg::new("bash", "5.2", "5.2.1")]);
            }
            c = c.distro(d).index_age_hours(if d == Distro::Arch { 30 * 24 } else { 4 });
            if d == Distro::Fedora {
                c = c.reboot_required();
            }
            w = w.ct(c);
        }
        w
    }

    /// Servidor poblado: 37 guests genéricos (34 encendidos, 3 apagados), 13 con actualizaciones, de varias distros.
    pub fn demo() -> World {
        let mut w = World::new().storage_demo().ct_config(105, &["auto_update = true"]);
        for i in 0..37u32 {
            let id = 100 + i;
            let distro = match id {
                104 | 109 | 111 | 112 | 120 | 126 | 133 => Distro::Alpine,
                105 => Distro::Fedora,
                125 | 127 | 128 | 134 => Distro::Ubuntu,
                _ => Distro::Debian,
            };
            let mut c = Ct::new(id).distro(distro).mem(if i % 2 == 1 { 1024 } else { 512 }).cores(1 + i % 4).size(2 + i % 5).storage(if id >= 130 { Storage::LvmThin } else { Storage::Zfs }).index_age_hours(5);
            if id == 106 || id == 131 {
                c = c.snapshots(&["pxu-20261107-043000"]);
            }
            c = c.tags(match id {
                106 | 110 | 116 | 123 => "infra;auto",
                103 | 108 | 119 | 125 => "apps;auto",
                _ if i % 3 != 0 => "apps",
                _ => "infra",
            });
            if (117..=119).contains(&id) {
                c = c.stopped().never_checked();
            }
            if id == 103 {
                c = c.desc("Hub with a bind mount and two snapshots").snapshots(&["pxu_pre_202611070430", "zfs_auto_5412"]).conf("mp0: local-zfs:subvol-103-mp0,mp=/mnt/data,backup=1,size=20G").ip("192.168.1.29/24,gw=192.168.1.1").held("docker-ce").updates(vec![
                    Pkg::new("openssl", "3.0.15-1", "3.0.16-1").security(),
                    Pkg::new("libssl3", "3.0.15-1", "3.0.16-1").security(),
                    Pkg::new("curl", "8.5.0-2", "8.6.0-1"),
                    Pkg::new("libcurl4", "8.5.0-2", "8.6.0-1"),
                    Pkg::new("tzdata", "2024a-0", "2024b-0"),
                    Pkg::new("base-files", "12.4", "12.5"),
                    Pkg::new("nodejs", "20.11.0", "20.12.0"),
                ]);
            }
            // 13 CT con actualizaciones en total (103 incluido)
            if matches!(id, 101 | 102 | 104 | 106 | 107 | 108 | 110 | 112 | 114 | 121 | 122 | 124) {
                let n = 1 + id % 3;
                c = c.updates((0..n).map(|k| Pkg::new(&format!("pkg-{k}"), "1.0", "1.1")).chain(std::iter::once(Pkg::new("libssl3", "3.0.15-1", "3.0.16-1").security())).collect());
            }
            if id == 110 {
                c = c.index_age_hours(32 * 24).reboot_required();
            }
            w = w.ct(c);
        }
        for s in ["117.conf.save", "117.conf.save.1", "119.conf.bak", "130.conf.bak"] {
            w = w.stray_conf(s);
        }
        w
    }
}

/// Hostname de la demo: unos cuantos realistas de 10-12 letras (para vigilar el ancho de NAME en las listas), el resto `ct-N`.
fn guest_name(id: u32) -> String {
    match id {
        114 => "llama-swap".into(),
        134 => "openweb-ui".into(),
        124 => "technitium".into(),
        128 => "uptime-kuma".into(),
        113 => "vaultwarden1".into(),
        _ => format!("ct-{id}"),
    }
}
