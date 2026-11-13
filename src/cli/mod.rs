//! `pxu list [--json] [--all-nodes]`
pub mod args;
pub mod live;
use pxu::share::update::check::{self, CheckOpts, Report, EXIT_USAGE};
use pxu::share::config::{self, Config};
use pxu::share::fmt::{self, trunc};
use pxu::share::proxmox::guests::{self, Inventory};
use pxu::share::proxmox::hostcheck;
use pxu::share::host::Host;
use pxu::share::labels::{index_label, status_label, summary_line};
use pxu::share::notify::{self, Transport};
use pxu::share::model::{Guest, RunState};
use pxu::share::update;
use serde_json::json;
use std::fmt::Write;
use std::sync::atomic::AtomicBool;

fn visible(inv: &Inventory, all_nodes: bool) -> Vec<&Guest> {
    inv.guests.iter().filter(|g| all_nodes || g.state != RunState::Remote).collect()
}

/// `conf.main` como objeto: las claves repetidas (`lxc.*`) pasan a arreglo.
fn guest_json(g: &Guest) -> serde_json::Value {
    let mut v = serde_json::to_value(g).expect("serializable");
    let mut cfg = serde_json::Map::new();
    for (k, val) in &g.conf.main {
        match cfg.get_mut(k) {
            Some(serde_json::Value::Array(a)) => a.push(json!(val)),
            Some(old) => *old = json!([old.clone(), val]),
            None => {
                cfg.insert(k.clone(), json!(val));
            }
        }
    }
    let pending: serde_json::Map<String, serde_json::Value> = g.conf.pending.iter().map(|(k, val)| (k.clone(), json!(val))).collect();
    v["conf"] = json!({ "description": g.conf.description, "config": cfg, "snapshots": g.conf.snapshots, "pending": pending });
    v
}

pub fn list_json(inv: &Inventory, all_nodes: bool) -> String {
    let gs = visible(inv, all_nodes);
    let doc = json!({
        "schema": 1,
        "node": inv.node,
        "summary": {
            "total": gs.len(),
            "running": gs.iter().filter(|g| g.is_running()).count(),
            "stopped": gs.iter().filter(|g| g.state == RunState::Stopped).count(),
            "with_updates": gs.iter().filter(|g| g.updates() > 0).count(),
        },
        "guests": gs.iter().map(|g| guest_json(g)).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&doc).expect("serializable")
}

pub fn list_text(inv: &Inventory, all_nodes: bool) -> String {
    let gs = visible(inv, all_nodes);
    let mut out = String::new();
    let name_w = gs.iter().map(|g| g.name.chars().count()).max().unwrap_or(4).clamp(4, 24);
    let os_w = gs.iter().map(|g| g.os_label.chars().count()).max().unwrap_or(2).clamp(2, 20);
    let _ = writeln!(out, "{:>5}  {:<4}  {:<name_w$}  {:<7}  {:<os_w$}  {:>11}  {:>9}  {:<16}  {:>11}  AUTO", "ID", "TYPE", "NAME", "STATE", "OS", "RAM", "DISK", "TAGS", "UPD all/sec");
    for g in &gs {
        let state = if g.lock.is_some() { "locked" } else { match g.state { RunState::Running => "run", RunState::Stopped => "stopped", RunState::Remote => "remote" } };
        let _ = writeln!(
            out,
            "{:>5}  {:<4}  {:<name_w$}  {:<7}  {:<os_w$}  {:>11}  {:>9}  {:<16}  {:>11}  {}",
            g.vmid,
            g.kind.label(),
            trunc(&g.name, name_w),
            state,
            trunc(&g.os_label, os_w),
            fmt::ram(g.mem_used_bytes, g.memory_mb),
            g.disk_bytes.map(fmt::bytes_short).unwrap_or_else(|| "—".into()),
            trunc(&g.tags.join(","), 16),
            fmt::upd(g.updates(), g.security(), g.check.is_some()),
            if g.auto_update { "yes" } else { "—" },
        );
    }
    let _ = writeln!(
        out,
        "{} guests · {} running · {} stopped · {} with updates",
        gs.len(),
        gs.iter().filter(|g| g.is_running()).count(),
        gs.iter().filter(|g| g.state == RunState::Stopped).count(),
        gs.iter().filter(|g| g.updates() > 0).count()
    );
    out
}

// ---------------------------------------------------------------- check

pub fn check_text(node: &str, r: &Report) -> String {
    let mut out = String::new();
    let name_w = r.results.iter().map(|c| c.name.chars().count()).max().unwrap_or(4).clamp(4, 24);
    let _ = writeln!(out, "pxu check · {node} · {}", if r.refresh { "index refresh ON (writes the guests' package cache)" } else { "read-only (no index refresh)" });
    let _ = writeln!(out, "{:>5}  {:<name_w$}  {:<6}  {:>5}  {:>4}  {:>3}  {:<6}  RESULT", "ID", "NAME", "PM", "INDEX", "UPD", "SEC", "REBOOT");
    for c in &r.results {
        let i = c.info.as_ref();
        let _ = writeln!(
            out,
            "{:>5}  {:<name_w$}  {:<6}  {:>5}  {:>4}  {:>3}  {:<6}  {}",
            c.vmid,
            trunc(&c.name, name_w),
            i.map_or("—", |i| i.pm.as_str()),
            i.map_or("—".into(), |i| index_label(i.index_age_s, i.index_age_s > 0)),
            i.map_or("—".into(), |i| i.packages.len().to_string()),
            i.map_or("—".into(), |_| c.security().to_string()),
            i.map_or("—", |i| if i.reboot_required { "yes" } else { "no" }),
            status_label(c),
        );
    }
    let _ = writeln!(out, "{}", summary_line(r));
    if let Some(h) = &r.host {
        out.push_str(&host_text(h));
    }
    out
}

/// Resumen de la revisión del host (solo lectura). Sin campos ausentes.
fn host_text(h: &hostcheck::HostInfo) -> String {
    let mut s = String::from("\nhost (read-only review):\n");
    if let Some(e) = &h.error {
        let _ = writeln!(s, "  error: {e}");
        return s;
    }
    let _ = writeln!(s, "  {} updates ({} security){}", h.updates(), h.security(), if h.reboot_pending() { " · reboot needed" } else { "" });
    for g in ["kernel", "firmware", "pve", "security", "other"] {
        let n = h.packages.iter().filter(|p| p.group() == g).count();
        if n > 0 {
            let _ = writeln!(s, "  {g}: {n}");
        }
    }
    if let Some(b) = &h.blocked {
        let _ = writeln!(s, "  BLOCKED: {b}");
    }
    if let Some(k) = &h.nvidia_missing_for {
        let _ = writeln!(s, "  NVIDIA: no module for kernel {k}");
    }
    let cts = h.outdated_cts();
    if !cts.is_empty() {
        let _ = writeln!(s, "  CT with outdated NVIDIA: {}", cts.iter().map(u32::to_string).collect::<Vec<_>>().join(", "));
    }
    for w in h.repo_warnings.iter().chain(&h.notes) {
        let _ = writeln!(s, "  ! {w}");
    }
    s
}

/// `pxu check [--no-refresh] [--json] [--notify] [vmid...]`. Códigos: 0 sin updates · 10 hay updates · 12 algún CT falló · 2 uso/config.
pub fn cmd_check(host: &dyn Host, only: &[u32], refresh: Option<bool>, json: bool, notify_flag: bool) -> i32 {
    let cfg = match Config::load(host) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pxu: {e}");
            return EXIT_USAGE;
        }
    };
    let inv = guests::load(host);
    let known = check::targets(&inv, &[]);
    if let Some(bad) = only.iter().find(|v| !known.iter().any(|g| g.vmid == **v)) {
        eprintln!("pxu: {bad} is not an LXC on node {} (VMs, templates and remote guests are not checked)", inv.node);
        return EXIT_USAGE;
    }
    let opts = CheckOpts::from_config(&cfg, refresh);
    let rep = check::run(host, &inv, &cfg, &opts, only, &AtomicBool::new(false), &mut |_| {});
    if let Err(e) = check::persist(host, &rep) {
        eprintln!("pxu: warning: could not save state: {e}");
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&rep.to_json()).expect("serializable"));
    } else {
        print!("{}", check_text(&inv.node, &rep));
    }
    if notify_flag {
        let mut ap = cfg.apprise.clone();
        ap.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
        for l in notify::notify_check(host, &ap, &rep) {
            eprintln!("pxu: notify: {l}");
        }
    }
    rep.exit_code()
}

