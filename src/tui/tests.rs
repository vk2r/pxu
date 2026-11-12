//! Snapshots de TestBackend (120×40 y 80×24) contra golden files en tests/snapshots/.
//! Regenerar: PXU_UPDATE_SNAPSHOTS=1 cargo test -p pxu
use super::app::{Action, App, Filter, View};
use super::views::{draw, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use pxu::share::proxmox::guests;
use pxu::share::mock::scenario::World;
use pxu::share::host::{Host, ReadOnlyHost};
use pxu::share::mock::MockHost;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;

/// Servidores genéricos armados en código: `basic-37` es el servidor poblado, el resto son casos predefinidos.
fn fixture(name: &str) -> ReadOnlyHost<MockHost> {
    let w = match name {
        "basic-37" => World::demo(),
        _ => World::preset(name).unwrap_or_else(|| panic!("unknown case {name}")),
    };
    ReadOnlyHost::new(w.build())
}

fn app(name: &str) -> App {
    let h = fixture(name);
    let mut a = App::new(guests::load(&h), true, true);
    a.clock = "00:16".into();
    a.now_epoch = h.now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    a
}

fn key(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn press(a: &mut App, s: &str) {
    for ch in s.chars() {
        a.on_key(key(KeyCode::Char(ch)));
    }
}

fn render(a: &App, w: u16, h: u16, color: bool) -> (String, Terminal<TestBackend>) {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    let th = Theme { color };
    t.draw(|f| draw(f, a, &th)).unwrap();
    let buf = t.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    (out, t)
}

/// Solo lo que hay dentro del marco de la ayuda (sin la pantalla de debajo).
fn help_popup(out: &str) -> String {
    out.lines()
        .filter_map(|l| {
            let a = l.find(['│', '┌', '└'])?;
            let b = l.rfind(['│', '┐', '┘'])?;
            Some(l[a..b].to_string() + "│")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn check_snapshot(name: &str, actual: &str) {
    let path = format!("{}/tests/snapshots/{name}.txt", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("PXU_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing snapshot {path}; run with PXU_UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name} differs (PXU_UPDATE_SNAPSHOTS=1 to accept)");
}

fn snap(name: &str, a: &App) {
    for (w, h) in [(120, 40), (80, 24)] {
        let (out, _) = render(a, w, h, true);
        for l in out.lines() {
            assert!(l.chars().count() <= w as usize);
        }
        check_snapshot(&format!("{name}-{w}x{h}"), &out);
    }
}

#[test]
fn list_all() {
    let mut a = app("basic-37");
    a.cursor = 3;
    snap("list-all", &a);
}

#[test]
fn list_scrolled_to_end() {
    let mut a = app("basic-37");
    a.on_key(key(KeyCode::End));
    snap("list-end", &a);
}

#[test]
fn list_filter_updates() {
    let mut a = app("basic-37");
    press(&mut a, "4");
    assert_eq!(a.visible().len(), 13);
    snap("list-updates", &a);
}

#[test]
fn list_filter_running_and_stopped() {
    let mut a = app("basic-37");
    press(&mut a, "2");
    assert_eq!(a.visible().len(), 34);
    press(&mut a, "3");
    assert_eq!(a.filter, Filter::Stopped);
    assert_eq!(a.visible().len(), 3);
    snap("list-stopped", &a);
}

#[test]
fn list_search() {
    let mut a = app("basic-37");
    press(&mut a, "/ct-110");
    assert_eq!(a.visible().iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), vec!["ct-110"]);
    a.on_key(key(KeyCode::Enter));
    snap("list-search", &a);
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.visible().len(), 37);
}

#[test]
fn detail_lxc() {
    let mut a = app("basic-37");
    press(&mut a, "/ct-103");
    a.on_key(key(KeyCode::Enter));
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.view, View::Detail(103));
    snap("detail-lxc", &a);
}

#[test]
fn detail_vm() {
    let mut a = app("vms");
    a.cursor = 3; // 203 opnsense
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.view, View::Detail(203));
    snap("detail-vm", &a);
}

#[test]
fn help_overlay_and_back() {
    let mut a = app("basic-37");
    press(&mut a, "?");
    snap("help-list", &a);
    let (out, _) = render(&a, 120, 40, true);
    let pop = help_popup(&out);
    assert!(!pop.contains('…') && pop.contains("── Keys") && pop.contains("── Columns"), "{out}");
    a.on_key(key(KeyCode::Char('x')));
    assert!(!a.help);
    a.on_key(key(KeyCode::Enter));
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.view, View::List);
}

#[test]
fn too_small_terminal_shows_message_only() {
    let a = app("basic-37");
    let (out, _) = render(&a, 60, 10, true);
    assert_eq!(out.trim(), "terminal too small (min 80×24)");
}

#[test]
fn no_color_has_no_foreground_colors_and_marks_cursor() {
    let a = app("basic-37");
    let (out, t) = render(&a, 120, 40, false);
    assert!(out.lines().any(|l| l.starts_with("> ")), "cursor marker `>` must survive NO_COLOR");
    let buf = t.backend().buffer();
    assert!(buf.content.iter().all(|c| c.fg == Color::Reset), "NO_COLOR must not set any fg color");
}

#[test]
fn quit_and_reload_actions() {
    let mut a = app("basic-37");
    assert_eq!(a.on_key(key(KeyCode::Char('r'))), Action::Reload);
    assert_eq!(a.on_key(key(KeyCode::Char('q'))), Action::Quit);
    // u (actualizar) en solo lectura: avisa en la línea de estado y no hace nada.
    let mut a = app("basic-37");
    assert_eq!(a.on_key(key(KeyCode::Char('u'))), Action::None);
    assert!(a.status.contains("read-only"));
}

/// Ejecuta lo que haría el worker (mismo `check::run`) y alimenta la app como lo hace el bucle.
fn run_check(a: &mut App, name: &str, refresh: Option<bool>, only: &[u32]) {
    use pxu::share::update::check::{self, CheckOpts};
    use std::sync::atomic::AtomicBool;
    let h = fixture(name);
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    let inv = guests::load(&h);
    let rep = check::run(&h, &inv, &cfg, &CheckOpts::from_config(&cfg, refresh), only, &AtomicBool::new(false), &mut |r| a.check_progress(r.clone()));
    a.check_done(rep);
}

#[test]
fn check_key_starts_a_job_and_view_reports_13_with_updates() {
    let mut a = app("basic-37");
    let act = a.on_key(key(KeyCode::Char('c')));
    assert_eq!(act, Action::Check { refresh: Some(false), only: vec![] }); // app() está en [RO]: `c` no refresca
    assert_eq!(a.view, View::Check);
    assert!(a.check.running);
    assert_eq!(a.check.total, 37);
    // mientras corre, una segunda `c` no lanza otro trabajo
    assert_eq!(a.on_key(key(KeyCode::Char('c'))), Action::None);
    run_check(&mut a, "basic-37", Some(false), &[]);
    assert!(!a.check.running);
    assert_eq!(a.check.report.as_ref().unwrap().summary().with_updates, 13);
    snap("check-done", &a);
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Down));
    snap("check-cursor", &a);
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.view, View::List);
}

#[test]
fn check_filtered_list_only_checks_the_visible_cts_and_c_refreshes_while_capital_c_is_read_only() {
    let mut a = rw_app("basic-37");
    press(&mut a, "4");
    match a.on_key(key(KeyCode::Char('c'))) {
        Action::Check { refresh: None, only } => assert_eq!(only.len(), 13),
        o => panic!("{o:?}"),
    }
    assert!(a.check.refresh, "c refreshes by default (shown as REFRESH)");
    a.check.running = false;
    a.view = View::List;
    // C: check solo lectura, sin confirmación
    assert!(matches!(a.on_key(key(KeyCode::Char('C'))), Action::Check { refresh: Some(false), .. }));
    assert!(!a.check.refresh);
    a.check.running = false;
    // con `[check] refresh = false`, `c` también es solo lectura
    a.refresh_default = false;
    assert!(matches!(a.on_key(key(KeyCode::Char('c'))), Action::Check { refresh: None, .. }));
    assert!(!a.check.refresh);
}

#[test]
fn check_view_running_cancel_and_failures() {
    let mut a = app("failures");
    a.on_key(key(KeyCode::Char('c')));
    // Esc durante la corrida pide cancelar, no sale
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::CancelCheck);
    assert_eq!(a.view, View::Check);
    snap("check-running", &a);
    run_check(&mut a, "failures", Some(false), &[]);
    assert_eq!(a.check.report.as_ref().unwrap().exit_code(), 12);
    snap("check-failures", &a);
}

// ---------------------------------------------------------------- update (M4)

use pxu::share::config::Config;
use pxu::share::update::{self, Event as UpdEvent};
use std::sync::atomic::AtomicBool;

fn rw_app(name: &str) -> App {
    let mut a = app(name);
    a.read_only = false;
    a
}

/// `u` + Enter con el motor real contra el mock: devuelve las acciones y los eventos del motor.
fn start_update(a: &mut App, name: &str) -> Vec<UpdEvent> {
    let h = fixture(name);
    let Action::PrepareUpdate { only } = a.on_key(key(KeyCode::Char('u'))) else { panic!("u should ask for confirmation") };
    let cfg = Config::load(&h).unwrap();
    a.open_update_modal(super::app::UpdateModal::new(update::select(&h, &a.inv, &cfg, &only), &cfg));
    assert!(a.modal.is_some());
    let mut evs = Vec::new();
    h.set_update_enabled(true);
    let inv = guests::load(&h);
    update::run(&h, &inv, &cfg, &update::Opts { only }, &AtomicBool::new(false), &mut |e| evs.push(e)).unwrap();
    evs
}

fn feed(a: &mut App, evs: &[UpdEvent]) {
    for e in evs {
        a.update_event(e.clone(), "00:16:00", a.now_epoch);
    }
}

#[test]
fn update_modal_lists_candidates_and_space_excludes() {
    let mut a = rw_app("update-ok");
    start_update(&mut a, "update-ok");
    snap("update-modal", &a);
    let m = a.modal.as_ref().unwrap();
    assert_eq!(m.chosen(), vec![401, 402, 403]);
    a.on_key(key(KeyCode::Char(' ')));
    assert_eq!(a.modal.as_ref().unwrap().chosen(), vec![402, 403], "Space excludes the CT under the cursor");
    a.on_key(key(KeyCode::Esc));
    assert!(a.modal.is_none() && a.view == View::List && !a.update.running, "Esc changes nothing");
}

#[test]
fn update_enter_starts_run_and_run_view_shows_queue_and_log() {
    let mut a = rw_app("update-ok");
    let evs = start_update(&mut a, "update-ok");
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::Update { only: vec![401, 402, 403] });
    assert!(a.view == View::Update && a.update.running && a.modal.is_none());
    let mid = evs.iter().position(|e| matches!(e, UpdEvent::Step(402, update::Step::Upgrade))).unwrap() + 4;
    feed(&mut a, &evs[..mid]);
    snap("update-running", &a);
    assert_eq!(a.on_key(key(KeyCode::Char('u'))), Action::None, "u is disabled during a run");
    // x abre el diálogo; Esc lo cierra; Enter confirma: terminar el CT y parar.
    a.on_key(key(KeyCode::Char('x')));
    assert!(a.update.confirm_cancel);
    snap("update-cancel", &a);
    a.on_key(key(KeyCode::Esc));
    assert!(!a.update.confirm_cancel && a.update.running);
    a.on_key(key(KeyCode::Char('x')));
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::CancelUpdate);
    assert_eq!(a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), Action::None, "Ctrl-C during a run opens the cancel dialog, it does not quit");
    assert!(a.update.confirm_cancel);
}

#[test]
fn cancel_bar_and_dialog_say_stop_now_before_install() {
    let mut a = rw_app("update-ok");
    let evs = start_update(&mut a, "update-ok");
    a.on_key(key(KeyCode::Enter));
    let at = evs.iter().position(|e| matches!(e, UpdEvent::Step(402, update::Step::Backup))).unwrap();
    feed(&mut a, &evs[..=at]);
    a.on_key(key(KeyCode::Char('x')));
    snap("update-cancel-backup", &a);
}

#[test]
fn update_run_finished_summary_and_ring_cap() {
    let mut a = rw_app("update-fail");
    let evs = start_update(&mut a, "update-fail");
    a.on_key(key(KeyCode::Char('u'))); // reabre selección (aún no hay corrida)
    a.modal = None;
    a.update.running = true;
    a.view = View::Update;
    feed(&mut a, &evs);
    let h = fixture("update-fail");
    let cfg = Config::load(&h).unwrap();
    h.set_update_enabled(true);
    let rep = update::run(&h, &guests::load(&h), &cfg, &update::Opts { only: vec![421, 422, 423, 424] }, &AtomicBool::new(false), &mut |_| {}).unwrap();
    a.update_done(rep, vec!["run_summary: sent via apprise-api (curl)".into()]);
    snap("update-finished", &a);
    assert!(!a.update.running);
    for i in 0..2500 {
        a.update_event(UpdEvent::Line(421, format!("line {i}")), "00:16:00", 0);
    }
    assert_eq!(a.update.log.len(), super::app::LOG_RING, "live log is a 2000-line ring");
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::None);
    assert_eq!(a.view, View::List);
}

#[test]
fn update_key_is_refused_in_read_only_and_without_positive_check() {
    let mut a = app("update-ok"); // read_only = true
    assert_eq!(a.on_key(key(KeyCode::Char('u'))), Action::None);
    assert!(a.status.contains("read-only"), "{}", a.status);
    let mut b = rw_app("failures");
    b.on_key(key(KeyCode::Char('/')));
    press(&mut b, "pve-locked");
    b.on_key(key(KeyCode::Enter));
    assert_eq!(b.on_key(key(KeyCode::Char('u'))), Action::None);
    assert!(b.status.contains("no CT with a positive check"), "{}", b.status);
}

#[test]
fn in_read_only_c_checks_without_refreshing_and_says_so() {
    let mut a = app("basic-37"); // read_only = true
    assert!(matches!(a.on_key(key(KeyCode::Char('c'))), Action::Check { refresh: Some(false), .. }));
    assert!(!a.check.refresh);
    assert!(a.status.contains("read-only") && a.status.contains("without refreshing"), "{}", a.status);
    a.check.running = false;
    assert!(matches!(a.on_key(key(KeyCode::Char('C'))), Action::Check { refresh: Some(false), .. }));
}

#[test]
fn checks_are_blocked_while_an_update_runs() {
    let mut a = app("basic-37");
    a.read_only = false;
    a.update.running = true;
    for k in ['c', 'C'] {
        assert_eq!(a.on_key(key(KeyCode::Char(k))), Action::None, "{k}");
        assert!(a.status.contains("update is running"), "{k}: {}", a.status);
        assert!(!a.check.running, "{k}");
    }
    a.update.running = false;
    assert!(matches!(a.on_key(key(KeyCode::Char('c'))), Action::Check { refresh: None, .. }));
}

#[test]
fn escape_sequences_from_a_ct_never_reach_the_terminal_buffer() {
    use ratatui::{backend::TestBackend, Terminal};
    let mut t = Terminal::new(TestBackend::new(60, 3)).unwrap();
    t.draw(|f| {
        let s = pxu::share::fmt::trunc("ok \u{1b}]0;pwned\u{7} x", 50);
        f.render_widget(ratatui::widgets::Paragraph::new(ratatui::text::Line::raw(s)), f.area());
    })
    .unwrap();
    let txt: String = t.backend().buffer().content().iter().map(|c| c.symbol()).collect();
    assert!(!txt.contains('\u{1b}') && !txt.contains('\u{7}'), "{txt:?}");
}

// ---------------------------------------------------------------- tecla `a` (auto-update)

fn auto_host(cfg: Option<&str>) -> ReadOnlyHost<MockHost> {
    let w = World::demo().auto_tags(&["auto"]);
    let h = ReadOnlyHost::new(match cfg {
        Some(c) => w.config_raw(c).build(),
        None => w.build(),
    });
    if cfg == Some("") {
        h.inner().remove_file(std::path::Path::new(pxu::share::config::CONFIG_PATH)).unwrap();
    }
    h
}

fn auto_app(h: &ReadOnlyHost<MockHost>) -> App {
    let (ro, reason) = super::read_only_state(h);
    let mut a = App::new(guests::load(h), ro, true);
    a.ro_reason = reason;
    a.clock = "00:16".into();
    a
}

fn goto(a: &mut App, vmid: u32) {
    a.cursor = a.visible().iter().position(|g| g.vmid == vmid).unwrap();
}

fn cfg_text(h: &ReadOnlyHost<MockHost>) -> String {
    String::from_utf8(h.read_file(std::path::Path::new(pxu::share::config::CONFIG_PATH)).unwrap()).unwrap()
}

