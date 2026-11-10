//! Bucle de eventos (ARCHITECTURE §7.2): bloquea en `read()`; sin tick ni animaciones.
pub mod app;
pub mod apprise;
pub mod logs;
pub mod sched;
pub mod views;
#[cfg(test)]
mod tests;

use app::{Action, App, AutoModal, HostStop, NvidiaModal, UpdateModal, View};
use apprise::AppriseModal;
use sched::SchedModal;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use pxu::share::update::check::{self, CheckOpts, CtResult, Report};
use pxu::share::config::{self, Config};
use pxu::share::proxmox::guests;
use pxu::share::host::Host;
use pxu::share::notify;
use pxu::share::model::GuestKind;
use pxu::share::update::{self, runlog, Event as UpdEvent, RunError, RunReport, LOG_DIR};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

fn epoch(h: &dyn Host) -> u64 {
    h.now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// HH:MM:SS en hora local.
pub fn hms(epoch: u64) -> String {
    let t = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// HH:MM en hora local (libc::localtime_r); con host mock, el reloj fijo del escenario.
pub fn hhmm(epoch: u64) -> String {
    let t = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

enum Msg {
    Progress(CtResult),
    Done(Report),
    Failed(String),
    Upd(UpdEvent),
    UpdDone(RunReport, Vec<String>),
    UpdFailed(String),
    HostStop(HostStop),
}

/// Un hilo por chequeo; el bucle principal sigue bloqueado en `read()` salvo mientras hay trabajo (poll 150 ms).
fn spawn_check<'s>(sc: &'s std::thread::Scope<'s, '_>, host: &'s dyn Host, cancel: &'s AtomicBool, refresh: Option<bool>, only: Vec<u32>) -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    cancel.store(false, Ordering::SeqCst);
    sc.spawn(move || {
        let cfg = match Config::load(host) {
            Ok(c) => c,
            Err(e) => return drop(tx.send(Msg::Failed(format!("config: {e}")))),
        };
        let inv = guests::load(host);
        let opts = CheckOpts::from_config(&cfg, refresh);
        let tx2 = tx.clone();
        let rep = check::run(host, &inv, &cfg, &opts, &only, cancel, &mut |r| drop(tx2.send(Msg::Progress(r.clone()))));
        if let Err(e) = check::persist(host, &rep) {
            let _ = tx.send(Msg::Failed(format!("state not saved: {e}")));
        }
        let _ = tx.send(Msg::Done(rep));
    });
    rx
}

/// Un hilo por corrida de update. El opt-in de escritura se da aquí (tras el Enter del modal) y se retira al acabar.
fn spawn_update<'s>(sc: &'s std::thread::Scope<'s, '_>, host: &'s dyn Host, cancel: &'s AtomicBool, only: Vec<u32>) -> Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    cancel.store(false, Ordering::SeqCst);
    sc.spawn(move || {
        let cfg = match Config::load_checked(host) {
            Ok((c, _)) => c,
            Err(e) => return drop(tx.send(Msg::UpdFailed(format!("config: {e}")))),
        };
        let mut ap = cfg.apprise.clone();
        ap.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
        let inv = guests::load(host);
        let optin = pxu::share::host::UpdateOptIn::new(host);
        let tx2 = tx.clone();
        let res = update::run(host, &inv, &cfg, &update::Opts { only }, cancel, &mut |e| drop(tx2.send(Msg::Upd(e))));
        let msg = match res {
            Ok(rep) => {
                let notes = update::notify_run(host, &ap, &rep);
                Msg::UpdDone(rep, notes)
            }
            Err(RunError::Locked) => Msg::UpdFailed("another pxu update is running (global lock held)".into()),
            Err(RunError::Io(e)) => Msg::UpdFailed(e),
        };
        drop(optin);
        let _ = tx.send(msg);
    });
    rx
}

