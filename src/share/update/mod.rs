//! Motor de `update` (ARCHITECTURE §3): CT a CT, con lock, backup temporal, simulación, upgrade, verificación,
//! rollback automático si falla y borrado del backup (salvo `keep`).
//! Solo habla con `&dyn Host`; las escrituras exigen el opt-in explícito de `Host::set_update_enabled`.
pub mod backup;
pub mod check;
pub mod hostupdate;
pub mod nvidia;
pub mod report;
pub mod runlog;

use self::backup::{Entry, Kind, Registry};
use self::check::{CheckOpts, Status};
use crate::share::config::Config;
use crate::share::proxmox::guests::Inventory;
use crate::share::host::{ExecOpts, Host, IoPrio, LineSink};
use crate::share::model::{Guest, GuestKind, RunState};
use crate::share::notify::{self, AppriseCfg, Level, Message};
use crate::share::state::{self, CheckInfo};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

pub const EXIT_OK: i32 = 0;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_LOCKED: i32 = 3;
pub const EXIT_PARTIAL: i32 = 20;
pub const EXIT_ABORTED: i32 = 30;
pub const LOG_DIR: &str = "/var/log/pxu";
const LOG_CAP: usize = 20000;

// ---------------------------------------------------------------- comandos (única fuente: motor, mock y lista blanca)

pub fn upgrade_argv(pm: &str, apt_mode: &str) -> Option<Vec<&'static str>> {
    Some(match pm {
        "apt" => vec![
            "env", "DEBIAN_FRONTEND=noninteractive", "apt-get", "-y", "-o", "Dpkg::Options::=--force-confold", "-o", "Dpkg::Options::=--force-confdef",
            if apt_mode == "upgrade" { "upgrade" } else { "dist-upgrade" },
        ],
        "apk" => vec!["apk", "upgrade", "--no-cache"],
        "pacman" => vec!["pacman", "-Syu", "--noconfirm"],
        "dnf" => vec!["dnf", "-y", "upgrade"],
        "zypper" => vec!["zypper", "-n", "update"],
        _ => return None,
    })
}

pub fn is_upgrade_argv(argv: &[&str]) -> bool {
    ["upgrade", "dist-upgrade"].iter().any(|m| upgrade_argv("apt", m).is_some_and(|a| a == argv))
        || ["apk", "pacman", "dnf", "zypper"].iter().any(|pm| upgrade_argv(pm, "").is_some_and(|a| a == argv))
}

pub fn is_verify_argv(argv: &[&str]) -> bool {
    matches!(argv, ["dpkg", "--audit"] | ["systemctl", "--failed", "--no-legend", "--plain"] | ["df", "-Pk", "/"])
}

// ---------------------------------------------------------------- tipos

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Pending,
    Preflight,
    Lock,
    Backup,
    Refresh,
    Upgrade,
    Verify,
    Rollback,
    Restore,
    Done,
}

impl Step {
    pub fn label(self) -> &'static str {
        match self {
            Step::Pending => "pending",
            Step::Preflight => "pre-checks",
            Step::Lock => "lock",
            Step::Backup => "backup",
            Step::Refresh => "refresh+simulate",
            Step::Upgrade => "upgrade",
            Step::Verify => "verify",
            Step::Rollback => "rollback",
            Step::Restore => "restore",
            Step::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Warn,
    Failed,
    Skipped,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct CtRun {
    pub vmid: u32,
    pub name: String,
    pub outcome: Outcome,
    /// Paso en el que terminó (o falló).
    pub step: Step,
    pub detail: String,
    pub packages_before: usize,
    pub packages_after: Option<usize>,
    /// De `packages_before`, cuántos son de seguridad.
    pub security: usize,
    pub reboot_required: bool,
    pub new_failed_units: Vec<String>,
    /// Backup previo y su destino ("… (deleted)", "… (kept)").
    pub backup: Option<String>,
    /// `Some("done")` / `Some("failed")` si el update falló y se intentó volver al backup; `None` si no hizo falta o no se intentó.
    pub rollback: Option<String>,
    /// Comando manual para volver atrás: solo cuando el backup se conserva (rollback fallido o timeout del upgrade).
    pub revert: Option<String>,
    /// Log de la corrida (el mismo para todas las entradas del informe).
    pub log_path: Option<String>,
    pub took_s: u64,
}

#[derive(Debug, Clone)]
pub enum Event {
    Queue(Vec<(u32, String)>),
    Start(u32),
    Step(u32, Step),
    Line(u32, String),
    Done(CtRun),
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// Nombre del nodo (para el título del aviso y el log).
    pub node: String,
    pub started_at: u64,
    pub finished_at: u64,
    pub results: Vec<CtRun>,
    /// `Some(motivo)` si se paró antes de acabar (cortacircuito o cancelación).
    pub aborted: Option<String>,
    pub cancelled: bool,
    /// Paso del driver NVIDIA (solo con `host_nvidia_auto`): driver del host y librerías de los CT. Aparte de `results`.
    pub nvidia: Option<nvidia::NvidiaRun>,
    /// `/var/log/pxu/run-….log`, escrito al final de la corrida.
    pub log_path: Option<String>,
}

impl RunReport {
    pub fn count(&self, o: Outcome) -> usize {
        self.results.iter().filter(|r| r.outcome == o).count()
    }
    pub fn exit_code(&self) -> i32 {
        if self.aborted.is_some() {
            EXIT_ABORTED
        } else if self.count(Outcome::Failed) > 0 || self.nvidia.as_ref().is_some_and(|n| n.outcome == Outcome::Failed || n.cts.iter().any(|c| c.outcome == Outcome::Failed)) {
            EXIT_PARTIAL
        } else {
            EXIT_OK
        }
    }
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "started_at": self.started_at,
            "took_s": self.finished_at.saturating_sub(self.started_at),
            "exit_code": self.exit_code(),
            "aborted": self.aborted,
            "log_path": self.log_path,
            "summary": {
                "total": self.results.len(), "ok": self.count(Outcome::Ok), "warn": self.count(Outcome::Warn),
                "failed": self.count(Outcome::Failed), "skipped": self.count(Outcome::Skipped), "cancelled": self.count(Outcome::Cancelled),
            },
            "host": self.results.iter().find(|r| r.vmid == 0),
            "cts": self.results.iter().filter(|r| r.vmid != 0).collect::<Vec<_>>(),
            "nvidia": self.nvidia,
        })
    }
}

#[derive(Debug)]
pub enum RunError {
    /// Otro `pxu update` (o el propio) tiene el lock global.
    Locked,
    Io(String),
}

// ---------------------------------------------------------------- selección

#[derive(Debug, Clone)]
pub struct Candidate {
    pub vmid: u32,
    pub name: String,
    pub packages: usize,
    pub security: usize,
    /// `Some(motivo)` ⇒ no se actualizará.
    pub skip: Option<String>,
}