#[test]
fn a_toggles_a_tag_enabled_ct_off_with_an_override_and_the_column_updates() {
    let src = "# mi config\n[update]\nauto_update_tags = [\"auto\"]\n";
    let h = auto_host(Some(src));
    let mut a = auto_app(&h);
    goto(&mut a, 116);
    assert!(a.selected().unwrap().auto_update, "116 carries the tag");
    assert_eq!(a.on_key(key(KeyCode::Char('a'))), Action::AskAutoUpdate { vmid: 116 });
    super::ask_auto_modal(&h, &mut a, 116);
    let m = a.auto_modal.as_ref().unwrap();
    assert!(m.value && m.initial, "opens with the saved value (on by tag)");
    a.on_key(key(KeyCode::Char(' ')));
    let m = a.auto_modal.as_ref().unwrap();
    assert!(!m.value && m.lines().iter().any(|l| l.contains("yes by tag auto")) && m.lines().iter().any(|l| l.contains("wins over")), "{:?}", m.lines());
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("[ ] Automatic updates") && out.contains("[Enter] save") && out.contains("Saves: [[ct]] 116 auto_update = false"), "{out}");
    // Esc cancela sin tocar nada
    a.on_key(key(KeyCode::Esc));
    assert!(a.auto_modal.is_none() && cfg_text(&h) == src);
    // `y` confirma → acción → el bucle escribe
    super::ask_auto_modal(&h, &mut a, 116);
    a.on_key(key(KeyCode::Char(' ')));
    let act = a.on_key(key(KeyCode::Enter));
    assert_eq!(act, Action::SetAutoUpdate { vmid: 116, auto: Some(false), driver: None });
    super::save_auto_update(&h, &mut a, 116, Some(false), None);
    let now = cfg_text(&h);
    assert!(now.starts_with(src) && now.contains("vmid = 116") && now.contains("auto_update = false"), "{now}");
    assert!(!a.guest(116).unwrap().auto_update, "AUTO column refreshed without restarting");
    assert!(a.status.contains("disabled"), "{}", a.status);
    // y otra vez: ahora hay bloque, solo cambia `auto_update`
    goto(&mut a, 116);
    super::ask_auto_modal(&h, &mut a, 116);
    assert!(!a.auto_modal.as_ref().unwrap().value, "opens with the saved value (off)");
    super::save_auto_update(&h, &mut a, 116, Some(true), None);
    let again = cfg_text(&h);
    assert_eq!(again.matches("vmid = 116").count(), 1, "{again}");
    assert!(again.contains("auto_update = true") && a.guest(116).unwrap().auto_update);
}

#[test]
fn a_from_the_detail_view_enables_a_ct_that_has_no_block_and_no_tag() {
    let h = auto_host(Some("[update]\nauto_update_tags = [\"auto\"]\n"));
    let mut a = auto_app(&h);
    a.view = View::Detail(100);
    assert_eq!(a.on_key(key(KeyCode::Char('a'))), Action::AskAutoUpdate { vmid: 100 });
    super::ask_auto_modal(&h, &mut a, 100);
    assert!(!a.auto_modal.as_ref().unwrap().value);
    super::save_auto_update(&h, &mut a, 100, Some(true), None);
    assert!(cfg_text(&h).trim_end().ends_with("[[ct]]\nvmid = 100\nauto_update = true") && a.guest(100).unwrap().auto_update);
}

#[test]
fn a_without_config_warns_it_will_create_it_and_leaves_read_only_after_saving() {
    let h = auto_host(Some(""));
    let mut a = auto_app(&h);
    assert!(a.read_only && a.ro_reason.is_none());
    goto(&mut a, 105);
    assert_eq!(a.on_key(key(KeyCode::Char('a'))), Action::AskAutoUpdate { vmid: 105 });
    super::ask_auto_modal(&h, &mut a, 105);
    let lines = a.auto_modal.as_ref().unwrap().lines().join("|");
    assert!(lines.contains("will be created with only the settings you change") && lines.contains("leaves read-only mode"), "{lines}");
    super::save_auto_update(&h, &mut a, 105, Some(true), None);
    assert_eq!(cfg_text(&h).trim(), "[[ct]]\nvmid = 105\nauto_update = true");
    assert!(!a.read_only, "with a valid config the app leaves read-only mode");
}

#[test]
fn a_is_refused_in_forced_read_only_and_with_an_invalid_config_and_nothing_is_written() {
    let src = "[update]\nauto_update_tags = [\"auto\"]\n";
    let h = auto_host(Some(src));
    let mut a = auto_app(&h);
    a.forced_ro = true;
    assert_eq!(a.on_key(key(KeyCode::Char('a'))), Action::None);
    assert!(a.status.contains("--read-only"), "{}", a.status);
    // el host tampoco deja escribir sin el opt-in
    assert!(h.write_config(b"x").is_err() && cfg_text(&h) == src);
    // config inválida: arranca en solo lectura con el motivo, y `a` no abre el modal
    let bad = "[update]\non_failure = \"Stop\"\n";
    let h = auto_host(Some(bad));
    let mut a = auto_app(&h);
    assert!(a.read_only && a.ro_reason.as_deref().is_some_and(|r| r.contains("on_failure")));
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("read-only: ") && out.contains("on_failure"), "reason visible on screen");
    goto(&mut a, 116);
    super::ask_auto_modal(&h, &mut a, 116);
    assert!(a.auto_modal.is_none() && a.status.contains("not changed"), "{}", a.status);
    // aun con el modal forzado, el resultado no valida → no se escribe
    super::save_auto_update(&h, &mut a, 116, Some(false), None);
    assert!(a.status.contains("not saved") && cfg_text(&h) == bad, "{}", a.status);
}

// ---------------------------------------------------------------- horario del timer (`S`)

fn timer_host(installed: bool, enabled: bool, cal: &str) -> ReadOnlyHost<MockHost> {
    ReadOnlyHost::new(World::demo().auto_tags(&["auto"]).timer(installed, enabled, cal).build())
}

fn timer_app(h: &ReadOnlyHost<MockHost>) -> App {
    let mut a = auto_app(h);
    a.clock = "00:16".into();
    a
}

fn typed(a: &mut App, s: &str) {
    for ch in s.chars() {
        a.on_key(key(KeyCode::Char(ch)));
    }
}

fn open_sched(h: &ReadOnlyHost<MockHost>, a: &mut App) {
    assert_eq!(a.on_key(key(KeyCode::Char('S'))), Action::AskSchedule);
    super::ask_schedule_modal(h, a);
    assert!(a.sched.is_some(), "{}", a.status);
}

/// Enter en el diálogo → validación con el host → confirmación.
fn validate(h: &ReadOnlyHost<MockHost>, a: &mut App) {
    match a.on_key(key(KeyCode::Enter)) {
        Action::CheckSchedule { expr } => super::check_schedule(h, a, &expr),
        other => panic!("expected CheckSchedule, got {other:?}"),
    }
}

#[test]
fn config_view_shows_the_real_timer_state_from_the_host() {
    let h = timer_host(true, true, "Mon..Fri *-*-* 03:00");
    let mut a = timer_app(&h);
    assert_eq!(a.on_key(key(KeyCode::Char('g'))), Action::OpenConfig);
    super::open_config(&h, &mut a);
    assert_eq!(a.view, View::Config);
    for (w, hh) in [(120, 40), (80, 24)] {
        let (out, _) = render(&a, w, hh, true);
        assert!(out.contains("units installed : yes") && out.contains("enabled         : yes") && out.contains("Mon..Fri *-*-* 03:00") && out.contains("next run        : Tue 2026-11-10"), "{out}");
    }
    // sin unidades
    let h = timer_host(false, false, "");
    let mut a = timer_app(&h);
    super::open_config(&h, &mut a);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("units installed : no") && out.contains("enabled         : no"), "{out}");
    // Esc vuelve a la lista
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.view, View::List);
}

#[test]
fn enabling_without_installed_units_installs_them_and_writes_the_dropin() {
    let h = timer_host(false, false, "");
    let mut a = timer_app(&h);
    open_sched(&h, &mut a);
    let m = a.sched.as_ref().unwrap();
    assert!(m.enable && m.today() == "not installed" && m.warnings.is_empty(), "{:?}", m.warnings);
    validate(&h, &mut a);
    let m = a.sched.as_ref().unwrap();
    let t = m.lines.join("|");
    assert!(t.contains("today: not installed → enabled, *-*-* 04:30") && t.contains("next run: Tue 2026-11-10 04:30:00") && t.contains("will be written") && t.contains("schedule.conf"), "{t}");
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("Save the schedule?") && out.contains("[y/Enter] save"), "{out}");
    assert!(h.inner().timer().ops.is_empty(), "nothing written before confirming");
    let Action::SaveSchedule(plan) = a.on_key(key(KeyCode::Char('y'))) else { panic!("expected SaveSchedule") };
    assert!(plan.install_units && plan.enable);
    super::save_schedule(&h, &mut a, &plan);
    let t = h.inner().timer();
    assert_eq!(t.ops, ["write units", "write drop-in", "daemon-reload", "enable --now"]);
    assert_eq!(t.dropin.as_deref(), Some("[Timer]\nOnCalendar=\nOnCalendar=*-*-* 04:30\n"));
    assert!(t.installed && t.enabled && a.status.contains("enabled"), "{}", a.status);
    assert!(a.timer.as_ref().unwrap().as_ref().unwrap().enabled, "Config view state refreshed");
}

#[test]
fn changing_the_schedule_writes_only_the_dropin() {
    let h = timer_host(true, true, "*-*-* 04:30:00");
    let mut a = timer_app(&h);
    open_sched(&h, &mut a);
    let m = a.sched.as_ref().unwrap();
    assert_eq!((m.preset, m.time.as_str()), (super::sched::Preset::Daily, "04:30"), "current schedule pre-filled");
    a.on_key(key(KeyCode::Tab));
    a.on_key(key(KeyCode::Right)); // Mon-Fri
    a.on_key(key(KeyCode::Tab));
    for _ in 0..5 {
        a.on_key(key(KeyCode::Backspace));
    }
    typed(&mut a, "03:00");
    validate(&h, &mut a);
    let t = a.sched.as_ref().unwrap().lines.join("|");
    assert!(t.contains("today: enabled, *-*-* 04:30:00 → enabled, Mon..Fri *-*-* 03:00") && !t.contains("will be written"), "{t}");
    let Action::SaveSchedule(plan) = a.on_key(key(KeyCode::Enter)) else { panic!() };
    super::save_schedule(&h, &mut a, &plan);
    let t = h.inner().timer();
    assert_eq!(t.ops, ["write drop-in", "daemon-reload", "enable --now"]);
    assert_eq!(t.dropin.as_deref(), Some("[Timer]\nOnCalendar=\nOnCalendar=Mon..Fri *-*-* 03:00\n"));
    // semanal: día + hora
    open_sched(&h, &mut a);
    assert_eq!(a.sched.as_ref().unwrap().preset, super::sched::Preset::Weekdays);
    a.on_key(key(KeyCode::Tab));
    a.on_key(key(KeyCode::Right)); // weekly
    a.on_key(key(KeyCode::Tab));
    a.on_key(key(KeyCode::Right));
    a.on_key(key(KeyCode::Right)); // Mon → Wed
    assert_eq!(a.sched.as_ref().unwrap().expr().unwrap(), "Wed *-*-* 03:00");
}

#[test]
fn disabling_runs_disable_now_and_keeps_the_schedule() {
    let h = timer_host(true, true, "*-*-* 04:30:00");
    let mut a = timer_app(&h);
    open_sched(&h, &mut a);
    a.on_key(key(KeyCode::Char(' '))); // Enable → off
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::None);
    let t = a.sched.as_ref().unwrap().lines.join("|");
    assert!(t.contains("today: enabled, *-*-* 04:30:00 → disabled"), "{t}");
    let Action::SaveSchedule(plan) = a.on_key(key(KeyCode::Char('y'))) else { panic!() };
    super::save_schedule(&h, &mut a, &plan);
    let t = h.inner().timer();
    assert_eq!(t.ops, ["daemon-reload", "disable --now"]);
    assert!(!t.enabled && t.dropin.is_none() && t.calendar == "*-*-* 04:30:00" && a.status.contains("disabled"), "{}", a.status);
    // Esc cancela sin tocar nada
    open_sched(&h, &mut a);
    a.on_key(key(KeyCode::Esc));
    assert!(a.sched.is_none() && h.inner().timer().ops.len() == 2);
}

#[test]
fn an_invalid_schedule_is_not_written() {
    let h = timer_host(true, false, "");
    let mut a = timer_app(&h);
    open_sched(&h, &mut a);
    // hora inválida: ni siquiera llega a systemd
    a.on_key(key(KeyCode::Tab));
    a.on_key(key(KeyCode::Tab));
    for _ in 0..5 {
        a.on_key(key(KeyCode::Backspace));
    }
    typed(&mut a, "25:99");
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::None);
    assert!(a.sched.as_ref().unwrap().error.as_deref().is_some_and(|e| e.contains("HH:MM")));
    // OnCalendar libre que systemd rechaza
    a.on_key(key(KeyCode::BackTab));
    a.on_key(key(KeyCode::Right));
    a.on_key(key(KeyCode::Right));
    a.on_key(key(KeyCode::Right)); // custom
    a.on_key(key(KeyCode::Tab));
    typed(&mut a, "every blue moon");
    validate(&h, &mut a);
    let m = a.sched.as_ref().unwrap();
    assert!(m.stage == super::sched::Stage::Edit && m.error.as_deref().is_some_and(|e| e.contains("invalid schedule")), "{:?}", m.error);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("! invalid schedule"), "{out}");
    assert!(h.inner().timer().ops.is_empty() && h.inner().timer().dropin.is_none());
    // y el host tampoco escribe si se le fuerza un horario con salto de línea
    let _optin = pxu::share::host::ScheduleEditOptIn::new(&h);
    let evil = pxu::share::schedule::SchedulePlan { install_units: false, on_calendar: Some("daily\nExecStart=/bin/true".into()), enable: true };
    assert!(h.apply_schedule(&evil).is_err() && h.inner().timer().dropin.is_none());
}

