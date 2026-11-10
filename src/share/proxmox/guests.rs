//! Carga del listado LXC/VM leyendo archivos (ARCHITECTURE §1.1): cero procesos.
use crate::share::config::Config;
use crate::share::host::{read_string, Host};
use crate::share::model::{Guest, GuestKind, RunState};
use crate::share::proxmox::hostcheck::{self, HostInfo};
use crate::share::proxmox::conf::{parse_props, parse_size, prop, Conf};
use crate::share::state;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Inventory {
    pub node: String,
    pub guests: Vec<Guest>,
    pub config_error: Option<String>,
    /// Última revisión del host (state.json); la fila sintética del host (vmid 0) sale de aquí.
    pub host: Option<HostInfo>,
}

impl Inventory {
    /// Los guests reales: la fila del host no cuenta en los totales.
    pub fn real(&self) -> impl Iterator<Item = &Guest> {
        self.guests.iter().filter(|g| g.kind != GuestKind::Host)
    }
    pub fn count(&self) -> usize {
        self.real().count()
    }
    pub fn running(&self) -> usize {
        self.real().filter(|g| g.is_running()).count()
    }
    pub fn stopped(&self) -> usize {
        self.real().filter(|g| g.state == RunState::Stopped).count()
    }
    pub fn with_updates(&self) -> usize {
        self.real().filter(|g| g.updates() > 0).count()
    }
    /// ¿Algún guest usa GPU? (decide si aparece la columna GPU).
    pub fn any_gpu(&self) -> bool {
        self.guests.iter().any(|g| g.gpu.any()) || self.host.as_ref().is_some_and(|h| h.has_nvidia())
    }
}

fn vmid_of(p: &Path) -> Option<u32> {
    p.file_name()?.to_str()?.strip_suffix(".conf")?.parse().ok()
}

fn local_node(host: &dyn Host) -> String {
    read_string(host, "/etc/hostname").map(|s| s.trim().split('.').next().unwrap_or("").to_string()).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "localhost".into())
}

fn read_u64(host: &dyn Host, path: &str) -> Option<u64> {
    read_string(host, path).ok()?.trim().parse().ok()
}

pub(crate) fn lxc_running(host: &dyn Host, vmid: u32) -> bool {
    // cgroup.procs está vacío en PVE 9 (los procesos viven en lxc/<id>/ns/); cgroup.events sí dice `populated 1`.
    read_string(host, &format!("/sys/fs/cgroup/lxc/{vmid}/cgroup.events")).map(|s| s.lines().any(|l| l.trim() == "populated 1")).unwrap_or(false)
}

fn vm_running(host: &dyn Host, vmid: u32) -> bool {
    read_u64(host, &format!("/var/run/qemu-server/{vmid}.pid")).map(|pid| host.exists(Path::new(&format!("/proc/{pid}")))).unwrap_or(false)
}

fn cpu_usec(host: &dyn Host, vmid: u32) -> Option<u64> {
    let s = read_string(host, &format!("/sys/fs/cgroup/lxc/{vmid}/cpu.stat")).ok()?;
    s.lines().find_map(|l| l.strip_prefix("usage_usec ")?.trim().parse().ok())
}

/// `/proc/meminfo` → (MemTotal, MemAvailable) en bytes; `None` si falta alguno.
pub(crate) fn parse_meminfo(s: &str) -> Option<(u64, u64)> {
    let kb = |key: &str| s.lines().find_map(|l| l.strip_prefix(key)?.trim().trim_end_matches("kB").trim().parse::<u64>().ok());
    Some((kb("MemTotal:")? * 1024, kb("MemAvailable:")? * 1024))
}

/// `/proc/stat` → (nº de CPUs, µs de CPU ocupada acumulados): línea `cpu` menos idle e iowait, USER_HZ = 100.
pub(crate) fn parse_stat(s: &str) -> Option<(u64, u64)> {
    let cores = s.lines().filter(|l| l.strip_prefix("cpu").is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))).count() as u64;
    let f: Vec<u64> = s.lines().find_map(|l| l.strip_prefix("cpu ")).map(|r| r.split_whitespace().map_while(|x| x.parse().ok()).collect())?;
    if cores == 0 || f.len() < 5 {
        return None;
    }
    // Solo los 8 primeros campos: guest y guest_nice (9 y 10) ya están dentro de user y nice.
    let busy = f.iter().take(8).sum::<u64>() - f[3] - f[4]; // idle y iowait
    Some((cores, busy * 10_000))
}

