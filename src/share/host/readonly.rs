//! `ReadOnlyHost`: envoltorio que solo deja pasar comandos de una lista blanca de lectura.
//! Todo lo demás se rechaza con `PermissionDenied` *sin* llegar al host interior.
use super::*;
use crate::share::proxmox::hostcheck;
use crate::share::update::hostupdate;
use crate::share::update::nvidia;
use crate::share::update::runlog;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

pub struct ReadOnlyHost<H: Host> {
    inner: H,
    allow_refresh: bool,
    /// Opt-in explícito de `update` (`set_update_enabled`): abre la lista blanca de escrituras de update, nada más.
    allow_update: AtomicBool,
    /// CT en curso (`set_update_target`; 0 = ninguno): las escrituras de update solo valen para este vmid. Sin objetivo, se deniegan.
    update_target: AtomicU32,
    /// Opt-in explícito de `update --host` (`set_host_update_enabled`): abre SOLO la lista blanca de `hostupdate`. Independiente de `allow_update`.
    allow_host_update: AtomicBool,
    /// Opt-in explícito de la TUI (`set_config_edit`): permite `write_config`, nada más.
    allow_config: AtomicBool,
    /// Opt-in explícito de la TUI (`set_schedule_edit`): permite `apply_schedule`, nada más.
    allow_schedule: AtomicBool,
    /// Opt-in explícito del paso NVIDIA (`set_nvidia_update`): solo los comandos exactos de ese permiso.
    nvidia: Mutex<Option<nvidia::Grant>>,
    /// Opt-in explícito de la página Storage (`set_backup_delete`): solo `pvesm free <este volid>`, nada más.
    backup_delete: Mutex<Option<String>>,
}

impl<H: Host> ReadOnlyHost<H> {
    pub fn new(inner: H) -> Self {
        ReadOnlyHost { inner, allow_refresh: false, allow_update: AtomicBool::new(false), update_target: AtomicU32::new(0), allow_config: AtomicBool::new(false), allow_schedule: AtomicBool::new(false), allow_host_update: AtomicBool::new(false), nvidia: Mutex::new(None), backup_delete: Mutex::new(None) }
    }
    /// Permite además el sondeo CON refresco de índices (`pxu check --refresh`): escribe la caché del guest.
    pub fn allowing_refresh(inner: H) -> Self {
        ReadOnlyHost { inner, allow_refresh: true, allow_update: AtomicBool::new(false), update_target: AtomicU32::new(0), allow_config: AtomicBool::new(false), allow_schedule: AtomicBool::new(false), allow_host_update: AtomicBool::new(false), nvidia: Mutex::new(None), backup_delete: Mutex::new(None) }
    }
    fn update_on(&self) -> bool {
        self.allow_update.load(Ordering::SeqCst)
    }
    /// El CT en curso, si el opt-in de update está activo y hay objetivo.
    fn target(&self) -> Option<u32> {
        Some(self.update_target.load(Ordering::SeqCst)).filter(|&t| t != 0 && self.update_on())
    }
    fn host_update_on(&self) -> bool {
        self.allow_host_update.load(Ordering::SeqCst)
    }
    fn backup_delete_allows(&self, argv: &[&str]) -> bool {
        matches!((argv, self.backup_delete.lock().unwrap().as_deref()), (["pvesm", "free", v], Some(g)) if *v == g)
    }
    fn nvidia_allows(&self, f: impl Fn(&nvidia::Grant) -> bool) -> bool {
        self.nvidia.lock().unwrap().as_ref().is_some_and(f)
    }
    pub fn inner(&self) -> &H {
        &self.inner
    }
}

/// Comandos del host permitidos en solo lectura: el envío de avisos (`curl`/`apprise` con la configuración
/// por stdin; ningún secreto en argv) y la consulta `pvesh get …/feature --feature snapshot`. Las demás lecturas de PVE
/// se hacen leyendo ficheros (`/etc/pve`, cgroup), así que no hay otro `pvesh`/`pct`/`qm` aquí.
pub fn host_cmd_is_read_only(argv: &[&str]) -> bool {
    match argv {
        ["curl", "--config", "-"] => true,
        // revisión del host (fase 1): solo lecturas y simulaciones; nunca instala (`-s`)
        a if a == hostcheck::APT_LIST || a == hostcheck::APT_SIM || a == hostcheck::LSPCI || a == hostcheck::DKMS => true,
        // versión instalada de Proxmox VE (solo lectura de la base de dpkg)
        a if a == crate::share::proxmox::guests::PVE_VERSION => true,
        // índice oficial de NVIDIA (solo lectura): decide si la TUI ofrece actualizar el driver
        a if a == nvidia::INDEX_CURL => true,
        ["apprise", "--config", "/dev/stdin", "-t", _, "-b", _, "-n", _] => true,
        // ¿admite el CT snapshots? Consulta pura de PVE (sin efectos): `{"hasFeature":0|1}`
        ["pvesh", "get", path, "--feature", "snapshot", "--output-format", "json"] => feature_path(path),
        // espacio libre del storage donde irá un vzdump (`avail`/`total`): consulta pura
        ["pvesh", "get", path, "--output-format", "json"] => storage_status_path(path),
        // página Storage: `lvs` con argumentos fijos (uso real de los discos thin)
        a if a == crate::share::storage::LVS => true,
        _ => false,
    }
}