#[test]
fn read_only_refuses_the_schedule() {
    let h = timer_host(true, true, "*-*-* 04:30");
    let mut a = timer_app(&h);
    a.forced_ro = true;
    assert_eq!(a.on_key(key(KeyCode::Char('S'))), Action::None);
    assert!(a.status.contains("--read-only"), "{}", a.status);
    // el envoltorio de solo lectura tampoco deja aplicar sin el opt-in explícito
    let plan = pxu::share::schedule::SchedulePlan { install_units: false, on_calendar: Some("*-*-* 05:00".into()), enable: true };
    assert_eq!(h.apply_schedule(&plan).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
    assert!(h.inner().timer().ops.is_empty());
    // sin opt-in tampoco desde el flujo normal (save_schedule lo da, y solo durante la llamada)
    super::save_schedule(&h, &mut a, &plan);
    assert_eq!(h.inner().timer().ops, ["write drop-in", "daemon-reload", "enable --now"]);
    assert_eq!(h.apply_schedule(&plan).unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn dialog_warns_when_the_scheduled_update_would_do_nothing_or_fail() {
    // sin config.toml
    let h = ReadOnlyHost::new(World::demo().timer(false, false, "").build());
    h.inner().remove_file(std::path::Path::new(pxu::share::config::CONFIG_PATH)).unwrap();
    let mut a = timer_app(&h);
    open_sched(&h, &mut a);
    assert!(a.sched.as_ref().unwrap().warnings.join("|").contains("will do nothing"), "{:?}", a.sched.as_ref().unwrap().warnings);
    // config válida pero sin ningún CT con auto-update
    let h = ReadOnlyHost::new(World::demo().config_raw("[update]\nauto_update_tags = [\"nobody\"]\n").timer(true, true, "*-*-* 04:30").build());
    let mut a = timer_app(&h);
    open_sched(&h, &mut a);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("no CT has auto-update enabled"), "{out}");
    // config inválida: avisa que fallará
    let h = ReadOnlyHost::new(World::demo().config_raw("[update]\non_failure = \"Stop\"\n").timer(true, true, "*-*-* 04:30").build());
    let mut a = timer_app(&h);
    open_sched(&h, &mut a);
    assert!(a.sched.as_ref().unwrap().warnings.join("|").contains("will fail"), "{:?}", a.sched.as_ref().unwrap().warnings);
    // la programación sigue siendo posible
    validate(&h, &mut a);
    assert!(a.sched.as_ref().unwrap().stage == super::sched::Stage::Confirm);
}

#[test]
fn an_unusual_current_schedule_opens_as_custom() {
    let h = timer_host(true, true, "Mon,Fri *-*-* 09:00:00");
    let mut a = timer_app(&h);
    open_sched(&h, &mut a);
    let m = a.sched.as_ref().unwrap();
    assert_eq!((m.preset, m.custom.as_str()), (super::sched::Preset::Custom, "Mon,Fri *-*-* 09:00:00"));
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("OnCalendar") && out.contains("Mon,Fri *-*-* 09:00:00") && out.contains("server's"), "{out}");
}

// --- CPU automático (muestreo sin `r`, reloj inyectado) ---

fn cpu_of(a: &App, vmid: u32) -> String {
    let g = a.guest(vmid).unwrap().clone();
    super::views::cpu_label(a, &g)
}

fn first(a: &App, run: bool) -> (u32, u32) {
    let r = a.inv.guests.iter().find(|g| g.kind == pxu::share::model::GuestKind::Lxc && g.is_running() == run).unwrap().vmid;
    (r, r)
}

#[test]
fn cpu_fills_without_r_after_second_tick() {
    let mut a = app("basic-37");
    let (id, _) = first(&a, true);
    a.cpu_seed(1_000_000);
    assert_eq!(cpu_of(&a, id), "…"); // esperando la segunda muestra
    let cur = a.cpu_prev[&id].0;
    let cores = a.guest(id).unwrap().cores.unwrap_or(1);
    // 1 s de reloj, medio core de CPU consumido
    a.cpu_sample([(id, cur + 500_000)].into(), 2_000_000);
    let want = 50.0 / cores as f32;
    assert!((a.cpu_pct[&id] - want).abs() < 0.01, "{} vs {want}", a.cpu_pct[&id]);
    assert!(!a.cpu_waiting);
    assert_ne!(cpu_of(&a, id), "…");
}

#[test]
fn cpu_stopped_ct_is_dash() {
    let mut a = app("basic-37");
    let (id, _) = first(&a, false);
    a.cpu_seed(1_000_000);
    assert_eq!(cpu_of(&a, id), "—");
    a.cpu_sample(Default::default(), 2_000_000);
    assert_eq!(cpu_of(&a, id), "—");
}

#[test]
fn cpu_normalised_by_cores_and_capped() {
    let mut a = app("basic-37");
    let (id, _) = first(&a, true);
    a.inv.guests.iter_mut().find(|g| g.vmid == id).unwrap().cores = Some(4);
    a.cpu_sample([(id, 0)].into(), 0);
    a.cpu_sample([(id, 2_000_000)].into(), 1_000_000); // 2 cores de 4 en 1 s
    assert!((a.cpu_pct[&id] - 50.0).abs() < 0.01);
    a.cpu_sample([(id, 100_000_000)].into(), 2_000_000); // desbordado: tope 100
    assert_eq!(a.cpu_pct[&id], 100.0);
}

#[test]
fn cpu_tick_reads_only_running_lxc_cpu_stat() {
    let h = fixture("basic-37");
    let mut a = app("basic-37");
    a.cpu_seed(1_000_000);
    super::cpu_tick(&h, &mut a, 4_000_000);
    let (run, _) = first(&a, true);
    let (stopped, _) = first(&a, false);
    assert!(a.cpu_pct.contains_key(&run)); // mock con cpu.stat fijo = 0 %
    assert!(!a.cpu_pct.contains_key(&stopped));
    assert!(a.cpu_prev.keys().all(|v| a.guest(*v).unwrap().is_running()));
    // el tick no toca el inventario (sin releer pct config): mismos objetos
    assert_eq!(a.inv.guests.len(), guests::load(&h).guests.len());
}

// ---------------------------------------------------------------- Apprise (`N`, `t`)

const SECRET_KEY: &str = "k3y-SECRET-xyz";
const SECRET_URL: &str = "tgram://123456:AAH-TOKEN-SECRET/777";
const KEEP: &str = "# my config\n[update]\nauto_update_tags = [\"auto\"] # tag\n\n[backup]\nmode = \"auto\"\n";

fn apprise_cfg(extra: &str) -> String {
    format!("{KEEP}\n[apprise]\n{extra}")
}

fn apprise_host(extra: &str) -> ReadOnlyHost<MockHost> {
    ReadOnlyHost::new(World::demo().auto_tags(&["auto"]).config_raw(&apprise_cfg(extra)).build())
}

fn open_apprise(h: &ReadOnlyHost<MockHost>, a: &mut App) {
    a.view = View::Config;
    assert_eq!(a.on_key(key(KeyCode::Char('N'))), Action::AskApprise);
    super::ask_apprise_modal(h, a);
    assert!(a.apprise_modal.is_some(), "{}", a.status);
}

/// Enter en el diálogo → validación de la config resultante → confirmación (o error en el diálogo).
fn check_apprise(h: &ReadOnlyHost<MockHost>, a: &mut App) {
    match a.on_key(key(KeyCode::Enter)) {
        Action::CheckApprise(e) => super::check_apprise(h, a, &e),
        other => panic!("expected CheckApprise, got {other:?}"),
    }
}

fn save_apprise(h: &ReadOnlyHost<MockHost>, a: &mut App) {
    let Action::SaveApprise(e) = a.on_key(key(KeyCode::Char('y'))) else { panic!("expected SaveApprise") };
    super::save_apprise(h, a, &e);
}

/// Pasa al campo `n` (0 = api_url, 1 = modo, 2 = secreto, 3 = tag, 4.. = eventos).
fn to_field(a: &mut App, n: usize) {
    while a.apprise_modal.as_ref().unwrap().field != n {
        a.on_key(key(KeyCode::Tab));
    }
}

#[test]
fn config_view_masks_apprise_secrets() {
    let h = apprise_host(&format!("api_url = \"http://user:pass@apprise.local:8000/path\"\napi_key = \"{SECRET_KEY}\"\nurls = [\"{SECRET_URL}\", \"ntfy://ntfy.sh/mi-tema\"]\n"));
    let mut a = timer_app(&h);
    super::open_config(&h, &mut a);
    for (w, hh) in [(120, 40), (80, 24)] {
        let (out, _) = render(&a, w, hh, true);
        assert!(out.contains("Notifications (Apprise)"), "{out}");
        for s in [SECRET_KEY, "AAH-TOKEN", "123456", "user:pass", "mi-tema", "/path"] {
            assert!(!out.contains(s), "{s} leaked:\n{out}");
        }
    }
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("http://apprise.local:8000") && out.contains("api_key         : ••• (set, plain text)") && out.contains("stored in plain text — save it in Config → n to encrypt") && out.contains("tgram://…, ntfy://…") && out.contains("updates_found"), "{out}");
    // sin [apprise]
    let h = ReadOnlyHost::new(World::demo().config_raw(KEEP).build());
    let mut a = timer_app(&h);
    super::open_config(&h, &mut a);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("status          : not configured"), "{out}");
}

#[test]
fn saving_an_api_key_keeps_the_rest_of_the_file_and_masks_the_confirmation() {
    let h = apprise_host("api_url = \"http://old.example:8000\"\nurls = [\"ntfy://ntfy.sh/x\"]\n# keep this comment\n");
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    // las urls guardadas abren el diálogo en modo urls; se pasa a la clave
    assert_eq!(a.apprise_modal.as_ref().unwrap().mode, pxu::share::config::AppriseMode::Urls);
    to_field(&mut a, 1);
    a.on_key(key(KeyCode::Right));
    to_field(&mut a, 0);
    for _ in 0..30 {
        a.on_key(key(KeyCode::Backspace));
    }
    typed(&mut a, "http://apprise.local:8000");
    to_field(&mut a, 2);
    typed(&mut a, SECRET_KEY);
    let (out, _) = render(&a, 120, 40, true);
    assert!(!out.contains(SECRET_KEY) && out.contains("••••"), "typed secret shown as dots:\n{out}");
    check_apprise(&h, &mut a);
    let m = a.apprise_modal.as_ref().unwrap();
    let t = m.lines.join("|").replace('\u{1}', " ");
    assert!(t.contains("today|  api_url ") && t.contains("http://old.example:8000") && t.contains("urls      ntfy://…") && t.contains("|new|") && t.contains("api_key   ••• (encrypted)") && t.contains("urls are removed") && !t.contains(SECRET_KEY), "{t}");
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("Save the notifications?") && !out.contains(SECRET_KEY), "{out}");
    assert!(!cfg_text(&h).contains(SECRET_KEY), "nothing written before confirming");
    save_apprise(&h, &mut a);
    let now = cfg_text(&h);
    assert!(now.starts_with(KEEP) && now.contains("api_key_enc = \"mockenc") && !now.contains(SECRET_KEY) && !now.contains("api_key = ") && now.contains("api_url = \"http://apprise.local:8000\"") && !now.contains("urls") && now.contains("# keep this comment"), "{now}");
    assert!(a.apprise_modal.is_none() && a.status.contains("saved"), "{}", a.status);
    assert!(!a.status.contains(SECRET_KEY));
    let (_, w) = pxu::share::config::Config::load_checked(&h).unwrap();
    assert!(w.iter().all(|w| !w.contains("[apprise]")), "{w:?}");
    // la vista Config se recargó, enmascarada
    let v = a.apprise.as_ref().unwrap().as_ref().unwrap().join("|");
    assert!(v.contains("api_key         : ••• (set, encrypted)") && !v.contains(SECRET_KEY) && !v.contains("plain text"), "{v}");
}

#[test]
fn saving_urls_writes_an_array() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key = \"old\"\n");
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    to_field(&mut a, 1);
    a.on_key(key(KeyCode::Char(' '))); // urls
    to_field(&mut a, 2);
    typed(&mut a, &format!("{SECRET_URL}, ntfy://ntfy.sh/mi-tema"));
    check_apprise(&h, &mut a);
    let t = a.apprise_modal.as_ref().unwrap().lines.join("|").replace('\u{1}', " ");
    assert!(t.contains("urls      ••• (encrypted)") && t.contains("api_key is removed") && !t.contains("AAH-TOKEN") && !t.contains("mi-tema"), "{t}");
    save_apprise(&h, &mut a);
    let now = cfg_text(&h);
    assert!(now.starts_with(KEEP) && now.contains("urls_enc = \"mockenc") && !now.contains(SECRET_URL) && !now.contains("mi-tema") && !now.contains("api_key"), "{now}");
    let (c, _) = pxu::share::config::Config::load_checked(&h).unwrap();
    assert!(c.apprise.urls.is_empty(), "no plain copy");
    assert_eq!(c.apprise.reveal(&h).unwrap().urls.len(), 2);
}

#[test]
fn changing_only_the_events_keeps_the_stored_secret_when_the_field_is_left_empty() {
    let h = apprise_host(&format!("api_url = \"http://apprise.local:8000\"\napi_key = \"{SECRET_KEY}\"\n"));
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("(stored — leave empty to keep)") && !out.contains(SECRET_KEY), "{out}");
    to_field(&mut a, 4);
    a.on_key(key(KeyCode::Char(' '))); // updates_found fuera
    to_field(&mut a, 6);
    a.on_key(key(KeyCode::Char(' '))); // run_summary fuera
    check_apprise(&h, &mut a);
    assert!(a.apprise_modal.as_ref().unwrap().lines.join("|").contains("on        update_failed, check_error"));
    save_apprise(&h, &mut a);
    let (c, _) = pxu::share::config::Config::load_checked(&h).unwrap();
    assert_eq!(c.apprise.reveal(&h).unwrap().api_key, SECRET_KEY, "the secret was kept (and is now stored encrypted)");
    assert!(c.apprise.api_key.is_empty() && !cfg_text(&h).contains(SECRET_KEY));
    assert_eq!(c.apprise.on, ["update_failed", "check_error"]);
    assert!(cfg_text(&h).starts_with(KEEP));
}

#[test]
fn an_invalid_result_writes_nothing_and_says_why() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key = \"old\"\n");
    let before = cfg_text(&h);
    // urls sin esquema
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    to_field(&mut a, 1);
    a.on_key(key(KeyCode::Right));
    to_field(&mut a, 2);
    typed(&mut a, "not-a-url");
    check_apprise(&h, &mut a);
    let m = a.apprise_modal.as_ref().unwrap();
    assert!(m.error.as_deref().is_some_and(|e| e.contains("not an Apprise URL")) && m.stage == super::apprise::Stage::Edit, "{:?}", m.error);
    // modo clave sin dirección de API: ningún transporte utilizable
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    to_field(&mut a, 0);
    for _ in 0..40 {
        a.on_key(key(KeyCode::Backspace));
    }
    check_apprise(&h, &mut a);
    assert!(a.apprise_modal.as_ref().unwrap().error.as_deref().is_some_and(|e| e.contains("no usable Apprise transport") && e.contains("nothing was written")));
    // una config ya inválida: ni siquiera se abre el diálogo
    let bad = ReadOnlyHost::new(World::demo().config_raw("[update]\non_failure = \"Stop\"\n").build());
    let mut a = timer_app(&bad);
    a.view = View::Config;
    assert_eq!(a.on_key(key(KeyCode::Char('N'))), Action::AskApprise);
    super::ask_apprise_modal(&bad, &mut a);
    assert!(a.apprise_modal.is_none() && a.status.contains("not changed") && a.status.contains("on_failure"), "{}", a.status);
    // y escribir directamente con una config inválida tampoco escribe
    let e = pxu::share::config::AppriseEdit { api_url: "http://x:1".into(), mode: pxu::share::config::AppriseMode::Key, secret: "k".into(), on: vec![], tag: String::new() };
    let _optin = pxu::share::host::ConfigEditOptIn::new(&bad);
    assert!(pxu::share::config::write_apprise(&bad, &e).unwrap_err().contains("nothing was written"));
    assert_eq!(cfg_text(&h), before);
}

#[test]
fn apprise_can_be_set_up_without_a_config_file() {
    let h = ReadOnlyHost::new(World::demo().build());
    h.inner().remove_file(std::path::Path::new(pxu::share::config::CONFIG_PATH)).unwrap();
    let mut a = auto_app(&h);
    open_apprise(&h, &mut a);
    to_field(&mut a, 0);
    typed(&mut a, "http://apprise.local:8000");
    to_field(&mut a, 2);
    typed(&mut a, "pxu");
    check_apprise(&h, &mut a);
    assert!(a.apprise_modal.as_ref().unwrap().lines.join("|").contains("will be created"));
    save_apprise(&h, &mut a);
    assert!(cfg_text(&h).contains("[apprise]") && cfg_text(&h).contains("api_key_enc = \"mockenc") && !cfg_text(&h).contains("api_key = "));
    assert!(!a.read_only, "a config now exists: the TUI leaves read-only");
}

#[test]
fn read_only_refuses_editing_and_testing_apprise() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key = \"pxu\"\n");
    let before = cfg_text(&h);
    let mut a = timer_app(&h);
    a.forced_ro = true;
    a.view = View::Config;
    assert_eq!(a.on_key(key(KeyCode::Char('N'))), Action::None);
    assert!(a.status.contains("--read-only"), "{}", a.status);
    assert_eq!(a.on_key(key(KeyCode::Char('t'))), Action::None);
    assert!(a.status.contains("--read-only") && !a.testing, "{}", a.status);
    // sin el opt-in explícito el host tampoco escribe
    let e = pxu::share::config::AppriseEdit { api_url: "http://other:1".into(), mode: pxu::share::config::AppriseMode::Key, secret: "k".into(), on: vec![], tag: String::new() };
    assert!(pxu::share::config::write_apprise(&h, &e).unwrap_err().contains("read-only"));
    assert_eq!(cfg_text(&h), before);
    // dos pruebas a la vez: la segunda se rechaza
    a.forced_ro = false;
    assert_eq!(a.on_key(key(KeyCode::Char('t'))), Action::TestNotify);
    assert!(a.testing && a.status.contains("sending"));
    assert_eq!(a.on_key(key(KeyCode::Char('t'))), Action::None);
}

#[test]
fn the_test_notice_reports_ok_or_the_error_without_secrets() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key = \"pxu\"\n");
    let mut a = timer_app(&h);
    a.testing = true;
    a.test_done(super::notify_test(&h));
    assert!(!a.testing && a.status.contains("test notice sent via apprise-api") && a.status.ends_with("ok"), "{}", a.status);
    let h = apprise_host(&format!("api_url = \"http://down.invalid:8000\"\napi_key = \"{SECRET_KEY}\"\n"));
    let mut a = timer_app(&h);
    a.testing = true;
    a.test_done(super::notify_test(&h));
    assert!(!a.testing && a.status.contains("failed (exit 5)") && a.status.contains("Could not resolve host") && !a.status.contains(SECRET_KEY), "{}", a.status);
    // sin transporte
    let h = apprise_host("");
    let r = super::notify_test(&h).unwrap_err();
    assert!(r.contains("no Apprise transport configured"), "{r}");
}

#[test]
fn secrets_never_reach_debug_output_or_error_text() {
    let e = pxu::share::config::AppriseEdit { api_url: "http://u:p@h:1/x".into(), mode: pxu::share::config::AppriseMode::Key, secret: SECRET_KEY.into(), on: vec![], tag: String::new() };
    let d = format!("{e:?}");
    assert!(!d.contains(SECRET_KEY) && !d.contains("u:p") && d.contains("•••"), "{d}");
    let c = pxu::share::notify::AppriseCfg { api_key: SECRET_KEY.into(), urls: vec![SECRET_URL.into()], ..Default::default() };
    let s = pxu::share::notify::scrub(&c, &format!("curl: failed {SECRET_URL} and /notify/{SECRET_KEY}"));
    assert!(!s.contains("AAH-TOKEN") && !s.contains(SECRET_KEY) && s.contains("•••"), "{s}");
    assert_eq!(pxu::share::config::mask_target("tgram://123/abc"), "tgram://…");
    assert_eq!(pxu::share::config::mask_target("nonsense"), "•••");
    assert_eq!(pxu::share::config::mask_api_url("https://a:b@host.example:8000/notify/x?y=1"), "https://host.example:8000");
}

#[test]
fn snapshots_of_the_config_view_and_the_apprise_dialog() {
    let a = ReadOnlyHost::new(World::demo().auto_tags(&["auto"]).timer(true, true, "Mon..Fri *-*-* 03:00").config_raw(&apprise_cfg("api_url = \"http://apprise.local:8000\"\napi_key_enc = \"mockenc707875\"\n")).build());
    let mut app = timer_app(&a);
    super::open_config(&a, &mut app);
    snap("config-view", &app);
    // config antigua con la clave en claro: Config lo dice
    let p = ReadOnlyHost::new(World::demo().auto_tags(&["auto"]).timer(true, true, "Mon..Fri *-*-* 03:00").config_raw(&apprise_cfg("api_url = \"http://apprise.local:8000\"\napi_key = \"pxu\"\n")).build());
    let mut plain = timer_app(&p);
    super::open_config(&p, &mut plain);
    snap("config-view-plain", &plain);
    open_apprise(&a, &mut app);
    snap("apprise-edit", &app);
    // caso real: de urls (cifradas) a la clave guardada, con tag y eventos
    let r = ReadOnlyHost::new(World::demo().auto_tags(&["auto"]).timer(true, true, "Mon..Fri *-*-* 03:00").config_raw(&apprise_cfg("api_url = \"http://192.168.1.28:8000\"\nurls_enc = \"mockenc757273\"\ntag = \"system\"\non = [\"update_failed\", \"run_summary\"]\n")).build());
    let mut real = timer_app(&r);
    super::open_config(&r, &mut real);
    open_apprise(&r, &mut real);
    to_field(&mut real, 1);
    real.on_key(key(KeyCode::Char(' ')));
    to_field(&mut real, 2);
    typed(&mut real, "newsecret");
    check_apprise(&r, &mut real);
    snap("apprise-confirm", &real);
}