fn now_s(h: &dyn Host) -> u64 {
    h.now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Candidatos: sin `only`, los CT con `auto_update`; con `only`, exactamente esos (cualquier CT, con el motivo si no procede).
/// Solo CT con chequeo positivo previo y reciente (`require_fresh_check_minutes`).
pub fn select(host: &dyn Host, inv: &Inventory, cfg: &Config, only: &[u32]) -> Vec<Candidate> {
    let now = now_s(host);
    let explicit = !only.is_empty();
    let mut out = Vec::new();
    for g in inv.guests.iter().filter(|g| if explicit { only.contains(&g.vmid) } else { g.kind == GuestKind::Lxc && g.auto_update }) {
        let mut c = Candidate { vmid: g.vmid, name: g.name.clone(), packages: g.updates(), security: g.security(), skip: None };
        let age = g.check.as_ref().map(|i| now.saturating_sub(i.checked_at));
        c.skip = if g.kind == GuestKind::Host {
            Some("the Proxmox host is never updated here: use `pxu update --host`".into())
        } else if g.kind != GuestKind::Lxc {
            Some("VM: not supported (LXC only)".into())
        } else if g.state == RunState::Remote {
            Some("CT on another node".into())
        } else if g.template {
            Some("template".into())
        } else if let Some(l) = &g.lock {
            Some(format!("locked by PVE: {l}"))
        } else if g.state == RunState::Stopped && cfg.update.stopped != "start_and_restore" {
            Some("stopped (set [update] stopped = \"start_and_restore\")".into())
        } else if g.check.is_none() {
            Some("never checked: run `pxu check` first".into())
        } else if age.is_some_and(|a| a > cfg.update.require_fresh_check_minutes * 60) {
            Some(format!("check older than {} min: run `pxu check` again", cfg.update.require_fresh_check_minutes))
        } else if g.updates() == 0 {
            Some("no updates pending".into())
        } else {
            None
        };
        // Sin selección explícita, "nada pendiente" no es noticia.
        if !explicit && c.skip.as_deref() == Some("no updates pending") {
            continue;
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------- contexto por CT

struct Ctx<'a> {
    host: &'a dyn Host,
    vmid: u32,
    log: Vec<String>,
    dropped: usize,
    capture: Vec<String>,
    /// `Some(motivo)`: este CT obliga a parar toda la corrida (no se sigue con el siguiente).
    abort: Option<String>,
    /// El upgrade venció su timeout: el estado del CT es incierto, no se hace rollback automático.
    hold: bool,
    /// El registro de backups estaba corrupto o manipulado: en esta corrida no se poda ni se borra nada salvo el backup recién creado.
    no_prune: bool,
    ev: &'a mut dyn FnMut(Event),
    /// Paquetes de seguridad de la lista pendiente (solo el host: lo fija antes de instalar, para el informe).
    security: usize,
}

impl LineSink for Ctx<'_> {
    fn line(&mut self, l: &str) {
        let l = crate::share::fmt::clean(l);
        self.say(&l);
        self.capture.push(l);
    }
}

impl Ctx<'_> {
    fn say(&mut self, l: &str) {
        // Frontera de confianza: lo que dice un CT nunca llega crudo a la TUI, al log ni a los avisos.
        let l = crate::share::fmt::mask_url_creds(&crate::share::fmt::clean(l.trim_end_matches(['\r', '\n'])));
        let l = l.as_str();
        if self.log.len() >= LOG_CAP {
            self.dropped += 1;
        } else {
            self.log.push(l.to_string());
        }
        (self.ev)(Event::Line(self.vmid, l.to_string()));
    }
    fn step(&mut self, s: Step) {
        self.say(&format!("== {}", s.label()));
        (self.ev)(Event::Step(self.vmid, s));
    }
    fn host_cmd(&mut self, argv: &[&str], o: &ExecOpts) -> Result<i32, String> {
        self.say(&format!("$ {}", argv.join(" ")));
        self.capture.clear();
        let h = self.host;
        h.host_cmd(argv, o, self).map(|e| e.code).map_err(|e| e.to_string())
    }
    fn guest(&mut self, argv: &[&str], o: &ExecOpts) -> Result<i32, String> {
        self.say(&format!("$ [CT {}] {}", self.vmid, argv.join(" ")));
        self.capture.clear();
        let h = self.host;
        let v = self.vmid;
        h.guest_exec(v, argv, o, self).map(|e| e.code).map_err(|e| e.to_string())
    }
}

fn failed_units(lines: &[String]) -> BTreeSet<String> {
    lines.iter().filter_map(|l| l.trim_start_matches(['●', '*', ' ']).split_whitespace().next()).filter(|u| u.contains('.')).map(String::from).collect()
}

/// `YYYYMMDDHHMMSS` UTC desde epoch (algoritmo civil de Hinnant).
pub fn stamp(epoch: u64) -> String {
    let days = (epoch / 86400) as i64;
    let rem = epoch % 86400;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

pub struct Opts {
    pub only: Vec<u32>,
}

struct Done {
    outcome: Outcome,
    step: Step,
    detail: String,
}

fn fail(step: Step, detail: impl Into<String>) -> Done {
    Done { outcome: Outcome::Failed, step, detail: detail.into() }
}

// ---------------------------------------------------------------- motor

/// Actualiza los candidatos en orden. `cancel` se mira entre CT (nunca se interrumpe un dpkg en curso).
pub fn run(host: &dyn Host, inv: &Inventory, cfg: &Config, opts: &Opts, cancel: &AtomicBool, ev_out: &mut dyn FnMut(Event)) -> Result<RunReport, RunError> {
    let started_at = now_s(host);
    // Todo lo que dice el motor se recoge también en el log único de la corrida.
    // El archivo se crea al tomar el lock y crece con cada línea: un corte brusco deja el log parcial (se lista como INCOMPLETE).
    let rec = std::cell::RefCell::new(runlog::Recorder::default());
    let live = std::cell::RefCell::new(None::<std::path::PathBuf>);
    let mut ev_fn = |e: Event| {
        runlog::feed_live(host, &rec, &live, now_s(host), &e);
        ev_out(e)
    };
    let ev: &mut dyn FnMut(Event) = &mut ev_fn;
    let _global = match host.try_lock(&host.lock_dir().join("pxu-run.lock"), true) {
        Ok(Some(g)) => g,
        Ok(None) => return Err(RunError::Locked),
        Err(e) => return Err(RunError::Io(format!("global lock: {e}"))),
    };
    *live.borrow_mut() = runlog::begin_live(host, started_at, &inv.node);
    let cands = select(host, inv, cfg, &opts.only);
    // El host solo entra en la corrida «de todos» (sin `only`) y solo con `[update] host_auto`.
    let plan = if opts.only.is_empty() { hostupdate::plan_auto(host, inv, cfg) } else { hostupdate::Plan::Off };
    let mut queue: Vec<(u32, String)> = if plan == hostupdate::Plan::Off { vec![] } else { vec![(0, "host".into())] };
    queue.extend(cands.iter().map(|c| (c.vmid, c.name.clone())));
    ev(Event::Queue(queue));
    // `select` no lista los CT con auto-update sin nada pendiente: el log los nombra igual, para que una corrida vacía diga por qué.
    if opts.only.is_empty() {
        let auto = idle_auto(inv, &cands);
        for g in &auto {
            ev(Event::Line(g.vmid, format!("CT {} {}: skipped — no updates pending", g.vmid, g.name)));
        }
        if plan == hostupdate::Plan::Off && cfg.update.host_auto {
            // `plan_auto` solo devuelve `Off` con `host_auto` cuando el host no tiene nada pendiente
            ev(Event::Line(0, "host: skipped — no updates pending".into()));
        }
        if auto.is_empty() && cands.is_empty() {
            ev(Event::Line(0, "no CT with the auto-update tag has anything to update".into()));
        }
    }
    let max_fail = if cfg.update.on_failure == "stop" { 1 } else { cfg.update.max_consecutive_failures.max(1) };
    let (mut results, mut streak, mut aborted, mut cancelled) = (Vec::new(), 0u32, None::<String>, false);
    if plan != hostupdate::Plan::Off {
        let (r, stop) = run_host(host, inv, cfg, &plan, cancel, ev);
        if r.outcome == Outcome::Cancelled {
            cancelled = true;
        }
        if let Some(why) = stop {
            aborted = Some(why);
        }
        results.push(r);
    }
    // Driver NVIDIA (opt-in `host_nvidia_auto`): justo tras el host y ANTES de los CT; solo en la corrida «de todos» y solo si el host no falló.
    let mut nvidia = None;
    if opts.only.is_empty() && aborted.is_none() && !cancelled {
        let reboot = results.iter().any(|r| r.vmid == 0 && r.reboot_required) || host.exists(Path::new("/var/run/reboot-required")) || inv.host.as_ref().is_some_and(|h| h.reboot_pending());
        rec.borrow_mut().begin("NVIDIA driver");
        if let Some((r, stop)) = nvidia::run(host, inv, cfg, reboot, cancel, ev) {
            if r.outcome == Outcome::Cancelled {
                cancelled = true;
            }
            if let Some(why) = stop {
                aborted = Some(why);
            }
            nvidia = Some(r);
        }
    }
    for c in &cands {
        let g = inv.guests.iter().find(|g| g.vmid == c.vmid).expect("candidate from inventory");
        let finish = |r: CtRun, ev: &mut dyn FnMut(Event), results: &mut Vec<CtRun>| {
            ev(Event::Done(r.clone()));
            results.push(r);
        };
        let bare = |o: Outcome, d: &str| CtRun {
            vmid: g.vmid, name: g.name.clone(), outcome: o, step: Step::Pending, detail: d.into(), packages_before: c.packages, packages_after: None, security: g.security(),
            reboot_required: false, new_failed_units: vec![], backup: None, rollback: None, revert: None, log_path: None, took_s: 0,
        };
        if cancel.load(Ordering::SeqCst) && !cancelled {
            cancelled = true;
            aborted = Some("cancelled by user".into());
        }
        if cancelled {
            finish(bare(Outcome::Cancelled, "cancelled before start"), ev, &mut results);
            continue;
        }
        if let Some(why) = &aborted {
            finish(bare(Outcome::Skipped, &format!("not started: {why}")), ev, &mut results);
            continue;
        }
        if let Some(why) = &c.skip {
            ev(Event::Line(g.vmid, format!("CT {} {}: skipped — {why}", g.vmid, g.name)));
            finish(bare(Outcome::Skipped, why), ev, &mut results);
            continue;
        }
        ev(Event::Start(g.vmid));
        let (r, stop) = run_ct(host, g, cfg, ev);
        match r.outcome {
            Outcome::Failed => streak += 1,
            Outcome::Ok | Outcome::Warn => streak = 0,
            _ => {}
        }
        finish(r, ev, &mut results);
        if let Some(why) = stop {
            aborted = Some(why);
        } else if streak >= max_fail {
            aborted = Some(format!("stopped after {streak} consecutive failure{}", if streak == 1 { "" } else { "s" }));
        }
    }
    let mut report = RunReport { node: inv.node.clone(), started_at, finished_at: now_s(host), results, aborted, cancelled, nvidia, log_path: None };
    runlog::finalize(host, cfg.general.log_retention_days, live.borrow_mut().take(), &rec.borrow(), &mut report);
    Ok(report)
}

/// Paso del host de la corrida programada (`host_auto`): el host va ANTES que cualquier CT. Devuelve su entrada del informe y, si los CT
/// no deben tocarse (el host falló, abortó, venció su timeout o se canceló), el motivo de parar. Un host saltado antes de empezar
/// (chequeo viejo, lock ocupado, bloqueado) no se tocó: los CT siguen.
fn run_host(host: &dyn Host, inv: &Inventory, cfg: &Config, plan: &hostupdate::Plan, cancel: &AtomicBool, ev: &mut dyn FnMut(Event)) -> (CtRun, Option<String>) {
    let bare = |o: Outcome, step: Step, d: &str| CtRun {
        vmid: 0, name: "host".into(), outcome: o, step, detail: d.into(), packages_before: inv.host.as_ref().map_or(0, |h| h.updates()), packages_after: None, security: inv.host.as_ref().map_or(0, |h| h.security()),
        reboot_required: false, new_failed_units: vec![], backup: None, rollback: None, revert: None, log_path: None, took_s: 0,
    };
    let skipped = |why: &str, ev: &mut dyn FnMut(Event)| {
        let r = bare(Outcome::Skipped, Step::Pending, why);
        ev(Event::Done(r.clone()));
        (r, None)
    };
    let hostupdate::Plan::Run { .. } = plan else {
        return match plan {
            hostupdate::Plan::Skip(why) => {
                ev(Event::Line(0, format!("host: skipped — {why}")));
                skipped(why, ev)
            }
            _ => skipped("not part of this run", ev),
        };
    };
    if cancel.load(Ordering::SeqCst) {
        let r = bare(Outcome::Cancelled, Step::Pending, "cancelled before start");
        ev(Event::Done(r.clone()));
        return (r, Some("cancelled by user".into()));
    }
    ev(Event::Start(0));
    // Opt-in de escritura del host derivado de `host_auto` (el plan solo es `Run` con él) y limitado a este paso.
    let optin = crate::share::host::HostUpdateOptIn::new(host);
    let out = hostupdate::run_unattended(host, inv, cfg, cancel, ev);
    drop(optin);
    match out {
        Ok(hostupdate::HostOutcome::Done(mut rep)) if !rep.results.is_empty() => {
            let r = rep.results.remove(0);
            match r.outcome {
                Outcome::Ok | Outcome::Warn => (r, None),
                Outcome::Cancelled => (r, Some("cancelled by user".into())),
                _ => (r, Some("host update failed".into())),
            }
        }
        Err(RunError::Locked) => skipped("another host update is running (lock held)", ev),
        Ok(_) => {
            let r = bare(Outcome::Failed, Step::Preflight, "unexpected host update result");
            ev(Event::Done(r.clone()));
            (r, Some("host update failed".into()))
        }
        Err(RunError::Io(e)) => {
            let r = bare(Outcome::Failed, Step::Lock, &e);
            ev(Event::Done(r.clone()));
            (r, Some("host update failed".into()))
        }
    }
}

/// Retira el CT objetivo de la lista blanca al salir de `run_ct` (también con pánico).
struct TargetScope<'a>(&'a dyn Host);

impl Drop for TargetScope<'_> {
    fn drop(&mut self) {
        self.0.set_update_target(None);
    }
}

fn run_ct(host: &dyn Host, g: &Guest, cfg: &Config, ev: &mut dyn FnMut(Event)) -> (CtRun, Option<String>) {
    let t0 = now_s(host);
    let mut cx = Ctx { host, vmid: g.vmid, log: Vec::new(), dropped: 0, capture: Vec::new(), abort: None, hold: false, no_prune: false, ev, security: 0 };
    let mut run = CtRun {
        vmid: g.vmid, name: g.name.clone(), outcome: Outcome::Failed, step: Step::Lock, detail: String::new(), packages_before: g.updates(), packages_after: None, security: g.security(),
        reboot_required: false, new_failed_units: vec![], backup: None, rollback: None, revert: None, log_path: None, took_s: 0,
    };
    let mut started_by_us = false;
    // Las escrituras de la lista blanca solo valen para este CT mientras dure esta función.
    host.set_update_target(Some(g.vmid));
    let _scope = TargetScope(host);
    // Los locks viven hasta después de devolver el CT a `stopped` (L5).
    let _locks;
    let d = match lock_ct(&mut cx) {
        Ok(l) => {
            _locks = l;
            ct_steps(&mut cx, g, cfg, &mut run, &mut started_by_us)
        }
        Err(d) => d,
    };
    // Restaurar el estado original del CT, pase lo que pase. Tras un timeout (`hold`) dpkg puede seguir vivo
    // dentro del CT: NO se apaga (un SIGTERM al CT lo cortaría a medias); queda encendido y se avisa.
    if started_by_us && cx.hold {
        cx.say(&format!("WARNING: CT {} was started by pxu and is LEFT RUNNING: the upgrade timed out and dpkg may still be working inside; stop it yourself once it finishes (pct shutdown {})", g.vmid, g.vmid));
    } else if started_by_us {
        cx.step(Step::Restore);
        match cx.host_cmd(&["pct", "shutdown", &g.vmid.to_string(), "--timeout", "60"], &ExecOpts::default()) {
            Ok(0) => cx.say("CT returned to stopped"),
            Ok(c) => cx.say(&format!("WARNING: could not stop the CT again (exit {c})")),
            Err(e) => cx.say(&format!("WARNING: could not stop the CT again: {e}")),
        }
    }
    run.outcome = d.outcome;
    run.step = d.step;
    run.detail = d.detail;
    if started_by_us && cx.hold {
        run.detail += &format!("; CT left RUNNING (it was stopped before): dpkg may still be working inside, shut it down manually: pct shutdown {}", g.vmid);
    }
    if run.outcome != Outcome::Failed {
        run.revert = None;
    }
    cx.say(&format!("== result: {:?}: {}", run.outcome, run.detail));
    if cx.dropped > 0 {
        let n = cx.dropped;
        cx.log.push(format!("[{n} more lines dropped: log cap {LOG_CAP}]"));
    }
    run.took_s = now_s(host).saturating_sub(t0);
    (run, cx.abort.take())
}

