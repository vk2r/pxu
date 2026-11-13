//! Instalación de las actualizaciones del HOST Proxmox: a mano (`pxu update --host`, tecla `u` sobre la fila del host) o, con el opt-in
//! `[update] host_auto = true`, como primer paso de la corrida programada (`run_unattended`, llamada desde `update::run`).
//!
//! Sin `host_auto` el host no se toca solo: ni el timer, ni `pxu update` sin `--host`, ni `auto_update` llegan aquí (`select` solo toma
//! LXC y la config rechaza `[[ct]] vmid = 0`). Las escrituras exigen su propio opt-in (`HostUpdateOptIn`), distinto del de los CT
//! (en la corrida programada se da solo con `host_auto` y solo durante el paso del host), y van contra la lista blanca EXACTA de este
//! módulo (`is_write_argv`); las lecturas auxiliares (`is_read_argv`) solo valen con ese opt-in.
//!
//! Orden (cualquier fallo aborta sin tocar nada): locks → `dpkg --audit` → espacio → lista fresca (a mano: ¿igual que la confirmada?;
//! desatendido: el último chequeo debe ser reciente, ver `plan_auto`, y la lista fresca se adopta) → simulación → backup de `/etc` →
//! snapshot ZFS (si la raíz es ZFS) → `full-upgrade` → autoremove (si es seguro) → nueva revisión. Nunca reinicia el host.
use super::runlog::{self, Recorder};
use super::{now_s, stamp, Ctx, CtRun, Event, Outcome, RunError, RunReport, Step, LOG_DIR};
use crate::share::config::Config;
use crate::share::host::{ExecOpts, Host};
use crate::share::notify::{self, AppriseCfg, Message};
use crate::share::proxmox::guests::Inventory;
use crate::share::proxmox::hostcheck::{self, HostInfo, HostPkg};
use crate::share::state;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

pub const BACKUP_DIR: &str = "/var/backups/pxu";
/// Copias de `/etc` que se conservan.
pub const KEEP_BACKUPS: usize = 5;
/// Mínimo libre en `/var/cache/apt/archives`.
pub const MIN_FREE_KB: u64 = 1024 * 1024;
/// Tope del `full-upgrade`: 3 h. Se espera de verdad (`no_kill_on_timeout`): jamás se mata apt/dpkg a medias.
pub const INSTALL_TIMEOUT_S: u64 = 3 * 3600;
/// Texto de recuperación si apt falla o se corta.
pub const RECOVERY: &str = "dpkg --configure -a && apt-get -f install";

// ---------------------------------------------------------------- comandos

/// Lecturas (solo con el opt-in del host).
pub const DPKG_AUDIT: &[&str] = &["dpkg", "--audit"];
pub const DF: &[&str] = &["df", "-Pk", "/var/cache/apt/archives"];
pub const FS_TYPE: &[&str] = &["findmnt", "-no", "FSTYPE", "/"];
pub const FS_SOURCE: &[&str] = &["findmnt", "-no", "SOURCE", "/"];
pub const DPKG_QUERY: &[&str] = &["dpkg-query", "-W", "-f=${Package} ${Version}\\n"];
pub const AUTOREMOVE_SIM: &[&str] = &["apt-get", "-s", "-o", "Debug::NoLocking=1", "autoremove"];

/// Escrituras (siempre con confirmación previa).
pub const BACKUP_MKDIR: &[&str] = &["install", "-d", "-m", "0700", BACKUP_DIR];
pub const DPKG_CONFIGURE: &[&str] = &["dpkg", "--configure", "-a"];
pub const INSTALL: &[&str] = &[
    "env", "DEBIAN_FRONTEND=noninteractive", "APT_LISTCHANGES_FRONTEND=none", "apt-get", "-y",
    "-o", "Dpkg::Options::=--force-confdef", "-o", "Dpkg::Options::=--force-confold", "full-upgrade",
];
pub const AUTOREMOVE: &[&str] = &["env", "DEBIAN_FRONTEND=noninteractive", "APT_LISTCHANGES_FRONTEND=none", "apt-get", "-y", "autoremove"];

fn ts14(s: &str) -> bool {
    s.len() == 14 && s.bytes().all(|b| b.is_ascii_digit())
}

/// `/var/backups/pxu/etc-<14 dígitos>.tar.zst|gz`.
pub fn is_backup_path(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    p.parent() == Some(Path::new(BACKUP_DIR))
        && name.strip_prefix("etc-").and_then(|r| r.strip_suffix(".tar.zst").or_else(|| r.strip_suffix(".tar.gz"))).is_some_and(ts14)
}

/// Dataset ZFS de la raíz (`rpool/ROOT/pve-1`): sin espacios, `@`, `-` inicial ni `/` inicial.
fn dataset_ok(d: &str) -> bool {
    !d.is_empty() && d.len() <= 200 && !d.starts_with(['-', '/']) && !d.contains("//") && d.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
}

fn snapshot_ok(s: &str) -> bool {
    s.split_once("@pxu-").is_some_and(|(d, t)| dataset_ok(d) && ts14(t))
}

pub fn is_write_argv(argv: &[&str]) -> bool {
    match argv {
        a if a == BACKUP_MKDIR || a == DPKG_CONFIGURE || a == INSTALL || a == AUTOREMOVE => true,
        ["tar", "--zstd", "-cpf", f, "-C", "/", "etc"] => f.ends_with(".tar.zst") && is_backup_path(Path::new(f)),
        ["tar", "-czpf", f, "-C", "/", "etc"] => f.ends_with(".tar.gz") && is_backup_path(Path::new(f)),
        ["chmod", "0600", f] => is_backup_path(Path::new(f)),
        ["zfs", "snapshot", s] => snapshot_ok(s),
        _ => false,
    }
}

pub fn is_read_argv(argv: &[&str]) -> bool {
    argv == DPKG_AUDIT || argv == DF || argv == FS_TYPE || argv == FS_SOURCE || argv == DPKG_QUERY || argv == AUTOREMOVE_SIM
}

/// Todo lo que el opt-in del host deja pasar: sus escrituras y sus lecturas auxiliares.
pub fn is_host_update_argv(argv: &[&str]) -> bool {
    is_write_argv(argv) || is_read_argv(argv)
}

// ---------------------------------------------------------------- parsers

/// `df -Pk`: KB libres (4.ª columna de la última línea).
pub fn parse_df_free_kb(text: &str) -> Option<u64> {
    text.lines().rev().find(|l| !l.trim().is_empty()).and_then(|l| l.split_whitespace().nth(3)).and_then(|v| v.parse().ok())
}

/// `dpkg --audit`: cualquier línea no vacía = paquetes a medio configurar.
pub fn parse_audit(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

/// `dpkg-query`: nombre → versión.
pub fn parse_versions(lines: &[String]) -> BTreeMap<String, String> {
    lines.iter().filter_map(|l| l.split_once(' ')).map(|(n, v)| (n.trim().to_string(), v.trim().to_string())).collect()
}

/// Paquetes que una simulación (`apt-get -s`) quitaría (`Remv <nombre> [versión]`).
pub fn parse_removals(text: &str) -> Vec<String> {
    text.lines().filter_map(|l| l.strip_prefix("Remv ")).filter_map(|r| r.split_whitespace().next()).map(String::from).collect()
}

/// `proxmox-kernel-6.17.9-1-pve-signed` → `6.17.9-1-pve` (los metapaquetes `proxmox-kernel-6.17` no cuentan).
pub fn kernel_of(pkg: &str) -> Option<String> {
    let r = pkg.strip_prefix("proxmox-kernel-").or_else(|| pkg.strip_prefix("pve-kernel-"))?;
    let r = r.strip_suffix("-signed").unwrap_or(r);
    (r.starts_with(|c: char| c.is_ascii_digit()) && r.ends_with("-pve")).then(|| r.to_string())
}

/// ¿Bloquea la simulación del `full-upgrade`? Quita proxmox-ve / pve-manager / el kernel en uso, apt no resuelve,
/// o quita cualquier otro paquete que no sea un kernel viejo (nunca el en uso).
pub fn sim_blocker(text: &str, running: &str) -> Option<String> {
    if let Some(b) = hostcheck::parse_sim(text) {
        return Some(b);
    }
    let removals = parse_removals(text);
    if let Some(n) = removals.iter().find(|n| !running.is_empty() && kernel_of(n).as_deref() == Some(running)) {
        return Some(format!("full-upgrade would remove the running kernel ({n})"));
    }
    let other: Vec<&str> = removals.iter().filter(|n| kernel_of(n).is_none()).map(String::as_str).collect();
    if other.is_empty() {
        return None;
    }
    let shown = other.iter().take(5).copied().collect::<Vec<_>>().join(", ");
    let more = if other.len() > 5 { format!(" and {} more", other.len() - 5) } else { String::new() };
    Some(format!("full-upgrade would remove package(s) that are not old kernels: {shown}{more}"))
}

/// Kernels que el autoremove NO puede quitar: el en uso y los dos más nuevos instalados (el anterior al nuevo incluido).
pub fn protected_kernels(installed: &BTreeMap<String, String>, running: &str) -> Vec<String> {
    let mut ks: Vec<String> = installed.keys().filter_map(|n| kernel_of(n)).collect();
    ks.sort_by(|a, b| hostcheck::kernel_cmp(b, a));
    ks.dedup();
    ks.truncate(2);
    if !running.is_empty() && !ks.iter().any(|k| k == running) {
        ks.push(running.to_string());
    }
    ks
}

/// Si el autoremove quitaría un kernel protegido, devuelve cuál.
pub fn autoremove_conflict(text: &str, protected: &[String]) -> Option<String> {
    parse_removals(text).into_iter().find(|n| kernel_of(n).is_some_and(|k| protected.contains(&k)))
}

fn key(p: &[HostPkg]) -> Vec<(String, String)> {
    let mut v: Vec<_> = p.iter().map(|p| (p.name.clone(), p.new.clone())).collect();
    v.sort();
    v
}

/// Paquetes agrupados para la confirmación (CLI y TUI): kernel · firmware · pve · security · other (los grupos vacíos no salen).
pub fn grouped(p: &[HostPkg]) -> Vec<(&'static str, Vec<&HostPkg>)> {
    ["kernel", "firmware", "pve", "security", "other"].into_iter().map(|g| (g, p.iter().filter(|x| x.group() == g).collect::<Vec<_>>())).filter(|(_, v)| !v.is_empty()).collect()
}

/// ¿Entra un kernel nuevo? Si sí, hará falta reiniciar después.
pub fn has_kernel(p: &[HostPkg]) -> bool {
    p.iter().any(|x| x.class == hostcheck::PkgClass::Kernel)
}

// ---------------------------------------------------------------- motor

/// Qué hace la corrida programada con el host (decidido solo con el estado guardado, sin tocar nada).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// `host_auto` apagado, o no hay nada pendiente: el host no forma parte de la corrida.
    Off,
    /// Se salta con este motivo (el host no se toca; los CT siguen).
    Skip(String),
    Run { packages: usize, security: usize },
}

/// Con `host_auto`: ¿el host entra en la corrida programada? Exige un chequeo del host reciente (`require_fresh_check_minutes`), como los CT.
pub fn plan_auto(host: &dyn Host, inv: &Inventory, cfg: &Config) -> Plan {
    if !cfg.update.host_auto {
        return Plan::Off;
    }
    let Some(h) = &inv.host else { return Plan::Skip("never checked: run `pxu check` first".into()) };
    if let Some(e) = &h.error {
        return Plan::Skip(format!("the last host review failed: {e}"));
    }
    if now_s(host).saturating_sub(h.checked_at) > cfg.update.require_fresh_check_minutes * 60 {
        return Plan::Skip(format!("check older than {} min: run `pxu check` again", cfg.update.require_fresh_check_minutes));
    }
    if let Some(b) = &h.blocked {
        return Plan::Skip(format!("blocked: {b}"));
    }
    match h.updates() {
        0 => Plan::Off,
        n => Plan::Run { packages: n, security: h.packages.iter().filter(|p| p.sec).count() },
    }
}

pub struct HostOpts {
    /// Lista que el usuario vio y confirmó: si la lista fresca difiere, no se instala (hay que confirmar de nuevo).
    pub confirmed: Vec<HostPkg>,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // un solo valor por corrida: no merece un Box
pub enum HostOutcome {
    /// Terminó (bien, con avisos, fallido) o abortó en una precomprobación (`aborted` con el motivo; no se tocó nada).
    Done(RunReport),
    /// La lista fresca difiere de la confirmada: mostrar la nueva y pedir confirmación otra vez.
    Reconfirm(Vec<HostPkg>),
    /// `dpkg --audit` sucio: no se sigue. Ofrecer `repair_dpkg` con confirmación aparte.
    DpkgDirty(Vec<String>),
    /// `dpkg --configure -a` terminó y la auditoría quedó limpia: empezar de nuevo (revisar y confirmar).
    Repaired,
}

struct Locks(#[allow(dead_code)] Vec<crate::share::host::LockGuard>);

fn lock_all(host: &dyn Host) -> Result<Locks, RunError> {
    let mut g = Vec::new();
    // el global es el mismo que usa el motor de CT: host y CT jamás corren a la vez
    for name in ["pxu-run.lock", "pxu-host-update.lock"] {
        match host.try_lock(&host.lock_dir().join(name), true) {
            Ok(Some(l)) => g.push(l),
            Ok(None) => return Err(RunError::Locked),
            Err(e) => return Err(RunError::Io(format!("lock {name}: {e}"))),
        }
    }
    Ok(Locks(g))
}

fn quick(cfg: &Config) -> ExecOpts<'static> {
    ExecOpts { timeout_s: Some(cfg.check.timeout_s), env: vec![("LC_ALL".into(), "C".into())], ..Default::default() }
}

fn long(secs: u64) -> ExecOpts<'static> {
    ExecOpts { timeout_s: Some(secs), no_kill_on_timeout: true, env: vec![("LC_ALL".into(), "C".into())], ..Default::default() }
}