/// Versión corta de Proxmox VE: `9.0.11` (de `9.0.11~pve1` o `9.0.11+deb13`); `None` si no se puede leer.
pub(crate) fn parse_pve_version(s: &str) -> Option<String> {
    let v = s.lines().next()?.trim();
    let v = v.split(|c| matches!(c, '~' | '+' | '-')).next()?;
    (!v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.') && v.starts_with(|c: char| c.is_ascii_digit())).then(|| v.to_string())
}

/// `dpkg-query -W -f='${Version}' pve-manager` (solo lectura).
pub(crate) fn pve_version(host: &dyn Host) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let o = crate::share::host::ExecOpts { timeout_s: Some(5), ..Default::default() };
    let e = host.host_cmd(&PVE_VERSION, &o, &mut out).ok()?;
    if e.code != 0 {
        return None;
    }
    parse_pve_version(&out.join("\n"))
}

pub const PVE_VERSION: [&str; 4] = ["dpkg-query", "-W", "-f=${Version}", "pve-manager"];

fn host_cpu(host: &dyn Host) -> Option<(u64, u64)> {
    parse_stat(&read_string(host, "/proc/stat").ok()?)
}

/// Muestra ligera del CPU acumulado (µs): LXC por `cpu.stat`, el host (vmid 0) por `/proc/stat`; no lee el inventario.
pub fn sample_cpu(host: &dyn Host, vmids: &[u32]) -> std::collections::HashMap<u32, u64> {
    vmids.iter().filter_map(|&v| if v == 0 { host_cpu(host).map(|(_, u)| (0, u)) } else { cpu_usec(host, v).map(|u| (v, u)) }).collect()
}

fn disk_bytes(conf: &Conf, kind: GuestKind) -> Option<u64> {
    let keys: &[&str] = match kind {
        GuestKind::Lxc => &["rootfs", "mp"],
        GuestKind::Vm => &["scsi", "virtio", "sata", "ide"],
        GuestKind::Host => &[],
    };
    let mut total = 0u64;
    let mut any = false;
    for (k, v) in &conf.main {
        let base = k.trim_end_matches(|c: char| c.is_ascii_digit());
        if !keys.contains(&base) {
            continue;
        }
        let (_, props) = parse_props(v);
        if let Some(sz) = prop(&props, "size").and_then(parse_size) {
            total += sz;
            any = true;
        }
    }
    any.then_some(total)
}

fn split_tags(s: &str) -> Vec<String> {
    s.split([';', ',']).map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()
}

fn conf_files(host: &dyn Host, dir: &Path) -> Vec<PathBuf> {
    host.read_dir(dir).unwrap_or_default().into_iter().filter(|p| vmid_of(p).is_some()).collect()
}

