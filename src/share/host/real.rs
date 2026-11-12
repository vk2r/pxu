use super::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;

/// Host real: FS real + `std::process::Command`.
pub struct RealHost {
    lock_dir: PathBuf,
}

impl RealHost {
    pub fn new() -> Self {
        RealHost { lock_dir: PathBuf::from("/run/lock") }
    }
}

impl Default for RealHost {
    fn default() -> Self {
        Self::new()
    }
}

/// Ejecuta `argv` sin shell. stdout+stderr comparten un pipe (ARCHITECTURE §3.3) y se entregan línea a línea.
/// `opts.stdin` se escribe desde un hilo aparte (los secretos nunca viajan en argv).
pub(super) fn run(argv: &[&str], opts: &ExecOpts, sink: &mut dyn LineSink) -> io::Result<ExitInfo> {
    let (prog, args) = argv.split_first().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty argv"))?;
    let (rd, wr) = std::io::pipe()?;
    let mut cmd = Command::new(prog);
    cmd.args(args).stdout(wr.try_clone()?).stderr(if opts.split_stderr { Stdio::piped() } else { Stdio::from(wr.try_clone()?) }).stdin(if opts.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    // Grupo de procesos propio: un Ctrl-C en la terminal no llega a pct/apt/dpkg (la cancelación la decide pxu).
    cmd.process_group(0);
    if opts.nice.is_some() || opts.ionice.is_some() {
        let (n, io) = (opts.nice, opts.ionice);
        // SAFETY: solo llama a nice(2) / ioprio_set(2) entre fork y exec (async-signal-safe).
        unsafe {
            cmd.pre_exec(move || {
                if let Some(n) = n {
                    libc::nice(n);
                }
                match io {
                    // IOPRIO_WHO_PROCESS=1, who=0 (yo); prioridad = clase<<13 | nivel
                    Some(IoPrio::Idle) => drop(libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13)),
                    Some(IoPrio::BestEffort7) => drop(libc::syscall(libc::SYS_ioprio_set, 1, 0, (2 << 13) | 7)),
                    None => {}
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn()?;
    drop(cmd); // cierra la copia del extremo de escritura del padre
    drop(wr);
    let err_thread = child.stderr.take().map(|e| {
        std::thread::spawn(move || {
            // acotado: solo se guardan las primeras líneas, el resto se lee y se descarta para no bloquear al hijo
            let mut v = Vec::new();
            for l in BufReader::new(e).lines().map_while(Result::ok) {
                if v.len() < 20 {
                    v.push(l);
                }
            }
            v
        })
    });
    if let (Some(data), Some(mut si)) = (opts.stdin.clone(), child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let pid = child.id() as libc::pid_t;
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(t) = opts.timeout_s {
        let flag = timed_out.clone();
        let kill = !opts.no_kill_on_timeout;
        std::thread::spawn(move || {
            if done_rx.recv_timeout(std::time::Duration::from_secs(t)) == Err(mpsc::RecvTimeoutError::Timeout) {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                if kill {
                    // SAFETY: el pid (y su grupo, que creó `process_group(0)`) sigue siendo del hijo mientras `wait` no haya vuelto.
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        });
    }
    let finished = std::sync::atomic::AtomicBool::new(false);
    let interrupted = std::sync::atomic::AtomicBool::new(false);
    let st = std::thread::scope(|sc| -> io::Result<std::process::ExitStatus> {
        if let Some(c) = opts.cancel {
            let (finished, interrupted) = (&finished, &interrupted);
            sc.spawn(move || {
                let tick = std::time::Duration::from_millis(100);
                loop {
                    if finished.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    if c.load(std::sync::atomic::Ordering::SeqCst) {
                        interrupted.store(true, std::sync::atomic::Ordering::SeqCst);
                        // SAFETY: el pid sigue siendo del hijo mientras `wait` no haya vuelto (`finished` se pone justo después).
                        unsafe { libc::kill(pid, libc::SIGTERM) };
                        // vzdump limpia su archivo parcial y su snapshot al recibir SIGTERM; si no acaba en 2 min se corta el grupo.
                        for _ in 0..1200 {
                            if finished.load(std::sync::atomic::Ordering::SeqCst) {
                                return;
                            }
                            std::thread::sleep(tick);
                        }
                        // SAFETY: igual que arriba.
                        unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                            libc::kill(pid, libc::SIGKILL);
                        }
                        return;
                    }
                    std::thread::sleep(tick);
                }
            });
        }
        for l in BufReader::new(rd).lines() {
            match l {
                Ok(l) => sink.line(&l),
                Err(_) => break, // salida no UTF-8: se corta la lectura, el exit code manda
            }
        }
        let st = child.wait();
        finished.store(true, std::sync::atomic::Ordering::SeqCst);
        st
    })?;
    let _ = done_tx.send(());
    if let Some(t) = err_thread {
        for l in t.join().unwrap_or_default() {
            sink.err_line(&l);
        }
    }
    if interrupted.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(io::Error::new(io::ErrorKind::Interrupted, CANCELLED_MSG));
    }
    if timed_out.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(io::Error::new(io::ErrorKind::TimedOut, format!(
            "timed out after {}s{}",
            opts.timeout_s.unwrap_or(0),
            if opts.no_kill_on_timeout { format!(" (not killed; finished with exit {})", st.code().unwrap_or(-1)) } else { String::new() }
        )));
    }
    Ok(ExitInfo { code: st.code().unwrap_or(-1) })
}

/// PATH fijo dentro del CT (`--clear-env` descarta el del host).
pub(super) const GUEST_PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Forma exacta de la ejecución en el CT: sin Perl (`pct exec`) y sin más opciones de lxc-attach
/// (nada de `-s`, `-e`, `--keep-env`, `-f`). Un CT parado hace que lxc-attach salga con error al instante.
pub(super) fn lxc_attach_argv<'a>(id: &'a str, argv: &[&'a str]) -> Vec<&'a str> {
    let mut full = vec!["lxc-attach", "-n", id, "--clear-env", "--set-var", GUEST_PATH, "--"];
    full.extend_from_slice(argv);
    full
}

/// Crea los directorios que falten con 0700 (los de pxu no deben listarse por otros usuarios).
fn create_private_dirs(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// El directorio de estado debe ser un directorio real (no symlink), de quien ejecuta (root) y sin acceso para grupo/otros.
fn check_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(dir)?;
    // SAFETY: geteuid no tiene precondiciones.
    let euid = unsafe { libc::geteuid() };
    if !m.file_type().is_dir() || m.uid() != euid || m.mode() & 0o077 != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} must be a real directory owned by the current user with mode 0700 (refusing to write)", dir.display())));
    }
    Ok(())
}

/// Escribe un archivo de systemd (0644, root) de forma atómica: tmp en el mismo directorio + rename; nunca sigue symlinks.
fn write_unit_atomic(path: &Path, data: &str) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("unit");
    let tmp = dir.join(format!(".{name}.pxu-tmp"));
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o644).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
    f.write_all(data.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// `systemctl ...`: error con la última línea de salida si sale con código distinto de 0.
fn systemctl(args: &[&str]) -> io::Result<Vec<String>> {
    let mut argv = vec!["systemctl"];
    argv.extend_from_slice(args);
    let mut out: Vec<String> = Vec::new();
    let o = ExecOpts { timeout_s: Some(60), ..ExecOpts::default() };
    let r = run(&argv, &o, &mut out)?;
    if r.code != 0 {
        return Err(io::Error::other(format!("systemctl {} failed (exit {}): {}", args.join(" "), r.code, out.last().map(String::as_str).unwrap_or(""))));
    }
    Ok(out)
}

pub(super) fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
    create_private_dirs(dir)?;
    check_private_dir(dir)?;
    let tmp = path.with_extension("tmp");
    // un `.tmp` viejo (corrida interrumpida) se quita sin seguir symlinks; luego O_EXCL|O_NOFOLLOW: nunca se escribe en algo ajeno
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
    f.write_all(data)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

impl Host for RealHost {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<PathBuf>> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(path)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
        v.sort();
        Ok(v)
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn guest_exec(&self, vmid: u32, argv: &[&str], o: &ExecOpts, sink: &mut dyn LineSink) -> io::Result<ExitInfo> {
        let id = vmid.to_string();
        let full = lxc_attach_argv(&id, argv);
        run(&full, o, sink)
    }
    fn host_cmd(&self, argv: &[&str], o: &ExecOpts, sink: &mut dyn LineSink) -> io::Result<ExitInfo> {
        run(argv, o, sink)
    }
    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        write_atomic(path, data)
    }
    fn write_config(&self, data: &[u8]) -> io::Result<()> {
        // SAFETY: geteuid no tiene precondiciones. Un archivo de otro dueño haría que `perm_problem` rechazara la config.
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "writing the config requires root"));
        }
        write_atomic(Path::new(crate::share::config::CONFIG_PATH), data)
    }
    fn timer_status(&self) -> io::Result<crate::share::schedule::TimerStatus> {
        use crate::share::schedule::*;
        let out = systemctl(&["show", TIMER_NAME, "-p", "LoadState,ActiveState,UnitFileState,NextElapseUSecRealtime,TimersCalendar"])?;
        Ok(parse_status(&out.join("\n")))
    }
    fn validate_calendar(&self, expr: &str) -> io::Result<crate::share::schedule::CalendarCheck> {
        let expr = crate::share::schedule::check_expr(expr).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut out: Vec<String> = Vec::new();
        let r = run(&["systemd-analyze", "calendar", "--", expr], &ExecOpts { timeout_s: Some(15), ..ExecOpts::default() }, &mut out)?;
        if r.code != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid schedule \"{expr}\": {}", out.first().map(String::as_str).unwrap_or("systemd-analyze rejected it"))));
        }
        crate::share::schedule::parse_analyze(&out.join("\n")).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unexpected systemd-analyze output"))
    }
    fn apply_schedule(&self, plan: &crate::share::schedule::SchedulePlan) -> io::Result<()> {
        use crate::share::schedule::*;
        // SAFETY: geteuid no tiene precondiciones.
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "changing the schedule requires root"));
        }
        if plan.install_units {
            write_unit_atomic(&Path::new(UNIT_DIR).join("pxu-update.service"), SERVICE_UNIT)?;
            write_unit_atomic(&Path::new(UNIT_DIR).join(TIMER_NAME), TIMER_UNIT)?;
            write_unit_atomic(&Path::new(UNIT_DIR).join("pxu-notify-flush.service"), FLUSH_SERVICE_UNIT)?;
            write_unit_atomic(&Path::new(UNIT_DIR).join(FLUSH_TIMER_NAME), FLUSH_TIMER_UNIT)?;
        }
        if let Some(e) = &plan.on_calendar {
            let e = check_expr(e).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            write_unit_atomic(Path::new(DROPIN_PATH), &dropin_text(e))?;
        }
        systemctl(&["daemon-reload"])?;
        systemctl(&[if plan.enable { "enable" } else { "disable" }, "--now", TIMER_NAME])?;
        if plan.install_units && plan.enable {
            systemctl(&["enable", "--now", FLUSH_TIMER_NAME])?;
        }
        Ok(())
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }
    fn remove_empty_dir(&self, path: &Path) -> io::Result<()> {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "is a symlink"));
        }
        std::fs::remove_dir(path)
    }
    fn read_head(&self, path: &Path, max: usize) -> io::Result<(Vec<u8>, u64)> {
        use std::io::Read;
        let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
        let m = f.metadata()?;
        if !m.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        let mut b = Vec::new();
        f.take(max as u64).read_to_end(&mut b)?;
        Ok((b, m.len()))
    }
    fn append_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no parent directory"))?;
        create_private_dirs(dir)?;
        check_private_dir(dir)?;
        let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
        f.write_all(data)
    }
    fn lock_dir(&self) -> &Path {
        &self.lock_dir
    }
    fn try_lock(&self, path: &Path, create: bool) -> io::Result<Option<LockGuard>> {
        use std::os::fd::AsRawFd;
        let f = std::fs::OpenOptions::new().read(true).write(create).create(create).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
        // SAFETY: fd válido mientras `f` viva; el lock lo libera el kernel al cerrar `f` o morir el proceso.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(Box::new(f)));
        }
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(e)
        }
    }
    fn file_meta(&self, path: &Path) -> io::Result<FileMeta> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)?;
        if m.file_type().is_symlink() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "is a symlink"));
        }
        Ok(FileMeta { mode: m.mode() & 0o7777, uid: m.uid(), is_dir: m.is_dir() })
    }
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Split(Vec<String>, Vec<String>);
    impl LineSink for Split {
        fn line(&mut self, l: &str) {
            self.0.push(l.into());
        }
        fn err_line(&mut self, l: &str) {
            self.1.push(l.into());
        }
    }

    #[test]
    fn split_stderr_keeps_warnings_out_of_stdout_and_mixed_is_default() {
        let argv = ["sh", "-c", "echo warn >&2; echo data"];
        let mut s = Split::default();
        run(&argv, &ExecOpts { split_stderr: true, ..Default::default() }, &mut s).unwrap();
        assert_eq!((s.0, s.1), (vec!["data".to_string()], vec!["warn".to_string()]));
        let mut m = Split::default();
        run(&argv, &ExecOpts::default(), &mut m).unwrap();
        assert_eq!((m.0.len(), m.1.len()), (2, 0));
    }

    #[test]
    fn write_atomic_refuses_unsafe_state_dirs_and_symlinked_tmp() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let base = std::env::temp_dir().join(format!("pxu-wa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // directorio nuevo: se crea 0700 y se escribe
        let ok = base.join("state");
        write_atomic(&ok.join("a.json"), b"1").unwrap();
        assert_eq!(std::fs::read(ok.join("a.json")).unwrap(), b"1");
        // un .tmp que es symlink no se sigue: se reemplaza y el destino ajeno queda intacto
        let victim = base.join("victim");
        std::fs::write(&victim, b"keep").unwrap();
        symlink(&victim, ok.join("b.tmp")).unwrap();
        write_atomic(&ok.join("b.json"), b"2").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
        // dir con permisos abiertos: falla en cerrado
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(write_atomic(&open.join("a.json"), b"x").unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        // dir que es symlink: falla en cerrado
        let link = base.join("link");
        symlink(&ok, &link).unwrap();
        assert_eq!(write_atomic(&link.join("a.json"), b"x").unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn guest_exec_uses_the_exact_lxc_attach_form() {
        let full = lxc_attach_argv("103", &["sh", "-c", "echo hi", "pxu-probe", "0"]);
        assert_eq!(
            full,
            ["lxc-attach", "-n", "103", "--clear-env", "--set-var", GUEST_PATH, "--", "sh", "-c", "echo hi", "pxu-probe", "0"]
        );
        assert!(!full.contains(&"pct") && !full.contains(&"-s") && !full.contains(&"-e") && !full.contains(&"--keep-env") && !full.contains(&"-f"));
    }
}