/// Columna donde empieza la etiqueta de cada campo de la confirmación (todas iguales, con o sin resaltado) y que no se salga del área.
#[test]
fn the_apprise_confirmation_aligns_labels_wraps_notes_and_fits_80x24() {
    let r = ReadOnlyHost::new(World::demo().auto_tags(&["auto"]).timer(true, true, "Mon..Fri *-*-* 03:00").config_raw(&apprise_cfg("api_url = \"http://192.168.1.28:8000\"\nurls_enc = \"mockenc757273\"\ntag = \"system\"\non = [\"update_failed\", \"run_summary\"]\n")).build());
    let mut a = timer_app(&r);
    super::open_config(&r, &mut a);
    open_apprise(&r, &mut a);
    to_field(&mut a, 1);
    a.on_key(key(KeyCode::Char(' ')));
    to_field(&mut a, 2);
    typed(&mut a, "newsecret");
    check_apprise(&r, &mut a);
    for (w, h) in [(80, 24), (120, 40)] {
        let (out, _) = render(&a, w, h, true);
        let lines: Vec<&str> = out.lines().collect();
        let field_cols: Vec<usize> = lines
            .iter()
            .filter_map(|l| ["api_url ", "api_key ", "urls ", "tag ", "on "].iter().filter_map(|k| l.find(&format!("  {k}")).map(|i| l[..i].chars().count())).next())
            .collect();
        assert!(field_cols.len() >= 8 && field_cols.iter().all(|c| *c == field_cols[0]), "{w}x{h}: {field_cols:?}\n{out}");
        assert!(out.contains("[y/Enter] save") && out.contains("the stored urls are removed"), "{w}x{h}\n{out}");
        if h == 24 {
            // el pie y la barra de teclas de Config siguen visibles y el marco de la vista no queda tapado
            assert!(out.contains("[s] schedule") && out.contains("[Esc] back"), "{out}");
            let last = lines.len();
            assert!(lines[last - 4].starts_with('─') || lines[last - 4].starts_with('└'), "{out}");
        }
    }
}

fn conf_lines(a: &App, vmid: u32, text: &str) -> Vec<String> {
    let mut g = a.guest(vmid).expect("guest").clone();
    g.conf = pxu::share::proxmox::conf::Conf::parse(text);
    super::views::config_lines(&g).into_iter().map(|(s, _)| s).collect()
}

const CONF_131: &str = "#<div align='center'>\n#  <a href='https://community-scripts.org' target='_blank' rel='noopener noreferrer'>\n#    <img src='https://example.org/logo.png' alt='Logo' style='width:81px'/>\n#  </a>\n#\n#  <h2 style='font-size: 24px; margin: 16px 0;'>Docker LXC</h2>\n#  <span><a href='https://github.com/x/y'>GitHub</a></span> | <a href='https://github.com/x/y/issues'>Issues</a>\n#</div>\narch: amd64\ncores: 4\ndev0: /dev/dri/renderD128,gid=992\ndev1: /dev/dri/renderD129,gid=992\ndev2: /dev/dri/card0,gid=44\ndev3: /dev/dri/card1,gid=44\ndev4: /dev/kfd,gid=104\nfeatures: fuse=1,keyctl=1,nesting=1\nhostname: stream\nmemory: 2048\nnet0: name=eth0,bridge=vmbr0,hwaddr=BC:24:11:20:B8:85,ip=dhcp,ip6=auto,type=veth\nonboot: 1\nostype: debian\nrootfs: local-lvm:vm-131-disk-0,size=30G\ntimezone: America/Santiago\nfoo: bar\n";

#[test]
fn detail_shows_devices_timezone_net_extras_and_description() {
    let a = app("basic-37");
    let v = conf_lines(&a, 103, CONF_131);
    let all = v.join("\n");
    for want in ["dev0 /dev/dri/renderD128 · gid=992", "dev1 /dev/dri/renderD129", "dev2 /dev/dri/card0 · gid=44", "dev3 /dev/dri/card1", "dev4 /dev/kfd · gid=104", "timezone America/Santiago", "ip6=auto", "type=veth", "Docker LXC", "https://github.com/x/y", "GitHub", "Issues"] {
        assert!(all.contains(want), "missing {want:?} in:\n{all}");
    }
    assert!(!all.contains('<') && !all.contains("rel='"), "html leaked:\n{all}");
    // «foo» desconocida cae en other; las conocidas no.
    let oi = v.iter().position(|s| s == "other").expect("other section");
    assert_eq!(v[oi + 1], "  foo: bar");
    assert_eq!(v.len(), oi + 2, "only unknown keys in other:\n{all}");
}

#[test]
fn detail_lxc_minimal_omits_absent_fields() {
    let a = app("basic-37");
    let mut g = a.guest(103).expect("guest").clone();
    g.tags.clear();
    g.lock = None;
    g.conf = pxu::share::proxmox::conf::Conf::parse("hostname: x\nmemory: 512\nswap: 0\ncores: 1\nnet0: name=eth0,bridge=vmbr0,ip=dhcp,firewall=0\n");
    let v: Vec<String> = super::views::config_lines(&g).into_iter().map(|(s, _)| s).collect();
    let all = v.join("\n");
    assert!(!all.contains('—'), "dash in detail:\n{all}");
    assert!(!v.iter().any(|s| s == "devices" || s == "other" || s == "features"), "{all}");
    assert!(all.contains("swap 0 MB") && all.contains("firewall=0") && all.contains("template no"), "{all}");
    assert!(all.contains("cores 1") && !all.contains("cpulimit") && !all.contains("hookscript"), "{all}");
}

#[test]
fn detail_vm_shows_unused_ipconfig_and_other_keys() {
    let a = app("vms");
    let v = conf_lines(&a, 203, "name: o\nmemory: 1024\nscsi0: local-lvm:vm-203-disk-0,size=32G\nunused0: local-lvm:vm-203-disk-1\nipconfig0: ip=dhcp\nciuser: root\ncipassword: hunter2\nvga: qxl\nserial0: socket\n");
    let all = v.join("\n");
    assert!(all.contains("unused0 local-lvm:vm-203-disk-1"));
    assert!(all.contains("ipconfig0 ip=dhcp") && all.contains("cloud-init"));
    assert!(!all.contains("hunter2"), "secret leaked");
    let oi = v.iter().position(|s| s == "other").unwrap();
    assert_eq!(&v[oi + 1..], &["  vga: qxl".to_string(), "  serial0: socket".to_string()]);
}

#[test]
fn description_html_only_markup_is_summarised() {
    let d = vec!["<div>".to_string(), "<br/>".to_string(), "</div>".to_string()];
    assert_eq!(super::views::description_lines(&d), vec!["(html, 3 lines)".to_string()]);
    assert_eq!(super::views::description_lines(&["plain & simple".to_string()]), vec!["plain & simple".to_string()]);
}

#[test]
fn long_config_lines_wrap_without_losing_text() {
    let a = app("basic-37");
    let long = "x".repeat(10) + &" word,".repeat(40) + "END";
    let v = conf_lines(&a, 103, &format!("hostname: x\nlong: {long}\n"));
    let l = v.iter().find(|s| s.starts_with("  long:")).unwrap();
    // El texto largo sigue completo en la línea; el recorte/wrap lo hace la vista, no config_lines.
    assert!(l.ends_with("END"));
    for w in [30usize, 59] {
        let rows = super::views::wrap_for_test(l, w);
        assert!(rows.iter().all(|r| r.chars().count() <= w), "{rows:?}");
        let joined: String = rows.concat().split_whitespace().collect();
        assert_eq!(joined, l.split_whitespace().collect::<String>());
    }
}

/// Servidor `host` (A4000 + Phoenix3) con la revisión del host ya hecha y guardada, como tras `pxu check`.
fn host_app() -> (App, ReadOnlyHost<MockHost>) {
    host_app_with(fixture("host"))
}

fn host_app_with(h: ReadOnlyHost<MockHost>) -> (App, ReadOnlyHost<MockHost>) {
    use pxu::share::update::check::{self, CheckOpts};
    use std::sync::atomic::AtomicBool;
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    let rep = check::run(&h, &guests::load(&h), &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[], &AtomicBool::new(false), &mut |_| {});
    check::persist(&h, &rep).unwrap();
    let mut a = App::new(guests::load(&h), true, true);
    a.clock = "00:16".into();
    a.now_epoch = h.now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    (a, h)
}

#[test]
fn host_row_shows_cpu_percent_and_ram_after_two_samples() {
    let (mut a, h) = host_app();
    let g = a.guest(0).unwrap();
    assert_eq!((g.cores, g.memory_mb), (Some(4), Some(64000)));
    assert_eq!(g.os_label, "proxmox ve 9.0.11");
    assert_eq!(g.cpu_usec, Some((400000 + 1000 + 200000 + 3000) * 10_000));
    assert_eq!(pxu::share::fmt::ram(g.mem_used_bytes, g.memory_mb), "24.4/62.5 GB");
    a.cpu_seed(1_000_000);
    assert_eq!(cpu_of(&a, 0), "…");
    // 4 CPUs, 1 s de reloj: 2 CPUs-segundo ocupados más = 50 %
    let cur = a.cpu_prev[&0].0;
    a.cpu_sample([(0, cur + 2_000_000)].into(), 2_000_000);
    assert_eq!(cpu_of(&a, 0), "50.0%");
    // el tick lee /proc/stat del mock (valor fijo): el host entra en la muestra
    super::cpu_tick(&h, &mut a, 5_000_000);
    assert!(a.cpu_pct.contains_key(&0));
}

#[test]
fn host_row_os_falls_back_to_plain_proxmox_ve_when_the_version_is_unreadable() {
    let (a, _) = host_app_with(ReadOnlyHost::new(pxu::share::mock::scenario::World::host_gpu().host_out("pve-version", "").build()));
    assert_eq!(a.guest(0).unwrap().os_label, "proxmox ve");
}

#[test]
fn host_row_and_gpu_column_in_the_list() {
    let (a, _) = host_app();
    let host = a.inv.guests.iter().find(|g| g.kind == pxu::share::model::GuestKind::Host).expect("host row");
    assert_eq!(host.vmid, 0);
    snap("host-list", &a);
    // sin GPU en ningún guest ni host NVIDIA: la columna no aparece
    let mut plain = app("basic-37");
    assert!(!plain.inv.any_gpu());
    plain.cursor = 0;
    snap("list-no-gpu-column", &plain);
}

#[test]
fn host_detail_shows_groups_repos_gpus_and_cts() {
    let (mut a, _) = host_app();
    let idx = a.visible().iter().position(|g| g.kind == pxu::share::model::GuestKind::Host).unwrap();
    a.cursor = idx;
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.view, View::Detail(0));
    snap("host-detail", &a);
}

fn host_idx(a: &App) -> usize {
    a.visible().iter().position(|g| g.kind == pxu::share::model::GuestKind::Host).unwrap()
}

#[test]
fn u_on_the_host_row_opens_the_confirmation_and_esc_changes_nothing() {
    let (mut a, h) = host_app();
    a.read_only = false;
    let before = h.inner().recorded().len();
    a.cursor = host_idx(&a);
    assert_eq!(a.on_key(key(KeyCode::Char('u'))), Action::None);
    assert!(a.host_modal.is_some() && a.modal.is_none() && !a.update.running);
    snap("host-confirm", &a);
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::None);
    assert!(a.host_modal.is_none() && !a.update.running);
    // también desde el detalle del host; Enter confirma y devuelve la lista vista
    a.on_key(key(KeyCode::Enter));
    a.on_key(key(KeyCode::Char('u')));
    assert!(a.host_modal.is_some());
    match a.on_key(key(KeyCode::Enter)) {
        Action::HostUpdate { confirmed } => assert_eq!(confirmed.len(), 7),
        other => panic!("{other:?}"),
    }
    assert!(a.update.running && a.view == View::Update);
    // solo se emitieron lecturas (la TUI no ejecuta nada al confirmar: lo hace el hilo)
    assert_eq!(h.inner().recorded().len(), before);
}

#[test]
fn u_on_the_host_is_blocked_in_read_only_mode() {
    let (mut a, _) = host_app();
    assert!(a.read_only);
    a.cursor = host_idx(&a);
    assert_eq!(a.on_key(key(KeyCode::Char('u'))), Action::None);
    assert!(a.host_modal.is_none() && !a.update.running);
}

#[test]
fn host_dirty_dpkg_modal_offers_the_repair() {
    let (mut a, _) = host_app();
    a.read_only = false;
    a.host_stop(super::app::HostStop::Dirty(vec!["The following packages are only half configured:".into(), " pve-manager  half-configured".into()]));
    snap("host-dpkg-dirty", &a);
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::HostRepair);
}

#[test]
fn host_reconfirm_shows_the_new_list_with_a_note() {
    let (mut a, _) = host_app();
    let fresh = a.inv.host.as_ref().unwrap().packages.clone();
    a.host_stop(super::app::HostStop::Reconfirm(fresh));
    let (out, _) = render(&a, 80, 24, false);
    assert!(out.contains("the pending list changed since you confirmed it"), "{out}");
}

#[test]
fn host_update_result_snapshot_and_guest_counter_excludes_the_host_row() {
    use pxu::share::host::HostUpdateOptIn;
    use pxu::share::update::hostupdate as hu;
    let (mut a, h) = host_app();
    // el contador de guests no cuenta la fila del host
    a.cursor = 0;
    let (out, _) = render(&a, 120, 40, false);
    let n = a.visible().iter().filter(|g| g.kind != pxu::share::model::GuestKind::Host).count();
    assert!(out.contains(&format!("{n} guests")), "{out}");
    let cfg = Config::load(&h).unwrap();
    let confirmed = a.inv.host.as_ref().unwrap().packages.clone();
    let mut evs = Vec::new();
    let rep = {
        let _g = HostUpdateOptIn::new(&h);
        match hu::run(&h, &guests::load(&h), &cfg, &hu::HostOpts { confirmed }, &AtomicBool::new(false), &mut |e| evs.push(e)).unwrap() {
            hu::HostOutcome::Done(r) => r,
            o => panic!("{o:?}"),
        }
    };
    a.read_only = false;
    a.update = super::app::UpdateState { running: true, started: "00:16".into(), ..Default::default() };
    a.view = View::Update;
    feed(&mut a, &evs);
    a.update_done(rep, vec!["run_summary: sent via apprise-api (curl)".into()]);
    snap("host-update-result", &a);
}

#[test]
fn update_engine_never_touches_the_host_even_when_asked() {
    let (_, h) = host_app();
    let before = h.inner().recorded().len();
    let cfg = Config::load(&h).unwrap();
    h.set_update_enabled(true);
    let inv = guests::load(&h);
    let _ = update::select(&h, &inv, &cfg, &[]);
    update::run(&h, &inv, &cfg, &update::Opts { only: vec![0, 134] }, &AtomicBool::new(false), &mut |_| {}).ok();
    use pxu::share::mock::Recorded;
    let after = h.inner().recorded();
    for r in &after[before..] {
        match r {
            Recorded::Host(a) => assert!(a[0] != "apt-get" && a[0] != "apt" && a[0] != "dkms", "host command {a:?}"),
            Recorded::Guest(id, _) => assert_ne!(*id, 0),
        }
    }
}

// ---------------------------------------------------------------- selector de columnas (`o`)

fn header_row(a: &App, w: u16) -> String {
    render(a, w, 40, true).0.lines().nth(3).unwrap().to_string()
}

#[test]
fn columns_default_hides_tags_and_an_old_config_without_ui_loads() {
    let a = app("basic-37");
    assert!(!a.col_shown("tags") && a.col_shown("gpu") && a.col_shown("upd"));
    assert!(!header_row(&a, 120).contains("TAGS"));
    // config antigua, sin `[ui]`: se aplica el valor por defecto sin error
    let c: pxu::share::config::Config = toml::from_str("[update]\nstopped = \"skip\"\n").unwrap();
    assert_eq!(c.ui.visible_columns(), a.columns);
    // la lista guardada manda; los nombres desconocidos se ignoran
    let c: pxu::share::config::Config = toml::from_str("[ui]\ncolumns = [\"RAM\", \"tags\", \"bogus\"]\n").unwrap();
    assert_eq!(c.ui.visible_columns(), ["ram", "tags"]);
}

#[test]
fn column_picker_applies_at_once_and_hiding_removes_it_from_the_render() {
    let mut a = app("basic-37");
    press(&mut a, "o");
    assert!(a.col_modal.is_some());
    assert!(render(&a, 120, 40, true).0.contains("[x] TYPE"));
    // TYPE es la primera; ↓ ×5 llega a TAGS (sexta)
    assert!(header_row(&a, 120).contains("TYPE"));
    a.on_key(key(KeyCode::Char(' ')));
    assert!(!header_row(&a, 120).contains("TYPE"));
    for _ in 0..5 {
        a.on_key(key(KeyCode::Char('j')));
    }
    a.on_key(key(KeyCode::Char(' ')));
    assert!(header_row(&a, 120).contains("TAGS"));
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::SaveColumns(a.columns.clone()));
    assert!(a.col_modal.is_none());
    // sin cambios, cerrar no guarda nada
    press(&mut a, "o");
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::None);
}