/// Lock por CT, lock de ProxMenux (sin crearlo) y su protocolo de reserva (M1/L4). Falla en cerrado.
fn lock_ct(cx: &mut Ctx) -> Result<Vec<crate::share::host::LockGuard>, Done> {
    let id = cx.vmid;
    let host = cx.host;
    let skip = |detail: &str| Done { outcome: Outcome::Skipped, step: Step::Lock, detail: detail.into() };
    cx.step(Step::Lock);
    let mut guards = Vec::new();
    match host.try_lock(&host.lock_dir().join(format!("pxu-ct-{id}.lock")), true) {
        Ok(Some(l)) => guards.push(l),
        Ok(None) => return Err(skip("another pxu process holds this CT")),
        Err(e) => return Err(fail(Step::Lock, format!("lock: {e}"))),
    }
    let pm_path = host.lock_dir().join(format!("proxmenux-lxc-update-{id}.lock"));
    match host.try_lock(&pm_path, false) {
        Ok(Some(l)) => {
            // ProxMenux guarda una reserva (UPID de una tarea de control de PVE) dentro del fichero: sin poder
            // confirmarla, el CT no se toca aunque el flock esté libre.
            if host.read_file(&pm_path).is_ok_and(|raw| !raw.iter().all(|b| b.is_ascii_whitespace())) {
                return Err(skip("ProxMenux control task unconfirmed (its lock file holds a reservation)"));
            }
            guards.push(l);
        }
        Ok(None) => return Err(skip("ProxMenux is updating this CT")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // ProxMenux no la ha creado: nada que respetar
        Err(e) => return Err(skip(&format!("cannot check ProxMenux lock: {e}"))),
    }
    if host.exists(&host.lock_dir().join(format!("proxmenux-lxc-check-invalid-{id}"))) {
        return Err(skip("ProxMenux marked this CT's update check as invalid"));
    }
    Ok(guards)
}

fn ct_steps(cx: &mut Ctx, g: &Guest, cfg: &Config, run: &mut CtRun, started_by_us: &mut bool) -> Done {
    let id = g.vmid.to_string();
    let host = cx.host;
    // --- pre-checks (CT detenido: se arranca y luego se restaura)
    cx.step(Step::Preflight);
    let nice = ExecOpts { nice: Some(cfg.general.nice), ionice: Some(io(cfg)), ..Default::default() };
    let mut gr = g.clone();
    if g.state == RunState::Stopped {
        match cx.host_cmd(&["pct", "start", &id], &ExecOpts::default()) {
            Ok(0) => {}
            Ok(c) => return fail(Step::Preflight, format!("pct start failed (exit {c})")),
            Err(e) => return fail(Step::Preflight, format!("pct start: {e}")),
        }
        *started_by_us = true;
        if !host.is_mock() {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        gr.state = RunState::Running;
    }
    match cx.guest(&["df", "-Pk", "/"], &nice) {
        Ok(0) => {
            let avail_kb = cx.capture.iter().filter_map(|l| l.split_whitespace().nth(3)?.parse::<u64>().ok()).next();
            match avail_kb {
                Some(kb) if kb / 1024 < cfg.update.min_free_mb => return fail(Step::Preflight, format!("only {} MB free on / (need {} MB)", kb / 1024, cfg.update.min_free_mb)),
                None => return fail(Step::Preflight, "cannot read free space on /"),
                _ => {}
            }
        }
        Ok(c) => return fail(Step::Preflight, format!("df failed (exit {c})")),
        Err(e) => return fail(Step::Preflight, format!("df: {e}")),
    }
    let units_cmd = ["systemctl", "--failed", "--no-legend", "--plain"];
    let before = match cx.guest(&units_cmd, &nice) {
        Ok(_) => failed_units(&cx.capture),
        Err(_) => BTreeSet::new(), // sin systemd (Alpine): sin comparación
    };
    // --- backup (siempre, salvo `mode = "none"` puesto a mano): sin backup no se toca el CT
    cx.step(Step::Backup);
    let mode = cfg.backup_mode(g.vmid);
    let entry = if mode == "none" {
        cx.say("WARNING: backup mode is \"none\": this CT is updated WITHOUT a backup and without automatic rollback");
        None
    } else {
        match create_backup(cx, g, cfg, mode) {
            Ok(e) => {
                run.backup = Some(e.describe());
                cx.say(&format!("backup ready: {}", e.describe()));
                cx.say(BACKUP_SCOPE_NOTE);
                Some(e)
            }
            Err(e) => return fail(Step::Backup, format!("backup failed, CT untouched: {e}")),
        }
    };
    let d = update_body(cx, g, cfg, run, &gr, &before, &nice);
    let Some(entry) = entry else { return d };
    settle(cx, g, cfg, run, started_by_us, entry, d)
}

/// Refresco + simulación, upgrade y verificación. Todo lo que falle aquí dispara el rollback de `settle`.
fn update_body(cx: &mut Ctx, g: &Guest, cfg: &Config, run: &mut CtRun, gr: &Guest, before: &BTreeSet<String>, nice: &ExecOpts) -> Done {
    let id = g.vmid.to_string();
    let host = cx.host;
    let units_cmd = ["systemctl", "--failed", "--no-legend", "--plain"];
    // --- refresco + simulación (reutiliza el sondeo con refresh=1 y edad mínima 0)
    cx.step(Step::Refresh);
    let co = CheckOpts { refresh: true, read_only: false, refresh_min_age_s: 0, timeout_s: cfg.check.timeout_s, nice: cfg.general.nice, stale_after_s: cfg.check.index_stale_days * 86400 };
    cx.say("$ (refresh index + simulate)");
    let res = check::check_one(host, gr, cfg, &co);
    let Some(info) = res.info.filter(|_| res.status == Status::Ok) else {
        return fail(Step::Refresh, format!("refresh failed: {:?}: {}", res.status, res.detail));
    };
    cx.say(&format!("{} package(s) to upgrade{}", info.packages.len(), if info.held.is_empty() { String::new() } else { format!(", {} held", info.held.len()) }));
    run.packages_before = info.packages.len();
    if info.packages.is_empty() {
        return Done { outcome: Outcome::Skipped, step: Step::Refresh, detail: "already up to date after refresh".into() };
    }
    // --- upgrade
    cx.step(Step::Upgrade);
    let Some(argv) = upgrade_argv(&info.pm, &cfg.update.apt_mode) else { return Done { outcome: Outcome::Skipped, step: Step::Upgrade, detail: format!("unsupported package manager: {}", info.pm) } };
    let uo = ExecOpts { nice: Some(cfg.general.nice), ionice: Some(io(cfg)), timeout_s: Some(cfg.update.timeout_min * 60), no_kill_on_timeout: true, ..Default::default() };
    match cx.guest(&argv, &uo) {
        Ok(0) => {}
        Ok(c) => return fail(Step::Upgrade, format!("{} exited with {c}", argv.iter().find(|a| !a.contains('=') && **a != "env").copied().unwrap_or("upgrade"))),
        Err(e) if e.contains("timed out") => {
            // H2: el upgrade no se mata a medias (el host esperó a que terminara); el estado del CT es incierto:
            // sin rollback automático, el backup se conserva, el CT se marca fallido y la corrida para.
            let why = format!("upgrade exceeded {} min ({e}); check inside the CT: lxc-attach -n {id} -- dpkg --configure -a", cfg.update.timeout_min);
            cx.say(&format!("WARNING: {why}"));
            cx.abort = Some(format!("upgrade of CT {id} exceeded its {} min timeout", cfg.update.timeout_min));
            cx.hold = true;
            return fail(Step::Upgrade, why);
        }
        Err(e) => return fail(Step::Upgrade, format!("upgrade: {e}")),
    }
    // --- verificación posterior
    cx.step(Step::Verify);
    if info.pm == "apt" {
        match cx.guest(&["dpkg", "--audit"], nice) {
            Ok(0) if cx.capture.iter().all(|l| l.trim().is_empty()) => cx.say("dpkg --audit: clean"),
            Ok(0) => return fail(Step::Verify, "dpkg --audit reports broken packages"),
            Ok(c) => return fail(Step::Verify, format!("dpkg --audit exited with {c}")),
            Err(e) => return fail(Step::Verify, format!("dpkg --audit: {e}")),
        }
    }
    let after = cx.guest(&units_cmd, nice).map(|_| failed_units(&cx.capture)).unwrap_or_default();
    run.new_failed_units = after.difference(before).cloned().collect();
    cx.say(&format!("failed units: {} before, {} after", before.len(), after.len()));
    let post = check::check_one(host, gr, cfg, &CheckOpts { refresh: false, ..co });
    if let (Status::Ok, Some(i)) = (post.status, &post.info) {
        run.packages_after = Some(i.packages.len());
        run.reboot_required = i.reboot_required;
        persist_info(host, g.vmid, i);
    }
    if !run.new_failed_units.is_empty() {
        return fail(Step::Verify, format!("new failed units after upgrade: {}", run.new_failed_units.join(", ")));
    }
    let mut detail = format!("{} → {} package(s) pending", run.packages_before, run.packages_after.unwrap_or(0));
    let mut outcome = Outcome::Ok;
    if run.reboot_required {
        if cfg.update.reboot == "if-required" {
            match cx.host_cmd(&["pct", "reboot", &id], &ExecOpts::default()) {
                Ok(0) => detail += "; rebooted (reboot-required)",
                _ => {
                    outcome = Outcome::Warn;
                    detail += "; reboot required but `pct reboot` failed";
                }
            }
            run.reboot_required = false;
        } else {
            outcome = Outcome::Warn;
            detail += "; reboot required";
        }
    }
    Done { outcome, step: Step::Done, detail }
}

fn io(cfg: &Config) -> IoPrio {
    if cfg.general.io_class == "best-effort" { IoPrio::BestEffort7 } else { IoPrio::Idle }
}

// ---------------------------------------------------------------- backups temporales (ARCHITECTURE §3.3)

/// Alcance real del backup: vzdump no incluye los bind mounts (`mpN` con `backup=0`) y un rollback no los revierte.
pub const BACKUP_SCOPE_NOTE: &str = "NOTE: the backup and the rollback cover only the CT's rootfs (system disk); bind mounts and mount points with backup=0 (e.g. data under /mnt/pve/...) are NOT saved and NOT reverted";

/// `auto`: snapshot si PVE dice que el CT los admite (`pvesh … feature`) y, si no, vzdump; `snapshot`/`vzdump`: solo esa forma.
/// El backup queda anotado (`Active`) antes de tocar el CT.
fn create_backup(cx: &mut Ctx, g: &Guest, cfg: &Config, mode: &str) -> Result<Entry, String> {
    let host = cx.host;
    let stamp = stamp(now_s(host));
    let entry = match mode {
        "snapshot" => match snapshot_supported(cx, g) {
            Ok(true) => take_snapshot(cx, g, cfg, &stamp)?,
            Ok(false) => return Err("this CT does not support snapshots (bind mounts or storage without snapshots); use backup mode \"auto\" or \"vzdump\"".into()),
            Err(e) => return Err(format!("cannot ask PVE whether this CT supports snapshots: {e}")),
        },
        "vzdump" => take_vzdump(cx, g, cfg, &stamp)?,
        _ => match snapshot_supported(cx, g) {
            Ok(true) => take_snapshot(cx, g, cfg, &stamp)?,
            other => {
                let why = match other {
                    Ok(_) => "PVE reports no snapshot support for this CT".to_string(),
                    Err(e) => format!("cannot ask PVE: {e}"),
                };
                cx.say(&format!("snapshot not used ({why}); using vzdump on storage \"{}\"", cfg.backup.storage));
                take_vzdump(cx, g, cfg, &stamp)?
            }
        },
    };
    let mut reg = Registry::load(host);
    for w in std::mem::take(&mut reg.warnings) {
        cx.say(&w);
    }
    if reg.tainted {
        cx.no_prune = true;
        reg.tainted = false;
    }
    let orphans = reg.hold_orphans(g.vmid);
    if orphans > 0 {
        cx.say(&format!("WARNING: {orphans} pxu backup(s) of this CT were left by an interrupted run: kept, never deleted automatically"));
    }
    reg.entries.push(entry.clone());
    if let Err(e) = reg.save(host) {
        if entry.kind == Kind::Vzdump {
            // sin registro no hay forma segura de borrarlo luego: se deshace y no se actualiza
            let _ = delete_backup(cx, &entry);
            return Err(format!("cannot record the backup in {}: {e}", backup::REGISTRY_PATH));
        }
        cx.say(&format!("WARNING: cannot record the snapshot in {} ({e}); it is still found by its pxu- name", backup::REGISTRY_PATH));
    }
    Ok(entry)
}

/// `pvesh get /nodes/<nodo>/lxc/<id>/feature --feature snapshot` → `{"hasFeature":0|1}`. Solo lectura: no deja nada a medias.
fn snapshot_supported(cx: &mut Ctx, g: &Guest) -> Result<bool, String> {
    if !backup::is_pve_id(&g.node) {
        return Err(format!("unusable node name {:?}", g.node));
    }
    let path = format!("/nodes/{}/lxc/{}/feature", g.node, g.vmid);
    match cx.host_cmd(&["pvesh", "get", &path, "--feature", "snapshot", "--output-format", "json"], &ExecOpts::default()) {
        Ok(0) => {}
        Ok(c) => return Err(format!("pvesh exit {c}")),
        Err(e) => return Err(e),
    }
    let v: serde_json::Value = serde_json::from_str(&cx.capture.join("\n")).map_err(|e| format!("unreadable pvesh answer: {e}"))?;
    match v.get("hasFeature") {
        Some(serde_json::Value::Number(n)) => Ok(n.as_i64() == Some(1)),
        Some(serde_json::Value::Bool(b)) => Ok(*b),
        _ => Err("pvesh answer has no hasFeature".into()),
    }
}

fn take_snapshot(cx: &mut Ctx, g: &Guest, cfg: &Config, stamp: &str) -> Result<Entry, String> {
    let id = g.vmid.to_string();
    let mut taken = g.conf.snapshots.clone();
    taken.extend(Registry::load(cx.host).entries.iter().filter(|e| e.vmid == g.vmid && e.kind == Kind::Snapshot).map(|e| e.name.clone()));
    let name = backup::unique_snapshot_name(stamp, &taken).ok_or("no free snapshot name for this second (pxu-<stamp>-2 … -99 exist)")?;
    let o = ExecOpts { nice: Some(cfg.general.nice), ionice: Some(io(cfg)), ..Default::default() };
    match cx.host_cmd(&["pct", "snapshot", &id, &name], &o) {
        Ok(0) => Ok(Entry { vmid: g.vmid, kind: Kind::Snapshot, name, stamp: stamp.into(), status: backup::Status::Active, storage: None }),
        Ok(c) => Err(format!("pct snapshot exit {c}")),
        Err(e) => Err(e),
    }
}

fn take_vzdump(cx: &mut Ctx, g: &Guest, cfg: &Config, stamp: &str) -> Result<Entry, String> {
    let id = g.vmid.to_string();
    // sin el storage del rootfs el rollback restauraría en `local`: no se hace el backup (CT intacto)
    let storage = backup::rootfs_storage(&g.conf).ok_or("cannot read the rootfs storage from the CT config (rollback needs it)")?;
    let o = ExecOpts { nice: Some(cfg.general.nice), ionice: Some(IoPrio::BestEffort7), ..Default::default() };
    let argv = ["vzdump", &id, "--mode", "snapshot", "--storage", &cfg.backup.storage, "--compress", "zstd", "--zstd", "1", "--ionice", "7", "--notes-template", "pxu pre-update {{guestname}}"];
    match cx.host_cmd(&argv, &o) {
        Ok(0) => {
            let archive = cx.capture.iter().find_map(|l| l.split("creating vzdump archive '").nth(1)?.split('\'').next().map(String::from));
            let volid = archive.and_then(|a| backup::volid(&cfg.backup.storage, &a, g.vmid)).ok_or("vzdump finished but its archive could not be identified (see log)")?;
            let stamp = Some(backup::vzdump_stamp(&volid)).filter(|s| !s.is_empty()).unwrap_or_else(|| stamp.into());
            Ok(Entry { vmid: g.vmid, kind: Kind::Vzdump, name: volid, stamp, status: backup::Status::Active, storage: Some(storage) })
        }
        Ok(c) => Err(format!("vzdump exit {c}")),
        Err(e) => Err(e),
    }
}

/// Borra un backup que consta como de pxu. Solo `pct delsnapshot pxu-…` o `pvesm free <vzdump-lxc>`.
fn delete_backup(cx: &mut Ctx, e: &Entry) -> Result<(), String> {
    let id = e.vmid.to_string();
    let r = match e.kind {
        Kind::Snapshot => cx.host_cmd(&["pct", "delsnapshot", &id, &e.name], &ExecOpts::default()),
        Kind::Vzdump => cx.host_cmd(&["pvesm", "free", &e.name], &ExecOpts::default()),
    };
    match r {
        Ok(0) => Ok(()),
        Ok(c) => Err(format!("exit {c}")),
        Err(e) => Err(e),
    }
}

/// Borra el backup y lo quita del registro. Si no se puede borrar, queda como `Kept` (la siguiente poda lo reintenta).
fn discard(cx: &mut Ctx, e: &Entry) -> Result<(), String> {
    match delete_backup(cx, e) {
        Ok(()) => {
            let host = cx.host;
            let mut reg = Registry::load(host);
            reg.remove(e);
            if let Err(err) = reg.save(host) {
                cx.say(&format!("WARNING: cannot update {}: {err}", backup::REGISTRY_PATH));
            }
            cx.say(&format!("backup deleted: {}", e.describe()));
            Ok(())
        }
        Err(why) => {
            let host = cx.host;
            let mut reg = Registry::load(host);
            reg.set_status(e, backup::Status::Kept);
            let _ = reg.save(host);
            cx.say(&format!("WARNING: could not delete {} ({why}); it stays registered and is retried by the next run", e.describe()));
            Err(why)
        }
    }
}

/// Conserva el backup (rollback fallido, timeout): nunca se borra solo.
fn hold(cx: &mut Ctx, e: &Entry) {
    let host = cx.host;
    let mut reg = Registry::load(host);
    reg.set_status(e, backup::Status::Held);
    if let Err(err) = reg.save(host) {
        cx.say(&format!("WARNING: cannot update {}: {err}", backup::REGISTRY_PATH));
    }
}

/// Deja el CT como estaba antes del update: parar → `pct rollback`/`pct restore --force --storage <rootfs>` → arrancar si estaba encendido y comprobar que responde.
fn rollback(cx: &mut Ctx, g: &Guest, cfg: &Config, e: &Entry, was_running: bool) -> Result<(), String> {
    let id = g.vmid.to_string();
    cx.step(Step::Rollback);
    cx.say(&format!("update failed: rolling back to {}", e.describe()));
    let _ = cx.host_cmd(&["pct", "shutdown", &id, "--timeout", "60"], &ExecOpts::default()); // best effort
    // `pct stop` sobre un CT ya detenido sale con 255 en PVE: solo se llama si sigue encendido, y si falla se vuelve a mirar el estado real.
    if crate::share::proxmox::guests::lxc_running(cx.host, g.vmid) {
        let stopped = match cx.host_cmd(&["pct", "stop", &id], &ExecOpts::default()) {
            Ok(0) => true,
            Ok(_) | Err(_) => !crate::share::proxmox::guests::lxc_running(cx.host, g.vmid),
        };
        if !stopped {
            return Err("pct stop failed and the CT is still running".into());
        }
    }
    let r = match e.kind {
        Kind::Snapshot => cx.host_cmd(&["pct", "rollback", &id, &e.name], &ExecOpts::default()),
        Kind::Vzdump => match e.storage.as_deref() {
            Some(st) => cx.host_cmd(&["pct", "restore", &id, &e.name, "--force", "--storage", st], &ExecOpts::default()),
            None => return Err("no rootfs storage recorded for this backup".into()),
        },
    };
    match r {
        Ok(0) => {}
        Ok(c) => return Err(format!("restore exit {c}")),
        Err(er) => return Err(format!("restore: {er}")),
    }
    if !was_running {
        cx.say("CT restored and left stopped, as before");
        return Ok(());
    }
    match cx.host_cmd(&["pct", "start", &id], &ExecOpts::default()) {
        Ok(0) => {}
        Ok(c) => return Err(format!("restored, but pct start exit {c}")),
        Err(er) => return Err(format!("restored, but pct start: {er}")),
    }
    let nice = ExecOpts { nice: Some(cfg.general.nice), ionice: Some(io(cfg)), ..Default::default() };
    for attempt in 0..3 {
        if !cx.host.is_mock() {
            std::thread::sleep(std::time::Duration::from_secs(if attempt == 0 { 5 } else { 3 }));
        }
        if matches!(cx.guest(&["df", "-Pk", "/"], &nice), Ok(0)) {
            cx.say("CT restored and running again");
            return Ok(());
        }
    }
    Err("restored, but the CT does not answer after starting".into())
}

/// Decide qué pasa con el backup según cómo acabó el CT:
/// éxito → poda a `keep` (con 0 se borra); sin cambios → se borra; fallo → rollback y borrado;
/// timeout del upgrade o rollback fallido → el backup se conserva, error y corrida detenida.
fn settle(cx: &mut Ctx, g: &Guest, cfg: &Config, run: &mut CtRun, started_by_us: &mut bool, entry: Entry, mut d: Done) -> Done {
    let host = cx.host;
    let keep = cfg.keep_for(g.vmid);
    match d.outcome {
        Outcome::Skipped => {
            // Nada que actualizar: el backup no sirve de nada.
            match discard(cx, &entry) {
                Ok(()) => run.backup = Some(format!("{} (deleted)", entry.describe())),
                Err(why) => d.detail += &format!("; WARNING: backup {} could not be deleted ({why})", entry.describe()),
            }
        }
        Outcome::Ok | Outcome::Warn => {
            let mut reg = Registry::load(host);
            reg.set_status(&entry, backup::Status::Kept);
            if let Err(e) = reg.save(host) {
                cx.say(&format!("WARNING: cannot update {}: {e}", backup::REGISTRY_PATH));
            }
            let plan = if cx.no_prune {
                cx.say("WARNING: the backup registry was corrupt or inconsistent: nothing is pruned in this run (the new backup is kept)");
                vec![]
            } else {
                reg.prune_plan(g.vmid, &g.conf.snapshots, keep)
            };
            let mut left = vec![];
            for victim in &plan {
                if discard(cx, victim).is_err() {
                    left.push(victim.describe());
                }
            }
            let mine_deleted = plan.iter().any(|v| v.name == entry.name && v.kind == entry.kind) && !left.contains(&entry.describe());
            run.backup = Some(format!("{} ({})", entry.describe(), if mine_deleted { "deleted" } else { "kept" }));
            if !left.is_empty() {
                d.detail += &format!("; WARNING: could not delete: {}", left.join(", "));
                if d.outcome == Outcome::Ok {
                    d.outcome = Outcome::Warn;
                }
            }
        }
        Outcome::Failed if cx.hold => {
            hold(cx, &entry);
            run.revert = Some(entry.manual_revert());
            run.backup = Some(format!("{} (kept)", entry.describe()));
            run.rollback = Some("not_attempted".into());
            d.detail += &format!("; NO automatic rollback (the upgrade may still have been mid-way): backup {} kept, decide manually: {}", entry.describe(), entry.manual_revert());
            cx.say(&format!("WARNING: backup {} kept; no rollback because the upgrade timed out", entry.describe()));
        }
        Outcome::Failed => match rollback(cx, g, cfg, &entry, !*started_by_us) {
            Ok(()) => {
                // El CT vuelve a su estado previo; si estaba detenido, ya lo está (no hay nada que restaurar luego).
                *started_by_us = false;
                run.rollback = Some("done".into());
                run.new_failed_units.clear();
                match discard(cx, &entry) {
                    Ok(()) => run.backup = Some(format!("{} (deleted after rollback)", entry.describe())),
                    Err(why) => {
                        d.detail += &format!("; WARNING: rolled back, but backup {} could not be deleted ({why})", entry.describe());
                        run.backup = Some(format!("{} (kept)", entry.describe()));
                    }
                }
                d.detail += "; rolled back to the pre-update backup";
            }
            Err(why) => {
                hold(cx, &entry);
                run.rollback = Some("failed".into());
                run.revert = Some(entry.manual_revert());
                run.backup = Some(format!("{} (kept)", entry.describe()));
                d.detail += &format!("; CRITICAL: automatic rollback FAILED ({why}); backup {} kept: {}", entry.describe(), entry.manual_revert());
                cx.abort = Some(format!("rollback of CT {} failed", g.vmid));
                cx.say(&format!("CRITICAL: rollback failed: {why}; backup {} kept", entry.describe()));
            }
        },
        Outcome::Cancelled => {}
    }
    d
}

fn persist_info(host: &dyn Host, vmid: u32, info: &CheckInfo) {
    let mut st = state::load_state(host);
    st.checks.insert(vmid.to_string(), info.clone());
    let _ = state::save_state(host, &st);
}

// ---------------------------------------------------------------- notificaciones

/// CT con la etiqueta de auto-update que `select` no lista porque no tienen nada pendiente.
pub fn idle_auto<'a>(inv: &'a Inventory, cands: &[Candidate]) -> Vec<&'a Guest> {
    inv.guests.iter().filter(|g| g.kind == GuestKind::Lxc && g.auto_update && !cands.iter().any(|c| c.vmid == g.vmid)).collect()
}

