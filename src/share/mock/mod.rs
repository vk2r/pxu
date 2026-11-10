//! MockHost: host simulado en memoria (archivos armados por `scenario::World`); responde comandos según el `Scenario`.
//! Compilado solo en debug/tests o con `--features mock` (nunca en el release por defecto).
pub mod scenario;
pub mod storage;
use crate::share::host::*;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

#[derive(Debug, Clone, Default)]
pub struct Scenario {
    pub node: String,
    pub now_epoch: u64,
    pub line_delay_ms: u64,
    pub fail: Vec<FailSpec>,
    pub locks_held: Vec<LockSpec>,
    /// Modo/dueño que `file_meta` informa para cualquier archivo (por defecto 0600 root).
    pub config_mode: Option<u32>,
    pub config_uid: Option<u32>,
    /// Estado inicial del timer `pxu-update` (por defecto: unidades sin instalar).
    pub timer: MockTimer,
    /// Espacio libre / total del storage de backups (GB) que informa `pvesh get …/storage/<s>/status` (por defecto 800 / 1000).
    pub storage_avail_gb: Option<f64>,
    pub storage_total_gb: Option<f64>,
    /// Datos de la página Storage (`pvesh get …/storage`, `…/content`, `lvs`); sin esto esas lecturas fallan.
    pub storage: Option<storage::Fixture>,
}

/// Timer simulado: lo que `timer_status` informa y `apply_schedule` cambia.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MockTimer {
    pub installed: bool,
    pub enabled: bool,
    /// `OnCalendar` efectivo (vacío = ninguno).
    pub calendar: String,
    /// Operaciones recibidas, en orden (`write units`, `write drop-in: ...`, `daemon-reload`, `enable --now`…).
    pub ops: Vec<String>,
    /// Último drop-in escrito.
    pub dropin: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FailSpec {
    pub vmid: u32,
    pub step: String,
    pub exit: i32,
    pub hang: bool,
}

#[derive(Debug, Clone)]
pub struct LockSpec {
    pub vmid: u32,
    pub by: String,
}

/// Comando recibido, tal como lo vería el host (registro para asserts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    Guest(u32, Vec<String>),
    Host(Vec<String>),
}

pub struct MockHost {
    /// Archivos del host simulado (rutas absolutas); no cambian: las escrituras de pxu van al `overlay`.
    files: BTreeMap<PathBuf, Vec<u8>>,
    scenario: Scenario,
    log: Mutex<Vec<Recorded>>,
    /// Escrituras de pxu (estado, outbox): nunca tocan los archivos base. `None` = borrado.
    overlay: Mutex<BTreeMap<PathBuf, Option<Vec<u8>>>>,
    lock_dir: PathBuf,
    /// CTs cuyo upgrade ya corrió: el sondeo siguiente no ve paquetes pendientes.
    upgraded: Mutex<HashSet<u32>>,
    /// El `full-upgrade` del host ya corrió: la revisión siguiente lee los `*-after` (o no ve pendientes).
    host_installed: std::sync::atomic::AtomicBool,
    /// El backup de `/etc` del host ya corrió: `apt-list-late.out` / `apt-sim-late.out` (si existen) reemplazan a los normales (la lista cambió entre medias).
    host_backed_up: std::sync::atomic::AtomicBool,
    /// Segundos que avanza el reloj simulado (`/exec/0/install.advance` en el `full-upgrade`: una instalación larga).
    clock_offset: std::sync::atomic::AtomicU64,
    /// Locks de pxu tomados (en memoria).
    locks: Arc<Mutex<HashSet<PathBuf>>>,
    timer: Mutex<MockTimer>,
    /// Backups borrados con `pvesm free` (página Storage): desaparecen de las lecturas de contenido, sin tocar los datos base.
    deleted: Mutex<HashSet<String>>,
}

struct MockLock(Arc<Mutex<HashSet<PathBuf>>>, PathBuf);
impl Drop for MockLock {
    fn drop(&mut self) {
        self.0.lock().unwrap().remove(&self.1);
    }
}

