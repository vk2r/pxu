//! Aviso único de una corrida de update (título + tabla Markdown) y su resumen de una línea para la lista de logs.
//! Una sola fuente de verdad: las mismas filas alimentan el aviso, la cabecera del log y la página Logs de la TUI.
use super::nvidia::NvidiaRun;
use super::runlog::{local_hm, local_ymd_hm};
use super::{CtRun, Outcome, RunReport};
use crate::share::notify::{Level, Message};

const HEADER: &str = "| Upgrades | Result | Packages | GPU driver | Time | Details |";
const CELL_MAX: usize = 300;

#[derive(Debug, Clone)]
pub struct Row {
    pub outcome: Outcome,
    pub upgrades: String,
    pub result: String,
    pub packages: String,
    pub gpu: String,
    pub time: String,
    pub details: String,
    pub manual: String,
}

/// `18m 42s`.
pub fn dur(s: u64) -> String {
    format!("{}m {:02}s", s / 60, s % 60)
}

/// Texto que viene de un CT o del sistema: sin saltos de línea ni `|` (rompen la tabla) ni acentos graves (abren código), con tope.
fn cell(s: &str) -> String {
    let s = crate::share::fmt::clean(&s.replace(['\n', '\r'], " ")).replace('|', "/").replace('`', "'");
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::share::fmt::trunc(&s, CELL_MAX)
}

/// Nombre de unidad systemd seguro para ir dentro de un comando en código en línea.
fn unit(u: &str) -> String {
    u.chars().filter(|c| c.is_ascii_alphanumeric() || "@._:-\\".contains(*c)).collect()
}

fn result_text(o: Outcome) -> &'static str {
    match o {
        Outcome::Ok => "✅ ok",
        Outcome::Warn => "⚠️ warn",
        Outcome::Failed => "❌ failed",
        Outcome::Skipped => "⏭️ skipped",
        Outcome::Cancelled => "⏭️ cancelled",
    }
}

fn worst(a: Outcome, b: Outcome) -> Outcome {
    let rank = |o: Outcome| match o {
        Outcome::Failed => 3,
        Outcome::Warn => 2,
        Outcome::Ok => 1,
        Outcome::Skipped | Outcome::Cancelled => 0,
    };
    if rank(b) > rank(a) {
        b
    } else {
        a
    }
}

fn packages(r: &CtRun) -> String {
    match (r.outcome, r.packages_before, r.security) {
        (Outcome::Skipped | Outcome::Cancelled, ..) | (_, 0, _) => "—".into(),
        (_, n, 0) => n.to_string(),
        (_, n, s) => format!("{n} ({s} security)"),
    }
}

/// Partes de «Details» de un CT o del host: motivo, estado del rollback y avisos del update.
fn details(r: &CtRun) -> Vec<String> {
    let mut d: Vec<String> = Vec::new();
    match r.outcome {
        Outcome::Skipped | Outcome::Cancelled => d.push(r.detail.clone()),
        Outcome::Failed => d.push(format!("{}: {}", r.step.label(), r.detail)),
        Outcome::Ok | Outcome::Warn => {
            if r.reboot_required {
                d.push("reboot required".into());
            }
            // «N packages updated (M security)» ya está en Packages y «reboot required: …» arriba; el resto son avisos.
            for seg in r.detail.split(" · ") {
                let s = seg.trim();
                let n_updated = s.split_whitespace().next().is_some_and(|w| w.parse::<u32>().is_ok()) && s.contains(" packages updated");
                if !s.is_empty() && !n_updated && !s.starts_with("reboot required") {
                    d.push(s.to_string());
                }
            }
        }
    }
    match r.rollback.as_deref() {
        Some("done") => d.push("rollback: ✅ restored".into()),
        Some("failed") => d.push("rollback: ❌ FAILED, backup kept".into()),
        Some(_) => d.push("rollback: not attempted, backup kept".into()),
        None if r.outcome == Outcome::Failed && r.vmid != 0 && r.backup.is_none() => d.push("no backup was taken".into()),
        None => {}
    }
    d
}