/// Un hilo para la instalación en el host (`repair` = `dpkg --configure -a`). Su opt-in es propio y se retira al acabar.
fn spawn_host_update<'s>(sc: &'s std::thread::Scope<'s, '_>, host: &'s dyn Host, cancel: &'s AtomicBool, confirmed: Vec<pxu::share::proxmox::hostcheck::HostPkg>, repair: bool) -> Receiver<Msg> {
    use pxu::share::update::hostupdate::{self as hu, HostOutcome};
    let (tx, rx) = mpsc::channel();
    cancel.store(false, Ordering::SeqCst);
    sc.spawn(move || {
        let cfg = match Config::load_checked(host) {
            Ok((c, _)) => c,
            Err(e) => return drop(tx.send(Msg::UpdFailed(format!("config: {e}")))),
        };
        let mut ap = cfg.apprise.clone();
        ap.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
        let inv = guests::load(host);
        let optin = pxu::share::host::HostUpdateOptIn::new(host);
        let tx2 = tx.clone();
        let mut ev = |e| drop(tx2.send(Msg::Upd(e)));
        let res = if repair { hu::repair_dpkg(host, &cfg, &mut ev) } else { hu::run(host, &inv, &cfg, &hu::HostOpts { confirmed }, cancel, &mut ev) };
        let msg = match res {
            Ok(HostOutcome::Done(rep)) => {
                let notes = hu::notify_run(host, &ap, &rep);
                Msg::UpdDone(rep, notes)
            }
            Ok(HostOutcome::Reconfirm(l)) => Msg::HostStop(HostStop::Reconfirm(l)),
            Ok(HostOutcome::DpkgDirty(d)) => Msg::HostStop(HostStop::Dirty(d)),
            Ok(HostOutcome::Repaired) => Msg::HostStop(HostStop::Repaired),
            Err(RunError::Locked) => Msg::UpdFailed("another pxu update is running (lock held)".into()),
            Err(RunError::Io(e)) => Msg::UpdFailed(e),
        };
        drop(optin);
        let _ = tx.send(msg);
    });
    rx
}

/// Sin config válida (ausente, mal formada, con errores o con permisos incorrectos) la TUI queda en solo lectura (DESIGN §8).
/// Devuelve si hay que bloquear y, si la causa es una config presente pero inválida, el motivo.
fn read_only_state(host: &dyn Host) -> (bool, Option<String>) {
    match Config::load_checked(host) {
        Ok(_) => (!host.exists(std::path::Path::new(config::CONFIG_PATH)), None),
        Err(e) => (true, Some(e)),
    }
}

/// `a`: lee la config (validada) y abre el modal con el estado de hoy y el nuevo.
fn ask_auto_modal(host: &dyn Host, app: &mut App, vmid: u32) {
    let cfg = match Config::load_checked(host) {
        Ok((c, _)) => c,
        Err(e) => return app.status = format!("auto-update not changed: {e}"),
    };
    let Some(g) = app.guest(vmid) else { return };
    let why = cfg.auto_update_why(vmid, &g.tags);
    let m = AutoModal::new(g, &why, !host.exists(std::path::Path::new(config::CONFIG_PATH)));
    app.open_auto_modal(m);
}

/// Confirmado: escribe el override (validado y atómico), recarga la config y sale de solo lectura si correspondía.
fn save_auto_update(host: &dyn Host, app: &mut App, vmid: u32, value: bool) {
    let res = {
        let _optin = pxu::share::host::ConfigEditOptIn::new(host);
        if vmid == 0 { config::write_host_auto(host, value) } else { config::write_auto_update(host, vmid, value) }
    };
    if let Err(e) = res {
        return app.status = format!("auto-update not saved: {e}");
    }
    let (ro, reason) = read_only_state(host);
    app.read_only = app.forced_ro || ro;
    app.ro_reason = reason;
    app.set_inventory(guests::load(host), None);
    let who = if vmid == 0 { "host".to_string() } else { format!("CT {vmid}") };
    app.status = format!("{who}: auto-update {} (saved in {})", if value { "enabled" } else { "disabled" }, config::CONFIG_PATH);
    let cfg = Config::load(host).unwrap_or_default();
    if !value {
        // desactivar el auto-update apaga también el driver, sin modal
        if cfg.nvidia_auto(vmid) {
            let res = {
                let _optin = pxu::share::host::ConfigEditOptIn::new(host);
                config::write_nvidia_auto(host, vmid, false)
            };
            if let Err(e) = res {
                app.status = format!("{} (NVIDIA driver flag not cleared: {e})", app.status);
            }
        }
        return;
    }
    // activar: si la máquina usa la GPU con un driver `.run`, se ofrece el opt-in en un modal aparte
    if let Some(o) = pxu::share::update::nvidia::offer(host, &app.inv, vmid, cfg.check.timeout_s) {
        let name = app.guest(vmid).map(|g| g.name.clone()).unwrap_or_default();
        app.open_nvidia_modal(NvidiaModal { vmid, name, current: o.current, newest: o.newest, checked: false });
    }
}