/// `pxu notify-flush`: reenvía la cola. 0 = vacía (o la está vaciando otro proceso), 5 = quedan avisos.
pub fn cmd_notify_flush(host: &dyn Host) -> i32 {
    let mut cfg = match Config::load(host) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pxu: {e}");
            return EXIT_USAGE;
        }
    };
    cfg.apprise.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
    let Some((sent, left)) = notify::notify_flush(host, &cfg.apprise) else {
        println!("outbox: busy, {} queued", notify::outbox_len(host));
        return 0;
    };
    println!("outbox: {sent} sent, {left} still queued");
    if left == 0 {
        0
    } else {
        5
    }
}

/// `pxu notify-test`: un aviso de prueba por el transporte configurado. 0 enviado · 2 sin transporte/config · 5 envío fallido.
pub fn cmd_notify_test(host: &dyn Host) -> i32 {
    let mut cfg = match Config::load(host) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pxu: {e}");
            return EXIT_USAGE;
        }
    };
    cfg.apprise.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
    let t = notify::resolve(host, &cfg.apprise);
    println!("transport: {}", t.label());
    if t == Transport::None {
        eprintln!("pxu: no Apprise transport configured: set [apprise] api_url in /etc/pxu/config.toml (or APPRISE_URLS with the apprise CLI installed)");
        return EXIT_USAGE;
    }
    let node = guests::load(host).node;
    let m = notify::test_message(&node);
    match notify::send_once(host, &cfg.apprise, &m) {
        Ok(_) => {
            println!("sent: ok");
            0
        }
        Err(e) => {
            eprintln!("pxu: send failed: {e}");
            5
        }
    }
}

// ---------------------------------------------------------------- update / config check

