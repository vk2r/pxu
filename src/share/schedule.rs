//! Timer systemd `pxu-update` (horario del update automático). Las unidades se embeben de `dist/systemd/`,
//! la misma fuente que usa `install.sh`. Todo contacto con systemd va por `Host` (`timer_status`,
//! `validate_calendar`, `apply_schedule`).

pub const UNIT_DIR: &str = "/etc/systemd/system";
pub const TIMER_NAME: &str = "pxu-update.timer";
pub const SERVICE_UNIT: &str = include_str!("../../dist/systemd/pxu-update.service");
pub const TIMER_UNIT: &str = include_str!("../../dist/systemd/pxu-update.timer");
pub const FAILED_SERVICE_UNIT: &str = include_str!("../../dist/systemd/pxu-update-failed.service");
pub const NOTIFY_FAILED_UNIT: &str = include_str!("../../dist/systemd/pxu-update-notify-failed.service");
/// Script del aviso de fallo: no es pxu; lo instalan `install.sh` y `apply_schedule`.
pub const FAILURE_NOTICE_SCRIPT: &str = include_str!("../../dist/systemd/pxu-failure-notice");
pub const FAILURE_NOTICE_PATH: &str = "/usr/local/bin/pxu-failure-notice";
pub const FLUSH_TIMER_NAME: &str = "pxu-notify-flush.timer";
pub const FLUSH_SERVICE_UNIT: &str = include_str!("../../dist/systemd/pxu-notify-flush.service");
pub const FLUSH_TIMER_UNIT: &str = include_str!("../../dist/systemd/pxu-notify-flush.timer");
pub const DROPIN_DIR: &str = "/etc/systemd/system/pxu-update.timer.d";
pub const DROPIN_PATH: &str = "/etc/systemd/system/pxu-update.timer.d/schedule.conf";

/// Estado real del timer, tal como lo informa systemd.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimerStatus {
    pub installed: bool,
    pub enabled: bool,
    pub active: String,
    /// `OnCalendar` efectivos (incluido el drop-in).
    pub calendars: Vec<String>,
    /// Próxima ejecución (hora del servidor), si systemd la conoce.
    pub next: Option<String>,
}

/// Resultado de `systemd-analyze calendar`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarCheck {
    pub normalized: String,
    pub next: Option<String>,
}

/// Lo que se guarda: unidades (si faltan), drop-in con el horario y activar/desactivar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulePlan {
    pub install_units: bool,
    pub on_calendar: Option<String>,
    pub enable: bool,
}

/// Contenido del drop-in: la línea vacía borra el `OnCalendar` de la unidad base.
pub fn dropin_text(expr: &str) -> String {
    format!("[Timer]\nOnCalendar=\nOnCalendar={expr}\n")
}

/// Expresión apta para el drop-in: una sola línea, sin caracteres de control.
pub fn check_expr(expr: &str) -> Result<&str, String> {
    let e = expr.trim();
    if e.is_empty() {
        return Err("empty schedule".into());
    }
    if e.len() > 200 || e.chars().any(|c| c.is_control()) {
        return Err("invalid characters in the schedule".into());
    }
    Ok(e)
}

pub const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// `HH:MM` → (h, m).
pub fn parse_time(s: &str) -> Result<(u32, u32), String> {
    let bad = || format!("time must be HH:MM (00:00-23:59), got \"{s}\"");
    let (h, m) = s.trim().split_once(':').ok_or_else(bad)?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let (h, m): (u32, u32) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok((h, m))
}

/// Presets: diario, lunes a viernes, semanal (`day` = índice en `DAYS`).
pub fn daily(h: u32, m: u32) -> String {
    format!("*-*-* {h:02}:{m:02}")
}
pub fn weekdays(h: u32, m: u32) -> String {
    format!("Mon..Fri *-*-* {h:02}:{m:02}")
}
pub fn weekly(day: usize, h: u32, m: u32) -> String {
    format!("{} *-*-* {h:02}:{m:02}", DAYS[day % 7])
}