impl MockHost {
    /// Instalación en el host simulada. Todo se decide por `/exec/0/<nombre>.out` (+ `.exit`); sin archivo hay un valor por defecto sano:
    /// auditoría limpia, 50 GB libres, raíz ext4, `tar`/`chmod`/`zfs` bien, instalación bien (`install.exit` ≠ 0 = falla), `dpkg-query` derivado de `apt-list.out`.
    fn host_update_cmd(&self, argv: &[&str], s: &mut dyn LineSink) -> io::Result<ExitInfo> {
        use crate::share::update::hostupdate as hu;
        let file = |name: &str| self.exec_file(0, name);
        let code_of = |name: &str| file(&format!("{name}.exit")).and_then(|c| c.trim().parse().ok()).unwrap_or(0);
        let say = |s: &mut dyn LineSink, t: &str| t.lines().for_each(|l| s.line(l));
        let installed = self.host_installed.load(std::sync::atomic::Ordering::SeqCst);
        let (name, default): (&str, String) = match argv {
            a if a == hu::DPKG_AUDIT => ("dpkg-audit", String::new()),
            a if a == hu::DF => ("df", "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/mapper/pve-root 98000000 20000000 50000000 29% /\n".into()),
            a if a == hu::FS_TYPE => ("findmnt-fstype", "ext4\n".into()),
            a if a == hu::FS_SOURCE => ("findmnt-source", "/dev/mapper/pve-root\n".into()),
            a if a == hu::AUTOREMOVE_SIM => ("autoremove-sim", String::new()),
            a if a == hu::DPKG_QUERY => {
                // antes: versión actual de `apt-list.out`; después: la nueva (derivado, salvo que el caso aporte el archivo)
                let list = crate::share::proxmox::hostcheck::parse_upgradable(&file("apt-list.out").unwrap_or_default());
                let text: String = list.iter().map(|p| format!("{} {}\n", p.name, if installed { &p.new } else { &p.cur })).collect();
                (if installed { "dpkg-query-after" } else { "dpkg-query" }, text)
            }
            a if a == hu::INSTALL => ("install", "Reading package lists...\nSetting up packages ...\n".into()),
            a if a == hu::AUTOREMOVE => ("autoremove", "Removing old packages ...\n".into()),
            a if a == hu::DPKG_CONFIGURE => ("dpkg-configure", "Setting up pending packages ...\n".into()),
            ["tar", ..] => {
                let dst = argv.iter().find(|a| a.contains("/etc-")).copied().unwrap_or("");
                let rc = code_of("tar");
                self.host_backed_up.store(true, std::sync::atomic::Ordering::SeqCst);
                if rc >= 2 {
                    // un tar real deja el archivo a medias
                    self.write_file(Path::new(dst), b"partial")?;
                    s.line("tar: error");
                    return Ok(ExitInfo { code: rc });
                }
                self.write_file(Path::new(dst), b"mock tar archive")?;
                return Ok(ExitInfo { code: rc });
            }
            ["chmod", ..] | ["install", "-d", ..] | ["zfs", ..] => (if argv[0] == "zfs" { "zfs" } else { "noop" }, String::new()),
            _ => return Err(io::Error::new(io::ErrorKind::NotFound, "mock: unsupported host-update command")),
        };
        say(s, &file(&format!("{name}.out")).unwrap_or(default));
        let rc = code_of(name);
        if argv == hu::INSTALL {
            if let Some(s) = file("install.advance").and_then(|t| t.trim().parse::<u64>().ok()) {
                self.clock_offset.fetch_add(s, std::sync::atomic::Ordering::SeqCst);
            }
        }
        if rc == 0 && argv == hu::INSTALL {
            self.host_installed.store(true, std::sync::atomic::Ordering::SeqCst);
            // un kernel nuevo exige reiniciar
            if file("apt-list.out").is_some_and(|t| t.contains("kernel")) {
                self.overlay.lock().unwrap().insert(PathBuf::from("/var/run/reboot-required"), Some(b"*** System restart required ***\n".to_vec()));
            }
        }
        if rc == 0 && argv == hu::DPKG_CONFIGURE {
            self.overlay.lock().unwrap().insert(PathBuf::from("/exec/0/dpkg-audit.out"), Some(Vec::new()));
        }
        Ok(ExitInfo { code: rc })
    }
    /// Paso NVIDIA simulado (`update::nvidia`): salidas y códigos salen de `/exec/0/nvidia-*.out|.exit`; `install` cambia `/proc/driver/nvidia/version`.
    fn nvidia_cmd(&self, argv: &[&str], s: &mut dyn LineSink) -> Option<io::Result<ExitInfo>> {
        use crate::share::update::nvidia as nv;
        let file = |n: &str| self.exec_file(0, n);
        let code = |n: &str| file(&format!("{n}.exit")).and_then(|c| c.trim().parse().ok()).unwrap_or(0);
        let say = |s: &mut dyn LineSink, t: &str| t.lines().for_each(|l| s.line(l));
        let proc_text = || crate::share::host::read_string(self, "/proc/driver/nvidia/version").unwrap_or_default();
        let hash = "a".repeat(64);
        let done = |c: i32| Some(Ok(ExitInfo { code: c }));
        match argv {
            a if a == nv::INDEX_CURL => {
                let rc = if file("nvidia-index.out").is_some() { code("nvidia-index") } else { 6 };
                match file("nvidia-index.out") {
                    Some(t) if rc == 0 => say(s, &t),
                    _ => s.line("curl: (6) Could not resolve host: download.nvidia.com"),
                }
                done(rc)
            }
            ["curl", .., url] if url.ends_with(".run.sha256sum") => {
                let name = url.rsplit('/').next().unwrap_or("").trim_end_matches(".sha256sum");
                say(s, &file("nvidia-sha.out").unwrap_or_else(|| format!("{hash}  {name}\n")));
                done(code("nvidia-sha"))
            }
            ["curl", .., "-o", dst, _] => {
                let rc = code("nvidia-download");
                if rc == 0 {
                    let _ = self.write_file(Path::new(dst), b"mock .run");
                }
                done(rc)
            }
            ["sha256sum", p] => {
                say(s, &file("nvidia-sha256sum.out").unwrap_or_else(|| format!("{hash}  {p}\n")));
                done(code("nvidia-sha256sum"))
            }
            ["sh", _, "--check"] => {
                s.line("Verifying archive integrity... OK");
                done(code("nvidia-check"))
            }
            ["sh", _, "--extract-only", ..] => done(code("nvidia-extract")),
            ["make", ..] => done(code("nvidia-make")),
            ["sh", p, "--no-questions", ..] => {
                let rc = code("nvidia-install");
                if rc == 0 && file("nvidia-install.noeffect").is_none() {
                    // el módulo nuevo queda cargado: /proc/driver/nvidia/version y nvidia-smi dicen la versión instalada
                    let v = p.rsplit('/').next().unwrap_or("").trim_start_matches("NVIDIA-Linux-x86_64-").trim_end_matches(".run");
                    let open = argv.contains(&"--kernel-module-type=open");
                    let kind = if open { "Open Kernel Module for x86_64" } else { "x86_64 Kernel Module" };
                    let _ = self.write_file(Path::new("/proc/driver/nvidia/version"), format!("NVRM version: NVIDIA UNIX {kind}  {v}  Tue Oct 21 2025\n").as_bytes());
                }
                s.line(if rc == 0 { "Installation of the NVIDIA Accelerated Graphics Driver is now complete." } else { "ERROR: Unable to build the NVIDIA kernel module." });
                done(rc)
            }
            ["modprobe", "-r", ..] => done(code("nvidia-unload")),
            ["modprobe", "nvidia"] => done(code("nvidia-modprobe")),
            ["systemctl", "stop" | "start", "nvidia-persistenced"] => done(0),
            ["install", "-d", "-m", "0700", d] if *d == nv::WORK_DIR => done(0),
            ["df", "-Pk", d] if *d == nv::WORK_DIR => {
                say(s, &file("nvidia-df.out").unwrap_or_else(|| "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/mapper/pve-root 98000000 20000000 50000000 29% /\n".into()));
                done(0)
            }
            ["rm", "-rf", "--", d] if *d == nv::WORK_DIR => {
                let mut o = self.overlay.lock().unwrap();
                for k in o.keys().cloned().collect::<Vec<_>>() {
                    if k.starts_with(nv::WORK_DIR) {
                        o.insert(k, None);
                    }
                }
                done(0)
            }
            ["nvidia-smi", ..] if argv == crate::share::proxmox::hostcheck::NVIDIA_SMI => {
                let rc = code("nvidia-smi");
                if rc == 0 {
                    s.line(&crate::share::proxmox::hostcheck::parse_nvrm(&proc_text()).unwrap_or_default());
                } else {
                    s.line("NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA driver.");
                }
                done(rc)
            }
            ["pct", "push", id, ..] => done(self.exec_file(0, &format!("nvidia-push-{id}.exit")).and_then(|c| c.trim().parse().ok()).unwrap_or(0)),
            _ => None,
        }
    }
    pub fn new(files: BTreeMap<PathBuf, Vec<u8>>, scenario: Scenario) -> Self {
        let scenario_timer = scenario.timer.clone();
        let lock_dir = PathBuf::from("/run/lock");
        MockHost { files, scenario, log: Mutex::new(Vec::new()), overlay: Mutex::new(BTreeMap::new()), lock_dir, upgraded: Mutex::new(HashSet::new()), host_installed: std::sync::atomic::AtomicBool::new(false), host_backed_up: std::sync::atomic::AtomicBool::new(false), clock_offset: std::sync::atomic::AtomicU64::new(0), locks: Arc::new(Mutex::new(HashSet::new())), timer: Mutex::new(scenario_timer), deleted: Mutex::new(HashSet::new()) }
    }
    /// Estado actual del timer simulado (para asserts).
    pub fn timer(&self) -> MockTimer {
        self.timer.lock().unwrap().clone()
    }
    pub fn scenario(&self) -> &Scenario {
        &self.scenario
    }
    /// Comandos emitidos hasta ahora.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }
    /// Estado del CT como lo ve el inventario: `cgroup.events` (con el overlay de start/stop).
    fn ct_running(&self, vmid: u32) -> bool {
        crate::share::proxmox::guests::lxc_running(self, vmid)
    }
    fn set_ct_running(&self, vmid: u32, on: bool) {
        let p = format!("/sys/fs/cgroup/lxc/{vmid}/cgroup.events");
        let _ = self.write_file(Path::new(&p), format!("populated {}\nfrozen 0\n", on as u8).as_bytes());
    }
    fn base_exists(&self, p: &Path) -> bool {
        let p = norm(p);
        self.files.contains_key(&p) || self.files.range(p.clone()..).next().is_some_and(|(k, _)| k.starts_with(&p))
    }
    /// Texto de un archivo base de `/exec/<vmid>/` (salidas simuladas de los comandos dentro del CT).
    fn exec_file(&self, vmid: u32, name: &str) -> Option<String> {
        let p = PathBuf::from(format!("/exec/{vmid}/{name}"));
        match self.overlay.lock().unwrap().get(&p) {
            Some(Some(d)) => return Some(String::from_utf8_lossy(d).into_owned()),
            Some(None) => return None,
            None => {}
        }
        self.files.get(&p).map(|d| String::from_utf8_lossy(d).into_owned())
    }
}