/// Marcado por SIGINT/SIGTERM/SIGHUP: el motor termina el CT en curso y para (ARCHITECTURE §3.2).
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(sig: libc::c_int) {
    // primero la señal (para que quien vea INTERRUPTED ya sepa cuál fue), después la bandera
    update::note_signal(sig);
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Avisa en stderr, en el instante, de que llegó una petición de cancelar y qué va a pasar (el modo de líneas no tiene pantalla donde verlo).
/// Dura lo que la corrida: al soltarlo, el hilo termina. Sin `println!`: con el terminal cerrado (SIGHUP) fallaría.
pub struct CancelWatch(std::sync::Arc<AtomicBool>);

impl Drop for CancelWatch {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

pub fn watch_cancel(driver: bool) -> CancelWatch {
    static REPORTED: AtomicBool = AtomicBool::new(false);
    spawn_watch(&INTERRUPTED, &REPORTED, driver, |m| {
        let _ = std::io::Write::write_fmt(&mut std::io::stderr(), format_args!("pxu: {m}\n"));
    })
}

fn spawn_watch(flag: &'static AtomicBool, reported: &'static AtomicBool, driver: bool, mut out: impl FnMut(String) + Send + 'static) -> CancelWatch {
    use std::sync::atomic::Ordering::SeqCst;
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    std::thread::spawn(move || {
        while !s.load(SeqCst) {
            if flag.load(SeqCst) && !reported.swap(true, SeqCst) {
                out(update::cancel_requested(update::CANCEL_SIGNAL.load(SeqCst), update::CRITICAL.load(SeqCst), driver));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    });
    CancelWatch(stop)
}

/// Con SIGHUP el terminal ya no existe: stdout/stderr van a /dev/null para que el texto final (`println!`) no falle ni mate a pxu con la corrida ya cerrada.
fn quiet_if_hung_up() {
    if update::CANCEL_SIGNAL.load(std::sync::atomic::Ordering::SeqCst) == libc::SIGHUP {
        if let Ok(n) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
            use std::os::fd::AsRawFd;
            // SAFETY: dup2 sobre los descriptores 1 y 2 con un descriptor válido.
            unsafe {
                libc::dup2(n.as_raw_fd(), 1);
                libc::dup2(n.as_raw_fd(), 2);
            }
        }
    }
}

/// Ctrl-C ya no mata a pxu: los hijos van en su propio grupo de procesos y no reciben la señal.
pub fn install_signal_handlers() {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
        // SSH cortado: pxu termina limpio, no mata a apt/dpkg (grupo propio) y deja su log
        libc::sigaction(libc::SIGHUP, &sa, std::ptr::null_mut());
    }
}

fn update_text(node: &str, rep: &update::RunReport) -> String {
    let mut s = format!("pxu update — node {node}\n\n{:<6} {:<18} {:<8} DETAIL\n", "VMID", "NAME", "RESULT");
    for r in &rep.results {
        let _ = writeln!(s, "{:<6} {:<18} {:<8} {}", r.vmid, trunc(&r.name, 18), format!("{:?}", r.outcome).to_lowercase(), r.detail);
        if let Some(c) = &r.revert {
            if r.outcome == update::Outcome::Failed {
                let _ = writeln!(s, "{:<6} revert (manual): {c}", "");
            }
        }
    }
    if let Some(n) = &rep.nvidia {
        let _ = writeln!(s, "\nNVIDIA driver: {} {}→{} · {}", format!("{:?}", n.outcome).to_lowercase(), n.from.as_deref().unwrap_or("none"), n.to.as_deref().unwrap_or("?"), n.detail);
        for c in &n.cts {
            let _ = writeln!(s, "  CT {} {}: {} · {}", c.vmid, c.name, format!("{:?}", c.outcome).to_lowercase(), c.detail);
        }
        if n.reboot_needed {
            let _ = writeln!(s, "  REBOOT the host and run the command again if it was asked above, or when nvidia-smi does not confirm the driver yet (pxu never reboots the host)");
        }
    }
    let _ = writeln!(s, "\n{} ok · {} warn · {} failed · {} skipped · {} cancelled", rep.count(update::Outcome::Ok), rep.count(update::Outcome::Warn), rep.count(update::Outcome::Failed), rep.count(update::Outcome::Skipped), rep.count(update::Outcome::Cancelled));
    if let Some(a) = &rep.aborted {
        let _ = writeln!(s, "run stopped: {a}");
    }
    if let Some(l) = &rep.log_path {
        let _ = writeln!(s, "log: {l}");
    }
    s
}

/// `pxu update`: 0 todo bien · 20 algún CT falló · 30 abortada (2 fallos seguidos o Ctrl-C) · 3 otro update en curso · 2 uso/config.
pub fn cmd_update(host: &dyn Host, only: &[u32], json: bool, yes: bool, notify_flag: bool) -> i32 {
    use std::io::{BufRead, IsTerminal, Write as _};
    // `update` escribe: la config pasa por la misma validación que `config check`; con errores no se toca nada.
    let mut cfg = match Config::load_checked(host) {
        Ok((c, warnings)) => {
            for w in warnings {
                eprintln!("pxu: config warning: {w}");
            }
            c
        }
        Err(e) => {
            eprintln!("pxu: {e}");
            return update::EXIT_USAGE;
        }
    };
    cfg.apprise.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
    let inv = guests::load(host);
    if let Some(bad) = only.iter().find(|v| !inv.guests.iter().any(|g| g.vmid == **v)) {
        eprintln!("pxu: {bad} is not a guest on node {}", inv.node);
        return update::EXIT_USAGE;
    }
    // Pre-chequeo: aún solo lectura (el opt-in llega después).
    let cands = update::select(host, &inv, &cfg, only);
    let todo: Vec<_> = cands.iter().filter(|c| c.skip.is_none()).collect();
    // `[update] host_auto`: el host va primero y los CT solo siguen si termina bien
    let hplan = if only.is_empty() { update::hostupdate::plan_auto(host, &inv, &cfg) } else { update::hostupdate::Plan::Off };
    let idle = if only.is_empty() { update::idle_auto(&inv, &cands).len() } else { 0 };
    if !json {
        println!("pxu update — node {}: {} CT to update, {} skipped", inv.node, todo.len(), cands.len() - todo.len() + idle);
        if idle > 0 {
            println!("  {idle} CT with auto-update: no updates pending");
        }
        match &hplan {
            update::hostupdate::Plan::Run { packages, security } => println!("  {:<5} {:<18} {} pkg ({} security) — FIRST; the CTs only follow if the host finishes well (never rebooted)", 0, "host", packages, security),
            update::hostupdate::Plan::Skip(w) => println!("  {:<5} {:<18} skipped: {w}", 0, "host"),
            update::hostupdate::Plan::Off if only.is_empty() && cfg.update.host_auto => println!("  {:<5} {:<18} skipped: no updates pending", 0, "host"),
            update::hostupdate::Plan::Off => {}
        }
        for c in &cands {
            match &c.skip {
                None => println!("  {:<5} {:<18} {} pkg ({} security)", c.vmid, trunc(&c.name, 18), c.packages, c.security),
                Some(w) => println!("  {:<5} {:<18} skipped: {w}", c.vmid, trunc(&c.name, 18)),
            }
        }
    }
    let host_runs = matches!(hplan, update::hostupdate::Plan::Run { .. });
    // Sin nada que hacer la corrida igual pasa por `update::run`: toma el lock y deja su log (`result: nothing to do`) con el motivo de cada omisión.
    let nothing = todo.is_empty() && !host_runs;
    let none: Vec<String> = todo.iter().filter(|c| cfg.backup_mode(c.vmid) == "none").map(|c| c.vmid.to_string()).collect();
    if !none.is_empty() {
        eprintln!("pxu: WARNING: backup mode \"none\" for CT {}: they are updated WITHOUT a backup and WITHOUT automatic rollback", none.join(", "));
    }
    // A-03: sin `--yes` solo se escribe tras la pregunta interactiva; con `--json` o sin TTY no hay a quién preguntar, así que se rechaza antes del opt-in
    if !nothing && !yes && (json || !std::io::stdin().is_terminal()) {
        eprintln!("pxu: `pxu update` needs --yes when it runs without a terminal or with --json (nothing was changed)");
        return update::EXIT_USAGE;
    }
    if !nothing && !yes {
        println!("backup before each CT: {} (temporary; keep {} per CT) · if the update fails the CT is rolled back automatically", cfg.backup.mode, cfg.backup.keep());
        println!("NOTE: backup and rollback cover only each CT's rootfs; bind mounts / mp with backup=0 (e.g. /mnt/pve/...) are NOT saved or reverted");
        if host_runs {
            println!("host_auto: the Proxmox HOST is updated FIRST (apt full-upgrade, /etc backup, ZFS snapshot if the root is ZFS); a host failure stops the run before any CT");
        }
        print!("writes to the listed {}. Proceed? [y/N] ", if host_runs { "host and CTs" } else { "CTs" });
        let _ = std::io::stdout().flush();
        let mut a = String::new();
        let _ = std::io::stdin().lock().read_line(&mut a);
        if !matches!(a.trim(), "y" | "Y" | "yes") {
            println!("aborted: nothing was changed");
            return update::EXIT_OK;
        }
    }
    install_signal_handlers();
    let optin = pxu::share::host::UpdateOptIn::new(host); // opt-in explícito (RAII: se retira en toda salida)
    let mut on_event = |e: update::Event| {
        if json {
            return;
        }
        match e {
            update::Event::Start(v) => print!("{v}: "),
            update::Event::Step(_, s) if s != update::Step::Lock => print!("{} · ", s.label()),
            update::Event::Done(r) => println!("{}{:?} — {}", if r.step == update::Step::Pending { format!("{}: ", r.vmid) } else { String::new() }, r.outcome, r.detail),
            _ => return,
        }
        let _ = std::io::stdout().flush();
    };
    let rep = match update::run(host, &inv, &cfg, &update::Opts { only: only.to_vec() }, &INTERRUPTED, &mut on_event) {
        Ok(r) => r,
        Err(update::RunError::Locked) => {
            eprintln!("pxu: another `pxu update` is running (lock {}/pxu-run.lock)", host.lock_dir().display());
            return update::EXIT_LOCKED;
        }
        Err(update::RunError::Io(e)) => {
            eprintln!("pxu: {e}");
            return update::EXIT_USAGE;
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&rep.to_json()).expect("serializable"));
    } else if nothing {
        println!("nothing to do (run `pxu check` first; only CTs with a positive recent check are updated)");
        if let Some(p) = &rep.log_path {
            println!("log: {p}");
        }
    } else {
        print!("\n{}", update_text(&inv.node, &rep));
    }
    if notify_flag {
        for l in update::notify_run(host, &cfg.apprise, &rep) {
            eprintln!("pxu: notify: {l}");
        }
    }
    drop(optin);
    rep.exit_code()
}

/// `pxu update --host`: instala las actualizaciones del host Proxmox, solo a mano. Mismos códigos que `update`
/// (0 ok · 20 la instalación falló · 30 abortada antes de tocar nada · 3 ocupado · 2 uso). Sin terminal: 2, también con `--yes` (el host nunca se instala desatendido).
pub fn cmd_update_host(host: &dyn Host, only: &[u32], json: bool, yes: bool, notify_flag: bool, nvidia: bool) -> i32 {
    use std::io::IsTerminal;
    let tty = std::io::stdin().is_terminal();
    if nvidia {
        return nvidia_host(host, only, json, yes, notify_flag, tty);
    }
    update_host(host, only, json, yes, notify_flag, tty)
}

/// La mini TUI, solo con terminal en la entrada y en la salida y sin `--json`; `None` = salida plana.
fn live_ui(json: bool, m: live::Live) -> Option<live::Ui> {
    use std::io::IsTerminal;
    live::wanted(std::io::stdin().is_terminal(), std::io::stdout().is_terminal(), json, std::env::var("TERM").ok().as_deref()).then(|| live::Ui::start(m, &INTERRUPTED)).flatten()
}

/// La pregunta del salto de rama en texto plano (con `--json` o si el terminal no admite la ventana). Por defecto No; el terminal cerrado aborta.
fn ask_branch_plain(o: &pxu::share::update::nvidia::BranchOffer) -> live::BranchAnswer {
    use std::io::BufRead;
    eprintln!("Newer NVIDIA driver branch: installed {} ({}), available {}.", o.from, o.flavor, o.to);
    eprintln!("Upgrading stops the GPU containers, gives them the new libraries and may need a reboot; GPU support is checked again and nothing is changed if it cannot be confirmed.");
    eprint!("Upgrade to the {}.x branch? [y/N] ", pxu::share::update::nvidia::major(&o.to).unwrap_or_default());
    let mut a = String::new();
    match std::io::stdin().lock().read_line(&mut a) {
        Ok(0) | Err(_) => live::BranchAnswer::Abort,
        Ok(_) if matches!(a.trim(), "y" | "Y" | "yes") => live::BranchAnswer::Yes,
        Ok(_) => live::BranchAnswer::No,
    }
}

/// `pxu update --host --nvidia`: instala el driver NVIDIA del host si no tiene (o lo reinstala aunque la versión ya coincida) en lugar del apt. Solo a mano, con terminal
/// y con `--yes` (sin él no se toca nada); el timer no pasa por aquí. Informa aparte el host y cada CT; nunca reinicia el host.
fn nvidia_host(host: &dyn Host, only: &[u32], json: bool, yes: bool, notify_flag: bool, tty: bool) -> i32 {
    use pxu::share::update::nvidia;
    if !only.is_empty() {
        eprintln!("pxu: --host cannot be combined with CT ids (the host is never part of a CT run)");
        return update::EXIT_USAGE;
    }
    if !tty {
        eprintln!("pxu: update --host --nvidia needs a terminal (never unattended, not even with --yes)");
        return update::EXIT_USAGE;
    }
    if !yes {
        eprintln!("pxu: --nvidia does nothing without --yes: it installs or reinstalls the host driver (stopping the GPU CTs, blocking nouveau, installing kernel headers if needed); nothing was changed");
        return update::EXIT_USAGE;
    }
    let mut cfg = match Config::load_checked(host) {
        Ok((c, warnings)) => {
            for w in warnings {
                eprintln!("pxu: config warning: {w}");
            }
            c
        }
        Err(e) => {
            eprintln!("pxu: {e}");
            return update::EXIT_USAGE;
        }
    };
    cfg.apprise.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
    install_signal_handlers();
    let inv = guests::load(host);
    let optin = pxu::share::host::UpdateOptIn::new(host);
    // ¿hay una rama mayor más nueva que soporta la GPU? (solo lecturas) Se pregunta en una ventana ANTES de la pantalla en vivo; `--yes` NO la responde
    let mut mode = nvidia::Mode::Nvidia;
    if let Some(offer) = nvidia::branch_offer(host, &cfg) {
        let answer = if json { None } else { live::ask_branch(&offer) }.unwrap_or_else(|| ask_branch_plain(&offer));
        match answer {
            live::BranchAnswer::Yes => mode = nvidia::Mode::Branch(nvidia::major(&offer.to).unwrap_or_default()),
            live::BranchAnswer::No => {
                // sin tocar el driver: solo el apt del host, por la misma ruta que `pxu update --host --yes`
                drop(optin);
                return update_host(host, only, json, true, notify_flag, tty);
            }
            live::BranchAnswer::Abort => {
                drop(optin);
                eprintln!("pxu: aborted: nothing was changed");
                return update::EXIT_ABORTED;
            }
        }
    }
    // con terminal: mini TUI en vivo; sin él (o con --json) el texto plano de siempre
    let ui = live_ui(json, live::Live::nvidia());
    // sin pantalla (modo de líneas) el aviso de cancelación sale en stderr; con ella lo da la propia pantalla
    let _watch = (ui.is_none() && !json).then(|| watch_cancel(true));
    let mut sink = ui.as_ref().map(|u| u.sink());
    // sin `println!`: con el terminal cerrado (SIGHUP) fallaría y mataría a pxu a mitad de la instalación
    let mut on_event = |e: update::Event| {
        if json {
            return;
        }
        if let Some(s) = sink.as_mut() {
            return s(e);
        }
        if let update::Event::Line(_, l) = e {
            let _ = std::io::Write::write_fmt(&mut std::io::stdout(), format_args!("  {l}\n"));
        }
    };
    let res = update::run_nvidia(host, &inv, &cfg, mode, &INTERRUPTED, &mut on_event);
    drop(optin);
    if let Some(u) = ui {
        u.finish(res.as_ref().ok().map(live::summarize));
    }
    quiet_if_hung_up();
    let rep = match res {
        Ok(r) => r,
        Err(update::RunError::Locked) => {
            eprintln!("pxu: another `pxu update` is running (lock {}/pxu-run.lock)", host.lock_dir().display());
            return update::EXIT_LOCKED;
        }
        Err(update::RunError::Io(e)) => {
            eprintln!("pxu: {e}");
            return update::EXIT_USAGE;
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&rep.to_json()).expect("serializable"));
    } else {
        print!("\n{}", update_text(&inv.node, &rep));
    }
    if notify_flag {
        for l in update::notify_run(host, &cfg.apprise, &rep) {
            eprintln!("pxu: notify: {l}");
        }
    }
    rep.exit_code()
}