/// Confirmado el modal del driver: `nvidia_auto` del CT (si lo es) y `host_nvidia_auto`, ambos validados y atómicos.
fn save_nvidia_auto(host: &dyn Host, app: &mut App, vmid: u32) {
    let res = {
        let _optin = pxu::share::host::ConfigEditOptIn::new(host);
        (if vmid != 0 { config::write_nvidia_auto(host, vmid, true) } else { Ok(()) }).and_then(|_| config::write_nvidia_auto(host, 0, true))
    };
    app.status = match res {
        Ok(()) => format!("{}: NVIDIA driver update enabled (saved in {})", if vmid == 0 { "host".to_string() } else { format!("CT {vmid}") }, config::CONFIG_PATH),
        Err(e) => format!("NVIDIA driver update not saved: {e}"),
    };
}

/// Cierre del selector de columnas: guarda `[ui] columns`. Si no se puede, la elección vale para esta sesión.
fn save_columns(host: &dyn Host, app: &mut App, cols: &[String]) {
    if app.forced_ro {
        return app.status = "columns changed for this session only (--read-only: config not written)".into();
    }
    let res = {
        let _optin = pxu::share::host::ConfigEditOptIn::new(host);
        config::write_columns(host, cols)
    };
    app.status = match res {
        Ok(()) => format!("columns saved in {}", config::CONFIG_PATH),
        Err(e) => format!("columns changed for this session only: {e}"),
    };
}

/// Relee `[apprise]` para la vista Config (secretos enmascarados).
fn load_apprise(host: &dyn Host, app: &mut App) {
    app.apprise = Some(Config::load(host).map(|c| config::apprise_summary(host, &c.apprise)));
}

/// `N`: lee `[apprise]` (config válida) y abre el diálogo; con la config ausente parte de los valores por defecto.
fn ask_apprise_modal(host: &dyn Host, app: &mut App) {
    match Config::load_checked(host) {
        Ok((c, _)) => app.open_apprise(AppriseModal::new(&c.apprise)),
        Err(e) => app.status = format!("notifications not changed: {e}"),
    }
}

/// Valida la config resultante entera (como `pxu config check`) sin escribir y pasa a la confirmación.
fn check_apprise(host: &dyn Host, app: &mut App, edit: &config::AppriseEdit) {
    app.apprise_checked(config::plan_apprise(host, edit).map(|p| p.lines));
}

/// Confirmado: escribe `[apprise]` (validado y atómico), recarga la config y la vista.
fn save_apprise(host: &dyn Host, app: &mut App, edit: &config::AppriseEdit) {
    let res = {
        let _optin = pxu::share::host::ConfigEditOptIn::new(host);
        config::write_apprise(host, edit)
    };
    if let Err(e) = res {
        return app.status = format!("notifications not saved: {e}");
    }
    let (ro, reason) = read_only_state(host);
    app.read_only = app.forced_ro || ro;
    app.ro_reason = reason;
    load_apprise(host, app);
    app.status = format!("notifications saved in {} (press t to send a test)", config::CONFIG_PATH);
}

/// `t`: un aviso de prueba como `pxu notify-test`, en un hilo (el curl puede tardar hasta `timeout_s`+5 s). Errores sin secretos.
fn spawn_notify_test<'s>(sc: &'s std::thread::Scope<'s, '_>, host: &'s dyn Host) -> Receiver<Result<String, String>> {
    let (tx, rx) = mpsc::channel();
    sc.spawn(move || {
        let _ = tx.send(notify_test(host));
    });
    rx
}