/// `dpkg --audit` → paquetes sucios (vacío = limpio). `Err` si no se pudo auditar.
fn audit(cx: &mut Ctx, cfg: &Config) -> Result<Vec<String>, String> {
    let rc = cx.host_cmd(DPKG_AUDIT, &quick(cfg))?;
    let dirty = parse_audit(&cx.capture);
    if rc != 0 && dirty.is_empty() {
        return Err(format!("dpkg --audit failed (rc {rc})"));
    }
    Ok(dirty)
}

/// `dpkg --configure -a` (con la confirmación aparte del usuario) y nueva auditoría.
pub fn repair_dpkg(host: &dyn Host, cfg: &Config, ev_out: &mut dyn FnMut(Event)) -> Result<HostOutcome, RunError> {
    let started_at = now_s(host);
    let rec = RefCell::new(Recorder::default());
    let mut ev_fn = |e: Event| {
        rec.borrow_mut().feed(now_s(host), &e);
        ev_out(e)
    };
    let ev: &mut dyn FnMut(Event) = &mut ev_fn;
    let _locks = lock_all(host)?;
    ev(Event::Queue(vec![(0, "host".into())]));
    ev(Event::Start(0));
    let mut cx = Ctx { host, vmid: 0, log: vec![], dropped: 0, capture: vec![], abort: None, hold: false, no_prune: false, ev, security: 0, cancel: None, interrupted: false, root_used_kb: None };
    cx.say(&format!("pxu host update (dpkg repair) · {}", stamp(started_at)));
    cx.step(Step::Preflight);
    let res = match cx.host_cmd(DPKG_CONFIGURE, &long(INSTALL_TIMEOUT_S)) {
        Ok(0) => match audit(&mut cx, cfg) {
            Ok(d) if d.is_empty() => return Ok(HostOutcome::Repaired),
            Ok(_) => "dpkg --audit is still dirty after dpkg --configure -a".to_string(),
            Err(e) => e,
        },
        Ok(rc) => format!("dpkg --configure -a failed (rc {rc})"),
        Err(e) => format!("dpkg --configure -a: {e}"),
    };
    let out = HostOutcome::Done(finish(&mut cx, started_at, Outcome::Failed, Step::Preflight, res, None, 0, false, None));
    Ok(seal(host, cfg, &rec, &RefCell::new(None), &node_name(host), out))
}

/// Nombre del nodo para el título del aviso (la reparación no tiene inventario).
fn node_name(host: &dyn Host) -> String {
    host.read_file(Path::new("/etc/hostname")).ok().map(|b| String::from_utf8_lossy(&b).trim().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "host".into())
}

/// Escribe el log único de una corrida que terminó (`Done`) y anota nodo y ruta en su informe.
fn seal(host: &dyn Host, cfg: &Config, rec: &RefCell<Recorder>, live: &RefCell<Option<PathBuf>>, node: &str, out: HostOutcome) -> HostOutcome {
    match out {
        HostOutcome::Done(mut rep) => {
            rep.node = node.into();
            runlog::finalize(host, cfg.general.log_retention_days, live.borrow_mut().take(), &rec.borrow(), &mut rep);
            HostOutcome::Done(rep)
        }
        other => other,
    }
}

/// Instala las actualizaciones del host. Ver la cabecera del módulo para el orden de las comprobaciones.
pub fn run(host: &dyn Host, inv: &Inventory, cfg: &Config, opts: &HostOpts, cancel: &AtomicBool, ev_out: &mut dyn FnMut(Event)) -> Result<HostOutcome, RunError> {
    let started_at = now_s(host);
    let rec = RefCell::new(Recorder::default());
    let live = RefCell::new(None::<PathBuf>);
    let mut ev_fn = |e: Event| {
        runlog::feed_live(host, &rec, &live, now_s(host), &e);
        ev_out(e)
    };
    let ev: &mut dyn FnMut(Event) = &mut ev_fn;
    let _locks = lock_all(host)?;
    *live.borrow_mut() = runlog::begin_live(host, started_at, &inv.node);
    ev(Event::Queue(vec![(0, "host".into())]));
    ev(Event::Start(0));
    super::CRITICAL.store(false, Ordering::SeqCst);
    let out = core(host, inv, cfg, opts.confirmed.clone(), false, started_at, cancel, ev)?;
    if cancel.load(Ordering::SeqCst) {
        // queda en el log: qué señal llegó y qué se hizo con ella
        let sig = super::CANCEL_SIGNAL.load(Ordering::SeqCst);
        let done = matches!(&out, HostOutcome::Done(r) if r.cancelled);
        ev(Event::Line(0, if done { format!("{}: stopped before installing anything", super::cancel_requested(sig, false, false)) } else { format!("{}: ignored while packages were installing; the run finished normally", super::cancel_requested(sig, true, false)) }));
    }
    Ok(seal(host, cfg, &rec, &live, &inv.node, out))
}

/// Primer paso de la corrida programada (`host_auto`): llamada solo desde `update::run`, que ya tiene `pxu-run.lock`, emite la cola y
/// `Start(0)` y decide con `plan_auto`. Mismas salvaguardas que `run`; en lugar de la confirmación se adopta la lista fresca.
/// Devuelve siempre `Done` si llegó a ejecutarse (el informe lleva una sola entrada, vmid 0).
pub fn run_unattended(host: &dyn Host, inv: &Inventory, cfg: &Config, cancel: &AtomicBool, ev: &mut dyn FnMut(Event)) -> Result<HostOutcome, RunError> {
    let started_at = now_s(host);
    let name = "pxu-host-update.lock";
    let _lock = match host.try_lock(&host.lock_dir().join(name), true) {
        Ok(Some(l)) => l,
        Ok(None) => return Err(RunError::Locked),
        Err(e) => return Err(RunError::Io(format!("lock {name}: {e}"))),
    };
    let stored = inv.host.as_ref().map(|h| h.packages.clone()).unwrap_or_default();
    core(host, inv, cfg, stored, true, started_at, cancel, ev)
}

#[allow(clippy::too_many_arguments)]
fn core(host: &dyn Host, inv: &Inventory, cfg: &Config, mut confirmed: Vec<HostPkg>, unattended: bool, started_at: u64, cancel: &AtomicBool, ev: &mut dyn FnMut(Event)) -> Result<HostOutcome, RunError> {
    let mut cx = Ctx { host, vmid: 0, log: vec![], dropped: 0, capture: vec![], abort: None, hold: false, no_prune: false, ev, security: 0, cancel: None, interrupted: false, root_used_kb: None };
    cx.security = confirmed.iter().filter(|p| p.sec).count();
    cx.say(&format!("pxu host update{} · {}", if unattended { " (scheduled run)" } else { "" }, stamp(started_at)));
    // precomprobaciones: sin tocar nada
    let pre = match prechecks(&mut cx, cfg, &mut confirmed, unattended, cancel) {
        Ok(p) => p,
        Err(Stop::Reconfirm(l)) => return Ok(HostOutcome::Reconfirm(l)),
        Err(Stop::Dirty(d)) if unattended => {
            let why = format!("dpkg --audit is not clean ({} lines)", d.len());
            return Ok(HostOutcome::Done(finish(&mut cx, started_at, Outcome::Skipped, Step::Preflight, format!("aborted before touching anything: {why}"), Some(why), confirmed.len(), false, None)));
        }
        Err(Stop::Dirty(d)) => return Ok(HostOutcome::DpkgDirty(d)),
        Err(Stop::Nothing) => return Ok(HostOutcome::Done(nothing_pending(&mut cx, started_at))),
        Err(Stop::Abort(why)) => {
            let detail = format!("aborted before touching anything: {why}");
            return Ok(HostOutcome::Done(finish(&mut cx, started_at, Outcome::Skipped, Step::Preflight, detail, Some(why), confirmed.len(), false, None)));
        }
    };
    cx.security = confirmed.iter().filter(|p| p.sec).count();
    let now = now_s(host);
    cx.step(Step::Backup);
    let backup = match backups(&mut cx, cfg, now) {
        Ok(b) => b,
        Err(why) => {
            let detail = format!("aborted before touching anything: {why}");
            return Ok(HostOutcome::Done(finish(&mut cx, started_at, Outcome::Skipped, Step::Backup, detail, Some(why), confirmed.len(), false, None)));
        }
    };
    if cancel.load(Ordering::SeqCst) {
        let why = super::cancel_reason();
        return Ok(HostOutcome::Done(finish(&mut cx, started_at, Outcome::Cancelled, Step::Backup, why.clone(), Some(why), confirmed.len(), false, Some(backup))));
    }
    // la copia de /etc tardó: la lista y la simulación se repiten justo antes de instalar
    cx.step(Step::Preflight);
    match verify_list(&mut cx, cfg, &mut confirmed, if unattended { Verify::Same } else { Verify::Confirmed }, &pre.running) {
        Ok(()) => {}
        Err(Stop::Reconfirm(l)) => return Ok(HostOutcome::Reconfirm(l)),
        Err(Stop::Dirty(d)) => return Ok(HostOutcome::DpkgDirty(d)),
        Err(Stop::Nothing) => return Ok(HostOutcome::Done(nothing_pending(&mut cx, started_at))),
        Err(Stop::Abort(why)) => {
            let detail = format!("aborted before installing: {why}");
            return Ok(HostOutcome::Done(finish(&mut cx, started_at, Outcome::Skipped, Step::Preflight, detail, Some(why), confirmed.len(), false, Some(backup))));
        }
    }
    // instalación
    cx.step(Step::Upgrade);
    let before = versions(&mut cx, cfg);
    let rc = cx.host_cmd(INSTALL, &long(INSTALL_TIMEOUT_S));
    let after = versions(&mut cx, cfg);
    let changed: Vec<&String> = after.iter().filter(|(n, v)| before.get(*n).is_some_and(|b| b != *v)).map(|(n, _)| n).collect();
    diff_log(&mut cx, &before, &after);
    let updated = changed.len();
    let security = confirmed.iter().filter(|p| p.sec && changed.contains(&&p.name)).count();
    match rc {
        Ok(0) => {}
        other => {
            let why = match other {
                Ok(rc) => format!("apt-get full-upgrade failed (rc {rc})"),
                Err(e) => format!("apt-get full-upgrade did not finish: {e}"),
            };
            let detail = format!("{why}. Recovery on the host: {RECOVERY}");
            let at = now_s(host);
            let _ = state::update_state(host, |st| st.host = Some(HostInfo::errored(at, &format!("install failed ({why}); see the last run log in {LOG_DIR}; recover with: {RECOVERY}"))));
            return Ok(HostOutcome::Done(finish(&mut cx, started_at, Outcome::Failed, Step::Upgrade, detail, None, confirmed.len(), false, Some(backup))));
        }
    }
    // autoremove, solo si no toca el kernel en uso ni el anterior
    let mut warn = None;
    let installed = after;
    cx.step(Step::Verify);
    match cx.host_cmd(AUTOREMOVE_SIM, &quick(cfg)) {
        Ok(0) => {
            let text = cx.capture.join("\n");
            let prot = protected_kernels(&installed, &pre.running);
            if let Some(k) = autoremove_conflict(&text, &prot) {
                let w = format!("autoremove skipped: it would remove {k} (the running or previous kernel); left installed");
                cx.say(&format!("WARNING: {w}"));
                warn = Some(w);
            } else if parse_removals(&text).is_empty() {
                cx.say("autoremove: nothing to remove");
            } else if !matches!(cx.host_cmd(AUTOREMOVE, &long(INSTALL_TIMEOUT_S)), Ok(0)) {
                warn = Some("apt-get autoremove failed (the install itself succeeded)".into());
            }
        }
        _ => warn = Some("autoremove skipped: its simulation failed".into()),
    }
    // nueva revisión: la fila refleja el estado real y el reinicio pendiente
    let opts_check = crate::share::update::check::CheckOpts::from_config(cfg, Some(false));
    let info = hostcheck::check_host(host, inv, &opts_check);
    let reboot = info.as_ref().is_some_and(|i| i.reboot_pending());
    let remaining = info.as_ref().map(|i| i.updates());
    if let Some(i) = info {
        let _ = state::update_state(host, |st| st.host = Some(i));
    }
    let mut detail = format!("{updated} packages updated ({security} security) · reboot required: {}", if reboot { "yes" } else { "no" });
    if let Some(w) = &warn {
        detail += &format!(" · {w}");
    }
    let outcome = if warn.is_some() { Outcome::Warn } else { Outcome::Ok };
    let mut report = finish(&mut cx, started_at, outcome, Step::Done, detail, None, confirmed.len(), reboot, Some(backup));
    if let Some(r) = report.results.first_mut() {
        r.packages_after = remaining;
    }
    Ok(HostOutcome::Done(report))
}

enum Stop {
    Reconfirm(Vec<HostPkg>),
    Dirty(Vec<String>),
    Abort(String),
    /// Solo desatendido: la lista fresca está vacía (el host ya estaba al día): nada que hacer, no es un fallo.
    Nothing,
}

/// Cómo se valida la lista pendiente fresca.
#[derive(Clone, Copy, PartialEq)]
enum Verify {
    /// A mano: debe ser igual a la confirmada (si no, `Reconfirm`).
    Confirmed,
    /// Desatendido, primera vez: la lista fresca pasa a ser la de referencia.
    Adopt,
    /// Desatendido, antes de instalar: debe seguir igual que la adoptada (no hay a quién reconfirmar: aborta).
    Same,
}

/// Informe de un host que ya no tenía nada pendiente al llegar (desatendido): terminó bien sin tocar nada.
fn nothing_pending(cx: &mut Ctx, started_at: u64) -> RunReport {
    cx.say("nothing to update: the host is already up to date");
    finish(cx, started_at, Outcome::Ok, Step::Done, "0 packages updated (0 security) · reboot required: no · already up to date".into(), None, 0, false, None)
}

impl From<String> for Stop {
    fn from(s: String) -> Stop {
        Stop::Abort(s)
    }
}

struct Pre {
    running: String,
}