fn gpu_host(n: &NvidiaRun) -> String {
    let icon = match n.outcome {
        Outcome::Ok => "✅",
        Outcome::Warn => "⚠️",
        Outcome::Failed => "❌",
        Outcome::Skipped | Outcome::Cancelled => return "—".into(),
    };
    match (&n.from, &n.to) {
        (Some(f), Some(t)) if f != t => format!("{icon} {f} → {t}"),
        (_, Some(t)) => format!("{icon} {t}"),
        _ => format!("{icon} driver"),
    }
}

fn gpu_details(n: &NvidiaRun) -> Option<String> {
    match n.outcome {
        // «host driver A → B; CT updated: ids»: lo de la versión ya está en su columna.
        Outcome::Ok => n.detail.split_once("; ").map(|(_, t)| t.to_string()).filter(|t| !t.starts_with("CT updated: none")),
        _ => Some(format!("NVIDIA: {}", n.detail)),
    }
}

fn gpu_ct(c: &super::nvidia::NvidiaCt, ver: &str) -> String {
    match c.outcome {
        Outcome::Ok => format!("✅ libs {ver}"),
        Outcome::Warn => format!("⚠️ libs {ver}"),
        Outcome::Failed => "❌ libs".into(),
        _ => "—".into(),
    }
}

const GPU_HINT: &str = "NVIDIA driver failed: check `dkms status` and `nvidia-smi` on the host (pxu does not roll the driver back; see the log)";

/// Filas de la tabla: el host primero (con su paso del driver NVIDIA), luego cada CT, también los fallidos y saltados.
pub fn rows(rep: &RunReport) -> Vec<Row> {
    let nv = rep.nvidia.as_ref();
    let mut out = Vec::new();
    let host_run = rep.results.iter().find(|r| r.vmid == 0);
    if host_run.is_some() || nv.is_some() {
        let mut outcome = host_run.map_or(nv.map_or(Outcome::Skipped, |n| n.outcome), |h| h.outcome);
        let mut d: Vec<String> = host_run.map(details).unwrap_or_default();
        let mut manual: Vec<String> = Vec::new();
        let mut secs = host_run.map_or(0, |h| h.took_s);
        let mut gpu = "—".to_string();
        if let Some(n) = nv {
            outcome = if host_run.is_some() { worst(outcome, n.outcome) } else { n.outcome };
            gpu = gpu_host(n);
            secs += n.took_s;
            d.extend(gpu_details(n));
            if n.outcome == Outcome::Failed {
                manual.push(GPU_HINT.into());
            }
            if n.reboot_needed {
                manual.push("reboot the host: nvidia-smi does not confirm the new driver".into());
            }
        }
        if let Some(h) = host_run {
            if h.outcome == Outcome::Failed {
                manual.push(format!("on the host: `{}`", super::hostupdate::RECOVERY));
            }
            if h.reboot_required {
                manual.push("reboot the host".into());
            }
        }
        let pk = host_run.map_or("—".into(), packages);
        out.push(Row { outcome, upgrades: "host".into(), result: result_text(outcome).into(), packages: pk, gpu, time: if secs == 0 { "—".into() } else { dur(secs) }, details: d.iter().map(|x| cell(x)).collect::<Vec<_>>().join(" · "), manual: manual.join(" · ") });
    }
    for r in rep.results.iter().filter(|r| r.vmid != 0) {
        let mut outcome = r.outcome;
        let mut d = details(r);
        let mut manual: Vec<String> = Vec::new();
        let mut gpu = "—".to_string();
        if let Some(c) = nv.and_then(|n| n.cts.iter().find(|c| c.vmid == r.vmid)) {
            gpu = gpu_ct(c, nv.and_then(|n| n.to.as_deref()).unwrap_or_default());
            if matches!(c.outcome, Outcome::Failed | Outcome::Warn) {
                d.push(format!("NVIDIA libs: {}", c.detail));
            }
            outcome = worst(outcome, c.outcome);
            if c.outcome == Outcome::Failed {
                manual.push(format!("NVIDIA libs failed in CT {}: see the log", r.vmid));
            }
        }
        // Rollback fallido o no intentado: el backup se conserva y el comando para volver atrás lo ejecuta VK.
        if let (Outcome::Failed, Some(rb)) = (r.outcome, r.rollback.as_deref()) {
            if rb != "done" {
                match r.revert.as_deref() {
                    Some(c) => manual.push(format!("`{}`", c.replace('`', "'"))),
                    None => manual.push(format!("restore CT {} from its backup: see the log", r.vmid)),
                }
            }
        }
        if r.outcome == Outcome::Warn {
            let us: Vec<String> = r.new_failed_units.iter().map(|u| unit(u)).filter(|u| !u.is_empty()).collect();
            manual.extend(us.iter().map(|u| format!("`systemctl status {u}` in CT {}", r.vmid)));
        }
        out.push(Row {
            outcome,
            upgrades: cell(&format!("{} {}", r.vmid, r.name)),
            result: result_text(outcome).into(),
            packages: packages(r),
            gpu,
            time: if matches!(r.outcome, Outcome::Skipped | Outcome::Cancelled) || r.took_s == 0 { "—".into() } else { dur(r.took_s) },
            details: d.iter().map(|x| cell(x)).collect::<Vec<_>>().join(" · "),
            manual: manual.join(" · "),
        });
    }
    // CT con librerías NVIDIA pero sin paquetes actualizados en esta corrida (p. ej. corrida solo del host): su propia fila.
    if let Some(n) = nv {
        let ver = n.to.as_deref().unwrap_or_default();
        for c in n.cts.iter().filter(|c| !rep.results.iter().any(|r| r.vmid == c.vmid)) {
            let manual = if c.outcome == Outcome::Failed { format!("NVIDIA libs failed in CT {}: see the log", c.vmid) } else { String::new() };
            out.push(Row { outcome: c.outcome, upgrades: cell(&format!("{} {}", c.vmid, c.name)), result: result_text(c.outcome).into(), packages: "—".into(), gpu: gpu_ct(c, ver), time: "—".into(), details: cell(&c.detail), manual });
        }
    }
    out
}

