//! Timer systemd `pxu-update` (horario del update automático). Las unidades se embeben de `dist/systemd/`,
//! la misma fuente que usa `install.sh`. Todo contacto con systemd va por `Host` (`timer_status`,
//! `validate_calendar`, `apply_schedule`).

pub const UNIT_DIR: &str = "/etc/systemd/system";
pub const TIMER_NAME: &str = "pxu-update.timer";
pub const SERVICE_UNIT: &str = include_str!("../../dist/systemd/pxu-update.service");
pub const TIMER_UNIT: &str = include_str!("../../dist/systemd/pxu-update.timer");
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
}