fn notify_test(host: &dyn Host) -> Result<String, String> {
    let mut cfg = Config::load(host).map_err(|e| format!("test not sent: {e}"))?;
    cfg.apprise.apply_env(std::env::var("APPRISE_URLS").ok().as_deref());
    let m = notify::test_message(&guests::load(host).node);
    match notify::send_once(host, &cfg.apprise, &m) {
        Ok(t) => Ok(format!("test notice sent via {}: ok", t.label())),
        Err(e) => Err(format!("test notice failed (exit 5): {}", notify::scrub(&cfg.apprise, &e))),
    }
}

/// Relee el estado del timer (lo muestra la vista Config).
fn load_timer(host: &dyn Host, app: &mut App) {
    app.timer = Some(host.timer_status().map_err(|e| e.to_string()));
}

/// `g`: vista Config con el estado real del timer.
fn open_config(host: &dyn Host, app: &mut App) {
    load_timer(host, app);
    load_apprise(host, app);
    app.log_days = Config::load(host).ok().map(|c| c.general.log_retention_days);
    app.view = View::Config;
}

/// `l`: lista los `run-*.log` (solo cabeceras) y abre la página.
fn open_logs(host: &dyn Host, app: &mut App) {
    app.open_logs(runlog::list(host, std::path::Path::new(LOG_DIR)));
}

/// Enter / `n` / `p`: lee el log entero (con tope) para el visor.
fn open_log(host: &dyn Host, app: &mut App, path: &std::path::Path) {
    app.log_loaded(runlog::read_lines(host, path));
}

/// Confirmado el diálogo de retención: `[general] log_retention_days` (validado y atómico).
fn save_log_retention(host: &dyn Host, app: &mut App, days: u32) {
    let res = {
        let _optin = pxu::share::host::ConfigEditOptIn::new(host);
        config::write_log_retention(host, days)
    };
    match res {
        Ok(()) => {
            app.log_days = Some(days);
            app.status = if days == 0 { format!("log retention: never delete (saved in {})", config::CONFIG_PATH) } else { format!("log retention: {days} days (saved in {})", config::CONFIG_PATH) };
        }
        Err(e) => app.status = format!("log retention not saved: {e}"),
    }
}

/// La captura de ratón solo está activa en la página Logs (clic, doble clic y rueda); en el resto la terminal selecciona texto.
fn sync_mouse(on: &mut bool, want: bool) {
    if *on != want {
        let _ = if want { crossterm::execute!(io::stdout(), EnableMouseCapture) } else { crossterm::execute!(io::stdout(), DisableMouseCapture) };
        *on = want;
    }
}

/// Avisos de contexto del diálogo: programar es posible aunque el update aún no tenga nada que hacer.
fn sched_warnings(host: &dyn Host, app: &App) -> Vec<String> {
    match Config::load_checked(host) {
        Err(e) => vec![format!("config is invalid: the scheduled update will fail with exit 2 ({e})")],
        Ok(_) if !host.exists(std::path::Path::new(config::CONFIG_PATH)) => vec![format!("no {}: the scheduled update will do nothing until a CT has auto-update (press a)", config::CONFIG_PATH)],
        Ok(_) if !app.inv.guests.iter().any(|g| (g.kind == GuestKind::Lxc || g.kind == GuestKind::Host) && g.auto_update) => vec!["no CT has auto-update enabled: the scheduled update will do nothing (press a on a CT)".into()],
        Ok(_) => vec![],
    }
}

/// `S`: lee el timer y abre el diálogo.
fn ask_schedule_modal(host: &dyn Host, app: &mut App) {
    match host.timer_status() {
        Ok(st) => {
            let w = sched_warnings(host, app);
            app.open_sched(SchedModal::new(st, w));
        }
        Err(e) => app.status = format!("schedule: cannot read the timer state: {e}"),
    }
}