fn count(rows: &[Row], o: Outcome) -> usize {
    rows.iter().filter(|r| r.outcome == o).count()
}

fn nvidia_failed(rep: &RunReport) -> bool {
    rep.nvidia.as_ref().is_some_and(|n| n.outcome == Outcome::Failed || n.cts.iter().any(|c| c.outcome == Outcome::Failed))
}

pub fn level(rep: &RunReport) -> Level {
    let rows = rows(rep);
    if count(&rows, Outcome::Failed) > 0 || nvidia_failed(rep) || rep.aborted.is_some() {
        Level::Failure
    } else if count(&rows, Outcome::Warn) > 0 {
        Level::Warning
    } else {
        Level::Success
    }
}

fn total_s(rep: &RunReport) -> u64 {
    rep.finished_at.saturating_sub(rep.started_at)
}

/// `3 ok`, `1 warn`, `2 failed`, `1 cancelled`, `1 skipped`: solo los que no son 0 (si no queda ninguno, `0 ok`).
fn count_parts(rows: &[Row]) -> Vec<String> {
    let mut parts = Vec::new();
    for (o, w) in [(Outcome::Ok, "ok"), (Outcome::Warn, "warn"), (Outcome::Failed, "failed")] {
        if count(rows, o) > 0 {
            parts.push(format!("{} {w}", count(rows, o)));
        }
    }
    if count(rows, Outcome::Cancelled) > 0 {
        parts.push(format!("{} cancelled", count(rows, Outcome::Cancelled)));
    }
    if count(rows, Outcome::Skipped) > 0 {
        parts.push(format!("{} skipped", count(rows, Outcome::Skipped)));
    }
    if parts.is_empty() {
        parts.push("0 ok".into());
    }
    parts
}

/// `pxu: update run on <host> — 3 ok, 1 warn, 2 failed · 18m 42s` (+ `STOPPED`, + `NVIDIA failed`). Sin contadores en 0.
pub fn title(rep: &RunReport) -> String {
    let rows = rows(rep);
    let mut t = format!("pxu: update run on {} — ", rep.node);
    if rep.aborted.is_some() {
        t += "STOPPED · ";
    }
    let parts: Vec<String> = count_parts(&rows).into_iter().filter(|p| !p.ends_with("skipped")).collect();
    t += &format!("{} · {}", if parts.is_empty() { "0 ok".to_string() } else { parts.join(", ") }, dur(total_s(rep)));
    if rep.nvidia.as_ref().is_some_and(|n| n.outcome == Outcome::Failed) {
        t += " · NVIDIA failed";
    } else if nvidia_failed(rep) {
        t += " · NVIDIA libs failed"; // el host quedó bien: falló alguna librería de CT
    }
    t
}