fn update_host(host: &dyn Host, only: &[u32], json: bool, yes: bool, notify_flag: bool, tty: bool) -> i32 {
    use hostcheck::HostPkg;
    use pxu::share::update::hostupdate as hu;
    use std::io::{BufRead, Write as _};
    if !only.is_empty() {
        eprintln!("pxu: --host cannot be combined with CT ids (the host is never part of a CT run)");
        return update::EXIT_USAGE;
    }
    if !tty {
        eprintln!("pxu: update --host needs a terminal (the host is never updated unattended, not even with --yes)");
        return update::EXIT_USAGE;
    }
    let mut cfg = match Config::load_checked(host) {
        Ok((c, warnings)) => {
            for w in warnings {
                eprintln!("pxu: config warning: {w}");
            }
            c
        }
        Err(e) => {
            eprintln!("pxu: {e}");
            return update::EXIT_USAGE;
        }
    };
    cfg.apprise.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
    let ask = |q: &str| -> bool {
        print!("{q} [y/N] ");
        let _ = std::io::stdout().flush();
        let mut a = String::new();
        let _ = std::io::stdin().lock().read_line(&mut a);
        matches!(a.trim(), "y" | "Y" | "yes")
    };
    let show = |pk: &[HostPkg], info: &hostcheck::HostInfo| {
        println!("pxu update --host — {} package(s) ({} security)", pk.len(), pk.iter().filter(|p| p.sec).count());
        for (g, v) in hu::grouped(pk) {
            println!("  {g} ({})", v.len());
            for p in v {
                println!("    {:<36} {} → {}", trunc(&p.name, 36), p.cur, p.new);
            }
        }
        if hu::has_kernel(pk) {
            println!("A new kernel is included: a REBOOT will be needed afterwards (pxu never reboots the host).");
        }
        if let Some(k) = &info.nvidia_missing_for {
            println!("NOTE: NVIDIA GPU, and there is no driver module for kernel {k} yet: install it before rebooting (pxu does not touch NVIDIA/DKMS).");
        }
    };
    install_signal_handlers();
    loop {
        // revisión de solo lectura: lo que se va a confirmar
        let inv = guests::load(host);
        let Some(info) = hostcheck::check_host(host, &inv, &CheckOpts::from_config(&cfg, Some(false))) else {
            eprintln!("pxu: this is not a Debian/Proxmox host (no apt): nothing to update");
            return update::EXIT_USAGE;
        };
        if let Some(e) = &info.error {
            eprintln!("pxu: the host review failed: {e}");
            return update::EXIT_ABORTED;
        }
        if let Some(b) = &info.blocked {
            eprintln!("pxu: blocked: {b}");
            return update::EXIT_ABORTED;
        }
        if info.packages.is_empty() {
            println!("host: nothing to update");
            return update::EXIT_OK;
        }
        let mut confirmed = info.packages.clone();
        loop {
            if !json {
                show(&confirmed, &info);
            }
            if !yes && !ask("Install these on the Proxmox host now?") {
                println!("aborted: nothing was changed");
                return update::EXIT_OK;
            }
            let optin = pxu::share::host::HostUpdateOptIn::new(host);
            let mut ui = live_ui(json, live::Live::apt());
            let _watch = (ui.is_none() && !json).then(|| watch_cancel(false));
            let sink = std::cell::RefCell::new(ui.as_ref().map(|u| Box::new(u.sink()) as Box<dyn FnMut(update::Event)>));
            // sin `println!`: con el terminal cerrado (SIGHUP) fallaría y mataría a pxu a mitad de la instalación
            let mut on_event = |e: update::Event| {
                if json {
                    return;
                }
                if let Some(s) = sink.borrow_mut().as_mut() {
                    return s(e);
                }
                let mut o = std::io::stdout();
                match e {
                    update::Event::Step(_, s) => {
                        let _ = writeln!(o, "== {}", s.label());
                    }
                    update::Event::Line(_, l) => {
                        let _ = writeln!(o, "  {l}");
                    }
                    _ => {}
                }
            };
            let out = hu::run(host, &inv, &cfg, &hu::HostOpts { confirmed: confirmed.clone() }, &INTERRUPTED, &mut on_event);
            drop(optin);
            if let Some(u) = ui.take() {
                u.finish(match &out {
                    Ok(hu::HostOutcome::Done(r)) => Some(live::summarize(r)),
                    _ => None,
                });
                // la pantalla ya cerró: lo que venga (p. ej. la reparación de dpkg) sale en texto plano
                *sink.borrow_mut() = None;
            }
            quiet_if_hung_up();
            match out {
                Err(update::RunError::Locked) => {
                    eprintln!("pxu: another pxu update is running (lock {}/pxu-run.lock or pxu-host-update.lock)", host.lock_dir().display());
                    return update::EXIT_LOCKED;
                }
                Err(update::RunError::Io(e)) => {
                    eprintln!("pxu: {e}");
                    return update::EXIT_USAGE;
                }
                Ok(hu::HostOutcome::Reconfirm(new)) => {
                    eprintln!("pxu: the pending list changed since you saw it");
                    if yes || !tty {
                        return update::EXIT_ABORTED;
                    }
                    confirmed = new;
                }
                Ok(hu::HostOutcome::DpkgDirty(d)) => {
                    eprintln!("pxu: dpkg --audit is not clean; nothing was changed:");
                    for l in d.iter().take(10) {
                        eprintln!("  {l}");
                    }
                    if yes || !tty || !ask("Run `dpkg --configure -a` to repair it (separate confirmation)?") {
                        eprintln!("pxu: repair it by hand (dpkg --configure -a) and run again");
                        return update::EXIT_ABORTED;
                    }
                    let optin = pxu::share::host::HostUpdateOptIn::new(host);
                    let r = hu::repair_dpkg(host, &cfg, &mut on_event);
                    drop(optin);
                    match r {
                        Ok(hu::HostOutcome::Repaired) => break, // empezar de nuevo: revisión y confirmación nuevas
                        Ok(hu::HostOutcome::Done(rep)) => {
                            eprintln!("pxu: {}", rep.results.first().map(|r| r.detail.clone()).unwrap_or_default());
                            return update::EXIT_ABORTED;
                        }
                        _ => return update::EXIT_ABORTED,
                    }
                }
                Ok(hu::HostOutcome::Repaired) => return update::EXIT_ABORTED,
                Ok(hu::HostOutcome::Done(rep)) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&rep.to_json()).expect("serializable"));
                    } else {
                        print!("\n{}", update_text(&inv.node, &rep));
                        if let Some(r) = rep.results.first() {
                            if let Some(b) = &r.backup {
                                println!("backup: {b}");
                            }
                            if r.outcome == update::Outcome::Failed {
                                println!("recovery (run on the host): {}", hu::RECOVERY);
                            }
                        }
                    }
                    if notify_flag {
                        for l in hu::notify_run(host, &cfg.apprise, &rep) {
                            eprintln!("pxu: notify: {l}");
                        }
                    }
                    return rep.exit_code();
                }
            }
        }
    }
}