/// `apt-get update` del host: solo con el refresco permitido (`pxu check --refresh`, la TUI o `check.refresh`). Escribe únicamente el índice de apt.
pub fn host_cmd_is_refresh(argv: &[&str]) -> bool {
    argv == hostcheck::APT_UPDATE
}

/// `/nodes/<nodo>/lxc/<vmid>/feature`, con nodo y vmid validados.
fn feature_path(p: &str) -> bool {
    let parts: Vec<&str> = p.split('/').collect();
    matches!(parts.as_slice(), ["", "nodes", node, "lxc", id, "feature"] if crate::share::update::backup::is_pve_id(node) && numeric(id))
}

/// `/nodes/<nodo>/storage`, `/nodes/<nodo>/lxc` y `/qemu` (`disk`/`maxdisk` por guest), `…/storage/<storage>/status` y `…/storage/<storage>/content` (página Storage), con nodo y storage validados.
fn storage_status_path(p: &str) -> bool {
    use crate::share::update::backup::is_pve_id;
    let parts: Vec<&str> = p.split('/').collect();
    match parts.as_slice() {
        ["", "nodes", node, "storage" | "lxc" | "qemu"] => is_pve_id(node),
        ["", "nodes", node, "storage", st, "status" | "content"] => is_pve_id(node) && is_pve_id(st),
        _ => false,
    }
}

/// Dentro de un CT solo se permite el script de sondeo embebido (nunca otro `sh -c`):
/// `sh -c <PROBE_SH> pxu-probe 0`; con `allow_refresh` también `… pxu-probe 1 <min_age_s>`.
pub fn guest_cmd_is_read_only(argv: &[&str], allow_refresh: bool) -> bool {
    match argv {
        ["sh", "-c", script, "pxu-probe", "0"] => *script == crate::share::update::check::PROBE_SH,
        ["sh", "-c", script, "pxu-probe", "1", age] => allow_refresh && *script == crate::share::update::check::PROBE_SH && age.parse::<u64>().is_ok(),
        // versión del driver NVIDIA que ve un CT en marcha (solo lectura)
        a if a == hostcheck::NVIDIA_SMI => true,
        _ => false,
    }
}

/// vmid canónico: dígitos sin ceros a la izquierda y sin `0`.
fn numeric(s: &str) -> bool {
    !s.is_empty() && s.len() <= 9 && !s.starts_with('0') && s.bytes().all(|b| b.is_ascii_digit())
}

fn safe_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Snapshot propio de pxu: `pxu-<…>` (los demás nunca se tocan).
pub(crate) fn pxu_snapshot(name: &str) -> bool {
    name.starts_with(crate::share::update::backup::SNAP_PREFIX) && safe_name(name)
}

/// Volumen vzdump de un CT: `<storage>:backup/vzdump-lxc-<vmid>-<…>`. Si es de pxu lo decide el registro del motor.
pub(crate) fn vzdump_volid(volid: &str, vmid: Option<&str>) -> bool {
    let Some((st, rest)) = volid.split_once(':') else { return false };
    let Some(file) = rest.strip_prefix("backup/vzdump-lxc-") else { return false };
    let (id, tail) = file.split_once('-').unwrap_or(("", ""));
    crate::share::update::backup::is_pve_id(st) && numeric(id) && vmid.is_none_or(|v| v == id) && !tail.is_empty() && file.len() <= 128 && file.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// Escrituras del host que solo `update` puede lanzar, ATADAS al CT en curso (`vmid`): backup (snapshot/vzdump), su rollback/borrado,
/// arranque/apagado/reinicio del CT. Un argv con otro vmid (o con un volid de otro CT) se rechaza.
pub fn host_cmd_is_update(argv: &[&str], vmid: u32) -> bool {
    let me = vmid.to_string();
    let mine = |id: &&str| *id == me && numeric(id);
    match argv {
        ["pct", "snapshot" | "delsnapshot" | "rollback", id, name] => mine(id) && pxu_snapshot(name),
        // `--storage` obligatorio: sin él PVE restaura en `local` (que no admite rootdir)
        ["pct", "restore", id, volid, "--force", "--storage", st] => mine(id) && vzdump_volid(volid, Some(&me)) && crate::share::update::backup::is_pve_id(st),
        ["pvesm", "free", volid] => vzdump_volid(volid, Some(&me)),
        ["pct", "start" | "reboot" | "stop", id] => mine(id),
        ["pct", "shutdown", id, "--timeout", "60"] => mine(id),
        ["vzdump", id, "--mode", "snapshot", "--storage", st, "--compress", "zstd", "--zstd", "1", "--ionice", "7", "--notes-template", "pxu pre-update {{guestname}}"] => mine(id) && crate::share::update::backup::is_pve_id(st),
        _ => false,
    }
}

/// Dentro del CT: los upgrades constantes del binario y las verificaciones (`crate::share::update`), más el sondeo con refresco.
pub fn guest_cmd_is_update(argv: &[&str]) -> bool {
    crate::share::update::is_upgrade_argv(argv) || crate::share::update::is_verify_argv(argv) || guest_cmd_is_read_only(argv, true)
}

/// Locks propios (`pxu-*.lock`, con creación) y el de ProxMenux solo si ya existe (sin `O_CREAT`).
fn lock_path_ok(dir: &Path, p: &Path, create: bool) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    p.parent() == Some(dir)
        && (name.starts_with("pxu-") && name.ends_with(".lock") || !create && name.starts_with("proxmenux-lxc-update-") && name.ends_with(".lock"))
        && !name.contains('/')
}