/// Corrida en la que nada se actualizó ni se intentó (todo saltado, sin parar ni cancelar): se anota en el log pero no avisa.
pub fn nothing_to_do(rep: &RunReport) -> bool {
    let rows = rows(rep);
    rep.aborted.is_none() && !rep.cancelled && rep.nvidia.is_none() && rows.iter().all(|r| r.outcome == Outcome::Skipped)
}

/// Resumen corto para la lista de logs: `❌ 3 ok · 1 warn · 2 failed`, `⛔ STOPPED · 1 ok`. Sin contadores en 0.
pub fn result_line(rep: &RunReport) -> String {
    if nothing_to_do(rep) {
        return "✅ nothing to do".into();
    }
    let rows = rows(rep);
    let mut parts = count_parts(&rows);
    if rep.nvidia.as_ref().is_some_and(|n| n.outcome == Outcome::Failed) {
        parts.push("NVIDIA failed".into());
    } else if nvidia_failed(rep) {
        parts.push("NVIDIA libs failed".into());
    }
    let icon = if rep.aborted.is_some() {
        "⛔ STOPPED · "
    } else {
        match level(rep) {
            Level::Failure => "❌ ",
            Level::Warning => "⚠️ ",
            _ => "✅ ",
        }
    };
    format!("{icon}{}", parts.join(" · "))
}

/// Cuerpo Markdown: una sola tabla con todas las filas (sin tope; el destino parte el mensaje si hace falta, p. ej. `overflow=split`).
pub fn body(rep: &RunReport) -> String {
    let rows = rows(rep);
    let shown: Vec<&Row> = rows.iter().collect();
    let end = if local_ymd_hm(rep.finished_at)[..10] == local_ymd_hm(rep.started_at)[..10] { local_hm(rep.finished_at) } else { local_ymd_hm(rep.finished_at) };
    let mut b = format!("**Update run on {}** · {} → {} · **total {}**\n", rep.node, local_ymd_hm(rep.started_at), end, dur(total_s(rep)));
    if let Some(a) = &rep.aborted {
        b += &format!("\n⛔ Run stopped: {}\n", cell(a));
    }
    if shown.is_empty() && nothing_to_do(rep) {
        b += "\nNothing to do: no CT or host had pending updates.\n";
    } else {
        let with_manual = shown.iter().any(|r| !r.manual.is_empty());
        let (extra, sep) = if with_manual { (" Manual |", "---|") } else { ("", "") };
        b += &format!("\n{HEADER}{extra}\n|---|---|---|---|---|---|{sep}\n");
        for r in shown {
            b += &format!("| {} | {} | {} | {} | {} | {} |", r.upgrades, r.result, r.packages, r.gpu, r.time, r.details);
            if with_manual {
                b += &format!(" {} |", r.manual);
            }
            b += "\n";
        }
    }
    if rep.results.iter().any(|r| r.vmid == 0 && r.reboot_required) {
        b += "\n⚠️ The host needs a REBOOT (new kernel). pxu never reboots the host.\n";
    }
    if rep.nvidia.as_ref().is_some_and(|n| n.reboot_needed) {
        b += "\n⚠️ The host needs a REBOOT: nvidia-smi does not confirm the new NVIDIA driver. pxu never reboots the host.\n";
    }
    if let Some(p) = &rep.log_path {
        b += &format!("\n📄 Full report: {p}");
    }
    b
}

/// El aviso único de la corrida (evento `run_summary`; `update::messages` decide si se manda según `[apprise] on`).
pub fn message(rep: &RunReport) -> Message {
    Message { title: title(rep), body: body(rep), level: level(rep), event: "run_summary".into(), created: 0 }
}

#[cfg(test)]
mod tests {
    use super::super::nvidia::NvidiaCt;
    use super::super::Step;
    use super::*;