pub fn load(host: &dyn Host) -> Inventory {
    let local = local_node(host);
    let (cfg, config_error) = match Config::load(host) {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(e)),
    };
    let st = state::load_state(host);
    let checks: std::collections::BTreeMap<u32, _> = st.checks.into_iter().filter_map(|(k, v)| Some((k.parse().ok()?, v))).collect();
    let mut guests = Vec::new();

    let nodes = host.read_dir(Path::new("/etc/pve/nodes")).unwrap_or_default();
    for node_dir in nodes {
        let node = node_dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        let is_local = node == local;
        for (sub, kind) in [("lxc", GuestKind::Lxc), ("qemu-server", GuestKind::Vm)] {
            for path in conf_files(host, &node_dir.join(sub)) {
                let Some(vmid) = vmid_of(&path) else { continue };
                let Ok(text) = read_string(host, &path.to_string_lossy()) else { continue };
                let conf = Conf::parse(&text);
                let name = conf
                    .get(if kind == GuestKind::Lxc { "hostname" } else { "name" })
                    .map(String::from)
                    .unwrap_or_else(|| format!("{}{vmid}", if kind == GuestKind::Lxc { "CT" } else { "VM" }));
                let state = match (is_local, kind) {
                    (false, _) => RunState::Remote,
                    (true, GuestKind::Lxc) if lxc_running(host, vmid) => RunState::Running,
                    (true, GuestKind::Vm) if vm_running(host, vmid) => RunState::Running,
                    _ => RunState::Stopped,
                };
                let tags = conf.get("tags").map(split_tags).unwrap_or_default();
                let ostype = conf.get("ostype").unwrap_or("").to_string();
                let check = if kind == GuestKind::Lxc { checks.get(&vmid).cloned() } else { None };
                let os_label = check.as_ref().map(|c| c.os.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| ostype.clone());
                let live = is_local && state == RunState::Running;
                guests.push(Guest {
                    vmid,
                    kind,
                    node: node.clone(),
                    name,
                    state,
                    lock: conf.get("lock").map(String::from),
                    template: conf.flag("template"),
                    auto_update: is_local && kind == GuestKind::Lxc && cfg.auto_update(vmid, &tags),
                    backup: cfg.backup_mode(vmid).to_string(),
                    tags,
                    ostype,
                    os_label,
                    cores: conf.get_u64("cores"),
                    memory_mb: conf.get_u64("memory"),
                    mem_used_bytes: if live && kind == GuestKind::Lxc { read_u64(host, &format!("/sys/fs/cgroup/lxc/{vmid}/memory.current")) } else { None },
                    cpu_usec: if live && kind == GuestKind::Lxc { cpu_usec(host, vmid) } else { None },
                    disk_bytes: disk_bytes(&conf, kind),
                    onboot: conf.flag("onboot"),
                    conf_path: path.to_string_lossy().into_owned(),
                    gpu: hostcheck::gpu_use(&conf, kind),
                    conf,
                    check,
                });
            }
        }
    }
    if let Some(h) = &st.host {
        let c = h.as_check();
        let mem = read_string(host, "/proc/meminfo").ok().and_then(|t| parse_meminfo(&t));
        let cpu = host_cpu(host);
        guests.push(Guest {
            vmid: 0,
            kind: GuestKind::Host,
            node: local.clone(),
            name: local.clone(),
            state: RunState::Running,
            lock: None,
            template: false,
            tags: vec![],
            ostype: String::new(),
            os_label: match pve_version(host) {
                Some(v) => format!("{} {v}", c.os),
                None => c.os.clone(),
            },
            cores: cpu.map(|(n, _)| n),
            memory_mb: mem.map(|(t, _)| t / (1024 * 1024)),
            mem_used_bytes: mem.map(|(t, a)| t.saturating_sub(a)),
            cpu_usec: cpu.map(|(_, u)| u),
            disk_bytes: None,
            onboot: false,
            auto_update: cfg.update.host_auto,
            backup: cfg.backup_mode(0).to_string(),
            conf_path: String::new(),
            conf: Conf::default(),
            check: (h.error.is_none()).then_some(c),
            gpu: Default::default(),
        });
    }
    guests.sort_by_key(|g| g.vmid);
    Inventory { node: local, guests, config_error, host: st.host }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::mock::MockHost;

    use crate::share::mock::scenario::World;

    pub fn fx(name: &str) -> MockHost {
        match name {
            "basic-37" => World::demo(),
            "vms" => World::preset("vms").unwrap(),
            "cluster" => World::preset("cluster").unwrap(),
            "failures" => World::preset("failures").unwrap(),
            _ => panic!("unknown case {name}"),
        }
        .build()
    }

    #[test]
    fn meminfo_parser_reads_total_and_available() {
        let t = "MemTotal:       65536000 kB\nMemFree: 1 kB\nMemAvailable:   40000000 kB\n";
        assert_eq!(parse_meminfo(t), Some((65536000 * 1024, 40000000 * 1024)));
        assert_eq!(parse_meminfo("MemTotal: 10 kB\n"), None);
        assert_eq!(parse_meminfo(""), None);
    }

    #[test]
    fn stat_parser_counts_cpus_and_busy_time_without_idle_and_iowait() {
        let t = "cpu  400 10 200 7000 50 0 30 0 0 0\ncpu0 1 1 1 1 1 0 0 0 0 0\ncpu1 1 1 1 1 1 0 0 0 0 0\nintr 5\n";
        // busy = 400+10+200+0+30 = 640 jiffies = 6_400_000 µs
        assert_eq!(parse_stat(t), Some((2, 6_400_000)));
        // guest (300) y guest_nice (5) ya están dentro de user y nice: no se suman otra vez
        let g = "cpu  400 10 200 7000 50 0 30 0 300 5\ncpu0 1 1 1 1 1 0 0 0 0 0\n";
        assert_eq!(parse_stat(g), Some((1, 6_400_000)));
        assert_eq!(parse_stat("intr 5\n"), None);
        assert_eq!(parse_stat("cpu  1 2\ncpu0 1\n"), None);
    }

    #[test]
    fn pve_version_parser_keeps_the_short_version() {
        assert_eq!(parse_pve_version("9.0.11\n").as_deref(), Some("9.0.11"));
        assert_eq!(parse_pve_version("9.0.11~pve1").as_deref(), Some("9.0.11"));
        assert_eq!(parse_pve_version("9.0.11+deb13u1").as_deref(), Some("9.0.11"));
        assert_eq!(parse_pve_version("9.0.11-1"), Some("9.0.11".into()));
        assert_eq!(parse_pve_version(""), None);
        assert_eq!(parse_pve_version("dpkg-query: no packages found"), None);
    }

    #[test]
    fn pve_version_reads_dpkg_query_and_fails_soft() {
        let h = World::host_gpu().build();
        assert_eq!(pve_version(&h).as_deref(), Some("9.0.11"));
        let h = World::host_gpu().host_out("pve-version", "").build();
        assert_eq!(pve_version(&h), None);
    }

    #[test]
    fn basic_37_matches_capture_counts() {
        let h = fx("basic-37");
        let inv = load(&h);
        assert_eq!(inv.node, "server");
        assert_eq!(inv.guests.len(), 37);
        assert_eq!(inv.running(), 34);
        assert_eq!(inv.stopped(), 3);
        assert_eq!(inv.with_updates(), 13);
        let mcp = inv.guests.iter().find(|g| g.vmid == 103).unwrap();
        assert_eq!((mcp.name.as_str(), mcp.updates(), mcp.security()), ("ct-103", 7, 2));
        assert!(mcp.auto_update, "tag auto");
        assert_eq!(mcp.conf.snapshots.len(), 2);
        assert_eq!(mcp.os_label, "debian 12");
        assert!(h.recorded().is_empty(), "listing must not spawn processes");
    }

    #[test]
    fn lxc_running_uses_cgroup_events_not_empty_cgroup_procs() {
        let h = fx("basic-37");
        assert_eq!(read_string(&h, "/sys/fs/cgroup/lxc/103/cgroup.procs").unwrap().trim(), "", "real PVE 9 leaves cgroup.procs empty");
        assert!(lxc_running(&h, 103));
        let stopped = load(&h).guests.into_iter().find(|g| g.state == RunState::Stopped && g.kind == GuestKind::Lxc).unwrap();
        assert!(!h.exists(Path::new(&format!("/sys/fs/cgroup/lxc/{}", stopped.vmid))));
        assert!(!lxc_running(&h, stopped.vmid));
    }

    #[test]
    fn backup_copies_of_confs_are_not_guests() {
        let h = fx("basic-37");
        let dir = Path::new("/etc/pve/nodes/server/lxc");
        let all = h.read_dir(dir).unwrap();
        assert!(all.iter().any(|p| p.to_string_lossy().ends_with(".conf.save")), "world has stray copies");
        assert!(all.iter().any(|p| p.to_string_lossy().ends_with(".conf.bak")));
        assert_eq!(conf_files(&h, dir).len(), 37);
        assert_eq!(load(&h).guests.len(), 37);
    }

    #[test]
    fn vms_cluster_and_locks() {
        let inv = load(&fx("vms"));
        let v = |id| inv.guests.iter().find(|g| g.vmid == id).unwrap();
        assert!(v(201).is_running() && v(201).kind == GuestKind::Vm);
        assert_eq!(v(204).state, RunState::Stopped);
        assert_eq!(v(204).conf.pending.len(), 2);
        let c = load(&fx("cluster"));
        assert_eq!(c.guests.iter().filter(|g| g.state == RunState::Remote).count(), 3);
        let f = load(&fx("failures"));
        assert_eq!(f.guests.iter().find(|g| g.vmid == 108).unwrap().lock.as_deref(), Some("backup"));
    }

    #[test]
    fn missing_everything_is_empty_not_a_crash() {
        let inv = load(&World::new().node("x").build());
        assert!(inv.guests.is_empty());
    }
}