/// `pxu config check`: TOML, claves, valores, permisos. 0 válido (avisos permitidos) · 2 inválido.
pub fn cmd_config_check(host: &dyn Host) -> i32 {
    let v = config::validate(host);
    println!("{}", config::CONFIG_PATH);
    for n in &v.notes {
        println!("  ok    {n}");
    }
    for w in &v.warnings {
        println!("  warn  {w}");
    }
    for e in &v.errors {
        println!("  ERROR {e}");
    }
    println!("{}", if v.errors.is_empty() { "config valid" } else { "config INVALID" });
    if v.errors.is_empty() { 0 } else { EXIT_USAGE }
}

/// `pxu config encrypt-secrets`: cifra con systemd-creds la `api_key`/`urls` de `[apprise]` que estén en claro (lo mismo que guardar en la TUI).
/// 0 hecho o nada que cifrar · 2 config inválida/sin permiso para escribir.
pub fn cmd_config_encrypt_secrets(host: &dyn Host) -> i32 {
    let cfg = match Config::load(host) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pxu: {e}");
            return EXIT_USAGE;
        }
    };
    let a = &cfg.apprise;
    if !a.has_plain_secret() {
        println!("nothing to encrypt: no plain-text api_key or urls in {}", config::CONFIG_PATH);
        return 0;
    }
    host.set_config_edit(true);
    let edit = config::AppriseEdit { api_url: a.api_url.clone(), mode: if a.api_key.is_empty() { config::AppriseMode::Urls } else { config::AppriseMode::Key }, secret: String::new(), on: a.on.clone(), tag: a.tag.clone() };
    match config::write_apprise(host, &edit) {
        Ok(()) => {
            println!("encrypted: the [apprise] secrets in {} are now stored with systemd-creds (no plain-text copy left)", config::CONFIG_PATH);
            0
        }
        Err(e) => {
            eprintln!("pxu: {e}");
            EXIT_USAGE
        }
    }
}