/// Valida el horario con systemd (sin escribir nada) y pasa a la confirmación.
fn check_schedule(host: &dyn Host, app: &mut App, expr: &str) {
    app.sched_checked(host.validate_calendar(expr).map_err(|e| e.to_string()));
}

/// Confirmado: aplica (opt-in de escritura solo durante la llamada) y relee el estado.
fn save_schedule(host: &dyn Host, app: &mut App, plan: &pxu::share::schedule::SchedulePlan) {
    let res = {
        let _optin = pxu::share::host::ScheduleEditOptIn::new(host);
        host.apply_schedule(plan)
    };
    if let Err(e) = res {
        load_timer(host, app);
        return app.status = format!("schedule not saved: {e}");
    }
    load_timer(host, app);
    app.status = match (&plan.on_calendar, plan.enable) {
        (_, false) => "timer pxu-update disabled".to_string(),
        (Some(e), _) => format!("timer pxu-update enabled: {e} (server time)"),
        (None, _) => "timer pxu-update enabled".to_string(),
    };
}

/// Primera muestra de CPU ~1 s tras abrir; luego cada ~3 s (solo `cpu.stat`, sin recargar el inventario).
const CPU_FIRST: Duration = Duration::from_secs(1);
const CPU_EVERY: Duration = Duration::from_secs(3);

/// Tick de CPU: lee `cpu.stat` de los LXC en marcha y calcula el % contra la muestra anterior.
pub fn cpu_tick(host: &dyn Host, app: &mut App, now_us: u64) {
    let ids: Vec<u32> = app.inv.guests.iter().filter(|g| (g.kind == GuestKind::Lxc && g.is_running()) || (g.kind == GuestKind::Host && g.cpu_usec.is_some())).map(|g| g.vmid).collect();
    app.cpu_sample(guests::sample_cpu(host, &ids), now_us);
}

