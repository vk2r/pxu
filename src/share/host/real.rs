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
        RealHost { lock_dir: PathBuf::from(PXU_LOCK_DIR) }
    }
    #[cfg(test)]
    fn with_lock_dir(dir: &Path) -> Self {
        RealHost { lock_dir: dir.to_path_buf() }
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
            read_bounded(BufReader::new(e), MAX_LINE_BYTES, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, |l| {
                if v.len() < 20 {
                    v.push(l.to_string());
                }
            });
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
        let no_kill = opts.no_kill_on_timeout;
        let grace = opts.no_kill_grace_s.unwrap_or(super::NO_KILL_GRACE_S);
        std::thread::spawn(move || {
            let waits = |s: u64| done_rx.recv_timeout(std::time::Duration::from_secs(s)) == Err(mpsc::RecvTimeoutError::Timeout);
            if !waits(t) {
                return;
            }
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            // Primero SIGTERM (vzdump/pct limpian lo suyo); SIGKILL solo si no acaba. Con `no_kill_on_timeout` (apt/dpkg) antes se espera
            // un margen finito: es el último recurso para que ninguna corrida quede esperando para siempre.
            let (before_term, before_kill) = if no_kill { (grace, grace.min(300)) } else { (0, 30) };
            if before_term > 0 && !waits(before_term) {
                return;
            }
            // SAFETY: el pid (y su grupo, que creó `process_group(0)`) sigue siendo del hijo mientras `wait` no haya vuelto.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            if waits(before_kill) {
                // SAFETY: igual que arriba.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
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
        read_bounded(BufReader::new(rd), MAX_LINE_BYTES, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES, |l| sink.line(l));
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

/// Lee `r` línea a línea con topes (A-04): una línea más larga que `max_line` se recorta (el resto se descarta hasta el salto),
/// y al pasar `max_bytes` o `max_lines` lo demás se lee y se tira. Si algo se recortó, entrega `TRUNCATED_MARK` una vez al final.
/// UTF-8 inválido se convierte con reemplazo en vez de cortar la lectura.
pub(super) fn read_bounded<R: BufRead>(mut r: R, max_line: usize, max_bytes: usize, max_lines: usize, mut f: impl FnMut(&str)) {
    use std::io::Read;
    let (mut total, mut lines, mut truncated) = (0usize, 0usize, false);
    let mut buf = Vec::new();
    loop {
        if total >= max_bytes || lines >= max_lines {
            truncated = true;
            // solo drenar: el hijo no debe bloquearse en un pipe lleno
            while let Ok(b) = r.fill_buf() {
                let n = b.len();
                if n == 0 {
                    break;
                }
                r.consume(n);
            }
            break;
        }
        buf.clear();
        let n = match r.by_ref().take(max_line as u64 + 1).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        total += n;
        if buf.last() == Some(&b'\n') {
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
        } else if buf.len() > max_line {
            // línea desmesurada: se conserva el comienzo y se descarta hasta el siguiente salto de línea
            truncated = true;
            buf.truncate(max_line);
            loop {
                let (used, done) = match r.fill_buf() {
                    Ok(b) if !b.is_empty() => match b.iter().position(|c| *c == b'\n') {
                        Some(i) => (i + 1, true),
                        None => (b.len(), false),
                    },
                    _ => (0, true),
                };
                r.consume(used);
                if done {
                    break;
                }
            }
        }
        lines += 1;
        f(&String::from_utf8_lossy(&buf));
    }
    if truncated {
        f(TRUNCATED_MARK);
    }
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
    write_atomic_mode(path, data, 0o600)
}

/// tmp + rename con `O_EXCL|O_NOFOLLOW` y el modo dado (el directorio ya está validado por quien llama o es del sistema).
fn write_atomic_mode(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    // un `.tmp` viejo (corrida interrumpida) se quita sin seguir symlinks; luego O_EXCL|O_NOFOLLOW: nunca se escribe en algo ajeno
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
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
    fn write_failure_notice(&self, data: &[u8]) -> io::Result<()> {
        // SAFETY: geteuid no tiene precondiciones.
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "writing the failure-notice credential requires root"));
        }
        write_atomic(Path::new(crate::share::notify::FAILURE_CRED_PATH), data)
    }
    fn remove_failure_notice(&self) -> io::Result<()> {
        // SAFETY: geteuid no tiene precondiciones.
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "removing the failure-notice credential requires root"));
        }
        match std::fs::remove_file(crate::share::notify::FAILURE_CRED_PATH) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
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
            write_unit_atomic(&Path::new(UNIT_DIR).join("pxu-update-failed.service"), FAILED_SERVICE_UNIT)?;
            write_unit_atomic(&Path::new(UNIT_DIR).join("pxu-update-notify-failed.service"), NOTIFY_FAILED_UNIT)?;
            write_atomic_mode(Path::new(FAILURE_NOTICE_PATH), FAILURE_NOTICE_SCRIPT.as_bytes(), 0o755)?;
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
        // Locks propios: el directorio se crea 0700 si falta y se valida (real, del usuario actual, sin acceso para otros) ANTES de abrir.
        // Los de ProxMenux (sin `create`, en su directorio) no se tocan.
        if create && path.parent() == Some(self.lock_dir.as_path()) {
            create_private_dirs(&self.lock_dir)?;
            check_private_dir(&self.lock_dir)?;
        }
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
    fn no_kill_timeout_waits_for_a_process_that_finishes_within_the_grace() {
        let t = std::time::Instant::now();
        let e = run(&["sh", "-c", "sleep 2; echo done"], &ExecOpts { timeout_s: Some(1), no_kill_on_timeout: true, no_kill_grace_s: Some(30), ..Default::default() }, &mut Vec::<String>::new()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(e.to_string().contains("not killed; finished with exit 0"), "{e}");
        assert!(t.elapsed().as_secs() < 10);
    }

    #[test]
    fn no_kill_timeout_is_finite_even_if_the_process_never_ends() {
        // ignora SIGTERM: tras el margen se corta el grupo (último recurso)
        let t = std::time::Instant::now();
        let e = run(&["sh", "-c", "trap '' TERM; sleep 60"], &ExecOpts { timeout_s: Some(1), no_kill_on_timeout: true, no_kill_grace_s: Some(1), ..Default::default() }, &mut Vec::<String>::new()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(t.elapsed().as_secs() < 20, "{:?}", t.elapsed());
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
    fn own_locks_live_in_a_validated_private_dir() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let base = std::env::temp_dir().join(format!("pxu-lk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        assert_eq!(PXU_LOCK_DIR, "/run/pxu");
        // directorio que falta: se crea 0700 y el lock funciona (y es exclusivo)
        let dir = base.join("pxu");
        let h = RealHost::with_lock_dir(&dir);
        let g = h.try_lock(&dir.join("pxu-run.lock"), true).unwrap().expect("free");
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert!(h.try_lock(&dir.join("pxu-run.lock"), true).unwrap().is_none());
        drop(g);
        // directorio con acceso para otros (alguien podría pre-crear el lock): falla en cerrado, sin abrir nada
        let open = base.join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let h = RealHost::with_lock_dir(&open);
        assert_eq!(h.try_lock(&open.join("pxu-run.lock"), true).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert!(!open.join("pxu-run.lock").exists());
        // directorio que es symlink: falla en cerrado
        let link = base.join("link");
        symlink(&dir, &link).unwrap();
        let h = RealHost::with_lock_dir(&link);
        assert_eq!(h.try_lock(&link.join("pxu-run.lock"), true).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        let _ = std::fs::remove_dir_all(&base);
    }

    fn bounded(input: &[u8], line: usize, bytes: usize, lines: usize) -> Vec<String> {
        let mut out = Vec::new();
        read_bounded(BufReader::new(input), line, bytes, lines, |l| out.push(l.to_string()));
        out
    }

    #[test]
    fn bounded_reader_keeps_normal_output_and_marks_nothing() {
        assert_eq!(bounded(b"a\r\nb\nlast", 16, 1000, 10), ["a", "b", "last"]);
        assert!(bounded(b"", 16, 1000, 10).is_empty());
    }

    #[test]
    fn bounded_reader_cuts_a_huge_line_without_newline_and_keeps_the_next_ones() {
        let mut data = vec![b'x'; 10_000];
        data.extend_from_slice(b"\nnext\n");
        let out = bounded(&data, 100, 1_000_000, 10);
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0].len(), 100);
        assert_eq!(out[1], "next");
        assert_eq!(out[2], TRUNCATED_MARK);
        // sin ningún salto de línea: una sola línea recortada + marca
        let out = bounded(&vec![b'y'; 5_000], 100, 1_000_000, 10);
        assert_eq!((out[0].len(), out[1].as_str()), (100, TRUNCATED_MARK));
    }

    #[test]
    fn bounded_reader_caps_total_lines_and_bytes_with_a_single_mark() {
        let many = "l\n".repeat(50);
        let out = bounded(many.as_bytes(), 16, 1_000_000, 5);
        assert_eq!(out.len(), 6, "{out:?}");
        assert_eq!(out.iter().filter(|l| *l == TRUNCATED_MARK).count(), 1);
        let out = bounded(many.as_bytes(), 16, 10, 1000);
        assert_eq!(out.last().unwrap(), TRUNCATED_MARK);
        assert!(out.len() <= 7);
    }

    #[test]
    fn bounded_reader_survives_invalid_utf8() {
        let out = bounded(b"ok\n\xff\xfe bad\nafter\n", 64, 1000, 10);
        assert_eq!(out.len(), 3);
        assert!(out[1].contains('\u{fffd}') && out[2] == "after");
    }

    #[test]
    fn run_drains_a_flood_of_output_without_blocking_and_truncates() {
        // ~30 MB sin saltos de línea: el hijo termina (se drena) y el sink recibe solo un trozo acotado + marca
        let mut out: Vec<String> = Vec::new();
        let e = run(&["sh", "-c", "head -c 30000000 /dev/zero | tr '\\0' 'a'; echo; echo after"], &ExecOpts { timeout_s: Some(60), ..Default::default() }, &mut out).unwrap();
        assert_eq!(e.code, 0);
        assert!(out.iter().map(|l| l.len()).sum::<usize>() <= MAX_OUTPUT_BYTES + 100, "bounded");
        assert!(out.iter().any(|l| l == TRUNCATED_MARK));
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