/// `pxu config disable-apprise`: `[apprise] transport = "none"` y borra la credencial cifrada del aviso de fallo. 0 hecho · 2 config inválida/sin permiso.
pub fn cmd_config_disable_apprise(host: &dyn Host) -> i32 {
    host.set_config_edit(true);
    match config::disable_apprise(host) {
        Ok(()) => {
            println!("apprise disabled: transport = \"none\" in {} and {} removed", config::CONFIG_PATH, pxu::share::notify::FAILURE_CRED_PATH);
            0
        }
        Err(e) => {
            eprintln!("pxu: {e}");
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_line_mode_prints_the_cancel_notice_once_as_soon_as_the_flag_is_set() {
        use std::sync::atomic::Ordering::SeqCst;
        static FLAG: AtomicBool = AtomicBool::new(false);
        static REPORTED: AtomicBool = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let w = spawn_watch(&FLAG, &REPORTED, true, move |m| {
            let _ = tx.send(m);
        });
        assert!(rx.recv_timeout(std::time::Duration::from_millis(150)).is_err(), "no notice before the signal");
        let t = std::time::Instant::now();
        FLAG.store(true, SeqCst);
        let m = rx.recv_timeout(std::time::Duration::from_secs(2)).expect("the notice comes out");
        assert!(t.elapsed() < std::time::Duration::from_secs(1) && m.starts_with("cancel requested"), "{m}");
        println!("--- line mode (stderr): pxu: {m}");
        assert!(rx.recv_timeout(std::time::Duration::from_millis(150)).is_err(), "only once");
        drop(w);
    }

    use super::*;
    use pxu::share::host::ReadOnlyHost;
    use pxu::share::mock::scenario::World;

    #[test]
    fn disabling_apprise_deletes_the_old_failure_credential_and_leaks_no_secret() {
        use pxu::share::notify::FAILURE_CRED_PATH;
        let raw = "[apprise]\napi_url = \"http://apprise.local:8000\"\napi_key = \"PLAINKEY\"\ntag = \"system\"\n";
        let h = ReadOnlyHost::new(World::demo().config_raw(raw).build());
        assert_eq!(cmd_config_encrypt_secrets(&h), 0);
        // el alta de la credencial de fallo la deja en el host
        let cred = std::path::Path::new(FAILURE_CRED_PATH);
        assert!(h.inner().read_file(cred).is_ok(), "the failure credential exists while Apprise is on");
        assert_eq!(cmd_config_disable_apprise(&h), 0);
        assert!(h.inner().read_file(cred).is_err(), "the old failure credential is gone");
        let now = String::from_utf8(h.inner().read_file(std::path::Path::new(config::CONFIG_PATH)).unwrap()).unwrap();
        assert!(now.contains("transport = \"none\""), "{now}");
        // la unidad de systemd solo apunta al archivo (se omite sin él) y no lleva ningún secreto
        let unit = pxu::share::schedule::NOTIFY_FAILED_UNIT;
        assert!(unit.contains("ConditionPathExists=/etc/pxu/failure-notice.cred") && !unit.contains("PLAINKEY") && !unit.contains("apprise.local"));
        // ni las URLs ni las claves aparecen en argv ni en el registro de comandos
        let log = format!("{:?}", h.inner().recorded());
        assert!(!log.contains("PLAINKEY") && !log.contains("apprise.local"), "{log}");
        // idempotente: sin credencial tampoco falla
        assert_eq!(cmd_config_disable_apprise(&h), 0);
    }

    #[test]
    fn disabling_apprise_needs_the_config_edit_grant_through_the_host() {
        let h = ReadOnlyHost::new(World::demo().build());
        assert!(h.remove_failure_notice().is_err(), "read-only host refuses without the grant");
    }

    #[test]
    fn encrypt_secrets_migrates_a_plain_config_and_is_a_no_op_afterwards() {
        let raw = "[apprise]\napi_url = \"http://apprise.local:8000\"\napi_key = \"PLAINKEY\"\ntag = \"system\"\n";
        let h = ReadOnlyHost::new(World::demo().config_raw(raw).build());
        assert_eq!(cmd_config_encrypt_secrets(&h), 0);
        let now = String::from_utf8(h.inner().read_file(std::path::Path::new(config::CONFIG_PATH)).unwrap()).unwrap();
        assert!(now.contains("api_key_enc = \"mockenc") && !now.contains("PLAINKEY") && now.contains("tag = \"system\""), "{now}");
        assert_eq!(cmd_config_encrypt_secrets(&h), 0, "nothing left to encrypt");
        assert_eq!(Config::load(&h).unwrap().apprise.reveal(&h).unwrap().api_key, "PLAINKEY");
    }

    #[test]
    fn update_exits_2_on_an_invalid_config_without_touching_anything() {
        for bad in ["[update]\non_failure = \"Stop\"\n", "[update]\napt_mode = \"Upgrade\"\n", "[update]\non_falure = \"stop\"\n"] {
            let h = ReadOnlyHost::new(World::demo().config_raw(bad).build());
            assert_eq!(cmd_update(&h, &[], true, true, false), update::EXIT_USAGE, "{bad}");
            assert!(h.inner().recorded().is_empty(), "no command ran for {bad}");
        }
    }

    #[test]
    fn update_without_yes_and_without_a_tty_or_with_json_exits_2_before_any_write() {
        // el mock tiene CT con trabajo pendiente: sin --yes y sin TTY (los tests no tienen TTY) o con --json se rechaza
        for json in [false, true] {
            let h = ReadOnlyHost::new(World::demo().build());
            assert_eq!(cmd_update(&h, &[], json, false, false), update::EXIT_USAGE, "json={json}");
            assert!(!h.inner().recorded().iter().any(|r| format!("{r:?}").contains("full-upgrade") || format!("{r:?}").contains("vzdump")), "no write (json={json})");
        }
    }

    #[test]
    fn update_host_without_a_tty_exits_2_before_any_command_even_with_yes() {
        for yes in [false, true] {
            let h = ReadOnlyHost::new(World::host_gpu().build());
            assert_eq!(update_host(&h, &[], false, yes, false, false), update::EXIT_USAGE, "yes={yes}");
            assert!(h.inner().recorded().is_empty(), "nothing ran (yes={yes})");
        }
    }

    #[test]
    fn sighup_is_handled_like_sigint_and_sigterm() {
        install_signal_handlers();
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
            assert_eq!(old.sa_sigaction, on_signal as extern "C" fn(libc::c_int) as usize, "signal {sig}");
        }
    }

    #[test]
    fn nvidia_does_nothing_without_yes_a_tty_or_with_ct_ids() {
        for (only, yes, tty) in [(vec![], false, true), (vec![], true, false), (vec![134u32], true, true)] {
            let h = ReadOnlyHost::new(World::host_gpu().host_nvidia_auto().build());
            assert_eq!(nvidia_host(&h, &only, false, yes, false, tty), update::EXIT_USAGE, "only={only:?} yes={yes} tty={tty}");
            assert!(h.inner().recorded().is_empty(), "nothing ran (only={only:?} yes={yes} tty={tty})");
        }
    }

    #[test]
    fn update_host_cannot_be_mixed_with_ct_ids() {
        let h = ReadOnlyHost::new(World::host_gpu().build());
        assert_eq!(update_host(&h, &[131], false, true, false, true), update::EXIT_USAGE);
        assert!(h.inner().recorded().is_empty());
    }

    #[test]
    fn update_host_with_yes_installs_and_plain_update_never_does() {
        use pxu::share::mock::Recorded;
        let installs = |h: &ReadOnlyHost<pxu::share::mock::MockHost>| h.inner().recorded().iter().filter(|r| matches!(r, Recorded::Host(a) if a.iter().any(|x| x == "full-upgrade") && a[0] == "env")).count();
        let h = ReadOnlyHost::new(World::host_gpu().build());
        assert_eq!(cmd_update(&h, &[], true, true, false), update::EXIT_OK);
        assert_eq!(installs(&h), 0, "plain `pxu update` never touches the host");
        assert_eq!(update_host(&h, &[], false, true, false, true), update::EXIT_OK);
        assert_eq!(installs(&h), 1);
    }
}
