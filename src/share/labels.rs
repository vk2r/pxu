//! Etiquetas de texto de `check` que comparten la CLI y la TUI.
use super::update::check::{CtResult, Report, Status};

pub fn index_label(age_s: u64, known: bool) -> String {
    if !known {
        return "—".into();
    }
    match age_s {
        s if s >= 2 * 86400 => format!("{}d", s / 86400),
        s if s >= 3600 => format!("{}h", s / 3600),
        s => format!("{}m", s / 60),
    }
}

pub fn status_label(r: &CtResult) -> String {
    match r.status {
        Status::Ok if r.stale => "stale".into(),
        Status::Ok => "ok".into(),
        Status::Unsupported => "unsupported".into(),
        Status::Stopped => "skip: stopped".into(),
        Status::Locked => format!("skip: {}", r.detail),
        Status::Timeout => "timeout".into(),
        Status::Error => format!("error: {}", r.detail),
        Status::Cancelled => "cancelled".into(),
    }
}

pub fn summary_line(r: &Report) -> String {
    let s = r.summary();
    format!(
        "{} checked · {} skipped · {} with updates · {} packages ({} security) · {} stale index · {} reboot pending · {} errors",
        s.checked + s.unsupported, s.stopped, s.with_updates, s.packages, s.security, s.stale, s.reboot, s.errors
    )
}