    fn ct(vmid: u32, name: &str, outcome: Outcome) -> CtRun {
        CtRun {
            vmid, name: name.into(), outcome, step: Step::Done, detail: String::new(), packages_before: 6, packages_after: Some(0), security: 2, reboot_required: false,
            new_failed_units: vec![], backup: None, rollback: None, revert: None, log_path: None, took_s: 62,
        }
    }
    fn rep(results: Vec<CtRun>) -> RunReport {
        RunReport { node: "server".into(), started_at: 1_794_000_000, finished_at: 1_794_001_122, results, aborted: None, cancelled: false, nvidia: None, log_path: Some("/var/log/pxu/run-20261110-0612.log".into()) }
    }
    fn nv(outcome: Outcome) -> NvidiaRun {
        NvidiaRun { outcome, from: Some("595.112.07".into()), to: Some("595.120.01".into()), detail: "host driver 595.112.07 → 595.120.01; CT updated: 129".into(), log_path: None, took_s: 120, reboot_needed: false, cts: vec![] }
    }
    fn row_of<'a>(b: &'a str, key: &str) -> &'a str {
        b.lines().find(|l| l.starts_with(&format!("| {key} "))).unwrap_or_else(|| panic!("no row {key}:\n{b}"))
    }

    #[test]
    fn all_ok_is_one_table_with_the_seven_columns_and_success_level() {
        let r = rep(vec![ct(113, "cap", Outcome::Ok), ct(114, "web", Outcome::Ok)]);
        let b = body(&r);
        assert!(!b.contains("## "), "no heading before the table: {b}");
        assert!(b.contains("**\n\n| Upgrades | Result | Packages | GPU driver | Time | Details |\n|---|---|---|---|---|---|\n"), "{b}");
        assert!(!b.contains("Manual"), "no Manual column when no row has a manual step: {b}");
        assert_eq!(b.matches("\n|---").count(), 1, "one table");
        assert_eq!(row_of(&b, "113 cap"), "| 113 cap | ✅ ok | 6 (2 security) | — | 1m 02s |  |");
        assert!(title(&r).starts_with("pxu: update run on server — 2 ok · 18m 42s"), "{}", title(&r));
        assert_eq!(level(&r), Level::Success);
        assert!(b.ends_with("📄 Full report: /var/log/pxu/run-20261110-0612.log"), "{b}");
        assert!(b.starts_with("**Update run on server** · "), "{b}");
        assert!(!b.contains("REBOOT"));
    }

    #[test]
    fn manual_column_appears_only_when_a_row_has_a_manual_step() {
        let mut f = ct(121, "dns", Outcome::Failed);
        f.rollback = Some("failed".into());
        let b = body(&rep(vec![ct(113, "cap", Outcome::Ok), f]));
        assert!(b.contains("| Details | Manual |\n|---|---|---|---|---|---|---|\n"), "{b}");
        assert!(row_of(&b, "113 cap").ends_with("|  |  |"), "every row gets the cell: {}", row_of(&b, "113 cap"));
        assert!(row_of(&b, "121 dns").contains("restore CT 121 from its backup"), "{b}");
        // un solo guest que falló y se restauró solo (el caso de forgejo): sin columna
        let mut g = ct(132, "forgejo", Outcome::Failed);
        g.rollback = Some("done".into());
        assert!(!body(&rep(vec![g])).contains("Manual"));
    }

    #[test]
    fn cancelled_is_labelled_cancelled_and_counted_apart_from_skipped() {
        let r = rep(vec![ct(113, "cap", Outcome::Ok), ct(114, "web", Outcome::Cancelled), ct(115, "db", Outcome::Skipped)]);
        let b = body(&r);
        assert!(row_of(&b, "114 web").contains("⏭️ cancelled"), "{b}");
        assert!(row_of(&b, "115 db").contains("⏭️ skipped"), "{b}");
        assert!(title(&r).contains("1 ok, 1 cancelled"), "{}", title(&r));
        assert!(result_line(&r).contains("1 cancelled · 1 skipped"), "{}", result_line(&r));
    }

    #[test]
    fn warn_row_names_the_unit_and_the_command_to_look_at_it() {
        let mut w = ct(134, "ct-134", Outcome::Warn);
        w.new_failed_units = vec!["cron.service".into()];
        w.detail = "new failed unit: cron.service".into();
        let r = rep(vec![w]);
        let b = body(&r);
        assert!(row_of(&b, "134 ct-134").contains("| ⚠️ warn |") && row_of(&b, "134 ct-134").ends_with("| `systemctl status cron.service` in CT 134 |"), "{b}");
        assert_eq!(level(&r), Level::Warning);
        assert!(title(&r).contains("1 warn"), "{}", title(&r));
    }

    #[test]
    fn failure_with_rollback_ok_has_no_manual_step() {
        let mut f = ct(121, "dns", Outcome::Failed);
        f.step = Step::Verify;
        f.detail = "bind9 failed".into();
        f.rollback = Some("done".into());
        f.backup = Some("vzdump (deleted)".into());
        let r = rep(vec![f]);
        let b = body(&r);
        let row = row_of(&b, "121 dns");
        assert!(row.contains("| ❌ failed |") && row.contains("rollback: ✅ restored") && row.ends_with("restored |"), "{row}");
        assert_eq!(level(&r), Level::Failure);
    }

    #[test]
    fn failure_with_rollback_failed_or_not_attempted_gives_the_pct_restore_command() {
        for rb in ["failed", "not_attempted"] {
            let mut f = ct(120, "media", Outcome::Failed);
            f.detail = "timed out after 1800 s".into();
            f.rollback = Some(rb.into());
            f.backup = Some("vzdump (kept)".into());
            f.revert = Some("pct restore 120 /var/lib/vz/dump/x.tar.zst --force".into());
            let b = body(&rep(vec![f]));
            let row = row_of(&b, "120 media");
            assert!(row.ends_with("| `pct restore 120 /var/lib/vz/dump/x.tar.zst --force` |"), "{rb}: {row}");
            assert!(row.contains(if rb == "failed" { "rollback: ❌ FAILED, backup kept" } else { "rollback: not attempted, backup kept" }), "{row}");
        }
    }

    #[test]
    fn skipped_ct_is_listed_with_its_reason() {
        let mut s = ct(122, "mail", Outcome::Skipped);
        s.detail = "no recent positive check".into();
        let b = body(&rep(vec![s]));
        assert_eq!(row_of(&b, "122 mail"), "| 122 mail | ⏭️ skipped | — | — | — | no recent positive check |");
    }

    #[test]
    fn host_row_goes_first_with_driver_time_sum_and_reboot_manual() {
        let mut h = ct(0, "host", Outcome::Ok);
        h.reboot_required = true;
        h.took_s = 550;
        h.detail = "7 packages updated (2 security) · reboot required: yes".into();
        let mut r = rep(vec![ct(113, "cap", Outcome::Ok), h]);
        r.nvidia = Some(nv(Outcome::Ok));
        let b = body(&r);
        let first = b.lines().find(|l| l.starts_with("| ") && !l.starts_with("| Upgrades")).unwrap();
        assert_eq!(first, "| host | ✅ ok | 6 (2 security) | ✅ 595.112.07 → 595.120.01 | 11m 10s | reboot required · CT updated: 129 | reboot the host |");
        assert!(b.contains("⚠️ The host needs a REBOOT"), "{b}");
    }

    #[test]
    fn host_only_run_gives_each_libs_ct_its_own_row() {
        let mut h = ct(0, "host", Outcome::Ok);
        h.took_s = 100;
        let mut r = rep(vec![h]);
        let mut n = nv(Outcome::Ok);
        n.to = Some("595.112.07".into());
        n.cts = vec![
            NvidiaCt { vmid: 129, name: "llama-swap".into(), outcome: Outcome::Ok, detail: "libs 595.112.07 installed, CT restarted".into() },
            NvidiaCt { vmid: 130, name: "bad".into(), outcome: Outcome::Failed, detail: "nvidia-smi failed".into() },
            NvidiaCt { vmid: 131, name: "wait".into(), outcome: Outcome::Skipped, detail: "libs not installed: reboot the host first".into() },
        ];
        r.nvidia = Some(n);
        let b = body(&r);
        assert!(row_of(&b, "129 llama-swap").contains("| ✅ ok | — | ✅ libs 595.112.07 | — | libs 595.112.07 installed, CT restarted |"), "{b}");
        assert!(row_of(&b, "130 bad").contains("| ❌ failed | — | ❌ libs | — | nvidia-smi failed |") && row_of(&b, "130 bad").contains("NVIDIA libs failed in CT 130"), "{b}");
        assert!(row_of(&b, "131 wait").contains("| ⏭️ skipped | — | — | — | libs not installed: reboot the host first |"), "{b}");
        assert!(row_of(&b, "host").contains("CT updated: 129"), "{b}");
    }

    #[test]
    fn ct_with_package_row_is_not_duplicated_by_its_libs_row() {
        let mut r = rep(vec![ct(129, "llama-swap", Outcome::Ok)]);
        let mut n = nv(Outcome::Ok);
        n.cts = vec![NvidiaCt { vmid: 129, name: "llama-swap".into(), outcome: Outcome::Ok, detail: "libs installed".into() }];
        r.nvidia = Some(n);
        assert_eq!(body(&r).matches("| 129 llama-swap |").count(), 1);
    }

    #[test]
    fn driver_failure_counts_as_a_failed_host_and_flags_the_title() {
        let mut r = rep(vec![ct(0, "host", Outcome::Ok), ct(113, "cap", Outcome::Ok)]);
        let mut n = nv(Outcome::Failed);
        n.detail = "dkms build failed".into();
        n.cts = vec![NvidiaCt { vmid: 113, name: "cap".into(), outcome: Outcome::Skipped, detail: "no driver".into() }];
        r.nvidia = Some(n);
        let b = body(&r);
        assert!(title(&r).ends_with("· NVIDIA failed") && title(&r).contains("1 ok, 1 failed"), "{}", title(&r));
        assert!(row_of(&b, "host").contains("| ❌ failed |") && row_of(&b, "host").contains("❌ 595.112.07 → 595.120.01") && row_of(&b, "host").contains("dkms status"), "{b}");
        assert_eq!(level(&r), Level::Failure);
    }

    #[test]
    fn stopped_run_says_so_in_title_and_body() {
        let mut r = rep(vec![ct(1, "a", Outcome::Failed), ct(2, "b", Outcome::Skipped)]);
        r.aborted = Some("stopped after 1 consecutive failure".into());
        assert!(title(&r).contains("— STOPPED · 1 failed"), "{}", title(&r));
        assert!(body(&r).contains("⛔ Run stopped: stopped after 1 consecutive failure"));
        assert!(result_line(&r).starts_with("⛔ STOPPED"), "{}", result_line(&r));
    }

    #[test]
    fn every_row_is_shown_with_no_cap() {
        let mut v: Vec<CtRun> = (100..130).map(|i| ct(i, "ok", Outcome::Ok)).collect();
        v.push(ct(900, "bad", Outcome::Failed));
        v.push(ct(901, "warn", Outcome::Warn));
        let b = body(&rep(v));
        let rows = b.lines().filter(|l| l.starts_with("| ") && !l.starts_with("| Upgrades")).count();
        assert_eq!(rows, 32, "{b}");
        assert!(b.contains("| 900 bad |") && b.contains("| 901 warn |"), "{b}");
        assert!(!b.contains("more ok"), "{b}");
    }

    #[test]
    fn cells_never_break_the_table() {
        let mut f = ct(5, "x|y\nz", Outcome::Failed);
        f.detail = "bad | thing\nsecond line `x`".into();
        let b = body(&rep(vec![f]));
        let row = b.lines().find(|l| l.contains("| 5 x")).unwrap();
        assert_eq!(row.matches('|').count(), 7, "{row}");
        assert!(!row.contains('`') || row.matches('`').count() % 2 == 0);
    }

    #[test]
    fn notice_routing_follows_the_apprise_on_list() {
        use crate::share::notify::AppriseCfg;
        let ok = rep(vec![ct(1, "a", Outcome::Ok)]);
        let bad = rep(vec![ct(1, "a", Outcome::Failed)]);
        let cfg = |on: &[&str]| AppriseCfg { on: on.iter().map(|s| s.to_string()).collect(), ..AppriseCfg::default() };
        assert_eq!(super::super::messages(&ok, &cfg(&["run_summary"])).len(), 1);
        assert!(super::super::messages(&ok, &cfg(&["update_failed"])).is_empty(), "failures only");
        let m = super::super::messages(&bad, &cfg(&["update_failed"]));
        assert!(m.len() == 1 && m[0].event == "update_failed");
        assert_eq!(super::super::messages(&bad, &cfg(&["run_summary", "update_failed"])).len(), 1, "never two notices");
        assert!(super::super::messages(&ok, &cfg(&[])).is_empty());
        assert!(super::super::messages(&rep(vec![]), &cfg(&["run_summary"])).is_empty());
    }
}