#[test]
fn column_choice_is_saved_in_config_and_reloaded() {
    let h = ReadOnlyHost::new(World::demo().config_raw("[update]\nstopped = \"skip\"\n").build());
    let mut a = App::new(guests::load(&h), false, true);
    let cols: Vec<String> = ["ram", "upd"].iter().map(|c| c.to_string()).collect();
    super::save_columns(&h, &mut a, &cols);
    assert!(a.status.starts_with("columns saved"), "{}", a.status);
    let text = String::from_utf8(h.read_file(std::path::Path::new(pxu::share::config::CONFIG_PATH)).unwrap()).unwrap();
    assert!(text.contains("[update]") && text.contains("columns = [\"ram\", \"upd\"]"), "{text}");
    let c = pxu::share::config::Config::load_checked(&h).unwrap().0;
    assert_eq!(c.ui.visible_columns(), cols);
    // sin config.toml no se crea (saldría del modo solo lectura): vale solo en esta sesión
    let h = ReadOnlyHost::new(World::demo().build());
    h.inner().remove_file(std::path::Path::new(pxu::share::config::CONFIG_PATH)).unwrap();
    let mut a = App::new(guests::load(&h), true, true);
    super::save_columns(&h, &mut a, &cols);
    assert!(a.status.contains("this session only"), "{}", a.status);
    assert!(!h.exists(std::path::Path::new(pxu::share::config::CONFIG_PATH)));
}

#[test]
fn column_picker_snapshot() {
    let mut a = app("basic-37");
    press(&mut a, "o");
    snap("columns-modal", &a);
}

#[test]
fn a_on_the_host_row_toggles_and_saves_host_auto() {
    let h = fixture("host");
    let (mut a, h) = host_app_with(h);
    assert!(!a.guest(0).unwrap().auto_update, "host_auto is off by default");
    goto(&mut a, 0);
    super::ask_auto_modal(&h, &mut a, 0);
    let m = a.auto_modal.as_ref().expect("modal for the host");
    assert!(!m.value && m.lines().iter().any(|l| l.contains("HOST first")), "{:?}", m.lines());
    a.on_key(key(KeyCode::Char(' ')));
    assert!(a.auto_modal.as_ref().unwrap().saves().contains("[update] host_auto = true"));
    super::save_auto_update(&h, &mut a, 0, Some(true), None);
    assert!(cfg_text(&h).contains("host_auto = true"), "{}", cfg_text(&h));
    assert!(a.guest(0).unwrap().auto_update, "AUTO column of the host refreshed");
    super::save_auto_update(&h, &mut a, 0, Some(false), None);
    assert!(cfg_text(&h).contains("host_auto = false") && !a.guest(0).unwrap().auto_update);
}

// ---------------------------------------------------------------- `a`: casilla del driver NVIDIA dentro de la ventana de auto-update

fn gpu_host(versions: &[&str], run_driver: bool) -> ReadOnlyHost<MockHost> {
    let w = World::host_gpu().config_raw("[update]\nstopped = \"skip\"\n");
    ReadOnlyHost::new(if run_driver { w.nvidia_run_driver(versions).build() } else { w.build() })
}

fn open_a(h: &ReadOnlyHost<MockHost>, a: &mut App, vmid: u32) {
    // si la fila no está visible en la lista (p. ej. un host sin inventario), se abre sin la tecla
    if let Some(i) = a.visible().iter().position(|g| g.vmid == vmid) {
        a.cursor = i;
        assert_eq!(a.on_key(key(KeyCode::Char('a'))), Action::AskAutoUpdate { vmid });
    }
    super::ask_auto_modal(h, a, vmid);
}

#[test]
fn the_driver_box_is_offered_for_a_gpu_machine_with_a_run_driver_even_when_up_to_date() {
    use pxu::share::update::nvidia::Newest;
    let h = gpu_host(&["595.112.07", "595.120.01", "596.10.01"], true);
    let mut a = auto_app(&h);
    open_a(&h, &mut a, 134);
    let d = a.auto_modal.as_ref().unwrap().driver.as_ref().expect("CT with NVIDIA GPU + newer driver");
    assert_eq!((d.current.as_str(), d.newest.clone(), d.checked), ("595.112.07", Newest::Newer("595.120.01".into()), false), "never a newer major (596)");
    // sin GPU NVIDIA (CT140, CT131 AMD): sin casilla y con el motivo
    for vmid in [140, 131] {
        let (mut a, h) = host_app_with(gpu_host(&["595.112.07", "595.120.01", "596.10.01"], true));
        open_a(&h, &mut a, vmid);
        let m = a.auto_modal.as_ref().unwrap();
        assert_eq!(m.driver.as_ref().err().map(String::as_str), Some("no NVIDIA GPU"));
        let (out, _) = render(&a, 120, 40, false);
        assert!(!out.contains("Include GPU driver update") && out.contains("not offered — no NVIDIA GPU"), "{out}");
    }
    // al día: la casilla sale igual
    let h2 = gpu_host(&["595.112.07", "596.10.01"], true);
    let mut a = auto_app(&h2);
    open_a(&h2, &mut a, 134);
    let d = a.auto_modal.as_ref().unwrap().driver.as_ref().expect("up to date");
    assert_eq!(d.newest, Newest::UpToDate);
    assert!(d.lines(true)[0].contains("595.112.07 (up to date)"));
    // driver de apt (sin nvidia-uninstall): sin casilla y con el motivo
    let h3 = gpu_host(&["595.112.07", "595.120.01"], false);
    let mut a = auto_app(&h3);
    open_a(&h3, &mut a, 134);
    assert_eq!(a.auto_modal.as_ref().unwrap().driver.as_ref().err().map(String::as_str), Some("driver not installed with the .run"));
    let (out, _) = render(&a, 120, 40, false);
    assert!(!out.contains("Include GPU driver update") && out.contains("driver not installed with the .run"), "{out}");
    // sin red / índice ilegible: sale igual, con aviso
    let h4 = ReadOnlyHost::new(World::host_gpu().config_raw("[update]\n").file("/usr/bin/nvidia-uninstall", b"").build());
    let mut a = auto_app(&h4);
    open_a(&h4, &mut a, 134);
    let d = a.auto_modal.as_ref().unwrap().driver.as_ref().expect("no network");
    assert_eq!(d.newest, Newest::Unknown);
    assert!(d.lines(true)[0].contains("unknown (could not reach nvidia.com)"));
    // el host del fixture usa un driver de apt: sin casilla
    let (mut a, h5) = host_app_with(fixture("host"));
    open_a(&h5, &mut a, 0);
    assert_eq!(a.auto_modal.as_ref().unwrap().driver.as_ref().err().map(String::as_str), Some("driver not installed with the .run"));
}

fn nvidia_cfg(auto_on: bool, host_on: bool, ct_on: bool) -> String {
    format!("[update]\nstopped = \"skip\"\nhost_nvidia_auto = {host_on}\n[[ct]]\nvmid = 134\nauto_update = {auto_on}\nnvidia_auto = {ct_on}\n")
}

fn nvidia_host(auto_on: bool, host_on: bool, ct_on: bool) -> ReadOnlyHost<MockHost> {
    ReadOnlyHost::new(World::host_gpu().config_raw(&nvidia_cfg(auto_on, host_on, ct_on)).nvidia_run_driver(&["595.112.07", "595.120.01"]).build())
}

#[test]
fn a_opens_with_the_saved_values_auto_on_off_by_driver_on_off() {
    for (auto_on, drv_on) in [(true, true), (true, false), (false, false), (false, true)] {
        let (mut a, h) = host_app_with(nvidia_host(auto_on, drv_on, drv_on));
        open_a(&h, &mut a, 134);
        let m = a.auto_modal.as_ref().expect("window");
        let d = m.driver.as_ref().expect("driver box");
        assert_eq!((m.value, m.initial, d.checked, d.initial), (auto_on, auto_on, drv_on, drv_on));
        assert_eq!(m.changes(), (None, None));
        let (out, _) = render(&a, 120, 40, false);
        let want = format!("[{}] Automatic updates", if auto_on { "x" } else { " " });
        assert!(out.contains(&want) && out.contains(&format!("[{}] Include GPU driver update", if drv_on { "x" } else { " " })), "{out}");
        // sin cambios: Enter no escribe
        let before = cfg_text(&h);
        assert_eq!(a.on_key(key(KeyCode::Enter)), Action::None);
        assert!(a.auto_modal.is_none() && cfg_text(&h) == before && a.status.contains("nothing written"), "{}", a.status);
    }
}

#[test]
fn the_driver_box_needs_auto_update_and_unchecking_auto_update_unchecks_it() {
    let (mut a, h) = host_app_with(nvidia_host(false, false, false));
    open_a(&h, &mut a, 134);
    // foco en el driver, auto-update desmarcado: Space no marca
    a.on_key(key(KeyCode::Down));
    assert_eq!(a.auto_modal.as_ref().unwrap().focus, 1);
    a.on_key(key(KeyCode::Char(' ')));
    assert!(!a.auto_modal.as_ref().unwrap().driver.as_ref().unwrap().checked);
    assert!(a.status.contains("needs Automatic updates"), "{}", a.status);
    let (out, _) = render(&a, 120, 40, false);
    assert!(out.contains("needs automatic updates"), "{out}");
    // marcar auto-update, luego el driver; desmarcar auto-update desmarca el driver
    a.on_key(key(KeyCode::Char('k')));
    a.on_key(key(KeyCode::Char(' ')));
    a.on_key(key(KeyCode::Char('j')));
    a.on_key(key(KeyCode::Char(' ')));
    let m = a.auto_modal.as_ref().unwrap();
    assert!(m.value && m.driver.as_ref().unwrap().checked);
    assert_eq!(m.saves(), "Saves: [[ct]] 134 auto_update = true + [[ct]] 134 nvidia_auto + [update] host_nvidia_auto");
    a.on_key(key(KeyCode::Up));
    a.on_key(key(KeyCode::Char(' ')));
    let m = a.auto_modal.as_ref().unwrap();
    assert!(!m.value && !m.driver.as_ref().unwrap().checked);
    assert_eq!(m.changes(), (None, None));
    // un CT con el driver activo: apagar auto-update apaga también el driver
    let (mut a, h) = host_app_with(nvidia_host(true, true, true));
    open_a(&h, &mut a, 134);
    a.on_key(key(KeyCode::Char(' ')));
    let m = a.auto_modal.as_ref().unwrap();
    assert!(!m.driver.as_ref().unwrap().checked);
    assert_eq!(m.changes(), (Some(false), Some(false)));
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::SetAutoUpdate { vmid: 134, auto: Some(false), driver: Some(false) });
    super::save_auto_update(&h, &mut a, 134, Some(false), Some(false));
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    assert!(!cfg.nvidia_auto(134) && !cfg.auto_update(134, &[]) && cfg.update.host_nvidia_auto, "{}", cfg_text(&h));
    assert!(a.status.contains("auto-update disabled") && a.status.contains("NVIDIA driver update disabled"), "{}", a.status);
}

#[test]
fn saving_driver_on_in_a_ct_also_turns_the_host_on() {
    let (mut a, h) = host_app_with(nvidia_host(true, false, false));
    open_a(&h, &mut a, 134);
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Char(' ')));
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::SetAutoUpdate { vmid: 134, auto: None, driver: Some(true) });
    super::save_auto_update(&h, &mut a, 134, None, Some(true));
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    assert!(cfg.nvidia_auto(134) && cfg.update.host_nvidia_auto && cfg.auto_update(134, &[]), "{}", cfg_text(&h));
    assert!(a.status.contains("NVIDIA driver update enabled") && !a.status.contains("auto-update"), "{}", a.status);
}

#[test]
fn saving_driver_off_in_a_ct_leaves_the_host_flag_and_auto_update_alone() {
    let (mut a, h) = host_app_with(nvidia_host(true, true, true));
    open_a(&h, &mut a, 134);
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Char(' ')));
    assert_eq!(a.auto_modal.as_ref().unwrap().saves(), "Saves: [[ct]] 134 nvidia_auto = false (host flag untouched)");
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::SetAutoUpdate { vmid: 134, auto: None, driver: Some(false) });
    super::save_auto_update(&h, &mut a, 134, None, Some(false));
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    assert!(!cfg.nvidia_auto(134) && cfg.update.host_nvidia_auto && cfg.auto_update(134, &[]), "{}", cfg_text(&h));
    assert!(!a.guest(134).unwrap().nvidia_auto, "column refreshed");
}

#[test]
fn saving_driver_off_in_the_host_warns_about_dependent_cts_and_turns_only_the_host_flag_off() {
    let cfg = "[update]\nstopped = \"skip\"\nhost_auto = true\nhost_nvidia_auto = true\n[[ct]]\nvmid = 134\nnvidia_auto = true\n";
    let h = ReadOnlyHost::new(World::host_gpu().config_raw(cfg).nvidia_run_driver(&["595.112.07", "595.120.01"]).build());
    let (mut a, h) = host_app_with(h);
    open_a(&h, &mut a, 0);
    let m = a.auto_modal.as_ref().unwrap();
    assert!(m.value && m.driver.as_ref().unwrap().dependents == 1 && m.warning().is_none());
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Char(' ')));
    let (out, _) = render(&a, 120, 40, false);
    assert!(out.contains("1 CT(s) with nvidia_auto get nothing while the host flag is off") && out.contains("Saves: [update] host_nvidia_auto = false"), "{out}");
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::SetAutoUpdate { vmid: 0, auto: None, driver: Some(false) });
    super::save_auto_update(&h, &mut a, 0, None, Some(false));
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    assert!(!cfg.update.host_nvidia_auto && cfg.nvidia_auto(134) && cfg.update.host_auto, "{}", cfg_text(&h));
}

#[test]
fn n_does_nothing_in_the_list_and_in_the_detail() {
    let (mut a, _h) = host_app_with(nvidia_host(true, true, true));
    goto(&mut a, 134);
    assert_eq!(a.on_key(key(KeyCode::Char('n'))), Action::None);
    assert!(a.auto_modal.is_none());
    a.on_key(key(KeyCode::Enter));
    assert_eq!(a.on_key(key(KeyCode::Char('n'))), Action::None);
    assert!(a.auto_modal.is_none());
    let (out, _) = render(&a, 120, 40, false);
    assert!(!out.contains("[n]"), "{out}");
}

fn print_window(title: &str, a: &App) {
    let (out, _) = render(a, 120, 40, false);
    println!("--- {title} ---\n{}", help_popup(&out));
}

#[test]
fn a_window_snapshots_for_the_host_a_gpu_ct_and_a_ct_without_gpu() {
    let cfg = "[update]\nstopped = \"skip\"\nhost_auto = true\nhost_nvidia_auto = true\n[[ct]]\nvmid = 134\nauto_update = true\nnvidia_auto = true\n";
    let h = ReadOnlyHost::new(World::host_gpu().config_raw(cfg).nvidia_run_driver(&["595.112.07", "595.120.01"]).build());
    let mut a = auto_app(&h);
    open_a(&h, &mut a, 134);
    print_window("CT 134 (GPU), checked", &a);
    snap("auto-modal-ct-gpu-checked", &a);
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Char(' ')));
    print_window("CT 134 (GPU), driver unchecked", &a);
    snap("auto-modal-ct-gpu-unchecked", &a);
    let (mut a, h) = host_app_with(h);
    open_a(&h, &mut a, 0);
    print_window("host, checked", &a);
    snap("auto-modal-host-gpu-checked", &a);
    a.on_key(key(KeyCode::Down));
    a.on_key(key(KeyCode::Char(' ')));
    print_window("host, driver unchecked", &a);
    snap("auto-modal-host-gpu-unchecked", &a);
    let (mut a, h) = host_app_with(h);
    open_a(&h, &mut a, 140);
    print_window("CT 140 (no GPU)", &a);
    snap("auto-modal-ct-no-gpu", &a);
    let h2 = gpu_host(&["595.112.07"], false);
    let mut a = auto_app(&h2);
    open_a(&h2, &mut a, 134);
    snap("auto-modal-ct-apt-driver", &a);
    let h3 = ReadOnlyHost::new(World::host_gpu().config_raw("[update]\n").file("/usr/bin/nvidia-uninstall", b"").build());
    let mut a = auto_app(&h3);
    open_a(&h3, &mut a, 134);
    snap("auto-modal-ct-gpu-offline", &a);
}

// ---------------------------------------------------------------- página Logs

fn log_entries() -> Vec<pxu::share::update::runlog::LogEntry> {
    use pxu::share::update::runlog::{LogEntry, LogMeta};
    let mk = |name: &str, started: u64, duration: u64, result: &str, size: u64| LogEntry {
        path: std::path::PathBuf::from(format!("/var/log/pxu/{name}")),
        name: name.into(),
        size,
        meta: Some(LogMeta { started, duration, result: result.into(), node: "pve".into(), complete: !result.contains("INCOMPLETE") }),
        started,
    };
    vec![
        mk("run-20261105-0300.log", 1_793_847_600, 1122, "✅ 5 ok", 12_000),
        mk("run-20261104-0300.log", 1_793_761_200, 2711, "❌ 3 ok · 1 warn · 2 failed", 48_000),
        mk("run-20261103-0300.log", 1_793_674_800, 40, "⛔ STOPPED · 1 ok", 3_100_000),
    ]
}