/// Salida de `systemctl show pxu-update.timer -p LoadState,ActiveState,UnitFileState,NextElapseUSecRealtime,TimersCalendar` (`clave=valor`).
pub fn parse_status(out: &str) -> TimerStatus {
    let mut s = TimerStatus::default();
    for l in out.lines() {
        let Some((k, v)) = l.split_once('=') else { continue };
        match k {
            "LoadState" => s.installed = v == "loaded",
            "ActiveState" => s.active = v.to_string(),
            "UnitFileState" => s.enabled = v == "enabled",
            "NextElapseUSecRealtime" => s.next = Some(v.trim().to_string()).filter(|v| !v.is_empty() && v != "n/a" && v != "0"),
            // `{ OnCalendar=*-*-* 04:30 ; next_elapse=... }` (varias entradas posibles)
            "TimersCalendar" => {
                for part in v.split("OnCalendar=").skip(1) {
                    let cal = part.split(" ;").next().unwrap_or("").trim().trim_end_matches('}').trim();
                    if !cal.is_empty() {
                        s.calendars.push(cal.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    s
}

/// Salida de `systemd-analyze calendar`: `Normalized form:` y `Next elapse:`.
pub fn parse_analyze(out: &str) -> Option<CalendarCheck> {
    let field = |name: &str| out.lines().find_map(|l| l.trim().strip_prefix(name).map(|v| v.trim().to_string()));
    let normalized = field("Normalized form:")?;
    Some(CalendarCheck { normalized, next: field("Next elapse:") })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropin_resets_the_base_calendar() {
        assert_eq!(dropin_text("Mon..Fri 03:00"), "[Timer]\nOnCalendar=\nOnCalendar=Mon..Fri 03:00\n");
    }

    #[test]
    fn expression_and_time_checks() {
        assert!(check_expr("daily\nExecStart=/bin/sh").is_err() && check_expr("  ").is_err());
        assert_eq!(check_expr(" *-*-* 04:30 "), Ok("*-*-* 04:30"));
        assert_eq!(parse_time("3:05"), Ok((3, 5)));
        for bad in ["24:00", "12:60", "1230", "ab:cd", "12:5", ""] {
            assert!(parse_time(bad).is_err(), "{bad}");
        }
        assert_eq!(weekly(2, 3, 0), "Wed *-*-* 03:00");
        assert_eq!(weekdays(3, 0), "Mon..Fri *-*-* 03:00");
    }

    #[test]
    fn parses_systemctl_show_and_analyze_output() {
        let s = parse_status("LoadState=loaded\nActiveState=active\nUnitFileState=enabled\nNextElapseUSecRealtime=Tue 2026-11-10 04:35:12 -03\nTimersCalendar={ OnCalendar=*-*-* 04:30 ; next_elapse=Tue 2026-11-10 04:30:00 -03 }\n");
        assert!(s.installed && s.enabled && s.active == "active");
        assert_eq!(s.calendars, ["*-*-* 04:30"]);
        assert_eq!(s.next.as_deref(), Some("Tue 2026-11-10 04:35:12 -03"));
        let n = parse_status("LoadState=not-found\nActiveState=inactive\nUnitFileState=\nNextElapseUSecRealtime=\nTimersCalendar=\n");
        assert!(!n.installed && !n.enabled && n.calendars.is_empty() && n.next.is_none());
        let c = parse_analyze("  Original form: daily\nNormalized form: *-*-* 00:00:00\n    Next elapse: Tue 2026-11-10 00:00:00 -03\n       From now: 9h left\n").unwrap();
        assert_eq!(c.normalized, "*-*-* 00:00:00");
        assert_eq!(c.next.as_deref(), Some("Tue 2026-11-10 00:00:00 -03"));
        assert!(parse_analyze("Failed to parse calendar specification").is_none());
    }

    #[test]
    fn embedded_units_match_install_sh_source() {
        assert!(TIMER_UNIT.contains("[Install]") && SERVICE_UNIT.contains("pxu update --yes"));
    }

    /// Valor de `key=` de la sección del servicio, en segundos (`20h`, `4h`, `30min`).
    fn unit_secs(unit: &str, key: &str) -> u64 {
        let v = unit.lines().find_map(|l| l.strip_prefix(&format!("{key}="))).unwrap_or_else(|| panic!("{key} missing"));
        let (n, mul) = if let Some(n) = v.strip_suffix("min") { (n, 60) } else if let Some(n) = v.strip_suffix('h') { (n, 3600) } else { panic!("unit {v}") };
        n.parse::<u64>().unwrap() * mul
    }

    #[test]
    fn service_timeout_is_a_finite_last_resort_above_the_internal_deadline_and_below_24h() {
        let start = unit_secs(SERVICE_UNIT, "TimeoutStartSec");
        assert!(start > crate::share::update::RUN_DEADLINE_S, "systemd must outlive pxu's own deadline");
        assert!(start < 24 * 3600, "a stuck run must never block the next day's");
        assert!(!SERVICE_UNIT.contains("infinity"));
        // un stop deja terminar apt/dpkg del paso en curso (3 h por defecto) antes del SIGKILL final
        assert!(unit_secs(SERVICE_UNIT, "TimeoutStopSec") >= 3 * 3600);
        assert!(SERVICE_UNIT.contains("KillMode=mixed"));
    }

    #[test]
    fn failure_notice_is_a_separate_unit_that_does_not_run_pxu() {
        assert!(SERVICE_UNIT.contains("OnFailure=pxu-update-failed.service"));
        assert!(FAILED_SERVICE_UNIT.contains("Type=oneshot") && FAILED_SERVICE_UNIT.contains("systemd-cat"));
        assert!(!FAILED_SERVICE_UNIT.contains("/usr/local/bin/pxu"), "the notice must not depend on pxu");
        let install = include_str!("../../install.sh");
        assert!(install.contains("pxu-update-failed.service"), "install.sh installs it with the other units");
    }

    #[test]
    fn check_plus_run_deadline_plus_margin_stay_under_the_service_timeout() {
        use crate::share::update::{check::CHECK_DEADLINE_S, notify_worst_s, RUN_DEADLINE_S, RUN_MARGIN_S};
        let start = unit_secs(SERVICE_UNIT, "TimeoutStartSec");
        assert!(CHECK_DEADLINE_S + RUN_DEADLINE_S + RUN_MARGIN_S < start, "check + update + margin must fit the 20 h of the two ExecStart");
        // con los máximos de la config: check entero (plazo) + las 16 h + todos los avisos (con reintentos) siguen por debajo de TimeoutStartSec
        assert!(notify_worst_s() <= RUN_MARGIN_S, "the notices ({} s) must fit the margin ({RUN_MARGIN_S} s)", notify_worst_s());
        assert!(CHECK_DEADLINE_S + RUN_DEADLINE_S + notify_worst_s() < start, "{} s vs {start} s", CHECK_DEADLINE_S + RUN_DEADLINE_S + notify_worst_s());
        assert!(start < 24 * 3600);
    }

    #[test]
    fn apprise_failure_notice_is_wired_without_pxu_and_without_secrets_in_argv() {
        assert!(SERVICE_UNIT.contains("OnFailure=pxu-update-failed.service pxu-update-notify-failed.service"), "the journal entry stays and the Apprise notice is added");
        let u = NOTIFY_FAILED_UNIT;
        assert!(u.contains("LoadCredentialEncrypted=pxu-apprise:/etc/pxu/failure-notice.cred"), "{u}");
        assert!(u.contains("ConditionPathExists=/etc/pxu/failure-notice.cred"), "without Apprise configured the unit is skipped");
        assert!(u.contains("ExecStart=/usr/local/bin/pxu-failure-notice") && !u.contains("/usr/local/bin/pxu ") && !u.contains("pxu update"), "does not run pxu");
        assert!(u.lines().filter(|l| l.starts_with("ExecStart")).all(|l| !l.contains("://") && !l.contains("apikey")), "no secrets in the unit");
        assert_eq!(FAILURE_NOTICE_PATH, "/usr/local/bin/pxu-failure-notice");
        assert!(FAILURE_NOTICE_SCRIPT.contains("--config -") && FAILURE_NOTICE_SCRIPT.contains("--config /dev/stdin"), "secrets go by stdin");
        assert!(include_str!("../../install.sh").contains("pxu-update-notify-failed.service") && include_str!("../../install.sh").contains("pxu-failure-notice"));
    }

    /// Ejecuta el script real con `curl`/`apprise` falsos que anotan argv y stdin.
    fn run_notice(cred: Option<&str>) -> (i32, String, String) {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("pxu-notice-{}-{}", std::process::id(), cred.map_or(0, |c| c.len())));
        let _ = std::fs::remove_dir_all(&d);
        let (bin, creds) = (d.join("bin"), d.join("creds"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&creds).unwrap();
        for t in ["curl", "apprise"] {
            let p = bin.join(t);
            std::fs::write(&p, format!("#!/bin/sh\necho \"{t} $*\" > \"{0}/argv\"\ncat > \"{0}/stdin\"\n", d.display())).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let script = d.join("pxu-failure-notice");
        std::fs::write(&script, FAILURE_NOTICE_SCRIPT).unwrap();
        if let Some(c) = cred {
            std::fs::write(creds.join("pxu-apprise"), c).unwrap();
        }
        let mut cmd = std::process::Command::new("sh");
        cmd.arg(&script).env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        if cred.is_some() {
            cmd.env("CREDENTIALS_DIRECTORY", &creds);
        } else {
            cmd.env_remove("CREDENTIALS_DIRECTORY");
        }
        let code = cmd.status().unwrap().code().unwrap();
        let read = |n: &str| std::fs::read_to_string(d.join(n)).unwrap_or_default();
        let r = (code, read("argv"), read("stdin"));
        let _ = std::fs::remove_dir_all(&d);
        r
    }

    #[test]
    fn failure_notice_script_sends_by_stdin_and_works_without_a_credential() {
        let (code, argv, stdin) = run_notice(Some("# pxu-failure-notice transport=api\nurl = \"http://a/notify/SECRETKEY\""));
        assert_eq!(code, 0);
        assert_eq!(argv.trim(), "curl --config -");
        assert!(stdin.contains("SECRETKEY") && !argv.contains("SECRETKEY"));
        let (code, argv, stdin) = run_notice(Some("# pxu-failure-notice transport=cli\ntgram://SECRETTOK/1"));
        assert_eq!(code, 0);
        assert!(argv.starts_with("apprise --config /dev/stdin") && argv.contains("-n failure") && !argv.contains("SECRETTOK"), "{argv}");
        assert!(stdin.contains("tgram://SECRETTOK/1"));
        assert_eq!(run_notice(None).0, 0, "no credential: nothing to send, not a failure");
        assert_ne!(run_notice(Some("garbage")).0, 0, "an unknown format is reported");
    }
}