fn prechecks(cx: &mut Ctx, cfg: &Config, confirmed: &mut Vec<HostPkg>, unattended: bool, cancel: &AtomicBool) -> Result<Pre, Stop> {
    cx.step(Step::Preflight);
    // (b) dpkg limpio
    let dirty = audit(cx, cfg)?;
    if !dirty.is_empty() {
        cx.say(&format!("dpkg --audit is not clean ({} lines): not continuing", dirty.len()));
        return Err(Stop::Dirty(dirty));
    }
    // (c) espacio
    cx.host_cmd(DF, &quick(cfg))?;
    let free = parse_df_free_kb(&cx.capture.join("\n")).ok_or_else(|| "cannot read the free space of /var/cache/apt/archives".to_string())?;
    if free < MIN_FREE_KB {
        return Err(Stop::Abort(format!("not enough free space in /var/cache/apt/archives ({} MB free, 1024 MB needed)", free / 1024)));
    }
    // (d) lista fresca y simulación
    let running = crate::share::host::read_string(cx.host, "/proc/sys/kernel/osrelease").map(|s| s.trim().to_string()).unwrap_or_default();
    verify_list(cx, cfg, confirmed, if unattended { Verify::Adopt } else { Verify::Confirmed }, &running)?;
    if cancel.load(Ordering::SeqCst) {
        return Err(Stop::Abort(super::cancel_reason()));
    }
    Ok(Pre { running })
}

/// Lista pendiente fresca (¿igual que la confirmada?) y simulación del `full-upgrade` (¿bloquea?). Se usa antes del backup y otra vez justo antes de instalar.
fn verify_list(cx: &mut Ctx, cfg: &Config, confirmed: &mut Vec<HostPkg>, how: Verify, running: &str) -> Result<(), Stop> {
    let rc = cx.host_cmd(hostcheck::APT_LIST, &quick(cfg))?;
    if rc != 0 {
        return Err(Stop::Abort(format!("apt list failed (rc {rc})")));
    }
    let fresh = hostcheck::parse_upgradable(&cx.capture.join("\n"));
    if fresh.is_empty() {
        return Err(if how == Verify::Adopt { Stop::Nothing } else { Stop::Abort("nothing to update".into()) });
    }
    match how {
        Verify::Adopt => *confirmed = fresh,
        Verify::Confirmed if key(&fresh) != key(confirmed) => {
            cx.say("the pending list changed since it was confirmed: confirm again");
            return Err(Stop::Reconfirm(fresh));
        }
        Verify::Same if key(&fresh) != key(confirmed) => return Err(Stop::Abort("the pending list changed while the /etc backup was running".into())),
        _ => {}
    }
    let rc = cx.host_cmd(hostcheck::APT_SIM, &long(cfg.check.timeout_s * 3))?;
    let text = cx.capture.join("\n");
    if let Some(b) = sim_blocker(&text, running) {
        return Err(Stop::Abort(b));
    }
    if rc != 0 {
        return Err(Stop::Abort(format!("apt-get -s full-upgrade failed (rc {rc})")));
    }
    Ok(())
}

/// `/var/backups/pxu` debe ser un directorio real (no symlink), de root y 0700.
fn check_backup_dir(host: &dyn Host) -> Result<(), String> {
    let m = host.file_meta(Path::new(BACKUP_DIR)).map_err(|e| format!("{BACKUP_DIR} is not usable ({e})"))?;
    if !m.is_dir {
        return Err(format!("{BACKUP_DIR} is not a directory"));
    }
    if m.uid != 0 {
        return Err(format!("{BACKUP_DIR} is not owned by root (uid {})", m.uid));
    }
    if m.mode != 0o700 {
        return Err(format!("{BACKUP_DIR} must be mode 0700 (it is {:04o})", m.mode));
    }
    Ok(())
}

/// (e) tar de `/etc` (0600, 5 copias) y snapshot ZFS de la raíz si la hay. Devuelve el resumen para el informe.
fn backups(cx: &mut Ctx, cfg: &Config, now: u64) -> Result<String, String> {
    let ts = stamp(now);
    let host = cx.host;
    // un symlink se rechaza ANTES de `install -d` (que lo seguiría y cambiaría el modo del destino); el resto, después
    if host.exists(Path::new(BACKUP_DIR)) {
        host.file_meta(Path::new(BACKUP_DIR)).map_err(|e| format!("{BACKUP_DIR} is not usable ({e})"))?;
    }
    cx.host_cmd(BACKUP_MKDIR, &quick(cfg)).map_err(|e| format!("backup dir: {e}"))?;
    check_backup_dir(host)?;
    let zstd = ["/usr/bin/zstd", "/bin/zstd"].iter().any(|p| host.exists(Path::new(p)));
    let file = format!("{BACKUP_DIR}/etc-{ts}.tar.{}", if zstd { "zst" } else { "gz" });
    let tar: Vec<&str> = if zstd { vec!["tar", "--zstd", "-cpf", &file, "-C", "/", "etc"] } else { vec!["tar", "-czpf", &file, "-C", "/", "etc"] };
    // rc 1 = «un fichero cambió mientras se leía»: el tar sirve; rc ≥ 2 = fallo
    match cx.host_cmd(&tar, &ExecOpts { timeout_s: Some(1800), ..Default::default() }) {
        Ok(0) => {}
        Ok(1) => cx.say("WARNING: tar reported rc 1 (some files changed while being read); the backup is usable"),
        Ok(rc) => return Err(drop_partial(cx, &file, format!("the /etc backup failed (tar rc {rc})"))),
        Err(e) => return Err(drop_partial(cx, &file, format!("the /etc backup failed: {e}"))),
    }
    if !matches!(cx.host_cmd(&["chmod", "0600", &file], &quick(cfg)), Ok(0)) {
        return Err(drop_partial(cx, &file, "could not set 0600 on the /etc backup".into()));
    }
    let mut files: Vec<_> = host.read_dir(Path::new(BACKUP_DIR)).unwrap_or_default().into_iter().filter(|p| is_backup_path(p)).collect();
    files.sort();
    for old in files.iter().take(files.len().saturating_sub(KEEP_BACKUPS)) {
        if host.remove_file(old).is_ok() {
            cx.say(&format!("pruned old backup {}", old.display()));
        }
    }
    let mut summary = format!("/etc → {file}");
    let fstype = cx.host_cmd(FS_TYPE, &quick(cfg)).ok().filter(|rc| *rc == 0).map(|_| cx.capture.join("").trim().to_string()).unwrap_or_default();
    if fstype == "zfs" {
        cx.host_cmd(FS_SOURCE, &quick(cfg)).map_err(|e| format!("findmnt: {e}"))?;
        let ds = cx.capture.join("").trim().to_string();
        if !dataset_ok(&ds) {
            return Err(format!("the root dataset name is not usable ({ds:?})"));
        }
        let snap = format!("{ds}@pxu-{ts}");
        match cx.host_cmd(&["zfs", "snapshot", &snap], &quick(cfg)) {
            Ok(0) => summary += &format!("; ZFS snapshot {snap}"),
            Ok(rc) => return Err(format!("the ZFS snapshot {snap} failed (rc {rc})")),
            Err(e) => return Err(format!("the ZFS snapshot failed: {e}")),
        }
    } else {
        let w = format!("no root snapshot (root is {})", if fstype.is_empty() { "not ZFS" } else { &fstype });
        cx.say(&w);
        summary += &format!("; {w}");
    }
    Ok(summary)
}