fn logs_app() -> App {
    let mut a = app("basic-37");
    a.open_logs(log_entries());
    a
}

#[test]
fn l_asks_the_loop_to_open_the_logs_page() {
    let mut a = app("basic-37");
    assert_eq!(a.on_key(key(KeyCode::Char('l'))), Action::OpenLogs);
}

#[test]
fn logs_list_snapshot_and_selection_keys() {
    let mut a = logs_app();
    let (out, _) = render(&a, 80, 24, true);
    let date = pxu::share::update::runlog::local_ymd_hm(1_793_847_600); // la zona horaria cambia el texto: sin golden file
    for want in ["Date", "Duration", "Result", "Size", date.as_str(), "18m 42s", "5 ok", "STOPPED", "3.0 MB", "47 KB"] {
        assert!(out.contains(want), "{want} missing in:\n{out}");
    }
    assert!(out.find("5 ok").unwrap() < out.find("3 ok").unwrap(), "newest first");
    press(&mut a, "jj");
    assert_eq!(a.logs.sel, 2);
    press(&mut a, "j");
    assert_eq!(a.logs.sel, 2);
    press(&mut a, "k");
    assert_eq!(a.logs.sel, 1);
}

#[test]
fn an_unclosed_log_is_listed_as_incomplete_without_a_duration() {
    use pxu::share::update::runlog::{LogEntry, LogMeta};
    let mut a = app("basic-37");
    a.open_logs(vec![LogEntry {
        path: "/var/log/pxu/run-20261106-0300.log".into(),
        name: "run-20261106-0300.log".into(),
        size: 2048,
        meta: Some(LogMeta { started: 1_793_934_000, duration: 0, result: "⛔ INCOMPLETE".into(), node: "pve".into(), complete: false }),
        started: 1_793_934_000,
    }]);
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("INCOMPLETE") && out.contains("—") && out.contains("2 KB"), "{out}");
    assert!(!out.contains("0m 00s"), "{out}");
}

#[test]
fn empty_logs_page_says_so() {
    let mut a = app("basic-37");
    a.open_logs(vec![]);
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("no run logs yet"));
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::None);
}

#[test]
fn enter_opens_and_the_viewer_scrolls_with_n_p_and_q_keeps_the_selection() {
    let mut a = logs_app();
    press(&mut a, "j");
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::OpenLog("/var/log/pxu/run-20261104-0300.log".into()));
    a.log_loaded(Ok((1..=100).map(|i| format!("line {i}")).collect()));
    snap("logs-viewer", &a);
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("run-20261104-0300.log · ❌") && out.contains("3 ok · 1 warn · 2 failed · 45m 11s"));
    assert!(out.contains("[n/p] next/prev"), "n/p in the viewer bar at 80 columns:\n{out}");
    press(&mut a, "j");
    assert_eq!(a.logs.viewer.as_ref().unwrap().top, 1);
    a.on_key(key(KeyCode::PageDown));
    assert!(a.logs.viewer.as_ref().unwrap().top > 10);
    press(&mut a, "G");
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("line 100"));
    press(&mut a, "g");
    assert_eq!(a.logs.viewer.as_ref().unwrap().top, 0);
    assert_eq!(a.on_key(key(KeyCode::Char('n'))), Action::OpenLog("/var/log/pxu/run-20261103-0300.log".into()));
    assert_eq!(a.logs.sel, 2);
    assert_eq!(a.on_key(key(KeyCode::Char('n'))), Action::None); // ya es el último
    assert_eq!(a.on_key(key(KeyCode::Char('p'))), Action::OpenLog("/var/log/pxu/run-20261104-0300.log".into()));
    press(&mut a, "q");
    assert!(a.logs.viewer.is_none() && a.view == View::Logs && a.logs.sel == 1);
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.view, View::List);
}

#[test]
fn mouse_click_selects_double_click_opens_and_wheel_scrolls() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    use std::time::{Duration, Instant};
    let ev = |kind, row| MouseEvent { kind, column: 5, row, modifiers: KeyModifiers::NONE };
    let mut a = logs_app();
    render(&a, 80, 24, true); // el dibujo fija dónde están las filas
    let y0 = a.logs.rows_y.get();
    let t0 = Instant::now();
    assert_eq!(a.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), y0 + 1), t0), Action::None);
    assert_eq!(a.logs.sel, 1);
    let open = a.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), y0 + 1), t0 + Duration::from_millis(200));
    assert_eq!(open, Action::OpenLog("/var/log/pxu/run-20261104-0300.log".into()));
    // un segundo clic lento en otra fila solo selecciona
    assert_eq!(a.on_mouse(ev(MouseEventKind::Down(MouseButton::Left), y0 + 2), t0 + Duration::from_secs(5)), Action::None);
    assert_eq!(a.logs.sel, 2);
    a.on_mouse(ev(MouseEventKind::ScrollUp, y0), t0);
    assert_eq!(a.logs.sel, 0);
    // visor: la rueda desplaza
    a.log_loaded(Ok((1..=100).map(|i| format!("l{i}")).collect()));
    render(&a, 80, 24, true);
    a.on_mouse(ev(MouseEventKind::ScrollDown, 10), t0);
    assert_eq!(a.logs.viewer.as_ref().unwrap().top, 3);
}

#[test]
fn log_retention_dialog_takes_digits_and_saves_a_number() {
    let mut a = app("basic-37");
    a.read_only = false;
    a.log_days = Some(30);
    a.view = View::Config;
    press(&mut a, "L");
    assert_eq!(a.log_modal.as_deref(), Some("30"));
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("will write [general] log_retention_days = 30"));
    a.on_key(key(KeyCode::Backspace));
    a.on_key(key(KeyCode::Backspace));
    press(&mut a, "x7");
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::SaveLogRetention(7));
    assert!(a.log_modal.is_none());
    press(&mut a, "L");
    a.on_key(key(KeyCode::Esc));
    assert!(a.log_modal.is_none());
    a.forced_ro = true;
    press(&mut a, "L");
    assert!(a.log_modal.is_none() && a.status.contains("--read-only"));
}

#[test]
fn list_table_keeps_two_space_gaps_and_tags_use_extra_width() {
    use ratatui::style::Modifier;
    let mut a = app("basic-37");
    let long = "alpha-tag,bravo-tag,charlie-tag,delta-tag,echo-tag,foxtrot-tag,golf-tag";
    for g in a.inv.guests.iter_mut() {
        g.tags = long.split(',').map(String::from).collect();
    }
    a.columns.push("tags".into());
    let th = Theme { color: true };
    let mut shown = Vec::new();
    for w in [120u16, 230] {
        let mut t = Terminal::new(TestBackend::new(w, 40)).unwrap();
        t.draw(|f| draw(f, &a, &th)).unwrap();
        let buf = t.backend().buffer().clone();
        let line = |y: u16| -> String { (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        // Selección: el vídeo inverso cubre toda la fila, como antes.
        assert!(buf[(w - 1, 4)].modifier.contains(Modifier::REVERSED), "w={w}: selection bar must span the row");
        // Huecos de 2 espacios: cada cabecera va precedida por exactamente 2 espacios (sin reparto del sobrante).
        let head = line(3);
        let titles: Vec<&str> = head.split("  ").filter(|t| !t.trim().is_empty()).collect();
        assert!(titles.len() >= 6, "w={w}: header {head:?}");
        let tags_x = head.find("TAGS").unwrap();
        let name_x = head.find("NAME").unwrap();
        assert!(tags_x > name_x + 9, "w={w}");
        assert_eq!(&head[tags_x - 2..tags_x], "  ", "w={w}: gap before TAGS");
        // La línea 5 (primer guest): cuánto de las tags se ve.
        let row = line(5);
        shown.push(row.matches("-tag").count());
        // La tabla no se estira con espacios: AUTO no está pegado al borde si TAGS aún tiene texto.
    }
    assert!(shown[1] > shown[0], "TAGS must show more text at 230 than at 120: {shown:?}");
}

#[test]
fn footer_key_bars_single_line_left_aligned() {
    let keys = ["[↑↓/jk] move", "[1-4] filter", "[/] search", "[s] sort", "[c] check", "[u] update (asks)", "[r] reload", "[a] auto", "[b] backup", "[o] columns", "[d] storage", "[l] logs", "[g] config", "[S] schedule", "[?] help", "[q] quit"];
    let a = app("basic-37");
    // 230 columnas: todas las teclas en una sola línea, con huecos de 2 espacios.
    let (out, _) = render(&a, 230, 40, true);
    let want = keys.join("  ");
    let n = out.lines().filter(|l| l.contains("[q] quit")).count();
    assert_eq!(n, 1);
    assert!(out.lines().any(|l| l == want), "list keys must be one line with 2-space gaps");
    assert!(!out.lines().any(|l| l.contains("[r] reload") && !l.contains("[q] quit")), "no second key line at 230");
    // 80 columnas: ninguna tecla queda cortada.
    let (out, _) = render(&a, 80, 24, true);
    for k in ["[↑↓] move", "[1-4] filter", "[/] search", "[s] sort", "[c] check", "[u] update", "[r] reload", "[a] auto", "[b] bkp", "[o] cols", "[g] cfg", "[d] space", "[l] logs", "[?] help", "[q] quit"] {
        assert!(out.contains(k), "80 cols: missing {k}");
    }
    for w in [80u16, 120, 160, 230] {
        let (out, _) = render(&a, w, 40, true);
        assert!(out.lines().all(|l| l.chars().count() <= w as usize), "w={w}: line too wide");
    }
}

#[test]
fn list_name_column_is_as_wide_as_longest_name() {
    let mut a = app("basic-37");
    a.inv.guests[0].name = "find-my-device".into();
    let th = Theme { color: true };
    let longest = a.inv.guests.iter().map(|g| g.name.chars().count()).max().unwrap().max(4);
    assert_eq!(longest, 14);
    for tags in [true, false] {
        a.columns.retain(|c| c != "tags");
        if tags {
            a.columns.push("tags".into());
        }
        for w in [120u16, 230] {
            let mut t = Terminal::new(TestBackend::new(w, 40)).unwrap();
            t.draw(|f| draw(f, &a, &th)).unwrap();
            let buf = t.backend().buffer().clone();
            let head: String = (0..w).map(|x| buf[(x, 3)].symbol().to_string()).collect();
            let name_x = head.find("NAME").unwrap();
            // NAME termina donde empieza STATE menos el hueco de 2 espacios.
            let next_x = head.find("STATE").unwrap();
            let name_w = next_x - 2 - name_x;
            assert_eq!(name_w, longest, "w={w} tags={tags}: NAME width");
        }
    }
}

#[test]
fn list_columns_measure_content_and_tags_hide_when_too_narrow() {
    let mut a = app("basic-37");
    a.inv.guests[0].name = "find-my-device".into();
    for g in a.inv.guests.iter_mut().take(8) {
        g.tags = vec!["172.17.0.1".into(), "172.18.0.1".into(), "192.168.1.11".into()];
    }
    let th = Theme { color: true };
    let width_of = |head: &str, a: &str, b: &str| head.find(b).unwrap() - 2 - head.find(a).unwrap();
    for w in [120u16, 230] {
        let mut t = Terminal::new(TestBackend::new(w, 40)).unwrap();
        t.draw(|f| draw(f, &a, &th)).unwrap();
        let buf = t.backend().buffer().clone();
        let head: String = (0..w).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert_eq!(width_of(&head, "NAME", "STATE"), 14, "w={w}: NAME is never cut");
        let os = a.inv.guests.iter().map(|g| g.os_label.chars().count()).max().unwrap().max(2);
        // CPU va alineada a la derecha y mide 6 fijo: su cabecera empieza 3 columnas después del inicio.
        assert_eq!(width_of(&head, "OS", "CPU") - 3, os, "w={w}: OS");
        let disk = a.inv.guests.iter().map(|g| g.disk_bytes.map_or(1, |b| pxu::share::fmt::bytes_short(b).chars().count())).max().unwrap().max(4);
        let after = if head.contains("TAGS") { "TAGS" } else { "UPD" };
        assert_eq!(width_of(&head, "DISK", after), disk, "w={w}: DISK");
        if let Some(x) = head.find("TAGS") {
            let rest: String = head[x..].split("  ").next().unwrap().into();
            assert!(rest.trim_end().chars().count() >= 4, "w={w}: TAGS shown");
        }
    }
}

// ---------------------------------------------------------------- tecla `b` (backup antes del update)

#[test]
fn b_disables_the_backup_of_a_ct_and_enables_it_again_keeping_the_config() {
    let src = "# mi config\n[update]\nauto_update_tags = [\"auto\"]\n\n[[ct]]\nvmid = 116\nauto_update = false\n";
    let h = auto_host(Some(src));
    let mut a = auto_app(&h);
    goto(&mut a, 116);
    assert_eq!(a.selected().unwrap().backup, "auto");
    assert_eq!(a.on_key(key(KeyCode::Char('b'))), Action::AskBackup { vmid: 116 });
    super::ask_backup_modal(&h, &mut a, 116);
    let m = a.backup_modal.as_ref().unwrap();
    assert!(m.disabled && m.lines.iter().any(|l| l.contains("auto by [backup] mode (global)")) && m.lines.iter().any(|l| l.contains("no automatic rollback")), "{:?}", m.lines);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("Disable the backup before updating CT 116") && out.contains("WARNING") && out.contains("[y/Enter] save"), "{out}");
    // Esc cancela sin tocar nada
    a.on_key(key(KeyCode::Esc));
    assert!(a.backup_modal.is_none() && cfg_text(&h) == src && a.status.contains("unchanged"));
    // `y` confirma → el bucle escribe y la columna se actualiza
    super::ask_backup_modal(&h, &mut a, 116);
    assert_eq!(a.on_key(key(KeyCode::Char('y'))), Action::SetBackup { vmid: 116, disabled: true });
    super::save_backup(&h, &mut a, 116, true);
    let now = cfg_text(&h);
    assert!(now.starts_with("# mi config\n") && now.contains("auto_update = false") && now.contains("backup = \"none\""), "{now}");
    assert_eq!(now.matches("vmid = 116").count(), 1);
    assert_eq!(a.guest(116).unwrap().backup, "none");
    assert!(a.status.contains("WITHOUT backup"), "{}", a.status);
    // volver a activar: sin override hereda el global, el bloque con otras claves se queda
    super::ask_backup_modal(&h, &mut a, 116);
    let m = a.backup_modal.as_ref().unwrap();
    assert!(!m.disabled && m.lines.iter().any(|l| l.contains("none by [[ct]] override")), "{:?}", m.lines);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("Enable the backup before updating CT 116"), "{out}");
    assert_eq!(a.on_key(key(KeyCode::Enter)), Action::SetBackup { vmid: 116, disabled: false });
    super::save_backup(&h, &mut a, 116, false);
    assert_eq!(cfg_text(&h), src, "round trip");
    assert_eq!(a.guest(116).unwrap().backup, "auto");
}

#[test]
fn b_with_global_none_enables_the_ct_with_backup_auto() {
    let h = auto_host(Some("[backup]\nmode = \"none\"\n"));
    let mut a = auto_app(&h);
    goto(&mut a, 116);
    assert_eq!(a.selected().unwrap().backup, "none");
    super::ask_backup_modal(&h, &mut a, 116);
    let m = a.backup_modal.as_ref().unwrap();
    assert!(!m.disabled && m.lines.iter().any(|l| l.contains("the global [backup] mode is none: this CT gets backup = \"auto\"")), "{:?}", m.lines);
    assert_eq!(a.on_key(key(KeyCode::Char('y'))), Action::SetBackup { vmid: 116, disabled: false });
    super::save_backup(&h, &mut a, 116, false);
    assert!(cfg_text(&h).contains("[[ct]]\nvmid = 116\nbackup = \"auto\""), "{}", cfg_text(&h));
    assert_eq!(a.guest(116).unwrap().backup, "auto");
    // y otra vez: lo vuelve a desactivar
    super::ask_backup_modal(&h, &mut a, 116);
    assert!(a.backup_modal.as_ref().unwrap().disabled);
}

#[test]
fn b_turns_a_snapshot_override_into_none_and_works_from_the_detail_view() {
    let h = auto_host(Some("[backup]\nmode = \"vzdump\"\n[[ct]]\nvmid = 105\nbackup = \"snapshot\"\n"));
    let mut a = auto_app(&h);
    a.view = View::Detail(105);
    assert_eq!(a.on_key(key(KeyCode::Char('b'))), Action::AskBackup { vmid: 105 });
    super::ask_backup_modal(&h, &mut a, 105);
    let m = a.backup_modal.as_ref().unwrap();
    assert!(m.disabled && m.lines.iter().any(|l| l.contains("snapshot by [[ct]] override")) && m.lines.iter().any(|l| l.contains("override (snapshot) is replaced")), "{:?}", m.lines);
    assert_eq!(a.on_key(key(KeyCode::Char('y'))), Action::SetBackup { vmid: 105, disabled: true });
    super::save_backup(&h, &mut a, 105, true);
    assert!(cfg_text(&h).contains("backup = \"none\"") && !cfg_text(&h).contains("snapshot"));
    // detalle muestra el modo
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("backup before update: none (no rollback)"), "{out}");
    // volver a activar hereda el global
    super::save_backup(&h, &mut a, 105, false);
    assert_eq!(a.guest(105).unwrap().backup, "vzdump", "re-enabling inherits the global mode");
    assert_eq!(cfg_text(&h), "[backup]\nmode = \"vzdump\"\n[[ct]]\nvmid = 105\n");
    a.view = View::Detail(105);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("backup before update: yes, mode vzdump"), "{out}");
}