/// Un único aviso al final de la corrida (tabla de `report`). Se envía con `run_summary`; con solo `update_failed`, únicamente si algo falló
/// (un CT, el host o el driver NVIDIA) o la corrida paró por un fallo.
pub fn messages(report: &RunReport, c: &AppriseCfg) -> Vec<Message> {
    if (report.results.is_empty() && report.nvidia.is_none()) || report::nothing_to_do(report) {
        return vec![];
    }
    let mut m = report::message(report);
    if !c.wants("run_summary") {
        if !(c.wants("update_failed") && report::level(report) == Level::Failure) {
            return vec![];
        }
        m.event = "update_failed".into();
    }
    vec![m]
}

/// Envía los avisos del resultado; con transporte `none` no hace nada. Una línea por aviso.
pub fn notify_run(host: &dyn Host, c: &AppriseCfg, report: &RunReport) -> Vec<String> {
    let mut lines = Vec::new();
    for m in messages(report, c) {
        match notify::notify(host, c, &m) {
            notify::Outcome::Sent(t) => lines.push(format!("{}: sent via {t}", m.event)),
            notify::Outcome::Queued(e) => lines.push(format!("{}: send failed ({e}); queued", m.event)),
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
    use crate::share::host::ReadOnlyHost;
    use crate::share::mock::{MockHost, Recorded};
    use crate::share::update::backup::{Kind, Registry, Status};
    use crate::share::{config::Config, proxmox::guests};

    use crate::share::mock::scenario::{std_updates, Ct, FailAt, World};

    /// CT genéricos con tag `auto`, tres updates pendientes y un chequeo reciente; backup `auto`.
    fn fleet(ids: &[u32]) -> World {
        ids.iter().fold(World::new().backup_mode("auto").require_fresh_minutes(60), |w, &id| w.ct(Ct::new(id).tags("auto").updates(std_updates()).checked_minutes_ago(5)))
    }
    /// Casos de update: cada nombre es un servidor genérico armado en código (no hay datos en disco).
    fn world(name: &str) -> World {
        match name {
            "update-ok" | "update-two-fails" => World::preset(if name == "update-ok" { "update-ok" } else { "update-fail" }).unwrap(),
            "update-backup-fail" => fleet(&[411, 412, 413]).fail(412, FailAt::Backup, 255),
            "update-timeout" => fleet(&[431, 432]).hang(431, FailAt::Upgrade),
            "update-pm-lock" => fleet(&[441, 442, 443]).proxmenux_reservation(441).proxmenux_invalid_marker(442),
            "update-no-backup" => fleet(&[481]).backup_mode("none"),
            "update-timeout-backup" => fleet(&[471, 472]).backup_mode("vzdump").hang(471, FailAt::Upgrade),
            "update-timeout-stopped" => World::new()
                .ct(Ct::new(491).tags("auto").stopped().updates(std_updates()).checked_minutes_ago(5))
                .backup_mode("vzdump")
                .require_fresh_minutes(60)
                .stopped_policy("start_and_restore")
                .hang(491, FailAt::Upgrade),
            // 451 ok · 452 upgrade falla · 453 upgrade y rollback fallan · 454/457 con bind mount (sin snapshots) · 455 apagado · 458 ya tiene un snapshot de ese segundo
            "update-backup-policy" => {
                let w = fleet(&[451, 452, 453, 456]).stopped_policy("start_and_restore");
                w.ct(Ct::new(454).tags("auto").updates(std_updates()).checked_minutes_ago(5).bind_mount(0))
                    .ct(Ct::new(455).tags("auto").stopped().updates(std_updates()).checked_minutes_ago(5))
                    .ct(Ct::new(457).tags("auto").updates(std_updates()).checked_minutes_ago(5).bind_mount(0))
                    .ct(Ct::new(458).tags("auto").updates(std_updates()).checked_minutes_ago(5).snapshots(&["pxu-20261108031600"]))
                    .fail(452, FailAt::Upgrade, 100)
                    .fail(453, FailAt::Upgrade, 100)
                    .fail(453, FailAt::Rollback, 1)
                    .fail(455, FailAt::Upgrade, 100)
                    .fail(457, FailAt::Upgrade, 100)
            }
            "update-keep" => {
                let reg = serde_json::json!({"schema": 1, "entries": [
                    {"vmid": 461, "kind": "vzdump", "name": "local:backup/vzdump-lxc-461-2026_10_30-03_00_00.tar.zst", "stamp": "202610300300", "status": "kept"},
                    {"vmid": 461, "kind": "snapshot", "name": "pxu-202611030300", "stamp": "202611030300", "status": "kept"},
                    {"vmid": 461, "kind": "snapshot", "name": "pxu-202611010300", "stamp": "202611010300", "status": "held"},
                    {"vmid": 999, "kind": "snapshot", "name": "pxu-202611010300", "stamp": "202611010300", "status": "kept"},
                ]});
                World::new()
                    .ct(Ct::new(461).tags("auto").updates(std_updates()).checked_minutes_ago(5).snapshots(&["pxu-202611010300", "pxu-202611020300", "pxu-202611030300", "manual-before-upgrade", "zfs_auto_5412", "pxu-custom"]))
                    .ct(Ct::new(462).tags("auto").updates(std_updates()).checked_minutes_ago(5).snapshots(&["pxu-202611010300"]))
                    .backup_mode("auto")
                    .require_fresh_minutes(60)
                    .keep(2)
                    .ct_config(462, &["keep = 0"])
                    .file("/var/lib/pxu/backups.json", reg.to_string().as_bytes())
            }
            "failures" => World::preset("failures").unwrap(),
            _ => panic!("unknown case {name}"),
        }
    }
    fn host(name: &str) -> ReadOnlyHost<MockHost> {
        ReadOnlyHost::new(world(name).build())
    }
    fn go(h: &ReadOnlyHost<MockHost>, only: &[u32]) -> RunReport {
        h.set_update_enabled(true);
        let cfg = Config::load(h).unwrap();
        let inv = guests::load(h);
        run(h, &inv, &cfg, &Opts { only: only.to_vec() }, &AtomicBool::new(false), &mut |_| {}).unwrap()
    }
    fn inv_of(h: &ReadOnlyHost<MockHost>) -> Inventory {
        guests::load(h)
    }
    fn guest_cmds(h: &ReadOnlyHost<MockHost>, vmid: u32) -> Vec<String> {
        h.inner().recorded().into_iter().filter_map(|r| match r { Recorded::Guest(v, a) if v == vmid => Some(a.join(" ")), _ => None }).collect()
    }
    fn host_cmds(h: &ReadOnlyHost<MockHost>) -> Vec<String> {
        h.inner().recorded().into_iter().filter_map(|r| match r { Recorded::Host(a) => Some(a.join(" ")), _ => None }).filter(|c| !c.starts_with("curl")).collect()
    }

    #[test]
    fn upgrade_timeout_marks_ct_failed_and_stops_the_run() {
        let h = host("update-timeout");
        let rep = go(&h, &[431, 432]);
        let by = |id| rep.results.iter().find(|r| r.vmid == id).unwrap();
        assert_eq!(by(431).outcome, Outcome::Failed);
        assert!(by(431).detail.contains("dpkg --configure -a") && by(431).detail.contains("not killed"), "{}", by(431).detail);
        // la corrida se detiene: el siguiente CT ni se toca
        assert_eq!(by(432).outcome, Outcome::Skipped);
        assert!(guest_cmds(&h, 432).iter().all(|c| !c.contains("upgrade")));
        assert_eq!(rep.exit_code(), EXIT_ABORTED);
        assert!(rep.aborted.as_deref().unwrap().contains("431"));
    }

    #[test]
    fn upgrade_is_never_run_with_a_killing_timeout() {
        // El motor pide `no_kill_on_timeout`: el host real espera a apt-get/dpkg en lugar de mandar SIGKILL.
        let src = include_str!("mod.rs");
        assert!(src.contains("timeout_min * 60), no_kill_on_timeout: true"));
    }

    #[test]
    fn proxmenux_reservation_and_invalid_marker_skip_the_ct() {
        let h = host("update-pm-lock");
        let rep = go(&h, &[441, 442, 443]);
        let by = |id| rep.results.iter().find(|r| r.vmid == id).unwrap();
        assert_eq!(by(441).outcome, Outcome::Skipped);
        assert!(by(441).detail.contains("reservation"), "{}", by(441).detail);
        assert_eq!(by(442).outcome, Outcome::Skipped);
        assert!(by(442).detail.contains("invalid"), "{}", by(442).detail);
        assert_eq!(by(443).outcome, Outcome::Ok);
        for id in [441, 442] {
            assert!(guest_cmds(&h, id).is_empty(), "no guest command for {id}");
        }
        assert!(host_cmds(&h).iter().all(|c| !c.contains("441") && !c.contains("442")));
    }

    #[test]
    fn ct_output_is_sanitized_in_events_and_log() {
        let mut cx_lines = Vec::new();
        let h = host("update-ok");
        let mut ev = |e: Event| {
            if let Event::Line(_, l) = e {
                cx_lines.push(l);
            }
        };
        let mut cx = Ctx { host: &h, vmid: 401, log: Vec::new(), dropped: 0, capture: Vec::new(), abort: None, hold: false, no_prune: false, ev: &mut ev, security: 0 };
        cx.line("ok \x1b]0;pwned\x07 fetch https://user:secret@repo.example/x\x1b[2J");
        assert!(cx.log.iter().all(|l| !l.chars().any(|c| c.is_control())));
        assert!(!cx.log[0].contains("secret") && cx.log[0].contains("https://***@repo.example"), "{}", cx.log[0]);
        assert!(cx.capture.iter().all(|l| !l.contains('\u{1b}')));
        drop(cx);
        assert!(cx_lines.iter().all(|l| !l.contains('\u{1b}') && !l.contains('\u{7}')));
    }

    /// Host cuyo `stat` siempre falla (EACCES): la config no debe usarse (L1).
    struct NoStat(MockHost);
    impl Host for NoStat {
        fn read_file(&self, p: &std::path::Path) -> std::io::Result<Vec<u8>> { self.0.read_file(p) }
        fn read_dir(&self, p: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> { self.0.read_dir(p) }
        fn exists(&self, p: &std::path::Path) -> bool { self.0.exists(p) }
        fn guest_exec(&self, v: u32, a: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> std::io::Result<crate::share::host::ExitInfo> { self.0.guest_exec(v, a, o, s) }
        fn host_cmd(&self, a: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> std::io::Result<crate::share::host::ExitInfo> { self.0.host_cmd(a, o, s) }
        fn write_file(&self, p: &std::path::Path, d: &[u8]) -> std::io::Result<()> { self.0.write_file(p, d) }
        fn remove_file(&self, p: &std::path::Path) -> std::io::Result<()> { self.0.remove_file(p) }
        fn lock_dir(&self) -> &std::path::Path { self.0.lock_dir() }
        fn try_lock(&self, p: &std::path::Path, c: bool) -> std::io::Result<Option<crate::share::host::LockGuard>> { self.0.try_lock(p, c) }
        fn file_meta(&self, _: &std::path::Path) -> std::io::Result<crate::share::host::FileMeta> { Err(std::io::ErrorKind::PermissionDenied.into()) }
        fn now(&self) -> std::time::SystemTime { self.0.now() }
    }

    #[test]
    fn config_is_refused_when_stat_fails() {
        let h = NoStat(world("update-ok").build());
        let e = Config::load(&h).unwrap_err();
        assert!(e.contains("cannot stat"), "{e}");
    }

    #[test]
    fn update_opt_in_is_revoked_on_every_exit() {
        let h = host("update-ok");
        let blocked = |h: &ReadOnlyHost<MockHost>| {
            h.host_cmd(&["pct", "start", "401"], &ExecOpts::default(), &mut Vec::<String>::new()).is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
        };
        assert!(blocked(&h));
        {
            let _g = crate::share::host::UpdateOptIn::new(&h);
            h.set_update_target(Some(401));
            assert!(!blocked(&h));
        } // salida normal / return temprano
        assert!(blocked(&h));
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = crate::share::host::UpdateOptIn::new(&h);
            h.set_update_target(Some(401));
            panic!("boom");
        }));
        assert!(r.is_err() && blocked(&h), "revoked after a panic");
    }

    #[test]
    fn stamp_is_utc_civil_time() {
        assert_eq!(stamp(1794107760), "20261108031600");
        assert_eq!(stamp(1794107765), "20261108031605");
        assert_eq!(stamp(0), "19700101000000");
    }

    #[test]
    fn upgrade_argvs_are_recognised_and_nothing_else() {
        for pm in ["apt", "apk", "pacman", "dnf", "zypper"] {
            let a = upgrade_argv(pm, "dist-upgrade").unwrap();
            assert!(is_upgrade_argv(&a), "{pm}");
        }
        assert!(upgrade_argv("apt", "x").unwrap().contains(&"Dpkg::Options::=--force-confold"));
        assert!(!is_upgrade_argv(&["apt-get", "remove", "-y", "curl"]));
        assert!(!is_upgrade_argv(&["sh", "-c", "apt-get dist-upgrade"]));
    }

    #[test]
    fn update_ok_runs_each_step_in_order_and_restores_stopped_ct() {
        let h = host("update-ok");
        let rep = go(&h, &[]);
        assert_eq!(rep.exit_code(), 0);
        assert_eq!(rep.count(Outcome::Ok), 3, "{rep:?}");
        let g = guest_cmds(&h, 401);
        let kinds: Vec<&str> = g.iter().map(|c| if c.starts_with("df") { "df" } else if c.starts_with("systemctl") { "units" } else if c.contains("pxu-probe") { "probe" } else if c.contains("dist-upgrade") { "upgrade" } else if c.starts_with("dpkg") { "audit" } else { "?" }).collect();
        assert_eq!(kinds, ["df", "units", "probe", "upgrade", "audit", "units", "probe"], "{g:?}");
        assert!(g[2].ends_with("pxu-probe 1 0"), "refresh+simulate forces a refresh: {}", g[2]);
        assert!(g[6].ends_with("pxu-probe 0"), "post-verify probe does not refresh");
        let hc = host_cmds(&h);
        assert!(hc.iter().any(|c| c == "pct snapshot 401 pxu-20261108031600"), "default mode is auto: pct snapshot first: {hc:?}");
        assert!(hc.iter().any(|c| c == "pct delsnapshot 401 pxu-20261108031600"), "success deletes the backup (keep = 0): {hc:?}");
        assert!(hc.iter().any(|c| c == "pvesm free local:backup/vzdump-lxc-402-2026_11_08-03_16_00.tar.zst"), "{hc:?}");
        assert!(hc.iter().all(|c| !c.contains("rollback") && !c.contains("restore")), "{hc:?}");
        assert!(Registry::load(h.inner()).entries.is_empty(), "nothing left in the registry");
        assert!(hc.iter().any(|c| c.starts_with("vzdump 402 --mode snapshot --storage local --compress zstd --zstd 1 --ionice 7")), "per-CT backup override: {hc:?}");
        let (start, stop) = (hc.iter().position(|c| c == "pct start 403"), hc.iter().position(|c| c == "pct shutdown 403 --timeout 60"));
        assert!(start.is_some() && stop > start, "stopped CT is started then returned to stopped: {hc:?}");
        assert!(rep.log_path.as_deref().is_some_and(|p| runlog::is_run_log_path(std::path::Path::new(p))));
        assert_eq!(rep.results[0].log_path, rep.log_path);
        assert!(h.inner().read_file(std::path::Path::new(rep.log_path.as_ref().unwrap())).is_ok());
    }

    #[test]
    fn backup_failure_leaves_the_ct_untouched_and_does_not_stop_the_run() {
        let h = host("update-backup-fail");
        let rep = go(&h, &[]);
        let r = |id| rep.results.iter().find(|r| r.vmid == id).unwrap();
        assert_eq!((r(411).outcome, r(412).outcome, r(413).outcome), (Outcome::Ok, Outcome::Failed, Outcome::Ok));
        assert_eq!(r(412).step, Step::Backup);
        assert!(!guest_cmds(&h, 412).iter().any(|c| c.contains("dist-upgrade")), "no upgrade after a failed backup");
        assert_eq!(rep.exit_code(), EXIT_PARTIAL);
        assert!(rep.aborted.is_none(), "one failure is not a circuit break");
    }

    #[test]
    fn two_failures_in_a_row_stop_the_run_with_revert_command() {
        let h = host("update-two-fails");
        let rep = go(&h, &[]);
        let o: Vec<Outcome> = rep.results.iter().map(|r| r.outcome).collect();
        assert_eq!(o, [Outcome::Failed, Outcome::Failed, Outcome::Skipped, Outcome::Skipped]);
        assert_eq!(rep.exit_code(), EXIT_ABORTED);
        assert!(guest_cmds(&h, 423).is_empty() && guest_cmds(&h, 424).is_empty(), "later CTs are never touched");
        assert_eq!(rep.results[0].rollback.as_deref(), Some("done"), "421 was rolled back");
        assert!(rep.results[0].revert.is_none(), "a clean rollback needs no manual revert");
        assert!(rep.results[1].rollback.is_none() && rep.results[1].backup.is_none(), "422 never got a backup");
        let msgs = messages(&rep, &AppriseCfg::default());
        assert_eq!(msgs.len(), 1, "one notice per run");
        let m = &msgs[0];
        assert!(m.level == Level::Failure && m.body.contains("rollback: ✅ restored"), "{}", m.body);
        assert!(m.body.contains("no backup was taken"), "{}", m.body);
        assert!(msgs.iter().any(|m| m.event == "run_summary" && m.body.contains("stopped after 2 consecutive failures")));
    }

    #[test]
    fn explicit_selection_respects_proxmenux_lock_pve_lock_and_missing_check() {
        let h = host("failures");
        let rep = go(&h, &[105, 108, 109, 107]);
        let by = |id| rep.results.iter().find(|r| r.vmid == id).unwrap();
        assert!(by(105).detail.contains("ProxMenux"), "{:?}", by(105));
        assert!(by(108).detail.contains("locked by PVE"));
        assert!(by(109).detail.contains("stopped"));
        assert!(by(107).detail.contains("never checked"));
        assert!(rep.results.iter().all(|r| r.outcome == Outcome::Skipped));
        assert!(guest_cmds(&h, 105).iter().all(|c| !c.contains("upgrade")));
        assert!(host_cmds(&h).is_empty());
    }

    #[test]
    fn a_run_with_nothing_to_update_still_writes_its_log_and_does_not_notify() {
        let h = ReadOnlyHost::new(World::new().backup_mode("auto").require_fresh_minutes(60).ct(Ct::new(501).tags("auto").checked_minutes_ago(5)).build());
        let rep = go(&h, &[]);
        assert!(rep.results.iter().all(|r| r.outcome == Outcome::Skipped), "{:?}", rep.results);
        let path = rep.log_path.clone().expect("a run with nothing to do still has a run log");
        let text = String::from_utf8(h.read_file(std::path::Path::new(&path)).unwrap()).unwrap();
        let meta = runlog::parse_header(&text).expect("header");
        assert!(meta.complete && meta.result == "✅ nothing to do", "{meta:?}");
        assert!(text.contains("CT 501") && text.contains("no updates pending"), "the log says why each CT was skipped:\n{text}");
        assert!(!text.contains("| Upgrades"), "no empty table in a nothing-to-do summary:\n{text}");
        assert!(report::body(&rep).contains("Nothing to do: no CT or host had pending updates."));
        assert_eq!(idle_auto(&inv_of(&h), &[]).len(), 1, "the terminal count includes auto-update CTs with nothing pending");
        let listed = runlog::list(&h, std::path::Path::new(LOG_DIR));
        assert!(listed.iter().any(|e| e.meta.as_ref().is_some_and(|m| m.result == "✅ nothing to do")), "listed in Logs");
        assert!(messages(&rep, &AppriseCfg::default()).is_empty());
    }

    #[test]
    fn global_lock_blocks_a_second_run_and_is_released_after() {
        let h = host("update-ok");
        h.set_update_enabled(true);
        let held = h.try_lock(&h.lock_dir().join("pxu-run.lock"), true).unwrap().unwrap();
        let cfg = Config::load(&h).unwrap();
        let inv = guests::load(&h);
        let again = || run(&h, &inv, &cfg, &Opts { only: vec![] }, &AtomicBool::new(true), &mut |_| {});
        assert!(matches!(again(), Err(RunError::Locked)));
        drop(held);
        assert!(again().is_ok());
    }

    #[test]
    fn cancel_flag_cancels_every_pending_ct_and_exits_30() {
        let h = host("update-ok");
        h.set_update_enabled(true);
        let cfg = Config::load(&h).unwrap();
        let inv = guests::load(&h);
        let rep = run(&h, &inv, &cfg, &Opts { only: vec![] }, &AtomicBool::new(true), &mut |_| {}).unwrap();
        assert!(rep.results.iter().all(|r| r.outcome == Outcome::Cancelled) && rep.exit_code() == EXIT_ABORTED);
        assert!(h.inner().recorded().is_empty());
    }

    #[test]
    fn a_stopped_run_still_writes_its_log_with_the_stop_reason_in_the_header() {
        let h = host("update-ok");
        h.set_update_enabled(true);
        let cfg = Config::load(&h).unwrap();
        let inv = guests::load(&h);
        let rep = run(&h, &inv, &cfg, &Opts { only: vec![] }, &AtomicBool::new(true), &mut |_| {}).unwrap();
        let path = rep.log_path.clone().expect("a stopped run still has a run log");
        let text = String::from_utf8(h.read_file(std::path::Path::new(&path)).unwrap()).unwrap();
        let meta = runlog::parse_header(&text).expect("header");
        assert!(meta.result.contains("STOPPED"), "{}", meta.result);
        assert!(text.contains("STOPPED"), "the summary in the log carries the stop reason");
    }

    #[test]
    fn without_explicit_opt_in_nothing_can_be_written() {
        let h = host("update-ok");
        let (cfg, inv) = (Config::load(&h).unwrap(), guests::load(&h));
        let r = run(&h, &inv, &cfg, &Opts { only: vec![] }, &AtomicBool::new(false), &mut |_| {});
        assert!(matches!(r, Err(RunError::Io(_))), "no global lock without opt-in");
        let mut sink: Vec<String> = Vec::new();
        let up = upgrade_argv("apt", "dist-upgrade").unwrap();
        assert!(h.guest_exec(401, &up, &ExecOpts::default(), &mut sink).is_err());
        assert!(h.host_cmd(&["pct", "snapshot", "401", "pxu-20261108031600"], &ExecOpts::default(), &mut sink).is_err());
        assert!(h.write_file(std::path::Path::new("/var/log/pxu/401/1.log"), b"x").is_err());
        assert!(h.inner().recorded().is_empty());
        h.set_update_enabled(true);
        assert!(h.host_cmd(&["pct", "set", "401", "--onboot", "0"], &ExecOpts::default(), &mut sink).is_err(), "opt-in is not a blank cheque");
        assert!(h.host_cmd(&["pct", "snapshot", "401", "evil"], &ExecOpts::default(), &mut sink).is_err());
        assert!(h.host_cmd(&["pct", "delsnapshot", "401", "manual-before-upgrade"], &ExecOpts::default(), &mut sink).is_err(), "only pxu-<stamp> snapshots can be deleted");
        assert!(h.host_cmd(&["pct", "rollback", "401", "zfs_auto_1"], &ExecOpts::default(), &mut sink).is_err());
        assert!(h.host_cmd(&["pvesm", "free", "local:iso/debian.iso"], &ExecOpts::default(), &mut sink).is_err(), "only vzdump archives can be freed");
        assert!(h.guest_exec(401, &["apt-get", "remove", "-y", "curl"], &ExecOpts::default(), &mut sink).is_err());
    }

    #[test]
    fn config_check_flags_bad_permissions_values_and_unknown_keys() {
        let ok = crate::share::config::validate(&host("update-ok"));
        assert!(ok.errors.is_empty(), "{ok:?}");
        let bad_cfg = World::new().config_perms(0o644, 1000).config_raw("[update]\nstopped = \"maybe\"\nbogus = 1\n[nope]\nx = 1\n");
        let bad = crate::share::config::validate(&bad_cfg.clone().build());
        let all = bad.errors.join("|");
        assert!(all.contains("must be root") && all.contains("0644") && all.contains("stopped"), "{all}");
        assert!(bad.warnings.iter().any(|w| w.contains("bogus")) && bad.warnings.iter().any(|w| w.contains("[nope]")));
        assert!(Config::load(&bad_cfg.build()).is_err(), "load refuses a world-readable config");
        assert!(crate::share::config::validate(&World::new().config_raw("[update\n").build()).errors.iter().any(|e| e.contains("TOML")));
    }
    fn by(rep: &RunReport, id: u32) -> &CtRun {
        rep.results.iter().find(|r| r.vmid == id).unwrap()
    }
    fn for_ct(h: &ReadOnlyHost<MockHost>, id: u32) -> Vec<String> {
        host_cmds(h).into_iter().filter(|c| c.split_whitespace().any(|w| w == id.to_string()) || c.contains(&format!("-{id}-")) || c.contains(&format!("/{id}/"))).collect()
    }

    // --- política de backup temporal (VK) ---

    #[test]
    fn scenario_success_deletes_the_backup() {
        let h = host("update-backup-policy");
        let rep = go(&h, &[451]);
        let r = by(&rep, 451);
        assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
        assert!(r.backup.as_deref().unwrap().contains("deleted") && r.rollback.is_none() && r.revert.is_none(), "{r:?}");
        assert_eq!(for_ct(&h, 451), ["pvesh get /nodes/server/lxc/451/feature --feature snapshot --output-format json", "pct snapshot 451 pxu-20261108031600", "pct delsnapshot 451 pxu-20261108031600"]);
        assert!(Registry::load(h.inner()).entries.is_empty());
    }

    #[test]
    fn scenario_upgrade_failure_rolls_back_and_deletes_the_backup() {
        let h = host("update-backup-policy");
        let rep = go(&h, &[452]);
        let r = by(&rep, 452);
        assert_eq!((r.outcome, r.step), (Outcome::Failed, Step::Upgrade), "{r:?}");
        assert_eq!(r.rollback.as_deref(), Some("done"));
        assert!(r.revert.is_none() && r.backup.as_deref().unwrap().contains("deleted"), "{r:?}");
        let c = for_ct(&h, 452);
        let pos = |k: &str| c.iter().position(|x| x.starts_with(k)).unwrap_or_else(|| panic!("{k} missing in {c:?}"));
        assert!(pos("pct snapshot") < pos("pct shutdown") && pos("pct shutdown") < pos("pct rollback 452 pxu-20261108031600"), "{c:?}");
        assert!(!c.iter().any(|x| x.starts_with("pct stop")), "pct stop on an already stopped CT fails with 255 in PVE: {c:?}");
        assert!(pos("pct rollback") < pos("pct start") && pos("pct start") < pos("pct delsnapshot 452 pxu-20261108031600"), "delete only after the CT is back up: {c:?}");
        assert!(Registry::load(h.inner()).entries.is_empty());
        assert_eq!(rep.exit_code(), EXIT_PARTIAL);
    }

    #[test]
    fn scenario_rollback_failure_keeps_the_backup_and_stops_the_run() {
        let h = host("update-backup-policy");
        let rep = go(&h, &[453, 454, 456]);
        let r = by(&rep, 453);
        assert_eq!((r.outcome, r.rollback.as_deref()), (Outcome::Failed, Some("failed")), "{r:?}");
        assert!(r.backup.as_deref().unwrap().contains("kept"), "{r:?}");
        assert_eq!(r.revert.as_deref(), Some("pct status 453 | grep -q running && pct stop 453; pct rollback 453 pxu-20261108031600"));
        assert!(!host_cmds(&h).iter().any(|c| c.starts_with("pct delsnapshot 453") || c.starts_with("pvesm free")), "backup never deleted after a failed rollback: {:?}", host_cmds(&h));
        assert_eq!(by(&rep, 454).outcome, Outcome::Skipped);
        assert_eq!(by(&rep, 456).outcome, Outcome::Skipped);
        assert!(for_ct(&h, 454).is_empty() && guest_cmds(&h, 456).is_empty(), "later CTs are never touched");
        assert_eq!(rep.exit_code(), EXIT_ABORTED);
        assert!(rep.aborted.as_deref().unwrap().contains("453"), "{:?}", rep.aborted);
        let reg = Registry::load(h.inner());
        assert_eq!(reg.entries.len(), 1);
        assert_eq!((reg.entries[0].vmid, reg.entries[0].status), (453, Status::Held), "held: never auto-pruned");
        let msgs = messages(&rep, &AppriseCfg::default());
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert!(m.level == Level::Failure && m.body.contains("rollback: ❌ FAILED") && m.body.contains("pct rollback 453"), "{} / {}", m.title, m.body);
    }

    #[test]
    fn scenario_storage_without_snapshots_falls_back_to_vzdump() {
        let h = host("update-backup-policy");
        let rep = go(&h, &[454]);
        let r = by(&rep, 454);
        assert_eq!(r.outcome, Outcome::Ok, "{r:?}");
        let c = for_ct(&h, 454);
        assert!(c[0].starts_with("pvesh get /nodes/server/lxc/454/feature --feature snapshot"), "PVE is asked first: {c:?}");
        assert!(c[1].starts_with("vzdump 454 --mode snapshot --storage local"), "{c:?}");
        assert!(c.iter().all(|x| !x.starts_with("pct snapshot")), "no snapshot attempt when PVE says no: {c:?}");
        assert!(c.last().unwrap().starts_with("pvesm free local:backup/vzdump-lxc-454-"), "{c:?}");
        assert!(r.backup.as_deref().unwrap().contains("vzdump") && r.backup.as_deref().unwrap().contains("deleted"));
    }

    #[test]
    fn rollback_of_a_vzdump_restores_into_the_rootfs_storage() {
        let h = host("update-backup-policy");
        let rep = go(&h, &[457]);
        let r = by(&rep, 457);
        assert_eq!((r.outcome, r.rollback.as_deref()), (Outcome::Failed, Some("done")), "{r:?}");
        let c = for_ct(&h, 457);
        assert!(c.iter().any(|x| x == "pct restore 457 local:backup/vzdump-lxc-457-2026_11_08-03_16_00.tar.zst --force --storage local-lvm"), "restore must name the CT's own storage (local-lvm), not default to local: {c:?}");
        assert!(c.last().unwrap().starts_with("pvesm free local:backup/vzdump-lxc-457-"), "{c:?}");
        // Regresión (CT 111 real): `pct shutdown` ya lo dejó detenido; `pct stop` daría 255 "not running" y abortaría el rollback.
        assert!(!c.iter().any(|x| x.starts_with("pct stop")), "{c:?}");
        let pos = |k: &str| c.iter().position(|x| x.starts_with(k)).unwrap_or_else(|| panic!("{k} missing in {c:?}"));
        assert!(pos("pct shutdown") < pos("pct restore") && pos("pct restore") < pos("pct start") && pos("pct start") < pos("pvesm free"), "{c:?}");
        assert!(crate::share::proxmox::guests::lxc_running(h.inner(), 457), "CT running again after the rollback");
        let reg = Registry::load(h.inner());
        assert!(reg.entries.iter().all(|e| e.vmid != 457));
    }

    #[test]
    fn mock_imitates_pve_stop_and_shutdown_on_stopped_cts() {
        let h = host("update-backup-policy");
        let run = |a: &[&str]| h.inner().host_cmd(a, &ExecOpts::default(), &mut Vec::<String>::new()).unwrap().code;
        assert_eq!(run(&["pct", "shutdown", "457", "--timeout", "60"]), 0);
        assert!(!crate::share::proxmox::guests::lxc_running(h.inner(), 457));
        assert_eq!(run(&["pct", "stop", "457"]), 255);
        assert_eq!(run(&["pct", "start", "457"]), 0);
        assert!(crate::share::proxmox::guests::lxc_running(h.inner(), 457));
    }

    #[test]
    fn held_vzdump_backup_records_the_storage_and_the_manual_revert_names_it() {
        let h = host("update-timeout-backup");
        let _ = go(&h, &[471]);
        let reg = Registry::load(h.inner());
        let e = &reg.entries[0];
        assert_eq!(e.storage.as_deref(), Some("local-lvm"), "{e:?}");
        assert!(e.manual_revert().ends_with("--force --storage local-lvm"), "{}", e.manual_revert());
    }

    #[test]
    fn snapshot_name_in_the_same_second_gets_a_suffix() {
        let h = host("update-backup-policy");
        let rep = go(&h, &[458]);
        assert_eq!(by(&rep, 458).outcome, Outcome::Ok, "{:?}", by(&rep, 458));
        let c = for_ct(&h, 458);
        assert!(c.iter().any(|x| x == "pct snapshot 458 pxu-20261108031600-2"), "the name taken in this second is not reused: {c:?}");
        assert!(!c.iter().any(|x| x.starts_with("pct snapshot") && x.ends_with("pxu-20261108031600")), "{c:?}");
    }

    #[test]
    fn explicit_snapshot_mode_fails_cleanly_when_pve_says_no() {
        // CT 454 no admite snapshots (bind mount): mode snapshot explícito → sin backup, CT intacto
        let h2 = ReadOnlyHost::new(world("update-backup-policy").ct_config(454, &["backup = \"snapshot\""]).build());
        let rep = go(&h2, &[454]);
        let r = by(&rep, 454);
        assert_eq!((r.outcome, r.step), (Outcome::Failed, Step::Backup), "{r:?}");
        assert!(guest_cmds(&h2, 454).iter().all(|c| !c.contains("upgrade")) && host_cmds(&h2).iter().all(|c| !c.starts_with("pct snapshot") && !c.starts_with("vzdump")));
    }

    #[test]
    fn the_log_says_the_backup_covers_only_the_rootfs() {
        let h = host("update-backup-policy");
        let mut lines = vec![];
        let cfg = Config::load(&h).unwrap();
        let inv = crate::share::proxmox::guests::load(&h);
        h.set_update_enabled(true);
        let cancel = AtomicBool::new(false);
        let _ = run(&h, &inv, &cfg, &Opts { only: vec![451] }, &cancel, &mut |e| if let Event::Line(_, l) = e { lines.push(l) });
        h.set_update_enabled(false);
        assert!(lines.iter().any(|l| l.contains("cover only the CT's rootfs") && l.contains("NOT saved") && l.contains("NOT reverted")), "{lines:?}");
    }

    #[test]
    fn scenario_failed_backup_leaves_the_ct_untouched() {
        let h = host("update-two-fails");
        let rep = go(&h, &[422]);
        let r = by(&rep, 422);
        assert_eq!((r.outcome, r.step), (Outcome::Failed, Step::Backup));
        assert!(guest_cmds(&h, 422).iter().all(|c| !c.contains("upgrade")), "no backup ⇒ no update");
        assert!(host_cmds(&h).iter().all(|c| !c.contains("rollback") && !c.contains("restore") && !c.contains("shutdown")));
    }

    #[test]
    fn scenario_keep_prunes_oldest_pxu_backups_only() {
        let h = host("update-keep");
        let rep = go(&h, &[461]);
        assert_eq!(by(&rep, 461).outcome, Outcome::Ok, "{:?}", by(&rep, 461));
        let hc = host_cmds(&h);
        let dels: Vec<&String> = hc.iter().filter(|c| c.starts_with("pct delsnapshot") || c.starts_with("pvesm free")).collect();
        assert!(dels.iter().any(|c| c.as_str() == "pvesm free local:backup/vzdump-lxc-461-2026_10_30-03_00_00.tar.zst"), "{hc:?}");
        for never in ["manual-before-upgrade", "zfs_auto_5412", "pxu-custom", "pxu-202611010300"] {
            assert!(!dels.iter().any(|c| c.ends_with(never)), "{never} must stay: {dels:?}");
        }
        let reg = Registry::load(h.inner());
        assert!(reg.entries.iter().any(|e| e.vmid == 999), "other CT's entry untouched");
        assert!(reg.entries.iter().any(|e| e.vmid == 461 && e.name == "pxu-202611010300" && e.status == Status::Held), "held entry untouched");
        let kept = reg.entries.iter().filter(|e| e.vmid == 461 && e.status == Status::Kept).count();
        assert!(kept <= 2, "at most keep=2 kept: {:?}", reg.entries);
    }

    #[test]
    fn scenario_keep_zero_override_per_ct_prunes_everything_of_pxu() {
        let h = host("update-keep");
        let rep = go(&h, &[462]);
        assert_eq!(by(&rep, 462).outcome, Outcome::Ok);
        let hc = host_cmds(&h);
        assert!(hc.iter().any(|c| c == "pct delsnapshot 462 pxu-202611010300"), "old pxu snapshot pruned with keep=0: {hc:?}");
        assert!(Registry::load(h.inner()).entries.iter().all(|e| e.vmid != 462));
    }

    #[test]
    fn scenario_timeout_never_rolls_back_and_holds_the_backup() {
        let h = host("update-timeout-backup");
        let rep = go(&h, &[471, 472]);
        let r = by(&rep, 471);
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(!matches!(r.rollback.as_deref(), Some("done") | Some("failed")), "{r:?}");
        assert!(r.revert.as_deref().is_some_and(|v| v.contains("pct restore 471") && v.contains("--force")), "{r:?}");
        assert!(r.backup.as_deref().unwrap().contains("kept"), "{r:?}");
        assert!(host_cmds(&h).iter().all(|c| !c.contains("rollback") && !c.contains("restore") && !c.starts_with("pvesm free") && !c.starts_with("pct stop")), "{:?}", host_cmds(&h));
        assert_eq!(by(&rep, 472).outcome, Outcome::Skipped);
        assert_eq!(rep.exit_code(), EXIT_ABORTED);
        let reg = Registry::load(h.inner());
        assert_eq!(reg.entries.len(), 1);
        assert_eq!((reg.entries[0].kind, reg.entries[0].status), (Kind::Vzdump, Status::Held));
        let msgs = messages(&rep, &AppriseCfg::default());
        assert!(msgs.len() == 1 && msgs[0].body.contains("rollback: not attempted, backup kept"), "{msgs:?}");
    }

    #[test]
    fn timeout_on_a_ct_started_by_pxu_never_shuts_it_down() {
        // M5: CT apagado + start_and_restore + upgrade colgado ⇒ el CT queda encendido; ningún shutdown/stop
        let h = host("update-timeout-stopped");
        let rep = go(&h, &[491]);
        let r = by(&rep, 491);
        assert_eq!(r.outcome, Outcome::Failed, "{r:?}");
        assert!(r.detail.contains("LEFT RUNNING") || r.detail.contains("left RUNNING"), "{}", r.detail);
        let hc = host_cmds(&h);
        assert!(hc.iter().any(|c| c == "pct start 491"), "{hc:?}");
        assert!(hc.iter().all(|c| !c.starts_with("pct shutdown") && !c.starts_with("pct stop") && !c.contains("rollback") && !c.contains("restore") && !c.starts_with("pvesm free")), "{hc:?}");
        assert_eq!(rep.exit_code(), EXIT_ABORTED);
        let reg = Registry::load(h.inner());
        assert_eq!(reg.entries.len(), 1);
        assert_eq!(reg.entries[0].status, Status::Held);
    }

    #[test]
    fn corrupt_registry_is_quarantined_and_nothing_is_pruned() {
        // L12: keep=2 y snapshots viejos de pxu en el .conf: con el registro corrupto no se borra ninguno
        let h = host("update-keep");
        h.write_file(std::path::Path::new(backup::REGISTRY_PATH), b"{not json").unwrap();
        let mut lines = vec![];
        h.set_update_enabled(true);
        let (cfg, inv) = (Config::load(&h).unwrap(), crate::share::proxmox::guests::load(&h));
        let rep = run(&h, &inv, &cfg, &Opts { only: vec![461] }, &AtomicBool::new(false), &mut |e| if let Event::Line(_, l) = e { lines.push(l) }).unwrap();
        h.set_update_enabled(false);
        assert_eq!(by(&rep, 461).outcome, Outcome::Ok);
        let hc = host_cmds(&h);
        assert!(hc.iter().all(|c| !c.starts_with("pct delsnapshot") && !c.starts_with("pvesm free")), "{hc:?}");
        assert!(lines.iter().any(|l| l.contains("is corrupt") && l.contains("backups.json.corrupt-")), "{lines:?}");
        let copy = h.read_dir(std::path::Path::new("/var/lib/pxu")).unwrap().into_iter().find(|p| p.to_string_lossy().contains("backups.json.corrupt-")).expect("copy of the corrupt file");
        assert_eq!(h.read_file(&copy).unwrap(), b"{not json");
        assert!(by(&rep, 461).backup.as_deref().unwrap().contains("kept"));
    }

    #[test]
    fn tampered_registry_entries_are_dropped_and_never_reach_pvesm_free() {
        // M4: entradas con vmid que no cuadra con el nombre, snapshot ajeno, storage hostil, vmid 0
        let h = host("update-keep");
        let bad = serde_json::json!({"schema": 1, "entries": [
            {"vmid": 461, "kind": "vzdump", "name": "local:backup/vzdump-lxc-124-2026_10_30-03_00_00.tar.zst", "stamp": "20261030030000", "status": "kept"},
            {"vmid": 461, "kind": "snapshot", "name": "manual-before-upgrade", "stamp": "202611030300", "status": "kept"},
            {"vmid": 461, "kind": "vzdump", "name": "local:backup/vzdump-lxc-461-2026_10_30-03_00_00.tar.zst", "stamp": "x", "status": "kept"},
            {"vmid": 461, "kind": "vzdump", "name": "local:backup/vzdump-lxc-461-2026_10_30-03_00_00.tar.zst", "stamp": "20261030030000", "status": "kept", "storage": "a;b"},
            {"vmid": 0, "kind": "snapshot", "name": "pxu-202611030300", "stamp": "202611030300", "status": "kept"},
        ]});
        h.write_file(std::path::Path::new(backup::REGISTRY_PATH), bad.to_string().as_bytes()).unwrap();
        let reg = Registry::load(h.inner());
        assert!(reg.entries.is_empty() && reg.tainted && reg.warnings.iter().any(|w| w.contains("5 registry entries dropped")), "{:?}", reg.warnings);
        let rep = go(&h, &[461]);
        assert_eq!(by(&rep, 461).outcome, Outcome::Ok);
        let hc = host_cmds(&h);
        assert!(hc.iter().all(|c| !c.starts_with("pvesm free") && !c.starts_with("pct delsnapshot")), "{hc:?}");
        // una entrada válida pasa
        let good = backup::Entry { vmid: 7, kind: Kind::Vzdump, name: "local:backup/vzdump-lxc-7-x.tar.zst".into(), stamp: "20261030030000".into(), status: Status::Kept, storage: Some("local-lvm".into()) };
        assert!(backup::entry_check(&good).is_ok());
        assert!(backup::entry_check(&backup::Entry { vmid: 8, ..good }).is_err(), "volid of CT 7 on an entry of CT 8");
    }

    #[test]
    fn stopped_ct_failure_is_rolled_back_and_stays_stopped() {
        let h = host("update-backup-policy");
        let rep = go(&h, &[455]);
        let r = by(&rep, 455);
        assert_eq!((r.outcome, r.rollback.as_deref()), (Outcome::Failed, Some("done")), "{r:?}");
        let c = for_ct(&h, 455);
        assert!(c.last().unwrap().starts_with("pct delsnapshot 455"), "{c:?}");
        let last_start = c.iter().rposition(|x| x.starts_with("pct start"));
        let last_stop = c.iter().rposition(|x| x.starts_with("pct stop") || x.starts_with("pct shutdown"));
        assert!(last_stop > last_start, "CT ends stopped: {c:?}");
    }

    #[test]
    fn mode_none_takes_no_backup_and_has_no_way_back() {
        let h = host("update-no-backup");
        let rep = go(&h, &[]);
        let r = by(&rep, 481);
        assert_eq!(r.outcome, Outcome::Ok);
        assert!(r.backup.is_none());
        assert!(host_cmds(&h).is_empty(), "{:?}", host_cmds(&h));
    }

    #[test]
    fn backup_config_defaults_migration_and_validation() {
        let cfg_of = |toml: &str| {
            let h = World::new().config_raw(toml).build();
            (Config::load(&h), crate::share::config::validate(&h))
        };
        let (c, _) = cfg_of("");
        let c = c.unwrap();
        assert_eq!((c.backup_mode(1), c.keep_for(1)), ("auto", 0), "defaults: auto backup, keep 0");
        let (c, v) = cfg_of("[backup]\nkeep_snapshots = 3\n");
        assert_eq!(c.unwrap().keep_for(1), 3, "old key still read as keep");
        assert!(v.errors.is_empty() && v.warnings.iter().any(|w| w.contains("keep_snapshots") && w.contains("deprecated")), "{v:?}");
        let (c, _) = cfg_of("[backup]\nkeep = 2\n[[ct]]\nvmid = 5\nkeep = 0\nbackup = \"none\"\n");
        let c = c.unwrap();
        assert_eq!((c.keep_for(4), c.keep_for(5), c.backup_mode(5)), (2, 0, "none"));
        let (_, v) = cfg_of("[backup]\nkeep = -1\n");
        assert!(!v.errors.is_empty(), "negative keep rejected: {v:?}");
        let (_, v) = cfg_of("[backup]\nmode = \"none\"\n");
        assert!(v.warnings.iter().any(|w| w.contains("none")), "mode none warns: {v:?}");
        let (_, v) = cfg_of("[backup]\nmode = \"bogus\"\n");
        assert!(!v.errors.is_empty(), "{v:?}");
    }

}