/// pxu solo escribe su propio estado: `/var/lib/pxu/**` (sin `..`).
fn own_state_path(p: &Path, prefix: &str) -> bool {
    p.starts_with(prefix) && !p.components().any(|c| c == std::path::Component::ParentDir)
}

fn denied(what: &str, argv: &[&str]) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("read-only mode: refused {what}: {}", argv.first().copied().unwrap_or("")))
}

impl<H: Host> Host for ReadOnlyHost<H> {
    fn read_file(&self, p: &Path) -> io::Result<Vec<u8>> {
        self.inner.read_file(p)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
        self.inner.read_dir(p)
    }
    fn exists(&self, p: &Path) -> bool {
        self.inner.exists(p)
    }
    fn guest_exec(&self, vmid: u32, argv: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<ExitInfo> {
        if !guest_cmd_is_read_only(argv, self.allow_refresh) && !(self.target() == Some(vmid) && guest_cmd_is_update(argv)) && !self.nvidia_allows(|g| nvidia::is_guest_argv(argv, vmid, g)) {
            return Err(denied("guest command", argv));
        }
        self.inner.guest_exec(vmid, argv, o, s)
    }
    fn host_cmd(&self, argv: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<ExitInfo> {
        if !host_cmd_is_read_only(argv) && !(self.allow_refresh && host_cmd_is_refresh(argv)) && !self.target().is_some_and(|t| host_cmd_is_update(argv, t)) && !(self.host_update_on() && hostupdate::is_host_update_argv(argv)) && !self.nvidia_allows(|g| nvidia::is_host_argv(argv, g)) && !self.backup_delete_allows(argv) {
            return Err(denied("host command", argv));
        }
        self.inner.host_cmd(argv, o, s)
    }
    fn write_file(&self, p: &Path, data: &[u8]) -> io::Result<()> {
        if !own_state_path(p, "/var/lib/pxu") && !(self.update_on() && own_state_path(p, "/var/log/pxu")) && !((self.update_on() || self.host_update_on() || self.nvidia_allows(|_| true)) && runlog::is_run_log_path(p)) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("read-only mode: refused write to {}", p.display())));
        }
        self.inner.write_file(p, data)
    }
    fn write_config(&self, data: &[u8]) -> io::Result<()> {
        if !self.allow_config.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only mode: refused write to the config"));
        }
        self.inner.write_config(data)
    }
    fn timer_status(&self) -> io::Result<crate::share::schedule::TimerStatus> {
        self.inner.timer_status()
    }
    fn validate_calendar(&self, expr: &str) -> io::Result<crate::share::schedule::CalendarCheck> {
        self.inner.validate_calendar(expr)
    }
    fn apply_schedule(&self, plan: &crate::share::schedule::SchedulePlan) -> io::Result<()> {
        if !self.allow_schedule.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only mode: refused to change the schedule"));
        }
        self.inner.apply_schedule(plan)
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        if !own_state_path(p, "/var/lib/pxu/outbox") && !(self.update_on() && own_state_path(p, "/var/log/pxu")) && !(self.host_update_on() && (runlog::is_pxu_log_path(p) || hostupdate::is_backup_path(p))) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("read-only mode: refused delete of {}", p.display())));
        }
        self.inner.remove_file(p)
    }
    fn remove_empty_dir(&self, p: &Path) -> io::Result<()> {
        if !((self.update_on() || self.host_update_on()) && runlog::is_legacy_ct_dir(p)) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("read-only mode: refused delete of {}", p.display())));
        }
        self.inner.remove_empty_dir(p)
    }
    fn append_file(&self, p: &Path, data: &[u8]) -> io::Result<()> {
        // Lo único que pxu añade es el log en vivo de la corrida (`run-….log`, ya creado con su cabecera), con el mismo opt-in que escribirlo.
        if !((self.update_on() || self.host_update_on() || self.nvidia_allows(|_| true)) && runlog::is_run_log_path(p)) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("read-only mode: refused append to {}", p.display())));
        }
        self.inner.append_file(p, data)
    }
    fn lock_dir(&self) -> &Path {
        self.inner.lock_dir()
    }
    fn try_lock(&self, p: &Path, create: bool) -> io::Result<Option<LockGuard>> {
        // La cola de avisos se vacía también en solo lectura (`notify-flush`, `check --notify`): su lock no necesita opt-in.
        let outbox = create && p == self.inner.lock_dir().join("pxu-outbox.lock");
        if !(outbox || self.update_on() || self.host_update_on()) || !lock_path_ok(self.inner.lock_dir(), p, create) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("read-only mode: refused lock {}", p.display())));
        }
        self.inner.try_lock(p, create)
    }
    fn file_meta(&self, p: &Path) -> io::Result<FileMeta> {
        self.inner.file_meta(p)
    }
    fn set_update_enabled(&self, on: bool) {
        self.allow_update.store(on, Ordering::SeqCst);
        if !on {
            self.update_target.store(0, Ordering::SeqCst);
        }
    }
    fn set_nvidia_update(&self, g: Option<nvidia::Grant>) {
        *self.nvidia.lock().unwrap() = g;
    }
    fn set_backup_delete(&self, volid: Option<String>) {
        *self.backup_delete.lock().unwrap() = volid;
    }
    fn set_host_update_enabled(&self, on: bool) {
        self.allow_host_update.store(on, Ordering::SeqCst);
    }
    fn set_config_edit(&self, on: bool) {
        self.allow_config.store(on, Ordering::SeqCst);
    }
    fn set_schedule_edit(&self, on: bool) {
        self.allow_schedule.store(on, Ordering::SeqCst);
    }
    fn set_update_target(&self, vmid: Option<u32>) {
        self.update_target.store(vmid.unwrap_or(0), Ordering::SeqCst);
    }
    fn now(&self) -> SystemTime {
        self.inner.now()
    }
    fn is_mock(&self) -> bool {
        self.inner.is_mock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::mock::scenario::World;
    use crate::share::mock::{MockHost, Recorded};

    fn ro() -> ReadOnlyHost<MockHost> {
        ReadOnlyHost::new(World::demo().build())
    }

    #[test]
    fn listing_in_read_only_mode_issues_no_commands_at_all() {
        let h = ro();
        let inv = crate::share::proxmox::guests::load(&h);
        assert_eq!(inv.guests.len(), 37);
        assert!(h.inner().recorded().is_empty());
    }

    #[test]
    fn write_commands_never_reach_the_inner_host() {
        let h = ro();
        let o = ExecOpts::default();
        let mut sink: Vec<String> = Vec::new();
        let host_writes: &[&[&str]] = &[
            &["vzdump", "103", "--mode", "snapshot"],
            &["pct", "snapshot", "103", "pxu_pre"],
            &["pct", "rollback", "103", "pxu-202611080316"], // sin opt-in de update
            &["pct", "delsnapshot", "103", "pxu-202611080316"],
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-x.tar.zst", "--force", "--storage", "local-lvm"],
            &["pvesm", "free", "local:backup/vzdump-lxc-103-x.tar.zst"],
            &["pct", "stop", "103"],
            &["pct", "set", "103", "--onboot", "0"],
            &["pct", "exec", "103", "--", "apt-get", "update"],
            &["lxc-attach", "-n", "103", "--", "apt-get", "update"],
            &["lxc-attach", "-n", "103", "--clear-env", "--set-var", "PATH=/bin", "--", "sh", "-c", "rm -rf /"],
            &["pvesh", "create", "/nodes/server/lxc"],
            &["pvesh", "set", "/nodes/server/lxc/103/config"],
            &["pvesh", "delete", "/nodes/server/lxc/103"],
            &["qm", "stop", "200"],
            &["pvesh", "get", "/cluster/resources"], // L7: ya no está en la lista de lectura
            &["lxc-info", "-n", "103"],
            &["pct", "config", "103"],
            &["qm", "config", "200"],
            &["sh", "-c", "rm -rf /"],
            &["apt-get", "update"],
            &[],
        ];
        for argv in host_writes {
            let e = h.host_cmd(argv, &o, &mut sink).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{argv:?}");
        }
        let guest_writes: &[&[&str]] = &[
            &["apt-get", "update", "-qq"],
            &["apt-get", "-y", "dist-upgrade"],
            &["apk", "update"],
            &["apk", "upgrade", "--no-cache"],
            &["pacman", "-Sy"],
            &["dnf", "makecache"],
            &["zypper", "refresh"],
            &["sh", "-c", crate::share::update::check::PROBE_SH, "pxu-probe", "1", "0"], // sondeo CON refresco (no permitido por defecto)
            &["sh", "-c", "apt-get update", "pxu-probe", "0"], // script ajeno con el argumento "0"
            &["sh", "-c", "apt-get update"],
        ];
        for argv in guest_writes {
            let e = h.guest_exec(103, argv, &o, &mut sink).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{argv:?}");
        }
        assert!(h.inner().recorded().is_empty(), "inner host saw a command: {:?}", h.inner().recorded());
    }

    #[test]
    fn backup_commands_have_exact_shapes_even_with_the_update_opt_in() {
        let ok: &[&[&str]] = &[
            &["pct", "snapshot", "103", "pxu-202611080316"],
            &["pct", "delsnapshot", "103", "pxu-202611080316"],
            &["pct", "rollback", "103", "pxu-202611080316"],
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-2026_11_08-03_16_00.tar.zst", "--force", "--storage", "local-lvm"],
            &["pvesm", "free", "local:backup/vzdump-lxc-103-2026_11_08-03_16_00.tar.zst"],
            &["pct", "stop", "103"],
        ];
        for a in ok {
            assert!(host_cmd_is_update(a, 103), "{a:?}");
            assert!(!host_cmd_is_read_only(a), "{a:?} must stay blocked in read-only mode");
        }
        let bad: &[&[&str]] = &[
            &["pct", "rollback", "103", "manual-snap"],                 // snapshot ajeno
            &["pct", "delsnapshot", "103", "zfs_auto_1"],
            &["pct", "delsnapshot", "103", "pxu-x", "--force"],         // opciones extra
            &["pct", "rollback", "103", "pxu-1", "--start"],
            &["pct", "delsnapshot", "103", "pxu-../x"],
            &["pct", "delsnapshot", "10x", "pxu-1"],
            &["pct", "restore", "103", "local:backup/vzdump-lxc-104-x.tar.zst", "--force"], // otro CT
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-x.tar.zst"],            // sin --force: forma distinta
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-x.tar.zst", "--force"], // sin --storage: restauraría en `local`
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-x.tar.zst", "--force", "--storage", "-x"],
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-x.tar.zst", "--force", "--storage", "a;b"],
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-x.tar.zst", "--storage", "local-lvm", "--force"], // otro orden
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-x.tar.zst", "--force", "--storage", "local-lvm", "--unprivileged"],
            &["pct", "restore", "104", "local:backup/vzdump-lxc-103-x.tar.zst", "--force", "--storage", "local-lvm"], // backup de otro CT
            &["pct", "restore", "103", "/tmp/evil.tar", "--force"],
            &["pct", "restore", "103", "local:iso/debian.iso", "--force"],
            &["pvesm", "free", "local:iso/debian.iso"],                  // no es un vzdump
            &["pvesm", "free", "local:backup/vzdump-qemu-103-x.vma.zst"], // VM: no
            &["pvesm", "free", "local:backup/vzdump-lxc-103-x.tar.zst", "--storage", "x"],
            &["pvesm", "free", "local:vm-100-disk-0"],
            &["pvesm", "free", "local:backup/vzdump-lxc-103-x;rm.tar"],
            &["pvesm", "alloc", "local", "103", "x", "1G"],
            &["pct", "destroy", "103"],
            &["pct", "stop", "103", "--skiplock"],
        ];
        for a in bad {
            assert!(!host_cmd_is_update(a, 103), "{a:?}");
        }
        // el opt-in abre exactamente la lista de update, y solo para el CT en curso
        let h = ro();
        h.set_update_enabled(true);
        h.set_update_target(Some(103));
        let mut sink = Vec::<String>::new();
        assert!(h.host_cmd(&["pct", "delsnapshot", "103", "pxu-202611080316"], &ExecOpts::default(), &mut sink).is_ok());
        assert_eq!(h.host_cmd(&["pct", "delsnapshot", "103", "mine"], &ExecOpts::default(), &mut sink).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(h.host_cmd(&["pvesm", "free", "local:backup/dump.tar"], &ExecOpts::default(), &mut sink).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn the_backup_delete_opt_in_opens_exactly_one_pvesm_free() {
        let h = ro();
        let o = ExecOpts::default();
        let mut sink = Vec::<String>::new();
        let v = "remote-backup-3:backup/ct/131/2026-03-28T11:50:10Z";
        assert_eq!(h.host_cmd(&["pvesm", "free", v], &o, &mut sink).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        {
            let _g = BackupDeleteOptIn::new(&h, v);
            assert!(h.host_cmd(&["pvesm", "free", v], &o, &mut sink).is_ok());
            for bad in [&["pvesm", "free", "remote-backup-3:backup/ct/132/2026-03-28T11:50:10Z"][..], &["pvesm", "free", v, "--force"], &["pvesm", "alloc", v], &["pct", "destroy", "131"]] {
                assert_eq!(h.host_cmd(bad, &o, &mut sink).unwrap_err().kind(), io::ErrorKind::PermissionDenied, "{bad:?}");
            }
        }
        assert_eq!(h.host_cmd(&["pvesm", "free", v], &o, &mut sink).unwrap_err().kind(), io::ErrorKind::PermissionDenied, "the opt-in is withdrawn on drop");
    }

    #[test]
    fn update_writes_are_bound_to_the_ct_in_progress() {
        // mismos argv válidos, pero para otro CT (vmid activo = 104): todos rechazados
        let other: &[&[&str]] = &[
            &["pct", "snapshot", "103", "pxu-202611080316"],
            &["pct", "delsnapshot", "103", "pxu-202611080316"],
            &["pct", "rollback", "103", "pxu-202611080316"],
            &["pct", "restore", "103", "local:backup/vzdump-lxc-103-x.tar.zst", "--force", "--storage", "local-lvm"],
            &["pvesm", "free", "local:backup/vzdump-lxc-103-x.tar.zst"], // dump de otro CT
            &["pct", "start", "103"],
            &["pct", "stop", "103"],
            &["pct", "shutdown", "103", "--timeout", "60"],
            &["pct", "reboot", "103"],
            &["vzdump", "103", "--mode", "snapshot", "--storage", "local", "--compress", "zstd", "--zstd", "1", "--ionice", "7", "--notes-template", "pxu pre-update {{guestname}}"],
        ];
        for a in other {
            assert!(host_cmd_is_update(a, 103), "{a:?} valid for its own CT");
            assert!(!host_cmd_is_update(a, 104), "{a:?} must be refused for CT 104");
        }
        // un dump de 104 con `pct restore` sobre 103, y `pvesm free` de dump ajeno
        assert!(!host_cmd_is_update(&["pct", "restore", "103", "local:backup/vzdump-lxc-104-x.tar.zst", "--force", "--storage", "local-lvm"], 103));
        assert!(!host_cmd_is_update(&["pvesm", "free", "local:backup/vzdump-lxc-104-x.tar.zst"], 103));
        // vmid con ceros a la izquierda o 0
        assert!(!host_cmd_is_update(&["pct", "stop", "0103"], 103));
        assert!(!host_cmd_is_update(&["pct", "stop", "0"], 0));
        // vzdump: storage validado como identificador PVE (rechaza `..`)
        let vz = |st: &str| host_cmd_is_update(&["vzdump", "103", "--mode", "snapshot", "--storage", st, "--compress", "zstd", "--zstd", "1", "--ionice", "7", "--notes-template", "pxu pre-update {{guestname}}"], 103);
        assert!(vz("local") && !vz("..") && !vz("a/b") && !vz("-x"));
        // a través del host: sin objetivo nada; con objetivo, solo ese CT (comandos de guest incluidos)
        let h = ro();
        h.set_update_enabled(true);
        let (o, mut sink) = (ExecOpts::default(), Vec::<String>::new());
        assert!(h.host_cmd(&["pct", "start", "103"], &o, &mut sink).is_err(), "enabled without target ⇒ denied");
        let up = crate::share::update::upgrade_argv("apt", "dist-upgrade").unwrap();
        assert!(h.guest_exec(103, &up, &o, &mut sink).is_err());
        h.set_update_target(Some(104));
        assert!(h.host_cmd(&["pct", "start", "103"], &o, &mut sink).is_err());
        assert!(h.host_cmd(&["pvesm", "free", "local:backup/vzdump-lxc-103-x.tar.zst"], &o, &mut sink).is_err());
        assert!(h.guest_exec(103, &up, &o, &mut sink).is_err(), "upgrade/refresh probe of another CT refused");
        assert!(h.guest_exec(103, &["sh", "-c", crate::share::update::check::PROBE_SH, "pxu-probe", "1", "0"], &o, &mut sink).is_err(), "refresh probe of another CT refused");
        assert!(h.host_cmd(&["pct", "start", "104"], &o, &mut sink).is_ok());
        assert!(h.guest_exec(104, &up, &o, &mut sink).is_ok());
        h.set_update_enabled(false);
        assert!(h.host_cmd(&["pct", "start", "104"], &o, &mut sink).is_err(), "opt-in withdrawn also clears the target");
    }

    #[test]
    fn snapshot_feature_query_is_read_only_with_exact_argv() {
        let q = |p: &str| host_cmd_is_read_only(&["pvesh", "get", p, "--feature", "snapshot", "--output-format", "json"]);
        assert!(q("/nodes/server/lxc/121/feature"));
        assert!(!q("/nodes/server/lxc/121/config"));
        assert!(!q("/nodes/server/lxc/12x/feature"));
        assert!(!q("/nodes/se rver/lxc/121/feature"));
        assert!(!q("/nodes/../lxc/121/feature"));
        assert!(!q("/nodes/server/lxc/121/feature/x"));
        assert!(!q("/nodes/server/qemu/121/feature"));
        assert!(!host_cmd_is_read_only(&["pvesh", "get", "/nodes/server/lxc/121/feature", "--feature", "snapshot"]));
        assert!(!host_cmd_is_read_only(&["pvesh", "get", "/nodes/server/lxc/121/feature", "--feature", "clone", "--output-format", "json"]));
        assert!(!host_cmd_is_read_only(&["pvesh", "create", "/nodes/server/lxc/121/feature", "--feature", "snapshot", "--output-format", "json"]));
        // pasa el filtro sin opt-in de update
        let h = ro();
        let mut sink = Vec::<String>::new();
        let _ = h.host_cmd(&["pvesh", "get", "/nodes/server/lxc/103/feature", "--feature", "snapshot", "--output-format", "json"], &ExecOpts::default(), &mut sink);
        assert_eq!(h.inner().recorded().len(), 1);
    }

    #[test]
    fn storage_page_reads_are_exact_and_validated() {
        let q = |p: &str| host_cmd_is_read_only(&["pvesh", "get", p, "--output-format", "json"]);
        assert!(q("/nodes/server/storage"));
        assert!(q("/nodes/server/storage/local-lvm/content"));
        assert!(q("/nodes/server/storage/local-lvm/status"));
        // disk/maxdisk por guest
        assert!(q("/nodes/server/lxc") && q("/nodes/server/qemu"));
        assert!(!q("/nodes/se rver/lxc") && !q("/nodes/../qemu") && !q("/nodes/server/qemu/100") && !q("/nodes/server/lxc/"));
        assert!(!host_cmd_is_read_only(&["pvesh", "create", "/nodes/server/lxc", "--output-format", "json"]));
        assert!(!host_cmd_is_read_only(&["pvesh", "get", "/nodes/server/lxc", "--output-format", "text"]));
        assert!(!q("/nodes/server/storage/local-lvm"));
        assert!(!q("/nodes/server/storage/local-lvm/content/x"));
        assert!(!q("/nodes/server/storage/lo cal/content"));
        assert!(!q("/nodes/server/storage/../content"));
        assert!(!q("/nodes/../storage"));
        assert!(!q("/nodes/server/storage/-x/content"));
        assert!(!q("/nodes/server/lxc/121/config"));
        assert!(!host_cmd_is_read_only(&["pvesh", "create", "/nodes/server/storage/local/content", "--output-format", "json"]));
        assert!(!host_cmd_is_read_only(&["pvesh", "delete", "/nodes/server/storage/local/content", "--output-format", "json"]));
        // lvs: solo el argv exacto, sin variantes
        assert!(host_cmd_is_read_only(crate::share::storage::LVS));
        assert!(!host_cmd_is_read_only(&["lvs"]));
        assert!(!host_cmd_is_read_only(&["lvs", "-o", "all"]));
        let mut more = crate::share::storage::LVS.to_vec();
        more.push("/dev/pve/data");
        assert!(!host_cmd_is_read_only(&more));
        assert!(!host_cmd_is_read_only(&["lvremove", "-f", "pve/data"]));
    }

    #[test]
    fn read_only_commands_are_forwarded() {
        let h = ro();
        let o = ExecOpts::default();
        let mut sink: Vec<String> = Vec::new();
        let _ = h.host_cmd(&["curl", "--config", "-"], &o, &mut sink);
        let _ = h.guest_exec(103, &["sh", "-c", crate::share::update::check::PROBE_SH, "pxu-probe", "0"], &o, &mut sink);
        assert_eq!(
            h.inner().recorded(),
            vec![
                Recorded::Host(vec!["curl".into(), "--config".into(), "-".into()]),
                Recorded::Guest(103, vec!["sh".into(), "-c".into(), crate::share::update::check::PROBE_SH.into(), "pxu-probe".into(), "0".into()]),
            ]
        );
    }

    #[test]
    fn refresh_probe_needs_explicit_opt_in() {
        let h = ReadOnlyHost::allowing_refresh(World::demo().build());
        let (o, mut sink) = (ExecOpts::default(), Vec::<String>::new());
        let _ = h.guest_exec(103, &["sh", "-c", crate::share::update::check::PROBE_SH, "pxu-probe", "1", "86400"], &o, &mut sink);
        assert_eq!(h.inner().recorded().len(), 1);
        // incluso con refresco permitido, un script ajeno sigue denegado
        let e = h.guest_exec(103, &["sh", "-c", "apt-get update", "pxu-probe", "1", "0"], &o, &mut sink).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn host_review_commands_are_read_only_and_installing_apt_is_denied() {
        use crate::share::proxmox::hostcheck::*;
        for a in [APT_LIST, APT_SIM, LSPCI, DKMS] {
            assert!(host_cmd_is_read_only(a), "{a:?}");
        }
        // `apt-get update` solo con el refresco explícito
        assert!(!host_cmd_is_read_only(APT_UPDATE));
        let plain = ro();
        let (o, mut sink) = (ExecOpts::default(), Vec::<String>::new());
        assert_eq!(plain.host_cmd(APT_UPDATE, &o, &mut sink).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        let refreshing = ReadOnlyHost::allowing_refresh(World::demo().build());
        let _ = refreshing.host_cmd(APT_UPDATE, &o, &mut sink);
        assert_eq!(refreshing.inner().recorded(), vec![Recorded::Host(APT_UPDATE.iter().map(|s| s.to_string()).collect())]);
        // ningún apt que instale/quite/actualice pasa, ni en modo plano ni con refresco
        let bad: &[&[&str]] = &[
            &["apt-get", "upgrade"],
            &["apt-get", "-y", "upgrade"],
            &["apt-get", "full-upgrade"],
            &["apt-get", "-y", "full-upgrade"],
            &["apt-get", "dist-upgrade"],
            &["apt-get", "install", "-y", "pve-manager"],
            &["apt-get", "remove", "proxmox-ve"],
            &["apt-get", "purge", "proxmox-ve"],
            &["apt-get", "autoremove"],
            &["apt", "install", "x"],
            &["apt", "full-upgrade"],
            &["apt-get", "-s", "full-upgrade"],
            &["apt-get", "-s", "-o", "Debug::NoLocking=1", "install", "x"],
            &["apt-get", "-o", "Debug::NoLocking=1", "full-upgrade"],
            &["apt", "list", "--installed"],
            &["dkms", "install", "nvidia/1"],
            &["dkms", "remove", "nvidia/1", "--all"],
            &["lspci", "-nn", "-vv"],
        ];
        for h in [&plain, &refreshing] {
            for a in bad {
                assert_eq!(h.host_cmd(a, &o, &mut sink).unwrap_err().kind(), io::ErrorKind::PermissionDenied, "{a:?}");
            }
        }
        assert_eq!(plain.inner().recorded().len(), 0, "nothing denied reached the inner host");
        assert_eq!(refreshing.inner().recorded().len(), 1, "only the opted-in apt-get update was forwarded");
        // nvidia-smi dentro de un CT: solo esa forma exacta
        assert!(guest_cmd_is_read_only(NVIDIA_SMI, false));
        assert!(!guest_cmd_is_read_only(&["nvidia-smi", "-pm", "1"], false));
        assert!(!guest_cmd_is_read_only(&["nvidia-smi", "--query-gpu=driver_version", "--format=csv,noheader", "-r"], false));
    }

    #[test]
    fn only_own_state_is_writable_and_notifiers_have_fixed_shape() {
        let h = ro();
        for p in ["/etc/pve/lxc/103.conf", "/var/lib/pxu/../../etc/passwd", "/etc/pxu/config.toml", "/var/lib/pxu2/x"] {
            assert_eq!(h.write_file(Path::new(p), b"x").unwrap_err().kind(), io::ErrorKind::PermissionDenied, "{p}");
        }
        assert!(h.write_file(Path::new("/var/lib/pxu/state.json"), b"{}").is_ok());
        assert_eq!(h.remove_file(Path::new("/var/lib/pxu/state.json")).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        // curl: ni URL ni opciones extra en argv
        assert!(host_cmd_is_read_only(&["curl", "--config", "-"]));
        assert!(!host_cmd_is_read_only(&["curl", "-X", "POST", "http://x"]));
        assert!(!host_cmd_is_read_only(&["curl", "--config", "-", "-o", "/etc/passwd"]));
        assert!(!host_cmd_is_read_only(&["apprise", "-vv", "-t", "a", "-b", "b", "tgram://tok/chat"]));
    }

    #[test]
    fn nvidia_argvs_need_the_derived_grant_and_nothing_else_opens() {
        use crate::share::host::NvidiaOptIn;
        use crate::share::update::nvidia::Grant;
        let h = ro();
        let g = Grant::new("595.120.01", "6.17.13-2-pve", "proprietary", vec![134], vec![134]).unwrap();
        let cmds: Vec<Vec<String>> = vec![g.mkdir(), g.download(), g.sha256sum(), g.unload(), g.install(), g.ct_shutdown(134), g.ct_start(134), g.ct_push(134), g.cleanup()];
        let run = |h: &ReadOnlyHost<MockHost>, c: &Vec<String>| h.host_cmd(&c.iter().map(String::as_str).collect::<Vec<_>>(), &Default::default(), &mut Vec::<String>::new()).is_ok();
        assert!(cmds.iter().all(|c| !run(&h, c)), "denied without the opt-in");
        {
            let _o = NvidiaOptIn::new(&h, g.clone());
            assert!(cmds.iter().all(|c| run(&h, c)), "allowed under the grant");
            // otro CT, otra versión o el apt del host siguen cerrados
            assert!(!run(&h, &g.ct_shutdown(999)));
            assert!(!run(&h, &vec!["pct".into(), "destroy".into(), "134".into()]));
            assert!(!h.host_cmd(&["apt-get", "-y", "full-upgrade"], &Default::default(), &mut Vec::<String>::new()).is_ok());
            assert!(h.write_file(Path::new("/var/log/pxu/run-20261108-031600.log"), b"x").is_ok());
            assert!(h.write_file(Path::new("/var/log/pxu/other.log"), b"x").is_err());
            assert!(h.write_file(Path::new("/etc/passwd"), b"x").is_err());
        }
        assert!(cmds.iter().all(|c| !run(&h, c)), "the guard closes it again");
        assert!(!crate::share::update::hostupdate::is_write_argv(&["sh", "/var/tmp/pxu-nvidia/x.run", "--dkms"]));
    }
}