#[test]
fn b_is_refused_in_read_only_for_the_host_and_with_an_invalid_config() {
    let src = "[update]\nauto_update_tags = [\"auto\"]\n";
    let h = auto_host(Some(src));
    let mut a = auto_app(&h);
    a.forced_ro = true;
    goto(&mut a, 116);
    assert_eq!(a.on_key(key(KeyCode::Char('b'))), Action::None);
    assert!(a.status.contains("--read-only"), "{}", a.status);
    assert!(cfg_text(&h) == src && a.backup_modal.is_none());
    // host: no aplica
    let (mut a, _h) = host_app_with(fixture("host"));
    goto(&mut a, 0);
    assert_eq!(a.on_key(key(KeyCode::Char('b'))), Action::None);
    assert!(a.status.contains("host") && a.status.contains("not changed here"), "{}", a.status);
    // config inválida: no se abre el modal ni se escribe
    let bad = "[update]\non_failure = \"Stop\"\n";
    let h = auto_host(Some(bad));
    let mut a = auto_app(&h);
    goto(&mut a, 116);
    super::ask_backup_modal(&h, &mut a, 116);
    assert!(a.backup_modal.is_none() && a.status.contains("not changed"), "{}", a.status);
    super::save_backup(&h, &mut a, 116, true);
    assert_eq!(cfg_text(&h), bad);
}

#[test]
fn b_without_config_warns_it_will_create_it() {
    let h = auto_host(Some(""));
    let mut a = auto_app(&h);
    goto(&mut a, 105);
    super::ask_backup_modal(&h, &mut a, 105);
    let lines = a.backup_modal.as_ref().unwrap().lines.join("|");
    assert!(lines.contains("will be created with only that block"), "{lines}");
    super::save_backup(&h, &mut a, 105, true);
    assert_eq!(cfg_text(&h).trim(), "[[ct]]\nvmid = 105\nbackup = \"none\"");
}

#[test]
fn the_backup_column_is_visible_by_default_and_shows_only_yes_or_dash() {
    let h = auto_host(Some("[[ct]]\nvmid = 116\nbackup = \"none\"\n[[ct]]\nvmid = 105\nbackup = \"snapshot\"\n"));
    let a = auto_app(&h);
    let (out, _) = render(&a, 120, 40, true);
    let head = out.lines().find(|l| l.contains("BACKUP")).expect("header");
    assert!(head.find("BACKUP").unwrap() < head.find("AUTO").unwrap(), "{head}");
    assert!(pxu::share::config::UiCfg::default().visible_columns().iter().any(|c| c == "backup"));
    let col = head.find("BACKUP").unwrap();
    let cell = |id: &str| out.lines().find(|l| l.contains(&format!(" {id} "))).unwrap()[col..].split_whitespace().next().unwrap().to_string();
    assert_eq!(cell("116"), "-", "none (CT override)");
    assert_eq!(cell("105"), "yes", "snapshot override");
    assert_eq!(cell("100"), "yes", "global auto");
    assert!(!out.contains("snap ") && !out.contains("vzdump "), "no exact modes in the list");
    // selector `o`: marcada de entrada; una config con `columns` explícito sin "backup" no se toca
    assert!(a.col_shown("backup"));
    let c = pxu::share::config::UiCfg { columns: Some(vec!["os".into()]) };
    assert_eq!(c.visible_columns(), vec!["os".to_string()]);
}

#[test]
fn the_backup_column_is_a_dash_with_global_none() {
    let h = auto_host(Some("[backup]\nmode = \"none\"\n[[ct]]\nvmid = 105\nbackup = \"auto\"\n"));
    let a = auto_app(&h);
    let (out, _) = render(&a, 120, 40, true);
    let col = out.lines().find(|l| l.contains("BACKUP")).unwrap().find("BACKUP").unwrap();
    let cell = |id: &str| out.lines().find(|l| l.contains(&format!(" {id} "))).unwrap()[col..].split_whitespace().next().unwrap().to_string();
    assert_eq!(cell("100"), "-");
    assert_eq!(cell("105"), "yes");
}

// ---------------------------------------------------------------- página Storage (`d`)

/// La lista con `d` ya abierta y los datos leídos del mock (lo que hace el bucle en un hilo).
fn storage_app() -> App {
    let h = fixture("basic-37");
    let mut a = app("basic-37");
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    assert_eq!(a.on_key(key(KeyCode::Char('d'))), Action::OpenStorage);
    assert_eq!(a.view, View::Storage);
    a.storage.set_data(pxu::share::storage::load(&h, &a.inv.guests, &a.inv.node, &cfg));
    a
}

#[test]
fn storage_backup_column_is_the_pre_update_yes_dash_and_nothing_is_cut_at_120() {
    let h = fixture("basic-37");
    let mut a = app("basic-37");
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    a.on_key(key(KeyCode::Char('d')));
    for g in a.inv.guests.iter_mut().filter(|g| g.vmid == 100) {
        g.backup = "none".into();
    }
    a.storage.set_data(pxu::share::storage::load(&h, &a.inv.guests, &a.inv.node, &cfg));
    press(&mut a, "f");
    let (out, _) = render(&a, 120, 40, true);
    let row = |id: &str| out.lines().find(|l| l.contains(&format!(" {id} ct-"))).unwrap_or_else(|| panic!("row {id}\n{out}")).to_string();
    assert!(row("100").contains("— ") && row("100").contains(" -   ") && row("100").contains("no backup"), "{out}");
    assert!(row("101").contains(" yes "), "{out}");
    // nombre de storage largo y dos alertas enteros
    assert!(out.contains("remote-backup-3 pbs"), "{out}");
    // NAME y WHERE van enteros a 120 columnas: la que cede es ALERT (la doble del 133 se trunca con «…»; la completa está en el detalle)
    assert!(row("133").contains("remote-backup-3") && row("133").contains("last backup 44 d ago, disk"), "{out}");
}

#[test]
fn storage_section_titles_use_accent_and_white_sort_value() {
    let a = storage_app();
    let (out, t) = render(&a, 120, 40, true);
    let buf = t.backend().buffer();
    let fg_at = |needle: &str| -> Vec<Color> {
        let (y, line) = out.lines().enumerate().find(|(_, l)| l.contains(needle)).expect(needle);
        let x = line[..line.find(needle).unwrap()].chars().count();
        (0..needle.chars().count()).map(|i| buf[((x + i) as u16, y as u16)].fg).collect()
    };
    assert!(fg_at("Storages").iter().all(|c| *c == Color::Cyan));
    assert!(fg_at("Guests ·").iter().take(6).all(|c| *c == Color::Cyan));
    assert!(fg_at("· sort:").iter().all(|c| *c == Color::DarkGray));
    let sort = a.storage.sort.label();
    assert!(fg_at(&format!("sort: {sort}")).iter().rev().take(sort.chars().count()).all(|c| *c == Color::White));
    let (_, t) = render(&a, 120, 40, false);
    assert!(t.backend().buffer().content.iter().all(|c| c.fg == Color::Reset));
}

#[test]
fn d_opens_storage_and_shows_it_loading_before_the_data_arrives() {
    let mut a = app("basic-37");
    a.on_key(key(KeyCode::Char('d')));
    a.storage.loading = true;
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("pxu · storage") && out.contains("reading storages"), "{out}");
    assert_eq!(a.on_key(key(KeyCode::Char('r'))), Action::RefreshStorage);
    assert_eq!(a.on_key(key(KeyCode::Char('q'))), Action::None);
    assert_eq!(a.view, View::List);
}

#[test]
fn storage_page_snapshots() {
    let mut a = storage_app();
    snap("storage-list", &a);
    let (out, _) = render(&a, 120, 40, true);
    for want in ["Storages", "local-lvm", "lvmthin", "alloc 25.0G", "dedup 4.2×", "pxu pre-update target", "Guests · sort: used ↓", "USED/ALLOC", "LAST BACKUP", "alerts · no backup: 1 (100)"] {
        assert!(out.contains(want), "{want} missing:\n{out}");
    }
    let mut f = storage_app();
    press(&mut f, "f");
    let (out, _) = render(&f, 120, 40, true);
    for want in ["no backup", "last backup 44 d ago", "last backup 199 d ago"] {
        assert!(out.contains(want), "{want} missing:\n{out}");
    }
    // ordenado por % real: el 133 (99%) primero; el 131 (thin 99.8% pero real 66%) no sube
    press(&mut a, "S");
    press(&mut a, "S");
    assert_eq!(a.storage.ids()[0], 133, "sorted by the real % the fullest first");
    assert!(a.storage.ids().iter().position(|i| *i == 131).unwrap() > 5);
    // detalle del CT 131 (caso arr): 66% real + aviso de trim
    a.storage.detail = Some(131);
    snap("storage-detail", &a);
    let (out, _) = render(&a, 120, 40, true);
    for want in ["Disks", "local-lvm:vm-131-disk-0", "66%", "thin blocks 99.8% (3.0G) — reclaim with `pct fstrim 131`", "Snapshots (1)", "pxu-20261107-043000", "Backups (", "Pre-update backup (pxu)", "origin:"] {
        assert!(out.contains(want), "{want} missing:\n{out}");
    }
    assert!(!out.contains("99% full"), "{out}");
    // encabezado, fila del rootfs y lista: mismo usado/asignado (maxdisk 2.9G ≠ .conf 3.0G) y mismo %
    assert!(out.contains("rootfs 1.9G used of 2.9G · 66%") || out.contains("rootfs 2.0G used of 2.9G · 66%"), "{out}");
    let row = out.lines().find(|l| l.contains("local-lvm:vm-131-disk-0")).unwrap();
    assert!(row.contains("/ 2.9G") && row.contains("66%") && !row.contains("3.0G"), "{row}");
    let (lst, _) = render(&storage_app(), 120, 40, true);
    let lrow = lst.lines().find(|l| l.contains(" 131 ct-131")).unwrap();
    assert!(lrow.contains("/2.9G") && lrow.contains("66%"), "{lrow}");
    // el aviso de trim es solo del detalle: ni la lista ni el resumen lo mencionan
    let (list, _) = render(&storage_app(), 120, 40, true);
    assert!(!list.contains("fstrim") && !list.contains("thin blocks"), "{list}");
    // CT apagado: `—` en usado y %, sin 0%
    let mut z = storage_app();
    for _ in 0..4 {
        press(&mut z, "S");
    }
    let (out, _) = render(&z, 120, 40, true);
    let r = out.lines().find(|l| l.contains(" 117 ct-")).unwrap_or_else(|| panic!("row 117\n{out}"));
    assert!(r.contains("stopped") && r.contains("—/3.9G") && !r.contains(" 0%") && !r.contains("full"), "{r}");
    // CT 106: snapshot pxu marcado
    let mut b = storage_app();
    b.storage.detail = Some(106);
    let (out, _) = render(&b, 120, 40, true);
    assert!(out.contains("pxu-20261107-043000  (pxu pre-update snapshot)"), "{out}");
    // CT 103: backup de pxu en `local` marcado y último backup en pbs
    b.storage.detail = Some(103);
    let (out, _) = render(&b, 120, 40, true);
    assert!(out.contains("(pxu pre-update) backup/vzdump-lxc-103") && out.contains("remote-backup-3"), "{out}");
}

/// Detalle del CT `id` ya abierto, con los datos del mock.
fn storage_detail_app(id: u32) -> App {
    let mut a = storage_app();
    a.read_only = false;
    a.storage.detail = Some(id);
    a
}

fn sline(out: &str, needle: &str) -> String {
    out.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle}\n{out}")).to_string()
}

#[test]
fn storage_list_120x40_keeps_realistic_names_and_where_whole() {
    let a = storage_app();
    let (out, _) = render(&a, 120, 60, true);
    for (id, name) in [(114, "llama-swap"), (134, "openweb-ui"), (124, "technitium"), (128, "uptime-kuma"), (113, "vaultwarden1")] {
        let r = sline(&out, &format!(" {id} {name}"));
        assert!(r.contains(&format!("{name} ")) && r.contains("remote-backup-3"), "{r}");
        assert!(!r.contains("…") || r.rfind('…') > r.find("remote-backup-3"), "{r}");
    }
}

#[test]
fn storage_where_column_shows_the_storage_of_the_last_backup() {
    let mut a = storage_app();
    let (out, _) = render(&a, 120, 60, true);
    assert!(sline(&out, "LAST BACKUP").contains("LAST BACKUP WHERE"), "{out}");
    // PBS en la mayoría, `local` donde el último backup es un vzdump suelto (101), `—` sin backup (100)
    assert!(sline(&out, " 133 ct-133").contains("remote-backup-3"), "{out}");
    assert!(sline(&out, " 101 ct-101").contains(" local "), "{out}");
    let r100 = sline(&out, " 100 ct-100");
    assert!(r100.contains("— ") && !r100.contains("remote") && !r100.contains("local"), "{r100}");
    // ignora los restos de pxu: el 103 tiene un vzdump pre-update en `local` más reciente, pero el último backup es el del PBS
    let r103 = sline(&out, " 103 ct-103");
    assert!(r103.contains("remote-backup-3") && !r103.contains("local"), "{r103}");
    // 80x24: la columna se esconde como STATE y la tabla no se rompe
    let (small, _) = render(&a, 80, 24, true);
    assert!(!small.contains("WHERE") && small.lines().all(|l| l.chars().count() <= 80), "{small}");
    // mientras lee, `…`
    a.storage.data.as_mut().unwrap().partial = true;
    let (out, _) = render(&a, 120, 40, true);
    assert!(!sline(&out, " 133 ct-133").contains("remote-backup-3"), "{out}");
}

#[test]
fn storage_detail_backups_have_a_cursor_that_follows_and_x_asks_to_delete() {
    let mut a = storage_detail_app(103); // 2 backups: el PBS (el último) y el vzdump de pxu en local
    let (out, _) = render(&a, 120, 40, true);
    assert!(sline(&out, "Backups (2").contains("Backups (2"), "{out}");
    assert!(sline(&out, "(pxu pre-update) backup/").contains(" > "), "{out}");
    assert!(sline(&out, "[x] delete backup").contains("select backup"), "{out}");
    // el cursor baja al segundo y el estilo `sel` está en esa fila
    press(&mut a, "j");
    assert_eq!(a.storage.bsel, 1);
    let (out, t) = render(&a, 120, 40, true);
    let (y, line) = out.lines().enumerate().find(|(_, l)| l.contains("remote-backup-3 backup/ct/103")).unwrap();
    assert!(line.contains(" > "), "{line}");
    assert!(t.backend().buffer()[(5u16, y as u16)].modifier.contains(ratatui::style::Modifier::REVERSED), "{line}");
    press(&mut a, "j");
    assert_eq!(a.storage.bsel, 1, "stops at the last backup");
    press(&mut a, "k");
    press(&mut a, "k");
    assert_eq!(a.storage.bsel, 0);
    // sin backups, ↑↓ desplazan y `x` no abre nada
    let mut n = storage_detail_app(100);
    assert!(n.storage.backups().is_empty());
    assert_eq!(n.on_key(key(KeyCode::Char('x'))), Action::None);
    assert!(n.backup_del.is_none() && n.status.contains("no backup selected"), "{}", n.status);
}

#[test]
fn x_opens_the_delete_modal_with_the_only_backup_warning_and_the_pbs_note() {
    // CT 131: un solo backup, en el PBS
    let mut a = storage_detail_app(131);
    let b = a.storage.selected_backup().unwrap().clone();
    assert_eq!(a.on_key(key(KeyCode::Char('x'))), Action::None);
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("Delete this backup of CT 131?") && out.contains(&format!("volume : {}", b.volid)), "{out}");
    assert!(out.contains("storage: remote-backup-3 (Proxmox Backup Server)") && out.contains("size: "), "{out}");
    assert!(out.contains("WARNING: this is the only backup of CT 131"), "{out}");
    assert!(out.contains("space is freed after garbage collection on the PBS"), "{out}");
    assert!(out.contains(&format!("pxu runs: pvesm free {}", b.volid)) && out.contains("y/Enter") && out.contains("Esc/n"), "{out}");
    // la advertencia sale en `warn`
    let (_, t) = render(&a, 120, 40, true);
    let buf = t.backend().buffer();
    let (y, line) = out.lines().enumerate().find(|(_, l)| l.contains("WARNING")).unwrap();
    let x = line.find("WARNING").unwrap();
    assert_eq!(buf[(x as u16, y as u16)].fg, Color::Yellow);
    // otras teclas no cierran; Esc cancela sin borrar
    assert_eq!(a.on_key(key(KeyCode::Char('z'))), Action::None);
    assert!(a.backup_del.is_some());
    assert_eq!(a.on_key(key(KeyCode::Esc)), Action::None);
    assert!(a.backup_del.is_none() && a.status == "backup kept");
    // y confirma: la acción lleva el backup exacto
    a.on_key(key(KeyCode::Char('x')));
    assert_eq!(a.on_key(key(KeyCode::Char('y'))), Action::DeleteBackup { vmid: 131, item: b });
    assert!(a.backup_del.is_none());
    // un guest con 2 backups no lleva el aviso de único; uno en `local` no lleva la nota del PBS
    let mut c = storage_detail_app(103);
    c.on_key(key(KeyCode::Char('x')));
    let (out, _) = render(&c, 120, 40, true);
    assert!(out.contains("storage: local") && !out.contains("only backup") && !out.contains("garbage collection") && out.contains("(pxu pre-update)"), "{out}");
    c.on_key(key(KeyCode::Esc));
    press(&mut c, "j");
    c.on_key(key(KeyCode::Char('x')));
    let (out, _) = render(&c, 120, 40, true);
    assert!(!out.contains("only backup") && out.contains("garbage collection"), "{out}");
    snap("storage-backup-delete", &a_with_modal());
}