/// Archivo `/exec/0/<nombre>` que responde cada comando de la revisión del host.
fn host_review_file(argv: &[&str]) -> Option<&'static str> {
    use crate::share::proxmox::hostcheck::*;
    match argv {
        a if a == APT_UPDATE => Some("apt-update.out"),
        a if a == APT_LIST => Some("apt-list.out"),
        a if a == APT_SIM => Some("apt-sim.out"),
        a if a == LSPCI => Some("lspci.out"),
        a if a == DKMS => Some("dkms.out"),
        a if a == crate::share::proxmox::guests::PVE_VERSION => Some("pve-version.out"),
        _ => None,
    }
}

fn norm(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new("/").join(p)
    }
}

impl Host for MockHost {
    fn read_file(&self, p: &Path) -> io::Result<Vec<u8>> {
        match self.overlay.lock().unwrap().get(&norm(p)) {
            Some(Some(d)) => return Ok(d.clone()),
            Some(None) => return Err(io::ErrorKind::NotFound.into()),
            None => {}
        }
        self.files.get(&norm(p)).cloned().ok_or_else(|| io::ErrorKind::NotFound.into())
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = Vec::new();
        let ov = self.overlay.lock().unwrap();
        let dir = norm(p);
        let mut found = false;
        for k in self.files.keys().filter(|k| k.starts_with(&dir) && **k != dir) {
            found = true;
            let name = k.strip_prefix(&dir).unwrap().components().next().unwrap().as_os_str().to_owned();
            let q = p.join(&name);
            if name != ".keep" && ov.get(&dir.join(&name)) != Some(&None) && !v.contains(&q) {
                v.push(q);
            }
        }
        for (k, val) in ov.iter() {
            if val.is_some() && k.parent() == Some(dir.as_path()) {
                found = true;
                let q = p.join(k.file_name().unwrap());
                if !v.contains(&q) {
                    v.push(q);
                }
            }
        }
        if !found {
            return Err(io::ErrorKind::NotFound.into());
        }
        v.sort();
        Ok(v)
    }
    fn exists(&self, p: &Path) -> bool {
        match self.overlay.lock().unwrap().get(&norm(p)) {
            Some(v) => v.is_some(),
            None => self.base_exists(p),
        }
    }
    fn guest_exec(&self, vmid: u32, argv: &[&str], _o: &ExecOpts, sink: &mut dyn LineSink) -> io::Result<ExitInfo> {
        self.log.lock().unwrap().push(Recorded::Guest(vmid, argv.iter().map(|s| s.to_string()).collect()));
        let fail = |step: &str| self.scenario.fail.iter().find(|f| f.vmid == vmid && f.step == step);
        let done = self.upgraded.lock().unwrap().contains(&vmid);
        let emit = |sink: &mut dyn LineSink, text: &str, delay: bool| {
            for l in text.lines() {
                sink.line(l);
                if delay && self.scenario.line_delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(self.scenario.line_delay_ms));
                }
            }
        };
        // Sondeo: sh -c <script> pxu-probe <refresh> [min_age]
        if argv.get(3) == Some(&"pxu-probe") {
            let refresh = argv.get(4) == Some(&"1");
            if let Some(f) = fail("probe") {
                if f.hang {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "mock: probe hung"));
                }
                return Ok(ExitInfo { code: f.exit });
            }
            let base = if refresh { "probe-refresh" } else { "probe-norefresh" };
            let out = self.exec_file(vmid, &format!("{base}.out")).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("mock: no probe output for CT {vmid}")))?;
            // Tras un upgrade correcto ya no quedan paquetes pendientes.
            let out: String = out.lines().filter(|l| !(done && l.starts_with("@@pkg "))).map(|l| format!("{l}\n")).collect();
            emit(sink, &out, false);
            let code = self.exec_file(vmid, &format!("{base}.exit")).and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            return Ok(ExitInfo { code });
        }
        let read = |name: &str| self.exec_file(vmid, name);
        match argv {
            ["df", ..] => {
                let out = read("df.out").unwrap_or_else(|| "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/loop0 4000000 1000000 3000000 25% /\n".into());
                emit(sink, &out, false);
                Ok(ExitInfo { code: 0 })
            }
            ["dpkg", "--audit"] => {
                emit(sink, &read("audit.out").unwrap_or_default(), false);
                Ok(ExitInfo { code: 0 })
            }
            ["systemctl", "--failed", ..] => {
                let name = if done { "failed-after.out" } else { "failed-before.out" };
                emit(sink, &read(name).unwrap_or_default(), false);
                Ok(ExitInfo { code: 0 })
            }
            _ if crate::share::update::is_upgrade_argv(argv) => {
                if let Some(f) = fail("upgrade") {
                    if f.hang {
                        // el host real no mata el upgrade: espera a que acabe y devuelve TimedOut
                        emit(sink, "Reading package lists...\nUnpacking packages ...", true);
                        self.upgraded.lock().unwrap().insert(vmid);
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out after 1800s (not killed; finished with exit 0)"));
                    }
                    emit(sink, "Reading package lists...\nE: Sub-process /usr/bin/dpkg returned an error code (1)", true);
                    return Ok(ExitInfo { code: if f.exit == 0 { 100 } else { f.exit } });
                }
                let out = read("upgrade.out").unwrap_or_else(|| "Reading package lists...\nCalculating upgrade... Done\nUnpacking packages ...\nSetting up packages ...\nProcessing triggers ...\n".into());
                emit(sink, &out, true);
                self.upgraded.lock().unwrap().insert(vmid);
                Ok(ExitInfo { code: 0 })
            }
            ["rm", "-f", p] if *p == crate::share::update::nvidia::CT_RUN_PATH => Ok(ExitInfo { code: 0 }),
            ["sh", p, "--no-kernel-modules", ..] if *p == crate::share::update::nvidia::CT_RUN_PATH => {
                let rc = read("nvidia-libs.exit").and_then(|c| c.trim().parse().ok()).unwrap_or(0);
                if rc == 0 && read("nvidia-libs.noeffect").is_none() {
                    // las librerías quedan en la versión del módulo del host
                    let v = crate::share::host::read_string(self, "/proc/driver/nvidia/version").ok().and_then(|t| crate::share::proxmox::hostcheck::parse_nvrm(&t)).unwrap_or_default();
                    let _ = self.write_file(Path::new(&format!("/exec/{vmid}/nvidia-smi.out")), format!("{v}\n").as_bytes());
                }
                emit(sink, if rc == 0 { "Installation complete." } else { "ERROR: installer failed." }, false);
                Ok(ExitInfo { code: rc })
            }
            argv if argv == crate::share::proxmox::hostcheck::NVIDIA_SMI => {
                let out = read("nvidia-smi.out").ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "mock: nvidia-smi: not found"))?;
                emit(sink, &out, false);
                Ok(ExitInfo { code: if out.to_ascii_lowercase().contains("mismatch") { 18 } else { 0 } })
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "mock: unsupported guest command")),
        }
    }
    fn host_cmd(&self, argv: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<ExitInfo> {
        self.log.lock().unwrap().push(Recorded::Host(argv.iter().map(|s| s.to_string()).collect()));
        if let Some(r) = self.nvidia_cmd(argv, s) {
            return r;
        }
        // curl simulado (Apprise): lee la config por stdin; falla si el destino es down.invalid.
        if argv.first() == Some(&"curl") {
            if o.stdin.as_deref().is_some_and(|d| String::from_utf8_lossy(d).contains("down.invalid")) {
                s.line("curl: (6) Could not resolve host: down.invalid");
                return Ok(ExitInfo { code: 6 });
            }
            s.line("{\"success\":true}");
            return Ok(ExitInfo { code: 0 });
        }
        let vmid: u32 = argv.iter().skip(1).find_map(|a| a.parse().ok()).unwrap_or(0);
        let ok = |s: &mut dyn LineSink, l: &str| {
            s.line(l);
            Ok(ExitInfo { code: 0 })
        };
        match argv {
            a if a == crate::share::storage::LVS => match &self.scenario.storage {
                Some(f) => ok(s, &f.lvs),
                None => Ok(ExitInfo { code: 5 }),
            },
            ["pvesh", "get", path, ..] if path.ends_with("/storage") => match &self.scenario.storage {
                Some(f) => ok(s, &f.list),
                None => Ok(ExitInfo { code: 2 }),
            },
            ["pvesh", "get", path, ..] if path.ends_with("/lxc") || path.ends_with("/qemu") => match &self.scenario.storage {
                Some(f) => ok(s, if path.ends_with("/lxc") { &f.lxc } else { &f.qemu }),
                None => Ok(ExitInfo { code: 2 }),
            },
            ["pvesh", "get", path, ..] if path.ends_with("/content") => match self.scenario.storage.as_ref().and_then(|f| f.content.get(path.split('/').nth(4).unwrap_or(""))) {
                Some(j) => {
                    let gone = self.deleted.lock().unwrap();
                    if gone.is_empty() {
                        return ok(s, j);
                    }
                    let mut v: serde_json::Value = serde_json::from_str(j).unwrap_or_default();
                    if let Some(a) = v.as_array_mut() {
                        a.retain(|c| !c.get("volid").and_then(|x| x.as_str()).is_some_and(|id| gone.contains(id)));
                    }
                    ok(s, &v.to_string())
                }
                None => Ok(ExitInfo { code: 2 }),
            },
            // borrar un backup desde Storage: solo lo quita de la lista en memoria (un volid que no existe falla como pvesm)
            ["pvesm", "free", volid] if self.scenario.storage.as_ref().is_some_and(|f| f.content.values().any(|j| j.contains(&format!("\"{volid}\"")))) => {
                self.deleted.lock().unwrap().insert(volid.to_string());
                ok(s, "OK")
            }
            ["pvesh", "get", path, ..] if path.contains("/storage/") => {
                let g = |v: Option<f64>, d: f64| (v.unwrap_or(d) * (1u64 << 30) as f64) as u64;
                let (avail, total) = (g(self.scenario.storage_avail_gb, 800.0), g(self.scenario.storage_total_gb, 1000.0));
                ok(s, &format!("{{\"active\":1,\"avail\":{avail},\"total\":{total},\"used\":{},\"enabled\":1}}", total.saturating_sub(avail)))
            }
            ["pvesh", "get", path, ..] => {
                // /nodes/<nodo>/lxc/<id>/feature: `fail step = "snapshot"` = storage/bind mounts que no admiten snapshots
                let id: u32 = path.split('/').nth(4).and_then(|v| v.parse().ok()).unwrap_or(0);
                let has = !self.scenario.fail.iter().any(|f| f.vmid == id && (f.step == "snapshot" || f.step == "backup"));
                ok(s, &format!("{{\"hasFeature\":{}}}", has as u8))
            }
            ["pct", "snapshot", ..] | ["vzdump", ..] => {
                // `snapshot`/`vzdump` fallan solo ese comando (p. ej. storage sin snapshots); `backup` falla los dos.
                let own = if argv[0] == "vzdump" { "vzdump" } else { "snapshot" };
                if let Some(f) = self.scenario.fail.iter().find(|f| f.vmid == vmid && (f.step == "backup" || f.step == own) && !(f.hang && own == "vzdump")) {
                    s.line(if own == "snapshot" { "ERROR: storage does not support snapshots" } else { "ERROR: vzdump failed" });
                    return Ok(ExitInfo { code: if f.exit == 0 { 1 } else { f.exit } });
                }
                if argv[0] == "vzdump" {
                    s.line(&format!("INFO: creating vzdump archive '/var/lib/vz/dump/vzdump-lxc-{vmid}-2026_11_08-03_16_00.tar.zst'"));
                    // vzdump «largo»: espera a que lo cancelen y acaba como el real tras SIGTERM (Interrupted); sin `cancel` nunca acaba (TimedOut)
                    if self.scenario.fail.iter().any(|f| f.vmid == vmid && f.step == "vzdump" && f.hang) {
                        let c = o.cancel.ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "mock: vzdump hung"))?;
                        for _ in 0..500 {
                            if c.load(std::sync::atomic::Ordering::SeqCst) {
                                s.line("ERROR: Backup of VM interrupted by signal");
                                return Err(io::Error::new(io::ErrorKind::Interrupted, crate::share::host::CANCELLED_MSG));
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "mock: vzdump never cancelled"));
                    }
                }
                ok(s, "INFO: backup finished")
            }
            ["pct", "rollback" | "restore", ..] => {
                if let Some(f) = self.scenario.fail.iter().find(|f| f.vmid == vmid && f.step == "rollback") {
                    s.line("ERROR: unable to rollback");
                    return Ok(ExitInfo { code: if f.exit == 0 { 1 } else { f.exit } });
                }
                // el CT vuelve al estado del backup: el upgrade deshecho
                self.upgraded.lock().unwrap().remove(&vmid);
                ok(s, "OK")
            }
            // Como PVE 9.2: `stop`/`shutdown` sobre un CT detenido salen con 255 ("CT N not running"); `shutdown` deja el CT detenido.
            ["pct", op @ ("stop" | "shutdown"), ..] => {
                if !self.ct_running(vmid) {
                    s.line(&format!("CT {vmid} not running"));
                    return Ok(ExitInfo { code: 255 });
                }
                let _ = op;
                self.set_ct_running(vmid, false);
                ok(s, "OK")
            }
            ["pct", "start", ..] => {
                self.set_ct_running(vmid, true);
                ok(s, "OK")
            }
            ["pct", "delsnapshot" | "reboot", ..] | ["pvesm", "free", ..] => ok(s, "OK"),
            // Revisión del host: la salida sale de /exec/host/<nombre>.out (ausente = el comando falla como si no existiera).
            _ if crate::share::update::hostupdate::is_host_update_argv(argv) => self.host_update_cmd(argv, s),
            _ if host_review_file(argv).is_some() => {
                let mut name = host_review_file(argv).unwrap_or_default();
                // tras instalar, `apt list`/simulación salen de `*-after.out` (ausente = ya no hay pendientes)
                if self.host_installed.load(std::sync::atomic::Ordering::SeqCst) && (name == "apt-list.out" || name == "apt-sim.out") {
                    name = if name == "apt-list.out" { "apt-list-after.out" } else { "apt-sim-after.out" };
                    if self.exec_file(0, name).is_none() {
                        s.line("Listing...");
                        return Ok(ExitInfo { code: 0 });
                    }
                }
                if self.host_backed_up.load(std::sync::atomic::Ordering::SeqCst) && (name == "apt-list.out" || name == "apt-sim.out") {
                    let late = if name == "apt-list.out" { "apt-list-late.out" } else { "apt-sim-late.out" };
                    if self.exec_file(0, late).is_some() {
                        name = late;
                    }
                }
                // `apt-get update` simulado no necesita archivo; el resto falla (NotFound) si el caso no lo define.
                let text = match self.exec_file(0, name) {
                    Some(t) => t,
                    None if argv == ["apt-get", "update"] => "Reading package lists... Done\n".into(),
                    None => return Err(io::Error::new(io::ErrorKind::NotFound, format!("mock: {} not found", argv[0]))),
                };
                for l in text.lines() {
                    s.line(l);
                }
                let code = self.exec_file(0, &format!("{name}.exit")).and_then(|c| c.trim().parse().ok()).unwrap_or(0);
                Ok(ExitInfo { code })
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "mock: unsupported host command")),
        }
    }
    fn write_file(&self, p: &Path, data: &[u8]) -> io::Result<()> {
        self.overlay.lock().unwrap().insert(norm(p), Some(data.to_vec()));
        Ok(())
    }
    fn append_file(&self, p: &Path, data: &[u8]) -> io::Result<()> {
        let mut o = self.overlay.lock().unwrap();
        let cur = o.entry(norm(p)).or_insert(None);
        cur.get_or_insert_with(Vec::new).extend_from_slice(data);
        Ok(())
    }
    fn write_config(&self, data: &[u8]) -> io::Result<()> {
        self.write_file(Path::new(crate::share::config::CONFIG_PATH), data)
    }
    fn timer_status(&self) -> io::Result<crate::share::schedule::TimerStatus> {
        let t = self.timer.lock().unwrap();
        let calendars = if t.installed && !t.calendar.is_empty() { vec![t.calendar.clone()] } else { vec![] };
        let next = (t.installed && t.enabled).then(|| "Tue 2026-11-10 04:35:12 -03".to_string());
        Ok(crate::share::schedule::TimerStatus { installed: t.installed, enabled: t.enabled, active: if t.enabled { "active" } else { "inactive" }.into(), calendars, next })
    }
    fn validate_calendar(&self, expr: &str) -> io::Result<crate::share::schedule::CalendarCheck> {
        let expr = crate::share::schedule::check_expr(expr).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("invalid schedule \"{expr}\": Failed to parse calendar specification (mock)"));
        let mut time = "00:00:00".to_string();
        for tok in expr.split_whitespace() {
            let is_day = |d: &str| ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].contains(&d);
            let ok = if tok.contains(':') {
                let mut p = tok.split(':');
                let (h, m) = (p.next().and_then(|x| x.parse::<u32>().ok()), p.next().and_then(|x| x.parse::<u32>().ok()));
                let good = matches!((h, m), (Some(h), Some(m)) if h < 24 && m < 60) && p.next().is_none_or(|s| s.parse::<u32>().is_ok_and(|s| s < 60));
                if good {
                    time = format!("{:02}:{:02}:00", h.unwrap(), m.unwrap());
                }
                good
            } else {
                tok == "*-*-*" || ["daily", "weekly", "hourly", "monthly"].contains(&tok) || tok.split([',', '.']).all(|d| d.is_empty() || is_day(d))
            };
            if !ok {
                return Err(bad());
            }
        }
        Ok(crate::share::schedule::CalendarCheck { normalized: expr.to_string(), next: Some(format!("Tue 2026-11-10 {time} -03")) })
    }
    fn apply_schedule(&self, plan: &crate::share::schedule::SchedulePlan) -> io::Result<()> {
        use crate::share::schedule::*;
        let mut t = self.timer.lock().unwrap();
        if plan.install_units {
            t.installed = true;
            t.ops.push("write units".into());
        }
        if !t.installed {
            return Err(io::Error::new(io::ErrorKind::NotFound, "pxu-update.timer is not installed"));
        }
        if let Some(e) = &plan.on_calendar {
            let e = check_expr(e).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            t.dropin = Some(dropin_text(e));
            t.calendar = e.to_string();
            t.ops.push("write drop-in".into());
        }
        t.ops.push("daemon-reload".into());
        t.ops.push(if plan.enable { "enable --now" } else { "disable --now" }.into());
        t.enabled = plan.enable;
        Ok(())
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        if !self.exists(p) {
            return Err(io::ErrorKind::NotFound.into());
        }
        self.overlay.lock().unwrap().insert(norm(p), None);
        Ok(())
    }
    fn remove_empty_dir(&self, _p: &Path) -> io::Result<()> {
        Ok(())
    }
    fn lock_dir(&self) -> &Path {
        &self.lock_dir
    }
    fn try_lock(&self, path: &Path, create: bool) -> io::Result<Option<LockGuard>> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(id) = name.strip_prefix("proxmenux-lxc-update-").and_then(|n| n.strip_suffix(".lock")) {
            let held = id.parse::<u32>().is_ok_and(|id| self.scenario.locks_held.iter().any(|l| l.vmid == id && l.by == "proxmenux"));
            return match (held, create) {
                (true, _) => Ok(None),
                (false, false) if self.exists(path) => Ok(Some(Box::new(()))),
                (false, false) => Err(io::ErrorKind::NotFound.into()),
                (false, true) => Ok(Some(Box::new(()))),
            };
        }
        if !self.locks.lock().unwrap().insert(path.to_path_buf()) {
            return Ok(None);
        }
        Ok(Some(Box::new(MockLock(self.locks.clone(), path.to_path_buf()))))
    }
    fn file_meta(&self, path: &Path) -> io::Result<FileMeta> {
        if path == Path::new(crate::share::update::hostupdate::BACKUP_DIR) {
            // `/exec/0/backup-dir.meta`: «<modo octal> <uid> <dir|file|symlink>» (por defecto «0700 0 dir»)
            let t = self.exec_file(0, "backup-dir.meta").unwrap_or_else(|| "0700 0 dir".into());
            let f: Vec<&str> = t.split_whitespace().collect();
            if f.get(2) == Some(&"symlink") {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "is a symlink"));
            }
            return Ok(FileMeta { mode: u32::from_str_radix(f.first().unwrap_or(&"0700"), 8).unwrap_or(0o700), uid: f.get(1).and_then(|u| u.parse().ok()).unwrap_or(0), is_dir: f.get(2) != Some(&"file") });
        }
        if !self.exists(path) {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(FileMeta { mode: self.scenario.config_mode.unwrap_or(0o600), uid: self.scenario.config_uid.unwrap_or(0), is_dir: false })
    }
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(self.scenario.now_epoch + self.clock_offset.load(std::sync::atomic::Ordering::SeqCst))
    }
    fn is_mock(&self) -> bool {
        true
    }
}