pub fn run(host: &dyn Host, force_read_only: bool) -> io::Result<()> {
    let (no_config, ro_reason) = read_only_state(host);
    let mut app = App::new(guests::load(host), force_read_only || no_config, host.is_mock());
    if let Ok(c) = Config::load(host) {
        app.columns = c.ui.visible_columns();
        app.refresh_default = c.check.refresh;
    }
    app.forced_ro = force_read_only;
    app.ro_reason = ro_reason;
    let th = views::Theme::from_env();
    let mut term = ratatui::init();
    app.cpu_seed(wall_us());
    let mut next_cpu = Instant::now() + CPU_FIRST;
    let cancel = AtomicBool::new(false);
    let ucancel = AtomicBool::new(false);
    let res = std::thread::scope(|sc| -> io::Result<()> {
        let mut rx: Option<Receiver<Msg>> = None;
        let mut trx: Option<Receiver<Result<String, String>>> = None;
        let mut mouse_on = false;
        let res = (|| -> io::Result<()> {
            loop {
                if let Some(r) = &rx {
                    loop {
                        match r.try_recv() {
                            Ok(Msg::Progress(c)) => app.check_progress(c),
                            Ok(Msg::Done(rep)) => {
                                app.check_done(rep);
                                app.set_inventory(guests::load(host), None);
                            }
                            Ok(Msg::Failed(m)) => app.check_failed(m),
                            Ok(Msg::Upd(e)) => {
                                let now = epoch(host);
                                app.update_event(e, &hms(now), now);
                            }
                            Ok(Msg::UpdDone(rep, notes)) => {
                                app.update_done(rep, notes);
                                app.set_inventory(guests::load(host), None);
                            }
                            Ok(Msg::UpdFailed(m)) => app.update_failed(m),
                            Ok(Msg::HostStop(w)) => {
                                app.set_inventory(guests::load(host), None);
                                app.host_stop(w);
                            }
                            Err(TryRecvError::Empty) => break,
                            Err(TryRecvError::Disconnected) => {
                                app.check.running = false;
                                if app.update.running {
                                    app.update_failed("update worker ended unexpectedly".into());
                                }
                                rx = None;
                                break;
                            }
                        }
                    }
                }
                if let Some(r) = &trx {
                    match r.try_recv() {
                        Ok(res) => {
                            app.test_done(res);
                            trx = None;
                        }
                        Err(TryRecvError::Empty) => {}
                        Err(TryRecvError::Disconnected) => {
                            app.test_done(Err("test notice failed: worker ended unexpectedly".into()));
                            trx = None;
                        }
                    }
                }
                app.now_epoch = epoch(host);
                app.clock = hhmm(app.now_epoch);
                sync_mouse(&mut mouse_on, app.view == View::Logs);
                term.draw(|f| views::draw(f, &app, &th))?;
                let until_cpu = next_cpu.saturating_duration_since(Instant::now());
                let wait = if rx.is_some() || trx.is_some() { until_cpu.min(Duration::from_millis(150)) } else { until_cpu };
                let ev = if event::poll(wait)? { Some(event::read()?) } else { None };
                if Instant::now() >= next_cpu {
                    cpu_tick(host, &mut app, wall_us());
                    next_cpu = Instant::now() + CPU_EVERY;
                }
                let act = match ev {
                    Some(Event::Key(k)) if k.kind == KeyEventKind::Press => app.on_key(k),
                    Some(Event::Mouse(m)) => app.on_mouse(m, Instant::now()),
                    _ => Action::None, // Resize y demás: el siguiente ciclo redibuja
                };
                {
                    match act {
                        Action::Quit => {
                            cancel.store(true, Ordering::SeqCst);
                            ucancel.store(true, Ordering::SeqCst);
                            return Ok(());
                        }
                        Action::Reload => {
                            app.set_inventory(guests::load(host), Some(wall_us()));
                            next_cpu = Instant::now() + CPU_EVERY;
                            app.status = "reloaded confs".into();
                        }
                        Action::Check { refresh, only } => rx = Some(spawn_check(sc, host, &cancel, refresh, only)),
                        Action::PrepareUpdate { only } => match Config::load_checked(host) {
                            Ok((cfg, _)) => app.open_update_modal(UpdateModal::new(update::select(host, &app.inv, &cfg, &only), &cfg)),
                            Err(e) => app.status = format!("config: {e}"),
                        },
                        Action::SaveColumns(cols) => save_columns(host, &mut app, &cols),
                        Action::AskAutoUpdate { vmid } => ask_auto_modal(host, &mut app, vmid),
                        Action::SetAutoUpdate { vmid, value } => save_auto_update(host, &mut app, vmid, value),
                        Action::SetNvidiaAuto { vmid } => save_nvidia_auto(host, &mut app, vmid),
                        Action::OpenConfig => open_config(host, &mut app),
                        Action::OpenLogs => open_logs(host, &mut app),
                        Action::OpenLog(p) => open_log(host, &mut app, &p),
                        Action::SaveLogRetention(d) => save_log_retention(host, &mut app, d),
                        Action::AskSchedule => ask_schedule_modal(host, &mut app),
                        Action::CheckSchedule { expr } => check_schedule(host, &mut app, &expr),
                        Action::SaveSchedule(plan) => save_schedule(host, &mut app, &plan),
                        Action::AskApprise => ask_apprise_modal(host, &mut app),
                        Action::CheckApprise(e) => check_apprise(host, &mut app, &e),
                        Action::SaveApprise(e) => save_apprise(host, &mut app, &e),
                        Action::TestNotify => trx = Some(spawn_notify_test(sc, host)),
                        Action::Update { only } => rx = Some(spawn_update(sc, host, &ucancel, only)),
                        Action::HostUpdate { confirmed } => rx = Some(spawn_host_update(sc, host, &ucancel, confirmed, false)),
                        Action::HostRepair => rx = Some(spawn_host_update(sc, host, &ucancel, vec![], true)),
                        Action::CancelUpdate => {
                            ucancel.store(true, Ordering::SeqCst);
                            app.status = "stopping after the CT in progress: dpkg is never killed".into();
                        }
                        Action::CancelCheck => {
                            cancel.store(true, Ordering::SeqCst);
                            app.status = "cancelling: the CT in progress finishes first".into();
                        }
                        Action::None => {}
                    }
                }
            }
        })();
        res
    });
    let _ = crossterm::execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    res
}

fn wall_us() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}