fn a_with_modal() -> App {
    let mut a = storage_detail_app(131);
    a.on_key(key(KeyCode::Char('x')));
    a
}

#[test]
fn deleting_a_backup_is_blocked_in_read_only_and_for_protected_backups() {
    // solo lectura (`app.read_only`): el mensaje habitual y ningún modal
    let mut a = storage_detail_app(131);
    a.read_only = true;
    a.on_key(key(KeyCode::Char('x')));
    assert!(a.backup_del.is_none() && a.status.contains("backups cannot be deleted"), "{}", a.status);
    a.forced_ro = true;
    a.on_key(key(KeyCode::Char('x')));
    assert!(a.status.contains("read-only mode (--read-only)"), "{}", a.status);
    // protegido: el 102 (múltiplo de 3) tiene un segundo backup protegido
    let mut p = storage_detail_app(102);
    assert_eq!(p.storage.backups().len(), 2);
    press(&mut p, "j");
    assert!(p.storage.selected_backup().unwrap().protected);
    let (out, _) = render(&p, 120, 40, true);
    assert!(out.contains("(protected)"), "{out}");
    assert_eq!(p.on_key(key(KeyCode::Char('x'))), Action::None);
    assert!(p.backup_del.is_none() && p.status.starts_with("protected:"), "{}", p.status);
    // el primero (sin proteger) sí se puede
    press(&mut p, "k");
    p.on_key(key(KeyCode::Char('x')));
    assert!(p.backup_del.is_some());
}

#[test]
fn deleting_through_the_mock_removes_the_backup_runs_exactly_pvesm_free_and_cleans_the_registry() {
    let h = fixture("basic-37");
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    let inv = guests::load(&h);
    let d = pxu::share::storage::load(&h, &inv.guests, &inv.node, &cfg);
    let g = d.guests.iter().find(|g| g.vmid == 131).unwrap();
    let b = g.backups[0].clone();
    assert_eq!(pxu::share::storage::free_argv(&b.volid), ["pvesm", "free", b.volid.as_str()]);
    let msg = pxu::share::storage::delete_backup(&h, 131, &b).unwrap();
    assert_eq!(msg, format!("deleted {}", b.volid));
    // el comando que llegó al host es exactamente `pvesm free <volid>`
    let pvesm = |h: &pxu::share::mock::MockHost| h.recorded().into_iter().filter_map(|r| match r { pxu::share::mock::Recorded::Host(a) if a.first().map(String::as_str) == Some("pvesm") => Some(a), _ => None }).collect::<Vec<_>>();
    let sent = pvesm(h.inner());
    assert_eq!(sent, vec![vec!["pvesm".to_string(), "free".into(), b.volid.clone()]]);
    // y el backup desaparece de la lectura siguiente (sin backup → alerta)
    let d2 = pxu::share::storage::load(&h, &inv.guests, &inv.node, &cfg);
    let g2 = d2.guests.iter().find(|g| g.vmid == 131).unwrap();
    assert!(g2.backups.is_empty() && g2.alerts.contains(&pxu::share::storage::Alert::NoBackup));
    // volid de otro guest / protegido: rechazados sin llegar al host
    let other = pxu::share::storage::BackupItem { volid: "remote-backup-3:backup/ct/132/2026-11-01T03:00:00Z".into(), ..b.clone() };
    assert!(pxu::share::storage::delete_backup(&h, 131, &other).unwrap_err().contains("not a backup of 131"));
    let prot = pxu::share::storage::BackupItem { protected: true, ..b.clone() };
    assert!(pxu::share::storage::delete_backup(&h, 131, &prot).unwrap_err().contains("protected"));
    assert_eq!(pvesm(h.inner()).len(), 1);
}

#[test]
fn backup_volids_must_belong_to_the_guest() {
    use pxu::share::storage::backup_volid_ok as ok;
    assert!(ok("remote-backup-3:backup/ct/131/2026-03-28T11:50:10Z", 131));
    assert!(ok("remote-backup-3:backup/vm/200/2026-03-28T11:50:10Z", 200));
    assert!(ok("local:backup/vzdump-lxc-131-2026_11_08-03_16_00.tar.zst", 131));
    assert!(ok("local:backup/vzdump-qemu-200-2026_11_08-03_16_00.vma.zst", 200));
    for bad in [
        "remote-backup-3:backup/ct/132/2026-03-28T11:50:10Z",
        "remote-backup-3:backup/ct/1310/2026-03-28T11:50:10Z",
        "local:backup/vzdump-lxc-132-x.tar.zst",
        "local:backup/vzdump-lxc-131.tar.zst",
        "local:iso/debian.iso",
        "local:vm-131-disk-0",
        "local:backup/../vzdump-lxc-131-x.tar",
        "local:backup/vzdump-lxc-131-x;rm.tar",
        "local:backup/vzdump-lxc-131-x tar",
        "-x:backup/ct/131/2026",
        "backup/ct/131/2026",
        "local:backup/ct/131",
    ] {
        assert!(!ok(bad, 131), "{bad}");
    }
}

#[test]
fn storage_keys_sort_filter_and_back() {
    let mut a = storage_app();
    assert_eq!(a.storage.sort.label(), "used ↓");
    press(&mut a, "f");
    assert!(a.storage.only_alerts);
    let mut ids = a.storage.ids();
    ids.sort();
    assert_eq!(ids, vec![100, 101, 133]);
    let (out, _) = render(&a, 80, 24, true);
    assert!(out.contains("alerts only") && out.contains("no-bk") && out.contains("old-bk"), "{out}");
    press(&mut a, "f");
    assert_eq!(a.storage.ids().len(), 37);
    for _ in 0..5 {
        press(&mut a, "S");
    }
    assert_eq!(a.storage.sort.label(), "used ↓", "five sorts make a full cycle");
    press(&mut a, "G");
    assert_eq!(a.storage.sel, 36);
    press(&mut a, "g");
    assert_eq!(a.storage.sel, 0);
    // Enter abre, Esc cierra el detalle y otro Esc sale a la lista
    a.on_key(key(KeyCode::Enter));
    assert!(a.storage.detail.is_some());
    a.on_key(key(KeyCode::Esc));
    assert!(a.storage.detail.is_none() && a.view == View::Storage);
    a.on_key(key(KeyCode::Esc));
    assert_eq!(a.view, View::List);
    // ayuda propia
    let mut a = storage_app();
    press(&mut a, "?");
    for (w, h) in [(120, 40), (80, 24)] {
        let (out, t) = render(&a, w, h, true);
        assert!(!help_popup(&out).contains('…'), "{out}");
        // todas las descripciones presentes tras unir las líneas envueltas
        let flat: String = help_popup(&out).lines().map(|l| l.trim_matches(|c| c == '│' || c == ' ')).collect::<Vec<_>>().join(" ");
        let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
        if h == 40 {
            for d in ["cycle sort: used · alloc · % · last backup (oldest first) · id", "what Proxmox shows (disk / maxdisk of the guest), not the thin-pool blocks", "suggests `pct fstrim <id>` when they exceed the real use by > 15 points (pxu never runs it)", "ignores pxu pre-update leftovers", "logical size of the backups / space used in the datastore", "pxu backup target without room for the biggest CT (like the pre-vzdump check)"] {
                assert!(flat.contains(d), "missing {d:?} in {out}");
            }
            // continuaciones alineadas con la columna de descripción
            let lines: Vec<&str> = out.lines().collect();
            let pop = help_popup(&out);
            let pl: Vec<&str> = pop.lines().collect();
            let i = pl.iter().position(|l| l.starts_with("│ thin-pool blocks")).unwrap();
            let col = pl[i].find("suggests").unwrap_or_else(|| panic!("{:?}\n{pop}", pl[i]));
            assert!(pl[i + 1][..col].chars().all(|c| c == ' ' || c == '│') && !pl[i + 1][col..].starts_with(' '), "{out}");
            let i = lines.iter().position(|l| l.contains("│ thin-pool blocks")).unwrap();
            // claves y títulos en accent
            let buf = t.backend().buffer();
            let (x, y) = (lines[i].chars().position(|c| c == '│').unwrap() as u16 + 2, i as u16);
            assert_eq!(buf[(x, y)].fg, ratatui::style::Color::Cyan);
            let j = lines.iter().position(|l| l.contains("── Alerts")).unwrap();
            let x = lines[j].chars().position(|c| c == 'A').unwrap() as u16;
            assert_eq!(buf[(x, j as u16)].fg, ratatui::style::Color::Cyan);
        }
        if h == 24 {
            assert!(out.contains("any other key closes"), "{out}");
        }
    }
    snap("help-storage", &a);
}

#[test]
fn storage_page_without_data_or_with_nothing_readable() {
    // sin lecturas (escenario sin storage): la página no falla y dice qué no pudo leer
    let h = ReadOnlyHost::new(World::new().build());
    let mut a = App::new(guests::load(&h), true, true);
    a.clock = "00:16".into();
    a.on_key(key(KeyCode::Char('d')));
    let cfg = pxu::share::config::Config::load(&h).unwrap();
    a.storage.set_data(pxu::share::storage::load(&h, &a.inv.guests, &a.inv.node, &cfg));
    for (w, hh) in [(120, 40), (80, 24)] {
        let (out, _) = render(&a, w, hh, true);
        assert!(out.contains("nothing readable") && out.contains("no guests on this node"), "{out}");
    }
    a.on_key(key(KeyCode::Enter));
    assert!(a.storage.detail.is_none(), "no guest, no detail");
}

// ---------------------------------------------------------------- marca del driver en la columna GPU y en el detalle

#[test]
fn the_gpu_column_and_the_detail_show_when_the_driver_is_included() {
    let (a, h) = host_app_with(nvidia_host(true, true, true));
    let (out, _) = render(&a, 120, 40, false);
    assert!(out.contains("NVIDIA ✓+drv"), "{out}");
    println!("--- GPU column ---\n{out}");
    let (narrow, _) = render(&a, 80, 24, false);
    assert!(narrow.contains("NV ✓+drv"), "{narrow}");
    let mut a = auto_app(&h);
    goto(&mut a, 134);
    a.on_key(key(KeyCode::Enter));
    let (det, _) = render(&a, 120, 40, false);
    assert!(det.contains("gpu driver: in scheduled updates"), "{det}");
    let (a, _h) = host_app_with(nvidia_host(true, false, false));
    let (out, _) = render(&a, 120, 40, false);
    assert!(!out.contains("+drv"), "{out}");
    let (mut a, _h) = host_app_with(nvidia_host(true, false, false));
    goto(&mut a, 134);
    a.on_key(key(KeyCode::Enter));
    let (det, _) = render(&a, 120, 40, false);
    assert!(det.contains("gpu driver: not in updates"), "{det}");
}

fn typed_tag(a: &mut App, s: &str) {
    to_field(a, 3);
    typed(a, s);
}

#[test]
fn the_tag_opens_with_the_stored_value_and_shows_in_config() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key = \"pxu\"\ntag = \"system\"\n");
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    assert_eq!(a.apprise_modal.as_ref().unwrap().tag, "system");
    let (out, _) = render(&a, 120, 40, true);
    assert!(out.contains("Tag") && out.contains("system") && out.contains("only untagged targets"), "{out}");
    let (c, _) = pxu::share::config::Config::load_checked(&h).unwrap();
    let v = pxu::share::config::apprise_summary(&h, &c.apprise).join("|");
    assert!(v.contains("tag             : system"), "{v}");
    let h2 = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key = \"pxu\"\n");
    let (c2, _) = pxu::share::config::Config::load_checked(&h2).unwrap();
    assert!(pxu::share::config::apprise_summary(&h2, &c2.apprise).join("|").contains("tag             : —"));
}

#[test]
fn saving_a_new_tag_writes_it_and_emptying_removes_the_key() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key = \"pxu\"\n");
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    typed_tag(&mut a, "system,ops");
    check_apprise(&h, &mut a);
    assert!(a.apprise_modal.as_ref().unwrap().lines.join("|").contains("tag       system,ops"));
    save_apprise(&h, &mut a);
    let t = cfg_text(&h);
    assert!(t.contains("tag = \"system,ops\"") && t.starts_with(KEEP), "{t}");
    open_apprise(&h, &mut a);
    to_field(&mut a, 3);
    for _ in 0..12 {
        a.on_key(key(KeyCode::Backspace));
    }
    check_apprise(&h, &mut a);
    save_apprise(&h, &mut a);
    let t = cfg_text(&h);
    assert!(!t.contains("\ntag =") && t.contains("api_key_enc = "), "{t}");
}

#[test]
fn invalid_tag_characters_are_not_typed_and_not_written() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key = \"pxu\"\n");
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    typed_tag(&mut a, "a b\"c");
    assert_eq!(a.apprise_modal.as_ref().unwrap().tag, "abc");
    let e = pxu::share::config::AppriseEdit { api_url: "http://x:1".into(), mode: pxu::share::config::AppriseMode::Key, secret: "k".into(), on: vec![], tag: "a b".into() };
    assert!(pxu::share::config::set_apprise("", &e, Some("x")).unwrap_err().contains("not allowed"));
    let e = pxu::share::config::AppriseEdit { tag: "a\"b".into(), ..e };
    assert!(pxu::share::config::set_apprise("", &e, Some("x")).is_err());
}

#[test]
fn the_tag_applies_in_urls_mode_too() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\nurls = [\"ntfy://ntfy.sh/x\"]\n");
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    typed_tag(&mut a, "system");
    check_apprise(&h, &mut a);
    save_apprise(&h, &mut a);
    assert!(cfg_text(&h).contains("tag = \"system\"") && cfg_text(&h).contains("urls_enc ="));
}

#[test]
fn config_check_notes_plain_text_secrets_and_not_encrypted_ones() {
    let plain = apprise_host(&format!("api_url = \"http://apprise.local:8000\"\napi_key = \"{SECRET_KEY}\"\n"));
    let v = pxu::share::config::validate(&plain);
    assert!(v.notes.iter().any(|n| n.contains("plain text") && n.contains("encrypt")), "{:?}", v.notes);
    assert!(v.notes.iter().chain(&v.warnings).all(|n| !n.contains(SECRET_KEY)));
    let enc = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key_enc = \"mockenc707875\"\n");
    let v = pxu::share::config::validate(&enc);
    assert!(v.errors.is_empty() && v.typos.is_empty(), "{v:?}");
    assert!(v.notes.iter().all(|n| !n.contains("plain text")), "{:?}", v.notes);
}

#[test]
fn saving_encrypts_and_the_secret_never_appears_in_a_host_command() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\n");
    let mut a = timer_app(&h);
    open_apprise(&h, &mut a);
    to_field(&mut a, 2);
    typed(&mut a, SECRET_KEY);
    check_apprise(&h, &mut a);
    assert!(a.apprise_modal.as_ref().unwrap().lines.join("|").contains("stored encrypted"));
    save_apprise(&h, &mut a);
    let now = cfg_text(&h);
    assert!(now.contains("api_key_enc = \"mockenc") && !now.contains(SECRET_KEY) && !now.contains("api_key = "), "{now}");
    // la prueba de envío descifra y manda; la clave solo viaja por stdin
    assert!(super::notify_test_for_tests(&h).is_ok());
    let cmds: Vec<String> = h.inner().recorded().iter().map(|r| format!("{r:?}")).collect();
    assert!(cmds.iter().any(|c| c.contains("decrypt")) && cmds.iter().all(|c| !c.contains(SECRET_KEY)), "{cmds:?}");
}

#[test]
fn a_failed_decrypt_shows_a_clear_error_in_the_test_notice_and_sends_nothing() {
    let h = apprise_host("api_url = \"http://apprise.local:8000\"\napi_key_enc = \"garbage\"\n");
    let e = super::notify_test_for_tests(&h).unwrap_err();
    assert!(e.contains("cannot decrypt apprise secret"), "{e}");
    assert!(h.inner().recorded().iter().all(|r| !format!("{r:?}").contains("curl")));
}

#[test]
fn config_keys_work_without_shift_and_uppercase_stay_as_aliases() {
    for (lower, upper) in [('s', 'S'), ('n', 'N')] {
        for c in [lower, upper] {
            let mut a = app("demo");
            a.view = View::Config;
            let want = if lower == 's' { Action::AskSchedule } else { Action::AskApprise };
            assert_eq!(a.on_key(key(KeyCode::Char(c))), want, "{c}");
        }
    }
    for c in ['d', 'L'] {
        let mut a = app("demo");
        a.view = View::Config;
        assert_eq!(a.on_key(key(KeyCode::Char(c))), Action::None);
        assert!(a.log_modal.is_some() || !a.status.is_empty(), "{c}");
    }
}

#[test]
fn in_the_list_s_still_sorts_and_capital_s_opens_the_schedule() {
    let mut a = app("demo");
    assert_eq!(a.on_key(key(KeyCode::Char('S'))), Action::AskSchedule);
    let before = a.sort;
    assert_eq!(a.on_key(key(KeyCode::Char('s'))), Action::None);
    assert_ne!(a.sort, before);
}