/// Borra la copia a medias (una copia truncada con nombre válido pasaría por backup) y devuelve el motivo del aborto.
fn drop_partial(cx: &mut Ctx, file: &str, why: String) -> String {
    match cx.host.remove_file(Path::new(file)) {
        Ok(()) => cx.say(&format!("removed the incomplete backup {file}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => cx.say(&format!("WARNING: could not remove the incomplete backup {file}: {e}")),
    }
    why
}

fn versions(cx: &mut Ctx, cfg: &Config) -> BTreeMap<String, String> {
    match cx.host_cmd(DPKG_QUERY, &quick(cfg)) {
        Ok(0) => parse_versions(&cx.capture),
        _ => BTreeMap::new(),
    }
}

fn diff_log(cx: &mut Ctx, before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) {
    cx.say("== package diff (before → after)");
    let mut n = 0;
    for (name, new) in after {
        match before.get(name) {
            Some(old) if old == new => {}
            Some(old) => {
                cx.say(&format!("{name}: {old} → {new}"));
                n += 1;
            }
            None => {
                cx.say(&format!("{name}: (new) → {new}"));
                n += 1;
            }
        }
    }
    for (name, old) in before.iter().filter(|(n, _)| !after.contains_key(*n)) {
        cx.say(&format!("{name}: {old} → (removed)"));
        n += 1;
    }
    if n == 0 {
        cx.say("(no package changed)");
    }
}

/// Cierra la corrida y arma el informe con una sola entrada (vmid 0, «host»). El log lo escribe `runlog` con las líneas del evento.
#[allow(clippy::too_many_arguments)]
fn finish(cx: &mut Ctx, started_at: u64, outcome: Outcome, step: Step, detail: String, aborted: Option<String>, before: usize, reboot: bool, backup: Option<String>) -> RunReport {
    let host = cx.host;
    let at = now_s(host);
    let run = CtRun {
        vmid: 0, name: "host".into(), outcome, step, detail, packages_before: before, packages_after: None, security: cx.security,
        reboot_required: reboot, new_failed_units: vec![], backup, rollback: None, revert: None, log_path: None, took_s: at.saturating_sub(started_at),
    };
    (cx.ev)(Event::Done(run.clone()));
    RunReport { node: String::new(), started_at, finished_at: at, results: vec![run], aborted, cancelled: outcome == Outcome::Cancelled, nvidia: None, log_path: None }
}

// ---------------------------------------------------------------- avisos

/// El mismo aviso único que la corrida completa (con una sola fila: el host).
pub fn messages(report: &RunReport, c: &AppriseCfg) -> Vec<Message> {
    super::messages(report, c)
}

pub fn notify_run(host: &dyn Host, c: &AppriseCfg, report: &RunReport) -> Vec<String> {
    let mut lines = Vec::new();
    for m in messages(report, c) {
        match notify::notify(host, c, &m) {
            notify::Outcome::Sent(t) => lines.push(format!("{}: sent via {t}", m.event)),
            notify::Outcome::Queued(e) => lines.push(format!("{}: send failed ({e}); queued", m.event)),
            notify::Outcome::Lost(e) => lines.push(format!("{}: send failed and NOT queued ({e})", m.event)),
            notify::Outcome::Disabled => {
                lines.push("notifications disabled (no Apprise transport configured)".into());
                break;
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::host::{HostUpdateOptIn, ReadOnlyHost};
    use crate::share::mock::scenario::{World, HOST_UPGRADABLE};
    use crate::share::mock::{MockHost, Recorded};
    use crate::share::proxmox::guests;

    fn confirmed() -> Vec<HostPkg> {
        hostcheck::parse_upgradable(HOST_UPGRADABLE)
    }
    fn host_with(f: impl FnOnce(World) -> World) -> ReadOnlyHost<MockHost> {
        ReadOnlyHost::new(f(World::host_gpu()).build())
    }
    fn go(h: &ReadOnlyHost<MockHost>, conf: Vec<HostPkg>) -> Result<HostOutcome, RunError> {
        let _optin = HostUpdateOptIn::new(h);
        let cfg = Config::load(h).unwrap();
        let inv = guests::load(h);
        run(h, &inv, &cfg, &HostOpts { confirmed: conf }, &AtomicBool::new(false), &mut |_| {})
    }
    fn cmds(h: &ReadOnlyHost<MockHost>) -> Vec<Vec<String>> {
        h.inner().recorded().into_iter().filter_map(|r| if let Recorded::Host(a) = r { Some(a) } else { None }).collect()
    }
    fn writes(h: &ReadOnlyHost<MockHost>) -> Vec<String> {
        cmds(h).iter().filter(|a| is_write_argv(&a.iter().map(String::as_str).collect::<Vec<_>>())).map(|a| a.join(" ")).collect()
    }
    fn report(o: Result<HostOutcome, RunError>) -> RunReport {
        match o.expect("no lock error") {
            HostOutcome::Done(r) => r,
            other => panic!("expected Done, got {other:?}"),
        }
    }
    fn no_install(h: &ReadOnlyHost<MockHost>) -> bool {
        !cmds(h).iter().any(|a| a.iter().map(String::as_str).eq(INSTALL.iter().copied()))
    }

    #[test]
    fn happy_path_runs_the_exact_write_argv_in_order() {
        let h = host_with(|w| w);
        let r = report(go(&h, confirmed()));
        let run = &r.results[0];
        assert_eq!((run.vmid, run.outcome), (0, Outcome::Ok), "{}", run.detail);
        assert!(run.detail.contains("7 packages updated (2 security) · reboot required: yes"), "{}", run.detail);
        assert_eq!(r.exit_code(), 0);
        let w = writes(&h);
        assert_eq!(w.len(), 4, "{w:?}");
        assert_eq!(w[0], "install -d -m 0700 /var/backups/pxu");
        assert!(w[1].starts_with("tar -czpf /var/backups/pxu/etc-") && w[1].ends_with(".tar.gz -C / etc"), "{}", w[1]);
        assert!(w[2].starts_with("chmod 0600 /var/backups/pxu/etc-"), "{}", w[2]);
        assert_eq!(w[3], INSTALL.join(" "));
        assert!(w[3].contains("-y -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold full-upgrade"));
        assert!(!w.iter().any(|a| a.contains("zfs") || a.contains("autoremove")), "ext4 root, nothing to autoremove: {w:?}");
        assert!(run.backup.as_deref().is_some_and(|b| b.contains("no root snapshot (root is ext4)")), "{:?}", run.backup);
        // log del run con el diff antes → después
        let log = run.log_path.clone().expect("log written");
        assert!(runlog::is_run_log_path(Path::new(&log)), "{log}");
        let text = crate::share::host::read_string(&h, &log).unwrap();
        assert!(text.contains("pve-manager: 9.1.1 → 9.1.4"), "{text}");
        // la fila del host se recalculó y no queda en error
        let st = state::load_state(&h);
        assert!(st.host.as_ref().is_some_and(|i| i.error.is_none()));
    }

    #[test]
    fn notification_text_has_the_required_shape() {
        let h = host_with(|w| w);
        let r = report(go(&h, confirmed()));
        let c = AppriseCfg::default();
        let m = messages(&r, &c);
        assert_eq!(m.len(), 1);
        assert!(m[0].title.starts_with("pxu: update run on "), "{}", m[0].title);
        assert!(m[0].body.contains("| host | ✅ ok | 7 (2 security) |"), "{}", m[0].body);
        assert!(m[0].body.contains("📄 Full report: /var/log/pxu/run-"), "{}", m[0].body);
    }

    #[test]
    fn dirty_dpkg_audit_stops_before_touching_anything_and_repair_works() {
        let h = host_with(|w| w.file("/exec/0/dpkg-audit.out", b"The following packages are only half configured:\n pve-manager  half-configured\n"));
        match go(&h, confirmed()).unwrap() {
            HostOutcome::DpkgDirty(d) => assert_eq!(d.len(), 2),
            other => panic!("{other:?}"),
        }
        assert!(writes(&h).is_empty(), "{:?}", writes(&h));
        // reparación con confirmación aparte: dpkg --configure -a y auditoría limpia
        let _optin = HostUpdateOptIn::new(&h);
        let cfg = Config::load(&h).unwrap();
        assert!(matches!(repair_dpkg(&h, &cfg, &mut |_| {}).unwrap(), HostOutcome::Repaired));
        assert_eq!(writes(&h), vec!["dpkg --configure -a".to_string()]);
        drop(_optin);
        // empezar de nuevo: ahora la auditoría sale limpia y se instala
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Ok, "{}", r.results[0].detail);
    }

    #[test]
    fn low_disk_space_aborts_without_writes() {
        let h = host_with(|w| w.file("/exec/0/df.out", b"Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 100 1 900000 99% /\n"));
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Skipped);
        assert!(r.aborted.as_deref().is_some_and(|a| a.contains("not enough free space")), "{:?}", r.aborted);
        assert_eq!(r.exit_code(), 30);
        assert!(writes(&h).is_empty() && no_install(&h));
    }

    #[test]
    fn simulation_that_removes_proxmox_ve_blocks() {
        let h = host_with(|w| w.host_out("apt-sim", "Remv proxmox-ve [9.1.0]\nInst pve-manager [9.1.1] (9.1.4 Proxmox:stable [all])\n"));
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Skipped);
        assert!(r.aborted.as_deref().is_some_and(|a| a.contains("proxmox-ve")), "{:?}", r.aborted);
        assert!(writes(&h).is_empty() && no_install(&h));
    }

    #[test]
    fn simulation_that_removes_the_running_kernel_blocks() {
        let h = host_with(|w| w.host_out("apt-sim", "Remv proxmox-kernel-6.17.9-1-pve-signed [6.17.9-1]\n"));
        let r = report(go(&h, confirmed()));
        assert!(r.aborted.as_deref().is_some_and(|a| a.contains("running kernel")), "{:?}", r.aborted);
        assert!(writes(&h).is_empty());
    }

    #[test]
    fn list_that_differs_from_the_confirmed_one_asks_again() {
        let h = host_with(|w| w);
        let mut old = confirmed();
        old.pop();
        match go(&h, old).unwrap() {
            HostOutcome::Reconfirm(fresh) => assert_eq!(fresh.len(), 7),
            other => panic!("{other:?}"),
        }
        assert!(writes(&h).is_empty() && no_install(&h));
    }

    #[test]
    fn lock_busy_stops_everything() {
        let h = host_with(|w| w);
        for name in ["pxu-run.lock", "pxu-host-update.lock"] {
            let n = cmds(&h).len();
            let _g = {
                let _on = HostUpdateOptIn::new(&h);
                h.try_lock(&h.lock_dir().join(name), true).unwrap().expect("free")
            };
            assert!(matches!(go(&h, confirmed()), Err(RunError::Locked)), "{name}");
            assert_eq!(cmds(&h).len(), n, "no command ran while {name} was held");
        }
    }

    #[test]
    fn apt_failing_midway_marks_the_host_row_in_error_with_log_and_recovery() {
        let h = host_with(|w| w.file("/exec/0/install.exit", b"100").host_out("install", "E: Sub-process /usr/bin/dpkg returned an error code (1)\n"));
        let r = report(go(&h, confirmed()));
        let run = &r.results[0];
        assert_eq!(run.outcome, Outcome::Failed);
        assert_eq!(r.exit_code(), 20);
        assert!(run.detail.contains("rc 100") && run.detail.contains(RECOVERY), "{}", run.detail);
        let log = run.log_path.clone().expect("log");
        assert!(crate::share::host::read_string(&h, &log).unwrap().contains("Sub-process"));
        let err = state::load_state(&h).host.and_then(|i| i.error).expect("host row in error");
        assert!(err.contains(LOG_DIR) && err.contains(RECOVERY), "{err}");
        // sin rollback ni autoremove después del fallo
        assert!(!writes(&h).iter().any(|a| a.contains("autoremove")));
        let m = messages(&r, &AppriseCfg::default());
        assert!(m[0].body.contains(RECOVERY) && m[0].body.contains(&log), "{}", m[0].body);
    }

    #[test]
    fn autoremove_that_would_drop_the_previous_kernel_is_skipped() {
        let h = host_with(|w| {
            w.host_out("autoremove-sim", "Remv proxmox-kernel-6.17.9-1-pve-signed [6.17.9-1]\nRemv libfoo1 [1.0]\n")
                .host_out("dpkg-query-after", "proxmox-kernel-6.17.9-1-pve-signed 6.17.9-1\nproxmox-kernel-6.17.13-2-pve-signed 6.17.13-2\n")
        });
        let r = report(go(&h, confirmed()));
        let run = &r.results[0];
        assert_eq!(run.outcome, Outcome::Warn, "{}", run.detail);
        assert!(run.detail.contains("autoremove skipped"), "{}", run.detail);
        assert!(!writes(&h).iter().any(|a| a.contains("autoremove")), "{:?}", writes(&h));
    }

    #[test]
    fn safe_autoremove_runs() {
        let h = host_with(|w| w.host_out("autoremove-sim", "Remv libfoo1 [1.0]\n"));
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Ok, "{}", r.results[0].detail);
        assert_eq!(writes(&h).last().unwrap(), &AUTOREMOVE.join(" "));
    }

    #[test]
    fn zfs_root_gets_a_snapshot_and_ext4_does_not() {
        let z = host_with(|w| w.host_out("findmnt-fstype", "zfs\n").host_out("findmnt-source", "rpool/ROOT/pve-1\n"));
        let r = report(go(&z, confirmed()));
        let w = writes(&z);
        let snap = w.iter().find(|a| a.starts_with("zfs snapshot rpool/ROOT/pve-1@pxu-")).unwrap_or_else(|| panic!("{w:?}"));
        assert!(r.results[0].backup.as_deref().is_some_and(|b| b.contains(snap.trim_start_matches("zfs snapshot "))), "{:?}", r.results[0].backup);
        // un dataset raro no se acepta
        let bad = host_with(|w| w.host_out("findmnt-fstype", "zfs\n").host_out("findmnt-source", "rpool/ROOT/pve 1\n"));
        let r = report(go(&bad, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Skipped);
        assert!(no_install(&bad));
        // LVM/ext4: sin snapshot de raíz
        let e = host_with(|w| w);
        go(&e, confirmed()).unwrap();
        assert!(!writes(&e).iter().any(|a| a.contains("zfs")));
    }

    #[test]
    fn backup_uses_zstd_when_present_and_keeps_only_the_five_newest() {
        let mut names = Vec::new();
        let h = host_with(|w| {
            let mut w = w.file("/usr/bin/zstd", b"");
            for d in 1..=6 {
                let n = format!("/var/backups/pxu/etc-2026010{d}030000.tar.zst");
                w = w.file(&n, b"old");
                names.push(n);
            }
            w
        });
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Ok, "{}", r.results[0].detail);
        assert!(writes(&h)[1].starts_with("tar --zstd -cpf /var/backups/pxu/etc-") && writes(&h)[1].ends_with(".tar.zst -C / etc"));
        let left: Vec<_> = h.read_dir(Path::new(BACKUP_DIR)).unwrap().into_iter().filter(|p| is_backup_path(p)).collect();
        assert_eq!(left.len(), KEEP_BACKUPS, "{left:?}");
        assert!(!left.iter().any(|p| p.to_string_lossy().contains("20260101")) && !left.iter().any(|p| p.to_string_lossy().contains("20260102")));
    }

    #[test]
    fn tar_failure_aborts_before_installing() {
        let h = host_with(|w| w.file("/exec/0/tar.exit", b"2"));
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Skipped);
        assert!(r.aborted.as_deref().is_some_and(|a| a.contains("tar rc 2")), "{:?}", r.aborted);
        assert!(no_install(&h));
    }

    fn writes_of(h: &ReadOnlyHost<MockHost>) -> usize {
        writes(h).len()
    }
    fn apt_lists(h: &ReadOnlyHost<MockHost>) -> usize {
        cmds(h).iter().filter(|a| a.iter().map(String::as_str).eq(hostcheck::APT_LIST.iter().copied())).count()
    }

    #[test]
    fn apt_failure_names_the_run_log_after_the_run_start_and_that_file_exists() {
        // el reloj avanza 90 s durante el full-upgrade: el nombre del log usa la hora de INICIO
        let h = host_with(|w| w.file("/exec/0/install.exit", b"100").file("/exec/0/install.advance", b"90").host_out("install", "E: Sub-process /usr/bin/dpkg returned an error code (1)\n"));
        let started = now_s(&h);
        let r = report(go(&h, confirmed()));
        assert!(now_s(&h) >= started + 90, "the mock clock moved");
        let s = stamp(started);
        let expect = format!("{LOG_DIR}/run-{}-{}.log", &s[..8], &s[8..12]);
        assert_eq!(r.log_path.as_deref(), Some(expect.as_str()));
        assert_eq!(r.results[0].log_path.as_deref(), Some(expect.as_str()));
        assert!(h.exists(Path::new(&expect)), "the stored path exists");
        let text = crate::share::host::read_string(&h, &expect).unwrap();
        assert!(text.starts_with("pxu-run-log 1\n") && text.contains("Sub-process") && text.contains("── host ──"), "{text}");
    }

    #[test]
    fn the_run_log_has_one_host_section_with_timestamped_lines_and_no_legacy_files() {
        let h = host_with(|w| w);
        let r = report(go(&h, confirmed()));
        let text = crate::share::host::read_string(&h, r.log_path.as_deref().unwrap()).unwrap();
        assert!(text.contains("── host ──\n"), "{text}");
        assert!(text.contains("package diff") && text.contains("$ tar "), "{text}");
        let files = h.read_dir(Path::new(LOG_DIR)).unwrap_or_default();
        assert!(files.iter().all(|p| runlog::is_run_log_path(p)), "only the run log: {files:?}");
    }

    #[test]
    fn a_list_that_changes_during_the_backup_asks_again_without_installing() {
        let mut more = HOST_UPGRADABLE.to_string();
        more.push_str("newpkg/stable-security 2.0 amd64 [upgradable from: 1.0]\n");
        let h = host_with(|w| w.host_out("apt-list-late", &more));
        match go(&h, confirmed()).unwrap() {
            HostOutcome::Reconfirm(fresh) => assert_eq!(fresh.len(), 8),
            other => panic!("{other:?}"),
        }
        assert!(no_install(&h));
        assert_eq!(apt_lists(&h), 2, "list checked before and after the backup");
        // la copia de /etc ya estaba hecha y la repetición va DESPUÉS de tar
        let c = cmds(&h);
        let tar = c.iter().position(|a| a[0] == "tar").unwrap();
        let last_list = c.iter().rposition(|a| a.first().map(String::as_str) == Some("apt") && a.get(1).map(String::as_str) == Some("list")).unwrap();
        assert!(last_list > tar);
    }

    #[test]
    fn a_blocking_simulation_after_the_backup_aborts_without_installing() {
        let h = host_with(|w| w.host_out("apt-sim-late", "Remv proxmox-ve [9.1.0]\n"));
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Skipped);
        assert!(r.aborted.as_deref().is_some_and(|a| a.contains("proxmox-ve")), "{:?}", r.aborted);
        assert!(no_install(&h));
        assert!(writes_of(&h) >= 3, "the backup was made first: {:?}", writes(&h));
    }

    #[test]
    fn any_unexpected_removal_blocks_but_an_old_kernel_is_allowed() {
        let h = host_with(|w| w.host_out("apt-sim", "Remv libfoo1 [1.0]\nRemv libbar2 [2.0]\nInst pve-manager [9.1.1] (9.1.4 Proxmox:stable [all])\n"));
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Skipped);
        let why = r.aborted.as_deref().unwrap_or("");
        assert!(why.contains("libfoo1") && why.contains("libbar2"), "{why}");
        assert!(writes(&h).is_empty() && no_install(&h));
        // un kernel VIEJO (no el en uso, 6.17.9-1-pve) se puede quitar
        let ok = host_with(|w| w.host_out("apt-sim", "Remv proxmox-kernel-6.14.1-2-pve-signed [6.14.1-2]\nInst pve-manager [9.1.1] (9.1.4 Proxmox:stable [all])\n"));
        let r = report(go(&ok, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Ok, "{}", r.results[0].detail);
        assert!(!no_install(&ok));
        // el kernel en uso sigue bloqueando
        assert!(sim_blocker("Remv proxmox-kernel-6.17.9-1-pve-signed [6.17.9-1]\n", "6.17.9-1-pve").is_some_and(|b| b.contains("running kernel")));
        assert!(sim_blocker("Remv proxmox-kernel-6.14.1-2-pve-signed [x]\n", "6.17.9-1-pve").is_none());
        assert!(sim_blocker("Remv proxmox-kernel-6.17 [x]\n", "6.17.9-1-pve").is_some(), "a meta-package is not an old kernel");
    }

    #[test]
    fn a_failed_tar_deletes_the_partial_copy_and_aborts() {
        let h = host_with(|w| w.file("/exec/0/tar.exit", b"2"));
        let r = report(go(&h, confirmed()));
        assert_eq!(r.results[0].outcome, Outcome::Skipped);
        assert!(no_install(&h));
        let left: Vec<_> = h.read_dir(Path::new(BACKUP_DIR)).unwrap_or_default().into_iter().filter(|p| is_backup_path(p)).collect();
        assert!(left.is_empty(), "partial copy removed: {left:?}");
        let log = crate::share::host::read_string(&h, r.results[0].log_path.as_deref().unwrap()).unwrap();
        assert!(log.contains("removed the incomplete backup"), "{log}");
        // no se borra una copia anterior buena
        let h = host_with(|w| w.file("/exec/0/tar.exit", b"2").file("/var/backups/pxu/etc-20260101030000.tar.gz", b"good"));
        go(&h, confirmed()).unwrap();
        assert!(h.exists(Path::new("/var/backups/pxu/etc-20260101030000.tar.gz")));
    }

    #[test]
    fn the_backup_dir_must_be_a_real_root_0700_directory() {
        for (meta, why) in [("0700 0 symlink", "not usable"), ("0755 0 dir", "0700"), ("0700 1000 dir", "root"), ("0700 0 file", "not a directory")] {
            let h = host_with(|w| w.file("/var/backups/pxu/.old", b"x").file("/exec/0/backup-dir.meta", meta.as_bytes()));
            let r = report(go(&h, confirmed()));
            assert_eq!(r.results[0].outcome, Outcome::Skipped, "{meta}");
            assert!(r.aborted.as_deref().is_some_and(|a| a.contains(why)), "{meta}: {:?}", r.aborted);
            assert!(no_install(&h), "{meta}");
            assert!(!cmds(&h).iter().any(|a| a[0] == "tar" || a[0] == "chmod"), "{meta}: nothing written into it");
        }
        // un symlink se rechaza antes de `install -d` (que lo seguiría)
        let h = host_with(|w| w.file("/var/backups/pxu/.old", b"x").file("/exec/0/backup-dir.meta", b"0700 0 symlink"));
        go(&h, confirmed()).unwrap();
        assert!(writes(&h).is_empty(), "{:?}", writes(&h));
    }

    #[test]
    fn only_run_logs_can_be_appended_and_need_an_opt_in() {
        let h = ReadOnlyHost::new(World::host_gpu().build());
        let p = Path::new("/var/log/pxu/run-20261109-1200.log");
        assert!(h.write_file(p, b"x").is_err(), "denied without the opt-in");
        assert!(h.append_file(p, b"x").is_err());
        let _g = HostUpdateOptIn::new(&h);
        assert!(h.write_file(p, b"x").is_ok());
        assert!(h.append_file(p, b"y").is_ok(), "the live run log grows");
        assert!(h.append_file(Path::new("/var/log/pxu/other.log"), b"x").is_err());
        assert!(h.append_file(Path::new("/var/log/pxu/host-20261109120000.log"), b"x").is_err());
        assert!(h.write_file(Path::new("/var/log/pxu/other.log"), b"x").is_err());
        assert!(h.write_file(Path::new("/var/log/pxu/host-20261109120000.log"), b"x").is_err(), "the old host log name is no longer written");
        h.inner().write_file(Path::new("/var/log/pxu/host-20261109120000.log"), b"old").unwrap();
        assert!(h.remove_file(Path::new("/var/log/pxu/host-20261109120000.log")).is_ok(), "but the old logs can still be pruned");
        assert!(h.append_file(Path::new("/etc/passwd"), b"x").is_err());
    }

    #[test]
    fn write_whitelist_is_exact() {
        for ok in [
            vec!["dpkg", "--configure", "-a"],
            vec!["tar", "--zstd", "-cpf", "/var/backups/pxu/etc-20261109120000.tar.zst", "-C", "/", "etc"],
            vec!["tar", "-czpf", "/var/backups/pxu/etc-20261109120000.tar.gz", "-C", "/", "etc"],
            vec!["chmod", "0600", "/var/backups/pxu/etc-20261109120000.tar.gz"],
            vec!["zfs", "snapshot", "rpool/ROOT/pve-1@pxu-20261109120000"],
        ] {
            assert!(is_write_argv(&ok), "{ok:?}");
        }
        for bad in [
            vec!["apt-get", "-y", "upgrade"],
            vec!["apt-get", "-y", "full-upgrade"],
            vec!["apt", "full-upgrade", "-y"],
            vec!["apt-get", "-y", "--force-yes", "full-upgrade"],
            vec!["apt-get", "-y", "--allow-downgrades", "full-upgrade"],
            vec!["nice", "apt-get", "-y", "full-upgrade"],
            vec!["tar", "-czpf", "/tmp/etc-20261109120000.tar.gz", "-C", "/", "etc"],
            vec!["tar", "-czpf", "/var/backups/pxu/../x/etc-20261109120000.tar.gz", "-C", "/", "etc"],
            vec!["chmod", "0777", "/var/backups/pxu/etc-20261109120000.tar.gz"],
            vec!["chmod", "0600", "/etc/shadow"],
            vec!["zfs", "destroy", "rpool/ROOT/pve-1@pxu-20261109120000"],
            vec!["zfs", "snapshot", "rpool/ROOT/pve-1@manual"],
            vec!["reboot"],
            vec!["pct", "exec", "100", "--", "apt-get", "upgrade"],
        ] {
            assert!(!is_write_argv(&bad), "{bad:?}");
        }
    }

    #[test]
    fn host_writes_need_their_own_optin_and_ct_optin_is_not_enough() {
        let h = ReadOnlyHost::new(World::host_gpu().build());
        let mut all: Vec<Vec<&str>> = vec![INSTALL.to_vec(), AUTOREMOVE.to_vec(), DPKG_CONFIGURE.to_vec(), BACKUP_MKDIR.to_vec(), vec!["chmod", "0600", "/var/backups/pxu/etc-20261109120000.tar.gz"]];
        all.extend([DPKG_AUDIT.to_vec(), DF.to_vec(), AUTOREMOVE_SIM.to_vec()]);
        let run_all = |h: &ReadOnlyHost<MockHost>| -> Vec<bool> { all.iter().map(|a| h.host_cmd(a, &Default::default(), &mut Vec::<String>::new()).is_ok()).collect() };
        assert!(run_all(&h).iter().all(|ok| !ok), "denied by default");
        h.set_update_enabled(true);
        assert!(run_all(&h).iter().all(|ok| !ok), "the CT opt-in does not unlock host apt");
        h.set_update_enabled(false);
        {
            let _g = HostUpdateOptIn::new(&h);
            assert!(run_all(&h).iter().all(|ok| *ok), "allowed under the host opt-in");
            assert!(h.host_cmd(&["apt-get", "-y", "upgrade"], &Default::default(), &mut Vec::<String>::new()).is_err());
        }
        assert!(run_all(&h).iter().all(|ok| !ok), "the guard closes it again");
    }

    #[test]
    fn nothing_automatic_ever_reaches_the_host() {
        // plain engine (timer / `pxu update`) con el host pendiente y auto_update en CT
        let h = ReadOnlyHost::new(World::host_gpu().ct_config(134, &["auto_update = true"]).build());
        h.set_update_enabled(true);
        let cfg = Config::load(&h).unwrap();
        let inv = guests::load(&h);
        let cands = crate::share::update::select(&h, &inv, &cfg, &[]);
        assert!(cands.iter().all(|c| c.vmid != 0), "the host is not a candidate");
        let c = crate::share::update::select(&h, &inv, &cfg, &[0]);
        assert!(c.iter().all(|c| c.skip.is_some()), "an explicit vmid 0 is skipped: {c:?}");
        let _ = crate::share::update::run(&h, &inv, &cfg, &crate::share::update::Opts { only: vec![] }, &AtomicBool::new(false), &mut |_| {});
        let _ = crate::share::update::run(&h, &inv, &cfg, &crate::share::update::Opts { only: vec![0] }, &AtomicBool::new(false), &mut |_| {});
        assert!(no_install(&h), "no host full-upgrade: {:?}", cmds(&h));
        assert!(!cmds(&h).iter().any(|a| a.first().map(String::as_str) == Some("env") || a.first().map(String::as_str) == Some("dpkg")), "{:?}", cmds(&h));
    }

    #[test]
    fn auto_update_on_vmid_0_is_rejected() {
        let h = ReadOnlyHost::new(World::host_gpu().config_raw("[[ct]]\nvmid = 0\nauto_update = true\n").build());
        let e = Config::load_checked(&h).expect_err("rejected");
        assert!(e.to_string().contains("vmid = 0") && e.to_string().contains("auto_update"), "{e}");
        // el motor tampoco lo respeta aunque la config se haya cargado sin validar
        let cfg = Config::load(&h).unwrap();
        assert!(!cfg.auto_update(0, &[]));
    }

    // ---- corrida programada con `host_auto` (el host primero)

    use crate::share::host::UpdateOptIn;
    use crate::share::update::{self, Opts};

    /// Host Proxmox con CT134 `auto_update` y las revisiones ya guardadas (host y CT recientes).
    fn sched_world(f: impl FnOnce(World) -> World) -> ReadOnlyHost<MockHost> {
        let h = ReadOnlyHost::new(f(World::host_gpu().ct_config(134, &["auto_update = true"])).build());
        let cfg = Config::load(&h).unwrap();
        let rep = crate::share::update::check::run(&h, &guests::load(&h), &cfg, &crate::share::update::check::CheckOpts::from_config(&cfg, Some(false)), &[], &AtomicBool::new(false), &mut |_| {});
        crate::share::update::check::persist(&h, &rep).unwrap();
        h
    }
    fn timer_run(h: &ReadOnlyHost<MockHost>) -> RunReport {
        let _ct_optin = UpdateOptIn::new(h);
        let cfg = Config::load(h).unwrap();
        let inv = guests::load(h);
        update::run(h, &inv, &cfg, &Opts { only: vec![] }, &AtomicBool::new(false), &mut |_| {}).expect("no lock error")
    }
    fn is_install(r: &Recorded) -> bool {
        matches!(r, Recorded::Host(a) if a.iter().map(String::as_str).eq(INSTALL.iter().copied()))
    }
    /// Comandos registrados desde `from` (para ignorar la revisión previa de `sched_world`).
    fn since(h: &ReadOnlyHost<MockHost>, from: usize) -> Vec<Recorded> {
        h.inner().recorded().into_iter().skip(from).collect()
    }

    #[test]
    fn the_timer_does_not_touch_the_host_by_default() {
        let h = sched_world(|w| w);
        let cfg = Config::load(&h).unwrap();
        assert!(!cfg.update.host_auto, "host_auto is off unless the config says so");
        let rep = timer_run(&h);
        assert!(rep.results.iter().all(|r| r.vmid != 0), "{:?}", rep.results);
        assert!(!h.inner().recorded().iter().any(is_install) && writes(&h).is_empty(), "{:?}", writes(&h));
        assert_eq!(rep.results.iter().find(|r| r.vmid == 134).map(|r| r.outcome), Some(Outcome::Ok));
    }

    #[test]
    fn with_host_auto_the_host_goes_before_the_first_ct_with_the_same_safeguards() {
        let h = sched_world(|w| w.host_auto());
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        assert_eq!(rep.results[0].vmid, 0, "the host is the first entry");
        let host = &rep.results[0];
        assert_eq!(host.outcome, Outcome::Ok, "{}", host.detail);
        assert!(host.reboot_required && host.detail.contains("reboot required: yes"), "{}", host.detail);
        assert_eq!(rep.results.iter().find(|r| r.vmid == 134).unwrap().outcome, Outcome::Ok);
        let all = since(&h, n0);
        let install = all.iter().position(is_install).expect("host full-upgrade ran");
        let first_ct = all.iter().position(|r| matches!(r, Recorded::Guest(..))).expect("a CT command ran");
        assert!(install < first_ct, "host before any CT command ({install} vs {first_ct})");
        // secuencia exacta de escrituras del host: mismas salvaguardas que a mano, nada más
        let w = writes(&h);
        assert_eq!(w.len(), 4, "{w:?}");
        assert_eq!(w[0], "install -d -m 0700 /var/backups/pxu");
        assert!(w[1].starts_with("tar -czpf /var/backups/pxu/etc-") && w[2].starts_with("chmod 0600 /var/backups/pxu/etc-"), "{w:?}");
        assert_eq!(w[3], INSTALL.join(" "));
        let seq: Vec<String> = all.iter().filter_map(|r| if let Recorded::Host(a) = r { Some(a.join(" ")) } else { None }).collect();
        let pos = |p: &str| seq.iter().position(|c| c.starts_with(p)).unwrap_or_else(|| panic!("{p} missing in {seq:?}"));
        assert!(pos("dpkg --audit") < pos("df -Pk") && pos("df -Pk") < pos("apt-get -s") && pos("apt-get -s") < pos("install -d") && pos("install -d") < pos("tar") && pos("tar") < pos("env"), "{seq:?}");
        // jamás se reinicia el host
        assert!(!seq.iter().any(|c| c.contains("reboot") || c.contains("shutdown")), "{seq:?}");
        // informe y avisos
        let j = rep.to_json();
        assert_eq!(j["host"]["vmid"], 0);
        assert!(j["cts"].as_array().unwrap().iter().all(|c| c["vmid"] != 0));
        let m = update::messages(&rep, &AppriseCfg::default());
        assert_eq!(m.len(), 1, "one notice per run");
        let summary = &m[0];
        assert!(summary.body.contains("| host | ✅ ok |") && summary.body.contains("reboot the host") && summary.body.contains("REBOOT"), "{}", summary.body);
    }

    #[test]
    fn a_failed_host_leaves_every_ct_untouched_and_fails_the_run() {
        let h = sched_world(|w| w.host_auto().file("/exec/0/install.exit", b"100"));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        assert_eq!((rep.results[0].vmid, rep.results[0].outcome), (0, Outcome::Failed), "{}", rep.results[0].detail);
        let ct = rep.results.iter().find(|r| r.vmid == 134).unwrap();
        assert_eq!(ct.outcome, Outcome::Skipped);
        assert!(ct.detail.contains("host update failed"), "{}", ct.detail);
        assert!(!since(&h, n0).iter().any(|r| matches!(r, Recorded::Guest(..))), "no CT received any command");
        assert_eq!(rep.exit_code(), update::EXIT_ABORTED);
        let m = update::messages(&rep, &AppriseCfg::default());
        assert!(m.len() == 1 && m[0].level == crate::share::notify::Level::Failure && m[0].body.contains(RECOVERY), "{m:?}");
    }

    #[test]
    fn a_host_with_nothing_pending_lets_the_cts_follow() {
        let h = sched_world(|w| w.host_auto().host_out("apt-list", "Listing...\n"));
        let rep = timer_run(&h);
        assert!(rep.results.iter().all(|r| r.vmid != 0), "{:?}", rep.results);
        assert!(!h.inner().recorded().iter().any(is_install));
        assert_eq!(rep.results.iter().find(|r| r.vmid == 134).unwrap().outcome, Outcome::Ok);
        let log = String::from_utf8(h.read_file(std::path::Path::new(rep.log_path.as_deref().expect("log"))).unwrap()).unwrap();
        assert!(log.contains("host: skipped — no updates pending"), "the log says why the host was left out:\n{log}");
    }

    #[test]
    fn an_old_host_check_skips_the_host_but_the_cts_follow() {
        let h = sched_world(|w| w.host_auto());
        let mut st = state::load_state(&h);
        st.host.as_mut().unwrap().checked_at -= 3 * 3600;
        state::save_state(&h, &st).unwrap();
        let rep = timer_run(&h);
        let host = &rep.results[0];
        assert_eq!((host.vmid, host.outcome), (0, Outcome::Skipped));
        assert!(host.detail.contains("older than"), "{}", host.detail);
        assert!(writes(&h).is_empty() && !h.inner().recorded().iter().any(is_install));
        assert_eq!(rep.results.iter().find(|r| r.vmid == 134).unwrap().outcome, Outcome::Ok);
        assert_eq!(rep.exit_code(), 0);
    }

    #[test]
    fn host_auto_is_backward_compatible_and_validated() {
        let c: Config = toml::from_str("[update]\nstopped = \"skip\"\n").unwrap();
        assert!(!c.update.host_auto);
        let h = ReadOnlyHost::new(World::host_gpu().config_raw("[update]\nhost_auto = true\n").build());
        let (c, _) = Config::load_checked(&h).unwrap();
        assert!(c.update.host_auto);
        let t = crate::share::config::set_host_auto("# mi config\n[update]\nstopped = \"skip\"\n", true).unwrap();
        assert_eq!(t, "# mi config\n[update]\nstopped = \"skip\"\nhost_auto = true\n");
        assert!(crate::share::config::set_host_auto("", true).unwrap().contains("host_auto = true"));
    }

    // ---- controlador NVIDIA en la corrida programada

    fn gpu_world(f: impl FnOnce(World) -> World) -> ReadOnlyHost<MockHost> {
        sched_world(|w| f(w.nvidia_run_driver(&["595.112.07", "595.120.01", "596.10.01"]).file("/proc/sys/kernel/osrelease", b"6.17.13-2-pve\n").file("/lib/modules/6.17.13-2-pve/build/.keep", b"")))
    }
    fn argvs(h: &ReadOnlyHost<MockHost>, n0: usize) -> Vec<String> {
        since(h, n0).iter().filter_map(|r| match r { Recorded::Host(a) => Some(a.join(" ")), _ => None }).collect()
    }
    fn all_cmds(h: &ReadOnlyHost<MockHost>, n0: usize) -> Vec<String> {
        since(h, n0).iter().map(|r| match r { Recorded::Host(a) => format!("host {}", a.join(" ")), Recorded::Guest(id, a) => format!("ct{id} {}", a.join(" ")) }).collect()
    }

    #[test]
    fn without_the_opt_in_the_timer_never_touches_nvidia() {
        let h = gpu_world(|w| w.host_auto().ct_config(135, &["auto_update = true", "nvidia_auto = true"]));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        assert!(rep.nvidia.is_none());
        let c = all_cmds(&h, n0);
        let bad = ["pxu-nvidia", "download.nvidia.com", "--dkms", "--no-kernel-modules", "modprobe", "nvidia-uninstall", "nvidia-persistenced"];
        assert!(!c.iter().any(|a| bad.iter().any(|b| a.contains(b))), "{:?}", c.iter().filter(|a| bad.iter().any(|b| a.contains(b))).collect::<Vec<_>>());
    }

    #[test]
    fn driver_goes_after_the_host_and_before_the_cts() {
        let h = gpu_world(|w| w.host_auto().host_out("apt-list", "Listing...\nhtop/stable 3.4.1-1 amd64 [upgradable from: 3.4.0-1]\n").host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"]));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        let nv = rep.nvidia.as_ref().expect("driver entry");
        assert_eq!(nv.outcome, Outcome::Ok, "{}", nv.detail);
        assert_eq!((nv.from.as_deref(), nv.to.as_deref()), (Some("595.112.07"), Some("595.120.01")));
        let c = all_cmds(&h, n0);
        let pos = |p: &str| c.iter().position(|a| a.contains(p)).unwrap_or_else(|| panic!("{p} missing in {c:#?}"));
        let host_upd = c.iter().position(|a| a.starts_with("host ") && a.contains(&INSTALL.join(" "))).expect("host update");
        let install = pos("--dkms");
        let libs = pos("--no-kernel-modules");
        let ct_apt = c.iter().position(|a| a.starts_with("ct134") && !a.contains("nvidia") && a.contains("apt")).expect("CT apt");
        assert!(host_upd < pos("--check") && pos("--check") < install && install < libs && libs < ct_apt, "{c:#?}");
        // el módulo nuevo quedó cargado y no se reinició el host
        assert!(!c.iter().any(|a| a == "host reboot" || a.starts_with("host shutdown") || a.starts_with("host systemctl reboot")), "{c:?}");
        let j = rep.to_json();
        assert_eq!(j["nvidia"]["to"], "595.120.01");
        assert!(rep.results.iter().find(|r| r.vmid == 134).is_some());
    }

    #[test]
    fn pending_reboot_skips_the_driver() {
        // host_auto actualiza y deja reinicio pendiente (el host de prueba lo marca)
        let h = gpu_world(|w| w.host_auto().host_nvidia_auto().file("/var/run/reboot-required", b""));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Skipped, "{}", nv.detail);
        assert!(nv.detail.contains("reboot"), "{}", nv.detail);
        assert!(!argvs(&h, n0).iter().any(|a| a.contains("--dkms") || a.contains("modprobe")), "{:?}", argvs(&h, n0));
    }

    #[test]
    fn driver_failure_gives_no_libs_to_any_ct_and_keeps_going_safely() {
        let h = gpu_world(|w| w.host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"]).file("/exec/0/nvidia-install.exit", b"1"));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Failed, "{}", nv.detail);
        let c = all_cmds(&h, n0);
        assert!(!c.iter().any(|a| a.starts_with("ct") && a.contains("--no-kernel-modules")), "{c:#?}");
        assert!(rep.exit_code() != 0);
        let m = update::messages(&rep, &AppriseCfg::default());
        assert!(m.len() == 1 && m[0].level == crate::share::notify::Level::Failure && m[0].title.contains("NVIDIA failed"), "{m:?}");
    }

    #[test]
    fn sha_mismatch_stops_before_touching_anything() {
        let h = gpu_world(|w| w.host_nvidia_auto().file("/exec/0/nvidia-sha256sum.out", b"deadbeef  x\n"));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        assert_eq!(rep.nvidia.as_ref().unwrap().outcome, Outcome::Failed);
        let a = argvs(&h, n0);
        assert!(!a.iter().any(|a| a.contains("modprobe") || a.contains("--dkms") || a.starts_with("pct push")), "{a:?}");
    }

    fn nvidia_aborted_untouched(w: impl FnOnce(World) -> World, why: &str) {
        let h = gpu_world(|x| w(x.host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"])));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Failed, "{}", nv.detail);
        assert!(nv.detail.contains(why), "{}", nv.detail);
        let c = all_cmds(&h, n0);
        let bad = ["install -d", "download.nvidia.com/XFree86/Linux-x86_64/NVIDIA", "--dkms", "modprobe", "pct push", "--no-kernel-modules"];
        assert!(!c.iter().any(|a| bad.iter().any(|b| a.contains(b))), "{c:#?}");
    }

    #[test]
    fn nvidia_work_dir_that_is_a_symlink_is_refused() {
        nvidia_aborted_untouched(|w| w.file("/exec/0/nvidia-workdir.meta", b"0700 0 symlink"), "symlink");
    }

    #[test]
    fn nvidia_work_dir_with_another_owner_is_refused() {
        nvidia_aborted_untouched(|w| w.file("/exec/0/nvidia-workdir.meta", b"0700 1000 dir"), "not owned by root");
    }

    #[test]
    fn nvidia_work_dir_with_a_loose_mode_is_refused() {
        nvidia_aborted_untouched(|w| w.file("/exec/0/nvidia-workdir.meta", b"0755 0 dir"), "mode 0700");
    }

    #[test]
    fn nvidia_work_dir_that_is_a_file_is_refused() {
        nvidia_aborted_untouched(|w| w.file("/exec/0/nvidia-workdir.meta", b"0700 0 file"), "not a directory");
    }

    #[test]
    fn nvidia_ct_hostile_temp_dir_is_refused_before_copying() {
        for hostile in ["/var/tmp/pxu-nvidia.run", "/var/lib/pxu-nvidia.../../../tmp/x", "/tmp"] {
            let h = gpu_world(|w| w.host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"]).file("/exec/134/nvidia-mktemp.out", hostile.as_bytes()));
            let n0 = h.inner().recorded().len();
            let rep = timer_run(&h);
            let ct = rep.nvidia.as_ref().unwrap().cts.iter().find(|c| c.vmid == 134).expect("ct entry");
            assert_eq!(ct.outcome, Outcome::Failed, "{hostile}: {}", ct.detail);
            let c = all_cmds(&h, n0);
            assert!(!c.iter().any(|a| a.contains("pct push 134") || a.starts_with("ct134 ") && a.contains("--no-kernel-modules") || a.contains(hostile) && a.starts_with("ct134 rm")), "{hostile}: {c:#?}");
        }
    }

    #[test]
    fn nvidia_ct_copy_that_differs_is_not_executed() {
        let h = gpu_world(|w| w.host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"]).file("/exec/134/nvidia-ct-sha.out", b"deadbeef  x\n"));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        let ct = rep.nvidia.as_ref().unwrap().cts.iter().find(|c| c.vmid == 134).expect("ct entry");
        assert_eq!(ct.outcome, Outcome::Failed, "{}", ct.detail);
        let c = all_cmds(&h, n0);
        assert!(!c.iter().any(|a| a.starts_with("ct134 ") && a.contains("--no-kernel-modules")), "{c:#?}");
        assert!(c.iter().any(|a| a.starts_with("ct134 rm -rf -- /var/lib/pxu-nvidia.")), "the temp dir is removed: {c:#?}");
    }

    /// Entrada manual (`--nvidia` o la opción del menú `u`): mismo opt-in de escritura que la corrida de CT, sin `host_nvidia_auto`.
    fn manual_run(h: &ReadOnlyHost<MockHost>, mode: crate::share::update::nvidia::Mode) -> RunReport {
        let _ct_optin = UpdateOptIn::new(h);
        let cfg = Config::load(h).unwrap();
        let inv = guests::load(h);
        update::run_nvidia(h, &inv, &cfg, mode, &AtomicBool::new(false), &mut |_| {}).expect("no lock error")
    }

    #[test]
    fn nvidia_reinstalls_the_host_even_when_the_version_matches_and_the_timer_does_not() {
        // el índice solo llega a la versión cargada (595.112.07): el timer no hace nada, el modo manual reinstala
        let h = gpu_world(|w| w.nvidia_run_driver(&["595.112.07"]).ct_config(134, &["auto_update = true", "nvidia_auto = true"]));
        let n0 = h.inner().recorded().len();
        assert!(timer_run(&h).nvidia.is_none(), "without host_nvidia_auto and with the same version the timer leaves the driver alone");
        assert!(!argvs(&h, n0).iter().any(|a| a.contains("--dkms")), "{:?}", argvs(&h, n0));
        let n1 = h.inner().recorded().len();
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::Nvidia);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Ok, "{}", nv.detail);
        assert_eq!((nv.from.as_deref(), nv.to.as_deref()), (Some("595.112.07"), Some("595.112.07")));
        let c = all_cmds(&h, n1);
        assert!(c.iter().any(|a| a.starts_with("host ") && a.contains("--dkms")), "{c:#?}");
        assert!(c.iter().any(|a| a.starts_with("ct134 ") && a.contains("--no-kernel-modules")), "every GPU CT gets its libs: {c:#?}");
        assert!(!nv.reboot_needed && !c.iter().any(|a| a == "host reboot" || a.starts_with("host systemctl reboot")), "{c:?}");
    }

    /// Host con GPU, sin driver NVIDIA y sin cabeceras del kernel en marcha (el caso de `--nvidia` que instala desde cero).
    fn fresh_world(f: impl FnOnce(World) -> World) -> ReadOnlyHost<MockHost> {
        gpu_world(|w| f(w.nvidia_run_driver(&["595.112.07", "595.120.01"]).without_file("/usr/bin/nvidia-uninstall").without_file("/proc/driver/nvidia/version").without_file("/lib/modules/6.17.13-2-pve/build/.keep").file("/exec/0/nvidia-chips.out", b"<tr id=\"devid24B0\"><td>RTX A4000</td></tr>\n")))
    }

    #[test]
    fn nvidia_installs_the_driver_from_scratch_when_the_host_has_none() {
        let h = fresh_world(|w| w);
        let n0 = h.inner().recorded().len();
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::Nvidia);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Ok, "{}", nv.detail);
        assert_eq!((nv.from.as_deref(), nv.to.as_deref()), (None, Some("595.120.01")), "the newest in the index that lists the GPU");
        let c = all_cmds(&h, n0);
        let pos = |p: &str| c.iter().position(|a| a.contains(p)).unwrap_or_else(|| panic!("{p} missing in {c:#?}"));
        assert!(pos("apt-get -y install proxmox-headers-6.17.13-2-pve") < pos("update-initramfs -u") && pos("update-initramfs -u") < pos("--dkms"), "{c:#?}");
        assert!(c.iter().any(|a| a.starts_with("ct134 ") && a.contains("--no-kernel-modules")), "the GPU CTs get their libs: {c:#?}");
        assert!(c.iter().any(|a| a.contains("--kernel-module-type=open")), "an Ampere GPU takes the open module: {c:#?}");
        assert!(h.inner().exists(std::path::Path::new(crate::share::update::nvidia::NOUVEAU_CONF)), "nouveau is blocked for the next boot");
        assert!(!nv.reboot_needed, "{}", nv.detail);
    }

    #[test]
    fn nvidia_with_nouveau_loaded_blocks_it_and_asks_for_a_reboot_without_installing() {
        let h = fresh_world(|w| w.file("/sys/module/nouveau/.keep", b""));
        let n0 = h.inner().recorded().len();
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::Nvidia);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Warn, "{}", nv.detail);
        assert!(nv.reboot_needed && nv.detail.contains("reboot the host and run `pxu update --host --nvidia` again"), "{}", nv.detail);
        let c = all_cmds(&h, n0);
        assert!(c.iter().any(|a| a.contains("update-initramfs -u")), "{c:#?}");
        assert!(!c.iter().any(|a| a.contains("--dkms") || a.contains("-o /var/lib/pxu-nvidia") || a.starts_with("ct")), "nothing is downloaded or installed: {c:#?}");
        assert!(!c.iter().any(|a| a.contains("reboot")), "pxu never reboots: {c:#?}");
    }

    #[test]
    fn nvidia_without_an_nvidia_gpu_fails_and_touches_nothing() {
        let h = fresh_world(|w| w.host_out("lspci", "c5:00.0 Display controller [0380]: Advanced Micro Devices, Inc. [AMD/ATI] Phoenix3 [1002:1900] (rev c1)\n"));
        let n0 = h.inner().recorded().len();
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::Nvidia);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Failed, "{}", nv.detail);
        assert!(nv.detail.to_lowercase().contains("nvidia gpu"), "{}", nv.detail);
        let c = all_cmds(&h, n0);
        assert!(!c.iter().any(|a| a.contains("apt-get") || a.contains("update-initramfs") || a.contains("--dkms")), "{c:#?}");
        assert!(!h.inner().exists(std::path::Path::new(crate::share::update::nvidia::NOUVEAU_CONF)));
    }

    #[test]
    fn nvidia_with_an_apt_driver_still_skips() {
        // módulo cargado pero sin `nvidia-uninstall`: lo gestiona apt
        let h = gpu_world(|w| w.without_file("/usr/bin/nvidia-uninstall"));
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::Nvidia);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Skipped, "{}", nv.detail);
        assert!(nv.detail.contains("apt-managed drivers are updated with the host"), "{}", nv.detail);
    }

    #[test]
    fn reinstall_puts_libs_in_every_gpu_ct_without_any_per_ct_opt_in_and_a_stopped_one_is_started_and_stopped_again() {
        use crate::share::proxmox::guests::lxc_running;
        // 134 en marcha y 135 apagado, ninguno con `nvidia_auto`; una config vieja con `nvidia_auto = false` se ignora
        let h = gpu_world(|w| w.nvidia_run_driver(&["595.112.07"]).ct_config(134, &["nvidia_auto = false"]));
        assert!(!lxc_running(&h, 135));
        let n0 = h.inner().recorded().len();
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::Nvidia);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Ok, "{}", nv.detail);
        let by = |id: u32| nv.cts.iter().find(|c| c.vmid == id).unwrap_or_else(|| panic!("CT {id} missing: {:?}", nv.cts));
        assert_eq!((by(134).outcome, by(135).outcome), (Outcome::Ok, Outcome::Ok), "{} / {}", by(134).detail, by(135).detail);
        assert!(by(134).detail.contains("CT restarted"), "{}", by(134).detail);
        assert!(by(135).detail.contains("CT was stopped: started and stopped again"), "{}", by(135).detail);
        assert!(nv.detail.contains("CT updated: 134, 135"), "{}", nv.detail);
        let c = all_cmds(&h, n0);
        let pos = |p: &str| c.iter().position(|a| a == p || a.contains(p)).unwrap_or_else(|| panic!("{p} missing in {c:#?}"));
        assert!(pos("host pct start 135") < pos("ct135 sh") && pos("ct135 sh") < pos("host pct shutdown 135 --timeout 60"), "{c:#?}");
        assert!(!c.iter().any(|a| a.contains("pct reboot 135")), "a CT that was off is not restarted: {c:#?}");
        assert!(!lxc_running(&h, 135), "135 ends stopped, as it was");
        assert!(lxc_running(&h, 134));
    }

    #[test]
    fn a_failing_stopped_ct_is_left_stopped_and_reported_as_failed() {
        use crate::share::proxmox::guests::lxc_running;
        let h = gpu_world(|w| w.nvidia_run_driver(&["595.112.07"]).file("/exec/135/nvidia-ct-sha.out", b"deadbeef  x\n"));
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::Nvidia);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Ok, "the host result does not depend on the CTs: {}", nv.detail);
        let ct = nv.cts.iter().find(|c| c.vmid == 135).expect("ct entry");
        assert_eq!(ct.outcome, Outcome::Failed, "{}", ct.detail);
        assert!(ct.detail.contains("CT left stopped as it was"), "{}", ct.detail);
        assert!(!lxc_running(&h, 135), "still stopped");
        assert!(nv.detail.contains("(1 failed"), "{}", nv.detail);
        assert!(update::report::body(&rep).contains("135"), "the failed CT shows in the summary");
    }

    #[test]
    fn the_nvidia_step_announces_its_plan_the_download_size_and_each_ct_result_for_the_live_screen() {
        use crate::share::update::nvidia::{Note, Plan};
        let h = gpu_world(|w| w.nvidia_run_driver(&["595.112.07"]).ct_config(134, &["auto_update = true", "nvidia_auto = true"]).file("/exec/0/nvidia-head.out", b"HTTP/2 302\r\nlocation: x\r\n\r\nHTTP/2 200\r\ncontent-length: 423000000\r\n\r\n"));
        let _ct_optin = UpdateOptIn::new(&h);
        let cfg = Config::load(&h).unwrap();
        let inv = guests::load(&h);
        let mut notes: Vec<Note> = vec![];
        let mut order: Vec<String> = vec![];
        let rep = update::run_nvidia(&h, &inv, &cfg, crate::share::update::nvidia::Mode::Nvidia, &AtomicBool::new(false), &mut |e| match e {
            Event::Nv(n) => {
                order.push(format!("{n:?}").chars().take(8).collect());
                notes.push(n)
            }
            Event::Line(_, l) if l.starts_with("$ curl -fsSL") => order.push("download".into()),
            _ => {}
        })
        .expect("no lock error");
        assert_eq!(rep.nvidia.as_ref().unwrap().outcome, Outcome::Ok);
        let Some(Note::Plan(Plan { from, to, host, libs, .. })) = notes.first() else { panic!("the plan comes first: {notes:?}") };
        assert_eq!((from.as_str(), to.as_str(), *host), ("595.112.07", "595.112.07", true));
        assert!(libs.iter().any(|l| l.0 == 134), "{libs:?}");
        assert!(notes.contains(&Note::Size(423_000_000)), "the size is the LAST content-length: {notes:?}");
        assert!(notes.iter().any(|n| matches!(n, Note::Ct { vmid: 134, outcome: Outcome::Ok, .. })), "{notes:?}");
        let (size, dl) = (order.iter().position(|o| o.starts_with("Size")).unwrap(), order.iter().position(|o| o == "download").unwrap());
        assert!(size < dl, "the size is known before the download starts: {order:?}");
        // sin cabeceras no hay total, pero la descarga sigue
        let h2 = gpu_world(|w| w.nvidia_run_driver(&["595.112.07"]).file("/exec/0/nvidia-head.exit", b"22"));
        let _o = UpdateOptIn::new(&h2);
        let mut n2 = 0;
        let r2 = update::run_nvidia(&h2, &guests::load(&h2), &Config::load(&h2).unwrap(), crate::share::update::nvidia::Mode::Nvidia, &AtomicBool::new(false), &mut |e| n2 += matches!(e, Event::Nv(Note::Size(_))) as u32).unwrap();
        assert_eq!((n2, r2.nvidia.unwrap().outcome), (0, Outcome::Ok));
    }

    const A4000: &str = "01:00.0 VGA compatible controller [0300]: NVIDIA Corporation GA104GL [RTX A4000] [10de:24b0] (rev a1)\n01:00.1 Audio device [0403]: NVIDIA Corporation GA104 High Definition Audio Controller [10de:228b] (rev a1)\n";

    /// Reinstala con la lista de GPUs `chips` (None = sin red) y devuelve (resultado, líneas, comandos del host).
    fn chips_run(lspci: &str, chips: Option<&str>) -> (crate::share::update::nvidia::NvidiaRun, Vec<String>, Vec<String>) {
        let h = gpu_world(|w| {
            let w = w.nvidia_run_driver(&["595.112.07"]).host_out("lspci", lspci);
            match chips { Some(t) => w.host_out("nvidia-chips", t), None => w }
        });
        let _o = UpdateOptIn::new(&h);
        let n0 = h.inner().recorded().len();
        let mut lines = vec![];
        let rep = update::run_nvidia(&h, &guests::load(&h), &Config::load(&h).unwrap(), crate::share::update::nvidia::Mode::Nvidia, &AtomicBool::new(false), &mut |e| if let Event::Line(_, l) = e { lines.push(l) }).unwrap();
        (rep.nvidia.unwrap(), lines, argvs(&h, n0))
    }

    #[test]
    fn a_missing_gpu_stops_before_the_run_is_downloaded() {
        let (nv, lines, cmds) = chips_run(A4000, Some("<tr id=\"devid1FF2\"></tr>"));
        assert_eq!(nv.outcome, Outcome::Failed);
        assert!(nv.detail.contains("10de:24b0") && nv.detail.contains("595.112.07"), "{}", nv.detail);
        assert!(lines.iter().any(|l| l.contains("10de:24b0") && l.contains("not listed")), "{lines:?}");
        assert!(cmds.iter().any(|c| c.ends_with("/README/supportedchips.html")), "{cmds:?}");
        assert!(!cmds.iter().any(|c| c.contains(".run") && c.contains("curl")), "no .run curl (head, sha, download) after the check: {cmds:?}");
        assert!(!cmds.iter().any(|c| c.contains("--dkms") || c.contains("modprobe")), "{cmds:?}");
    }

    #[test]
    fn a_listed_gpu_continues_and_says_so_and_the_hdmi_audio_does_not_count() {
        // el audio 10de:228b no está en la lista y no importa
        let (nv, lines, cmds) = chips_run(A4000, Some("<tr id=\"devid24B0_1028_14AD\"></tr>"));
        assert_eq!(nv.outcome, Outcome::Ok, "{}", nv.detail);
        assert!(lines.iter().any(|l| l == "GPU 10de:24b0 listed in supportedchips for 595.112.07"), "{lines:?}");
        let (chips, dl) = (cmds.iter().position(|c| c.ends_with("supportedchips.html")).unwrap(), cmds.iter().position(|c| c.starts_with("curl") && c.contains(" -o ") && c.contains(".run")).unwrap());
        assert!(chips < dl, "the check comes before the download: {cmds:?}");
    }

    #[test]
    fn a_gpu_only_in_the_legacy_section_or_one_of_several_missing_stops_the_run() {
        let page = "<tr id=\"devid24B0\"></tr><h3>no longer supported</h3><tr id=\"devid1CB1\"></tr>";
        let two = format!("{A4000}02:00.0 VGA compatible controller [0300]: NVIDIA Corporation GP107GL [Quadro P1000] [10de:1cb1]\n");
        let (nv, _, _) = chips_run(&two, Some(page));
        assert_eq!(nv.outcome, Outcome::Failed);
        assert!(nv.detail.contains("10de:1cb1") && !nv.detail.contains("10de:24b0"), "{}", nv.detail);
    }

    #[test]
    fn the_chips_page_never_reaches_the_log_or_the_panel() {
        let page = format!("<html>SECRET-PAGE-BODY-LINE\n<tr id=\"devid24B0_1028_14AD\"></tr>\n{}</html>", "<p>filler row</p>\n".repeat(500));
        let (nv, lines, cmds) = chips_run(A4000, Some(&page));
        assert_eq!(nv.outcome, Outcome::Ok, "{}", nv.detail);
        assert!(!lines.iter().any(|l| l.contains("SECRET-PAGE-BODY-LINE") || l.contains("filler row") || l.contains("devid24B0")), "{} lines, page leaked", lines.len());
        assert!(cmds.iter().any(|c| c.contains("supportedchips-595.112.07.html")), "{cmds:?}");
    }

    #[test]
    fn an_unavailable_or_empty_page_warns_and_goes_on() {
        for chips in [None, Some("<html>no rows</html>")] {
            let (nv, lines, _) = chips_run(A4000, chips);
            assert_eq!(nv.outcome, Outcome::Ok, "{}", nv.detail);
            assert!(lines.iter().any(|l| l.starts_with("warning:") && l.contains("supported-GPU check was skipped")), "{lines:?}");
        }
    }

    #[test]
    fn a_cancel_before_touching_the_driver_installs_nothing() {
        let h = gpu_world(|w| w.nvidia_run_driver(&["595.112.07"]));
        let _ct_optin = UpdateOptIn::new(&h);
        let n0 = h.inner().recorded().len();
        let rep = update::run_nvidia(&h, &guests::load(&h), &Config::load(&h).unwrap(), crate::share::update::nvidia::Mode::Nvidia, &AtomicBool::new(true), &mut |_| {}).unwrap();
        // el mock no corta curl (ignora `cancel`): la corrida igual se cancela antes de tocar el driver
        let nv = rep.nvidia.unwrap();
        assert_eq!(nv.outcome, Outcome::Cancelled, "{}", nv.detail);
        assert!(!argvs(&h, n0).iter().any(|a| a.contains("--dkms")), "{:?}", argvs(&h, n0));
    }

    /// Cancela en cuanto el paso `step` (colgado en el mock) empieza a correr; devuelve el informe, lo que se ejecutó, los eventos y lo que tardó.
    fn cancel_in_step(step: &str) -> (RunReport, Vec<String>, Vec<Event>, std::time::Duration) {
        let h = gpu_world(|w| w.nvidia_run_driver(&["595.112.07"]).file(&format!("/exec/0/nvidia-{step}.hang"), b"1"));
        let _ct_optin = UpdateOptIn::new(&h);
        let n0 = h.inner().recorded().len();
        let cancel = AtomicBool::new(false);
        let mut evs: Vec<Event> = vec![];
        let t = std::time::Instant::now();
        let rep = update::run_nvidia(&h, &guests::load(&h), &Config::load(&h).unwrap(), crate::share::update::nvidia::Mode::Nvidia, &cancel, &mut |e| {
            if matches!(&e, Event::Line(_, l) if l.starts_with("mock: ") && l.ends_with("running…")) {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            evs.push(e);
        })
        .unwrap();
        (rep, argvs(&h, n0), evs, t.elapsed())
    }

    #[test]
    fn cancelling_in_any_step_before_the_install_stops_at_once_and_never_reaches_the_upgrade() {
        for (step, what, cmd) in [("download", "the download", "curl"), ("sha256sum", "the SHA-256 check", "sha256sum"), ("check", "the installer check", "--check"), ("extract", "the driver extract", "--extract-only"), ("make", "the driver test build", "make")] {
            let (rep, cmds, evs, took) = cancel_in_step(step);
            let nv = rep.nvidia.as_ref().unwrap();
            assert_eq!(nv.outcome, Outcome::Cancelled, "{step}: {}", nv.detail);
            assert!(nv.detail.contains(what) && (nv.detail.contains("not touched") || nv.detail.contains("nothing was installed")), "{step}: {}", nv.detail);
            assert!(rep.cancelled && rep.aborted.as_deref().is_some_and(|a| a.starts_with("cancelled by user")), "{:?}", rep.aborted);
            assert!(took < std::time::Duration::from_secs(2), "{step}: {took:?}");
            assert!(cmds.iter().any(|c| c.contains(cmd)), "{step}: {cmds:?}");
            assert!(!cmds.iter().any(|c| c.contains("--dkms") || c.contains("shutdown") || c.contains("modprobe -r")), "{step}: nothing past the build ran: {cmds:?}");
            assert!(!evs.iter().any(|e| matches!(e, Event::Step(_, Step::Upgrade))), "{step}: never reaches Upgrade");
            // el log dice que se pidió cancelar y qué se hizo
            assert!(evs.iter().any(|e| matches!(e, Event::Line(_, l) if l.starts_with("cancel requested") && l.contains("stopped during"))), "{step}: {evs:?}");
        }
    }

    #[test]
    fn a_cancel_during_the_install_is_ignored_and_the_log_says_so() {
        let h = gpu_world(|w| w.nvidia_run_driver(&["595.112.07"]));
        let _ct_optin = UpdateOptIn::new(&h);
        let cancel = AtomicBool::new(false);
        let mut lines: Vec<String> = vec![];
        let rep = update::run_nvidia(&h, &guests::load(&h), &Config::load(&h).unwrap(), crate::share::update::nvidia::Mode::Nvidia, &cancel, &mut |e| {
            if let Event::Step(_, Step::Upgrade) = &e {
                cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if let Event::Line(_, l) = e {
                lines.push(l);
            }
        })
        .unwrap();
        assert_eq!(rep.nvidia.as_ref().unwrap().outcome, Outcome::Ok, "{}", rep.nvidia.as_ref().unwrap().detail);
        assert!(lines.iter().any(|l| l.starts_with("cancel requested") && l.contains("ignored while the system was being changed")), "{lines:?}");
    }

    #[test]
    fn the_cancel_reason_and_notice_name_the_signal() {
        use crate::share::update::{cancel_reason_for, cancel_requested};
        assert_eq!(cancel_reason_for(0), "cancelled by user");
        assert!(cancel_reason_for(libc::SIGINT).contains("SIGINT: Ctrl+C"));
        assert!(cancel_reason_for(libc::SIGHUP).contains("SIGHUP") && cancel_reason_for(libc::SIGHUP).contains("SSH"));
        assert!(cancel_reason_for(libc::SIGTERM).contains("SIGTERM"));
        let safe = cancel_requested(libc::SIGHUP, false, true);
        assert!(safe.starts_with("cancel requested (SIGHUP") && safe.contains("stopping now") && safe.contains("nothing on the system has been changed"), "{safe}");
        let crit = cancel_requested(libc::SIGINT, true, true);
        assert!(crit.contains("SIGINT") && crit.contains("waits until the whole change finishes") && crit.contains("recorded"), "{crit}");
    }

    #[test]
    fn a_ct_libs_failure_does_not_mark_the_host_driver_as_failed() {
        let h = gpu_world(|w| w.host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"]).file("/exec/134/nvidia-ct-sha.out", b"deadbeef  x\n"));
        let rep = timer_run(&h);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Ok, "host ok: {}", nv.detail);
        let ct = nv.cts.iter().find(|c| c.vmid == 134).expect("ct entry");
        assert_eq!(ct.outcome, Outcome::Failed, "{}", ct.detail);
        assert!(nv.detail.contains("CT updated: 135") && nv.detail.contains("(1 failed"), "{}", nv.detail);
        assert_eq!(rep.exit_code(), update::EXIT_PARTIAL, "a failed CT still fails the run");
        let b = update::report::body(&rep);
        let host_row = b.lines().find(|l| l.starts_with("| host")).expect("host row");
        assert!(host_row.contains("| ✅ ok |") && !host_row.contains("❌"), "{host_row}");
        assert!(update::report::title(&rep).ends_with("· NVIDIA libs failed"), "{}", update::report::title(&rep));
    }

    #[test]
    fn reboot_is_asked_only_when_nvidia_smi_does_not_confirm_the_new_version() {
        // confirmada: sin aviso
        let ok = gpu_world(|w| w.host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"]));
        let nv = timer_run(&ok).nvidia.expect("entry");
        assert!(nv.outcome == Outcome::Ok && !nv.reboot_needed, "{}", nv.detail);
        // el instalador sale bien pero el módulo nuevo no queda cargado: aviso, host en Warn (no Failed), ningún CT recibe librerías
        let h = gpu_world(|w| w.host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"]).file("/exec/0/nvidia-install.noeffect", b""));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        let nv = rep.nvidia.as_ref().expect("entry");
        assert_eq!(nv.outcome, Outcome::Warn, "{}", nv.detail);
        assert!(nv.reboot_needed && nv.detail.contains("REBOOT"), "{}", nv.detail);
        assert!(rep.aborted.is_none(), "not a failure: the run goes on");
        assert_eq!(nv.cts.iter().find(|c| c.vmid == 134).map(|c| c.outcome), Some(Outcome::Skipped));
        let c = all_cmds(&h, n0);
        assert!(!c.iter().any(|a| a.starts_with("ct") && a.contains("--no-kernel-modules")), "{c:#?}");
        assert!(!c.iter().any(|a| a == "host reboot" || a.starts_with("host systemctl reboot")), "pxu never reboots: {c:?}");
        assert!(update::report::body(&rep).contains("needs a REBOOT"), "{}", update::report::body(&rep));
    }

    #[test]
    fn ct_libs_only_touches_that_ct_with_the_host_version() {
        let nv = "lxc.mount.entry: /dev/nvidia0 dev/nvidia0 none bind,optional,create=file";
        let h = gpu_world(|w| w.ct(crate::share::mock::scenario::Ct::new(136).conf(nv)).ct_config(134, &["nvidia_auto = true"]).ct_config(136, &["nvidia_auto = true"]));
        let n0 = h.inner().recorded().len();
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::CtLibs(134));
        let n = rep.nvidia.as_ref().expect("entry");
        assert_eq!(n.outcome, Outcome::Skipped, "the host is not touched: {}", n.detail);
        assert_eq!((n.from.as_deref(), n.to.as_deref()), (Some("595.112.07"), Some("595.112.07")), "the version the host already has");
        assert_eq!(n.cts.len(), 1);
        assert_eq!((n.cts[0].vmid, n.cts[0].outcome), (134, Outcome::Ok), "{}", n.cts[0].detail);
        let c = all_cmds(&h, n0);
        for bad in ["--dkms", "modprobe", "nvidia-persistenced", "pct stop", "pct shutdown", "ct136", "pct reboot 136", "pct push 136"] {
            assert!(!c.iter().any(|a| a.contains(bad)), "{bad} in {c:#?}");
        }
        assert!(c.iter().any(|a| a.starts_with("ct134 ") && a.contains("--no-kernel-modules")), "{c:#?}");
        assert_eq!(rep.exit_code(), update::EXIT_OK);
        // un CT sin GPU no entra
        let rep = manual_run(&h, crate::share::update::nvidia::Mode::CtLibs(113));
        assert_eq!(rep.nvidia.unwrap().outcome, Outcome::Skipped);
    }

    #[test]
    fn nvidia_ct_installer_runs_from_a_fresh_private_dir_and_it_is_removed() {
        let h = gpu_world(|w| w.host_nvidia_auto().ct_config(134, &["auto_update = true", "nvidia_auto = true"]));
        let n0 = h.inner().recorded().len();
        let rep = timer_run(&h);
        let ct = rep.nvidia.as_ref().unwrap().cts.iter().find(|c| c.vmid == 134).expect("ct entry");
        assert_eq!(ct.outcome, Outcome::Ok, "{}", ct.detail);
        let c = all_cmds(&h, n0);
        assert!(!c.iter().any(|a| a.contains("/var/tmp/pxu-nvidia")), "{c:#?}");
        let pos = |p: &str| c.iter().position(|a| a.contains(p)).unwrap_or_else(|| panic!("{p} missing in {c:#?}"));
        assert!(pos("ct134 mktemp -d") < pos("pct push") && pos("pct push") < pos("ct134 sha256sum") && pos("ct134 sha256sum") < pos("--no-kernel-modules") && pos("--no-kernel-modules") < pos("ct134 rm -rf --"), "{c:#?}");
    }

    #[test]
    fn nvidia_worst_s_covers_every_command_it_runs() {
        // 4 CT con GPU; dos mundos idénticos, uno con NVIDIA automático y otro sin: la diferencia de «peor caso por orden» es lo que cuesta el paso.
        use crate::share::mock::scenario::Ct;
        let nv = "lxc.mount.entry: /dev/nvidia0 dev/nvidia0 none bind,optional,create=file";
        let world = |on: bool| {
            gpu_world(|w| {
                let w = w.ct(Ct::new(136).conf(nv)).ct(Ct::new(137).conf(nv)).ct_config(134, &["auto_update = true", "nvidia_auto = true"]).ct_config(135, &["nvidia_auto = true"]).ct_config(136, &["nvidia_auto = true"]).ct_config(137, &["nvidia_auto = true"]);
                if on { w.host_nvidia_auto() } else { w }
            })
        };
        let (with, without) = (world(true), world(false));
        let (b0, b1) = (with.inner().exec_budget_s(), without.inner().exec_budget_s());
        let rep = timer_run(&with);
        timer_run(&without);
        let nvr = rep.nvidia.as_ref().expect("the NVIDIA step ran");
        assert!(nvr.cts.len() >= 3, "{:?}", nvr.cts.iter().map(|c| (c.vmid, &c.detail)).collect::<Vec<_>>());
        let cost = (with.inner().exec_budget_s() - b0) - (without.inner().exec_budget_s() - b1);
        let gpu_cts = guests::load(&with).guests.iter().filter(|g| g.gpu.nvidia).count() as u64;
        assert_eq!(gpu_cts, 4);
        let cfg = Config::load(&with).unwrap();
        assert!(cost > 0 && cost < u64::MAX / 2, "an NVIDIA command without timeout_s: {cost}");
        assert!(cost <= crate::share::update::nvidia::worst_s(&cfg, gpu_cts), "the commands add up to {cost} s but worst_s reserves {} s", crate::share::update::nvidia::worst_s(&cfg, gpu_cts));
        assert!(crate::share::update::nvidia::worst_s(&cfg, gpu_cts) < crate::share::update::RUN_DEADLINE_S, "with 4 GPU CTs the step must be able to start");
    }

    #[test]
    fn nothing_newer_leaves_no_entry() {
        let h = gpu_world(|w| w.host_nvidia_auto().nvidia_run_driver(&["595.112.07", "596.10.01"]));
        let rep = timer_run(&h);
        assert!(rep.nvidia.is_none(), "{:?}", rep.nvidia.as_ref().map(|n| &n.detail));
    }

    #[test]
    fn index_failure_skips_without_failing() {
        let h = sched_world(|w| w.host_nvidia_auto().file("/usr/bin/nvidia-uninstall", b""));
        let rep = timer_run(&h);
        if let Some(nv) = rep.nvidia.as_ref() { assert_ne!(nv.outcome, Outcome::Failed, "{}", nv.detail); }
        assert_eq!(rep.exit_code(), 0);
    }

    #[test]
    fn parsers_and_helpers() {
        assert_eq!(parse_df_free_kb("Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 10 1 777 9% /\n"), Some(777));
        assert_eq!(parse_df_free_kb(""), None);
        assert_eq!(kernel_of("proxmox-kernel-6.17.9-1-pve-signed").as_deref(), Some("6.17.9-1-pve"));
        assert_eq!(kernel_of("proxmox-kernel-6.17"), None);
        assert!(has_kernel(&confirmed()) && !has_kernel(&[]));
        let all = confirmed();
        let g = grouped(&all);
        assert_eq!(g.iter().map(|(n, _)| *n).collect::<Vec<_>>(), ["kernel", "firmware", "pve", "security", "other"]);
    }
}
