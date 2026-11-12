//! Chequeo de updates (ARCHITECTURE §2): un solo exec por CT con el script `probe.sh` embebido.
use crate::share::config::Config;
use crate::share::proxmox::guests::Inventory;
use crate::share::host::{ExecOpts, Host, LineSink};
use crate::share::model::{Guest, GuestKind, RunState};
use crate::share::proxmox::hostcheck::{self, HostInfo};
use crate::share::state::{self, CheckInfo, Pkg};
use serde::Serialize;
use serde_json::json;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

pub const PROBE_SH: &str = include_str!("../proxmox/probe.sh");

/// Códigos de salida de `pxu check` (estables; ARCHITECTURE §6).
pub const EXIT_OK: i32 = 0;
pub const EXIT_UPDATES: i32 = 10;
/// Algún CT no pudo sondearse (timeout, sh ausente, fallo): el resultado está incompleto.
pub const EXIT_CHECK_ERRORS: i32 = 12;
pub const EXIT_USAGE: i32 = 2;

/// Salida del script ya interpretada.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probe {
    pub os_id: String,
    pub os_ver: String,
    pub pm: String,
    pub cache_age: Option<u64>,
    pub refresh: String,
    pub pkgs: Vec<Pkg>,
    pub held: Vec<String>,
    pub reboot: bool,
    /// `None` ⇒ no hubo `@@end`: el sondeo no terminó.
    pub end: Option<i32>,
}

impl Probe {
    pub fn os_label(&self) -> String {
        if self.os_ver.is_empty() || self.os_ver == "0" {
            self.os_id.clone()
        } else {
            format!("{} {}", self.os_id, self.os_ver)
        }
    }
}

/// Parser tolerante: ignora cualquier línea sin prefijo `@@` (ruido de motd, warnings…).
pub fn parse_probe(lines: &[String]) -> Probe {
    let mut p = Probe::default();
    for l in lines {
        let l = crate::share::fmt::clean(l);
        let Some(rest) = l.trim_end().strip_prefix("@@") else { continue };
        let (tag, val) = rest.split_once(' ').unwrap_or((rest, ""));
        let f: Vec<&str> = val.split_whitespace().collect();
        match tag {
            "os" => {
                p.os_id = f.first().unwrap_or(&"").to_string();
                p.os_ver = f.get(1).unwrap_or(&"").to_string();
            }
            "pm" => p.pm = val.trim().to_string(),
            "cache_age" => p.cache_age = val.trim().parse::<i64>().ok().filter(|v| *v >= 0).map(|v| v as u64),
            "refresh" => p.refresh = val.trim().to_string(),
            "pkg" if f.len() >= 4 => p.pkgs.push(Pkg { name: f[0].into(), cur: f[1].into(), new: f[2].into(), sec: f[3] == "1" }),
            "held" if !f.is_empty() => p.held.push(f[0].into()),
            "reboot_required" => p.reboot = val.trim() == "1",
            "end" => p.end = Some(val.trim().parse().unwrap_or(-1)),
            _ => {}
        }
    }
    p
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    /// Gestor de paquetes no soportado (`@@pm none`): se lista, no se toca.
    Unsupported,
    Stopped,
    Locked,
    Timeout,
    Error,
    /// Interrumpido por el usuario antes de sondear.
    Cancelled,
}

impl Status {
    pub fn is_error(self) -> bool {
        matches!(self, Status::Timeout | Status::Error)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CtResult {
    pub vmid: u32,
    pub name: String,
    pub status: Status,
    pub detail: String,
    /// Índice de paquetes más viejo que `index_stale_days`.
    pub stale: bool,
    /// `skipped` | `ok` | `fresh` | `failed:N` | `unavailable`
    pub refresh: String,
    pub info: Option<CheckInfo>,
}

impl CtResult {
    pub fn updates(&self) -> usize {
        self.info.as_ref().map_or(0, |i| i.packages.len())
    }
    pub fn security(&self) -> usize {
        self.info.as_ref().map_or(0, |i| i.packages.iter().filter(|p| p.sec).count())
    }
    fn bare(g: &Guest, status: Status, detail: &str) -> CtResult {
        CtResult { vmid: g.vmid, name: g.name.clone(), status, detail: crate::share::fmt::clean(detail), stale: false, refresh: "skipped".into(), info: None }
    }
}

#[derive(Debug, Clone)]
pub struct CheckOpts {
    pub refresh: bool,
    /// Chequeo estrictamente solo lectura (`--no-refresh`, tecla `C`): ignora también `[[ct]] refresh = true`.
    pub read_only: bool,
    pub refresh_min_age_s: u64,
    pub timeout_s: u64,
    pub nice: i32,
    pub stale_after_s: u64,
}

impl CheckOpts {
    /// `refresh`: `Some(true)` fuerza el refresco, `Some(false)` fuerza solo lectura, `None` lo decide `[check] refresh`.
    pub fn from_config(cfg: &Config, refresh: Option<bool>) -> CheckOpts {
        CheckOpts {
            refresh: refresh.unwrap_or(cfg.check.refresh),
            read_only: refresh == Some(false),
            refresh_min_age_s: cfg.check.refresh_min_age_hours * 3600,
            timeout_s: cfg.check.timeout_s,
            nice: cfg.general.nice,
            stale_after_s: cfg.check.index_stale_days * 86400,
        }
    }
}

/// CTs que `check` recorre: LXC del nodo local que no son plantilla (VMs y remotos se listan, no se chequean).
pub fn targets<'a>(inv: &'a Inventory, only: &[u32]) -> Vec<&'a Guest> {
    inv.guests
        .iter()
        .filter(|g| g.kind == GuestKind::Lxc && g.state != RunState::Remote && !g.template)
        .filter(|g| only.is_empty() || only.contains(&g.vmid))
        .collect()
}

struct Collect(Vec<String>);
impl LineSink for Collect {
    fn line(&mut self, l: &str) {
        self.0.push(crate::share::fmt::clean(l));
    }
}

/// Sondea un CT. Un único `guest_exec`; nunca más.
pub fn check_one(host: &dyn Host, g: &Guest, cfg: &Config, o: &CheckOpts) -> CtResult {
    match g.state {
        RunState::Running => {}
        _ => return CtResult::bare(g, Status::Stopped, "stopped: not started to check"),
    }
    if let Some(l) = &g.lock {
        return CtResult::bare(g, Status::Locked, &format!("locked: {l}"));
    }
    let refresh = if o.read_only { false } else { cfg.ct.iter().find(|c| c.vmid == g.vmid).and_then(|c| c.refresh).unwrap_or(o.refresh) };
    let (r, age) = (if refresh { "1" } else { "0" }, o.refresh_min_age_s.to_string());
    let mut argv = vec!["sh", "-c", PROBE_SH, "pxu-probe", r];
    if refresh {
        argv.push(&age);
    }
    let opts = ExecOpts { nice: Some(o.nice), timeout_s: Some(if refresh { o.timeout_s * 3 } else { o.timeout_s }), ..Default::default() };
    let mut out = Collect(Vec::new());
    let exit = match host.guest_exec(g.vmid, &argv, &opts, &mut out) {
        Ok(e) => e,
        Err(e) if e.kind() == ErrorKind::TimedOut => return CtResult::bare(g, Status::Timeout, &e.to_string()),
        Err(e) => return CtResult::bare(g, Status::Error, &e.to_string()),
    };
    let p = parse_probe(&out.0);
    let Some(end) = p.end else {
        let tail = out.0.last().map(|l| l.trim().chars().take(80).collect::<String>()).unwrap_or_default();
        return CtResult::bare(g, Status::Error, &format!("probe did not finish (exit {}){}", exit.code, if tail.is_empty() { String::new() } else { format!(": {tail}") }));
    };
    if end != 0 {
        return CtResult::bare(g, Status::Error, &format!("{} query failed (rc {end})", p.pm));
    }
    let now = host.now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut res = CtResult::bare(g, Status::Ok, "");
    res.refresh = p.refresh.clone();
    if p.pm == "none" || p.pm.is_empty() {
        res.status = Status::Unsupported;
        res.detail = format!("no supported package manager ({})", p.os_label());
        return res;
    }
    res.stale = p.cache_age.is_some_and(|a| a > o.stale_after_s);
    let fp = state::fingerprint(&p.pkgs);
    res.info = Some(CheckInfo {
        checked_at: now,
        pm: p.pm.clone(),
        os: p.os_label(),
        index_age_s: p.cache_age.unwrap_or(0),
        reboot_required: p.reboot,
        packages: p.pkgs,
        held: p.held,
        fingerprint: fp,
    });
    res
}

#[derive(Debug, Clone)]
pub struct Report {
    pub started_at: u64,
    pub finished_at: u64,
    pub refresh: bool,
    pub results: Vec<CtResult>,
    /// Revisión del host (solo si se recorren todos los CT y el host es apt). Un fallo aquí NO afecta a `results`.
    pub host: Option<HostInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    pub total: usize,
    pub checked: usize,
    pub stopped: usize,
    pub with_updates: usize,
    pub packages: usize,
    pub security: usize,
    pub stale: usize,
    pub reboot: usize,
    pub errors: usize,
    pub unsupported: usize,
}

impl Report {
    pub fn summary(&self) -> Summary {
        let mut s = Summary { total: self.results.len(), ..Default::default() };
        for r in &self.results {
            match r.status {
                Status::Ok => s.checked += 1,
                Status::Unsupported => s.unsupported += 1,
                Status::Stopped | Status::Locked | Status::Cancelled => s.stopped += 1,
                Status::Timeout | Status::Error => s.errors += 1,
            }
            if r.updates() > 0 {
                s.with_updates += 1;
                s.packages += r.updates();
                s.security += r.security();
            }
            s.stale += r.stale as usize;
            s.reboot += r.info.as_ref().is_some_and(|i| i.reboot_required) as usize;
        }
        s
    }

    pub fn exit_code(&self) -> i32 {
        let s = self.summary();
        if s.errors > 0 {
            EXIT_CHECK_ERRORS
        } else if s.with_updates > 0 {
            EXIT_UPDATES
        } else {
            EXIT_OK
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let s = self.summary();
        json!({
            "schema": 1,
            "started_at": self.started_at,
            "took_s": self.finished_at.saturating_sub(self.started_at),
            "refresh": self.refresh,
            "exit_code": self.exit_code(),
            "summary": {
                "total": s.total, "checked": s.checked, "stopped": s.stopped, "unsupported": s.unsupported,
                "with_updates": s.with_updates, "packages": s.packages, "security": s.security,
                "stale": s.stale, "reboot_required": s.reboot, "errors": s.errors,
            },
            "cts": self.results,
            "host": self.host,
        })
    }
}

/// Plazo total de `pxu check`: pasado, los CT que faltan quedan sin sondear (`Timeout`) y se omite el host. Con `update::RUN_DEADLINE_S` y
/// `update::RUN_MARGIN_S` debe quedar por debajo de `TimeoutStartSec` de `pxu-update.service` (las dos `ExecStart` comparten ese tiempo).
pub const CHECK_DEADLINE_S: u64 = 3600;

/// Peor caso de la sonda de un CT: con refresco tiene `3 × timeout_s`, más la cola de SIGTERM/SIGKILL.
fn probe_worst_s(o: &CheckOpts) -> u64 {
    3 * o.timeout_s + 30
}

/// Recorre los CTs en orden, uno a uno. `progress` se llama tras cada CT; `cancel` se mira entre CTs
/// ("terminar el CT en curso y parar", ARCHITECTURE §3.2): los restantes quedan `cancelled`.
pub fn run(host: &dyn Host, inv: &Inventory, cfg: &Config, o: &CheckOpts, only: &[u32], cancel: &AtomicBool, progress: &mut dyn FnMut(&CtResult)) -> Report {
    let now = |h: &dyn Host| h.now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let started_at = now(host);
    let mut results = Vec::new();
    for g in targets(inv, only) {
        // una sonda solo empieza si su peor caso cabe en lo que queda del plazo
        let over = now(host).saturating_sub(started_at) + probe_worst_s(o) > CHECK_DEADLINE_S;
        let r = if cancel.load(Ordering::SeqCst) {
            CtResult::bare(g, Status::Cancelled, "cancelled")
        } else if over {
            CtResult::bare(g, Status::Timeout, "not checked: check time limit")
        } else {
            check_one(host, g, cfg, o)
        };
        progress(&r);
        results.push(r);
    }
    // El host se revisa después de los CT y de forma aislada: su fallo queda en `host.error`, no cambia los CT.
    let host_info = if only.is_empty() && !cancel.load(Ordering::SeqCst) && now(host).saturating_sub(started_at) + hostcheck::worst_s(o.timeout_s, inv.guests.iter().filter(|g| g.gpu.nvidia).count() as u64) <= CHECK_DEADLINE_S { hostcheck::check_host(host, inv, o) } else { None };
    Report { started_at, finished_at: now(host), refresh: o.refresh, results, host: host_info }
}

/// Guarda el resultado en `state.json` (los CT sin resultado nuevo conservan el anterior).
pub fn persist(host: &dyn Host, report: &Report) -> std::io::Result<()> {
    let mut st = state::load_state(host);
    for r in &report.results {
        if let (Status::Ok, Some(info)) = (r.status, &r.info) {
            st.checks.insert(r.vmid.to_string(), info.clone());
        }
    }
    if let Some(h) = &report.host {
        st.host = Some(h.clone());
    }
    state::save_state(host, &st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_cannot_carry_escape_sequences_into_the_ui() {
        let p = parse_probe(&["@@os debian\u{1b}]0;pwned\u{7} 12".into(), "@@pm ap\u{1b}[2Jt".into()]);
        assert!(!p.os_label().contains('\u{1b}') && !p.os_label().contains('\u{7}'), "{:?}", p.os_label());
        assert!(!p.pm.contains('\u{1b}'));
    }
    use crate::share::proxmox::guests;
    use crate::share::host::ReadOnlyHost;
    use crate::share::mock::{MockHost, Recorded};

    use crate::share::mock::scenario::World;

    /// `basic-37` = servidor poblado (37 guests); `distros` = un CT por gestor de paquetes; `failures` = un CT por cada tipo de falla.
    fn fx(name: &str) -> ReadOnlyHost<MockHost> {
        let w = match name {
            "basic-37" => World::demo(),
            _ => World::preset(name).unwrap(),
        };
        ReadOnlyHost::new(w.build())
    }

    fn run_all(h: &dyn Host, only: &[u32]) -> Report {
        let inv = guests::load(h);
        let cfg = Config::load(h).unwrap();
        run(h, &inv, &cfg, &CheckOpts::from_config(&cfg, Some(false)), only, &AtomicBool::new(false), &mut |_| {})
    }

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn parser_ignores_noise_and_requires_end() {
        let p = parse_probe(&lines("Welcome to Debian\n@@v 1\n@@os debian 12 \"\"\n@@pm apt\n@@cache_age 81234\n@@pkg curl 8.5-1 8.6-1 0\n@@pkg openssl 3.0.15 3.0.16 1\n@@held docker-ce\n@@reboot_required 1\n@@end 0\n"));
        assert_eq!((p.pm.as_str(), p.cache_age, p.pkgs.len(), p.reboot, p.end), ("apt", Some(81234), 2, true, Some(0)));
        assert!(p.pkgs[1].sec && !p.pkgs[0].sec);
        assert_eq!(p.held, ["docker-ce"]);
        assert_eq!(p.os_label(), "debian 12");
        let cut = parse_probe(&lines("@@v 1\n@@pm apt\n@@pkg curl a b 0\n"));
        assert_eq!(cut.end, None, "sin @@end el sondeo no cuenta");
        assert_eq!(parse_probe(&lines("@@cache_age -1")).cache_age, None);
    }

    #[test]
    fn basic_37_reports_13_cts_with_updates_in_one_exec_each() {
        let h = fx("basic-37");
        let r = run_all(&h, &[]);
        let s = r.summary();
        assert_eq!(s.with_updates, 13);
        assert_eq!(s.checked + s.stopped, 37);
        assert_eq!(s.stopped, 3);
        assert_eq!(s.errors, 0);
        assert_eq!(r.exit_code(), EXIT_UPDATES);
        let rec = h.inner().recorded();
        let guest: Vec<_> = rec.iter().filter(|c| matches!(c, Recorded::Guest(..))).collect();
        assert_eq!(guest.len(), 34, "exactly one guest_exec per running CT");
        assert!(guest.iter().all(|c| matches!(c, Recorded::Guest(_, a) if a[0] == "sh" && a[4] == "0" && a.len() == 5)), "no refresh anywhere");
        assert!(rec.iter().all(|c| !matches!(c, Recorded::Host(a) if a[..2] == ["apt-get", "update"])), "no apt-get update without refresh");
    }

    #[test]
    fn selected_vmids_only() {
        let h = fx("basic-37");
        let r = run_all(&h, &[103, 101]);
        assert_eq!(r.results.iter().map(|x| x.vmid).collect::<Vec<_>>(), [101, 103]);
        assert_eq!(h.inner().recorded().len(), 2);
        assert_eq!(r.results[1].updates(), 7);
        assert_eq!(r.results[1].security(), 2);
    }

    #[test]
    fn distros_cover_every_package_manager() {
        let h = fx("distros");
        let r = run_all(&h, &[]);
        let pm: Vec<(u32, &str)> = r.results.iter().map(|x| (x.vmid, x.info.as_ref().map_or("-", |i| i.pm.as_str()))).collect();
        for want in ["apt", "apk", "pacman", "dnf", "zypper"] {
            assert!(pm.iter().any(|(_, p)| *p == want), "missing {want}: {pm:?}");
        }
        assert!(r.results.iter().any(|x| x.status == Status::Unsupported), "NixOS-like CT is unsupported");
        assert!(r.results.iter().any(|x| x.info.as_ref().is_some_and(|i| i.reboot_required)));
        assert!(r.results.iter().any(|x| x.stale));
        assert_eq!(r.summary().errors, 0);
    }

    #[test]
    fn failures_scenario_gives_stable_statuses() {
        let h = fx("failures");
        let r = run_all(&h, &[]);
        let st = |id: u32| r.results.iter().find(|x| x.vmid == id).map(|x| x.status);
        assert_eq!(st(107), Some(Status::Timeout));
        assert_eq!(st(108), Some(Status::Locked));
        assert!(r.results.iter().any(|x| x.status == Status::Stopped));
        assert!(r.results.iter().any(|x| x.status == Status::Error && x.detail.contains("did not finish")), "{:?}", r.results.iter().map(|x| (x.vmid, x.status, &x.detail)).collect::<Vec<_>>());
        assert_eq!(r.exit_code(), EXIT_CHECK_ERRORS);
    }

    #[test]
    fn cancel_stops_between_cts() {
        let h = fx("basic-37");
        let inv = guests::load(&h);
        let cfg = Config::default();
        let cancel = AtomicBool::new(false);
        let mut n = 0;
        let r = run(&h, &inv, &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[], &cancel, &mut |_| {
            n += 1;
            if n == 5 {
                cancel.store(true, Ordering::SeqCst);
            }
        });
        assert_eq!(r.results.iter().filter(|x| x.status == Status::Cancelled).count(), r.results.len() - 5);
        assert_eq!(h.inner().recorded().len(), 5.min(r.results.iter().filter(|x| x.status != Status::Cancelled && x.status != Status::Stopped).count()));
    }

    #[test]
    fn persist_updates_state_and_keeps_others() {
        let h = fx("basic-37");
        let r = run_all(&h, &[103]);
        persist(&h, &r).unwrap();
        let st = state::load_state(&h);
        assert_eq!(st.checks["103"].packages.len(), 7);
        assert!(st.checks.len() > 5, "other CTs keep their previous result");
        assert!(!st.checks["103"].fingerprint.is_empty());
    }

    #[test]
    fn json_report_is_versioned() {
        let h = fx("basic-37");
        let j = run_all(&h, &[]).to_json();
        assert_eq!(j["schema"], 1);
        assert_eq!(j["summary"]["with_updates"], 13);
        assert_eq!(j["exit_code"], 10);
        assert_eq!(j["cts"].as_array().unwrap().len(), 37);
    }

    // ---- probe.sh ejecutado de verdad con gestores falsos (PATH hermético) ----

    /// Ejecuta el script con un PATH que solo contiene awk/date/stat y los stubs dados.
    fn run_probe(tag: &str, stubs: &[(&str, &str)], refresh: &str) -> (Probe, Vec<String>) {
        let (p, calls, _) = run_probe_root(tag, stubs, refresh, None);
        (p, calls)
    }

    /// Como `run_probe`, con raíz falsa (`PXU_PROBE_ROOT`): `seed` = edad en segundos del stamp de apt a crear antes;
    /// devuelve además la edad del stamp tras la ejecución (None si no existe).
    fn run_probe_root(tag: &str, stubs: &[(&str, &str)], refresh: &str, seed: Option<u64>) -> (Probe, Vec<String>, Option<u64>) {
        let dir = std::env::temp_dir().join(format!("pxu-probe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for tool in ["awk", "date", "stat", "sh", "cat", "mkdir", "touch"] {
            let p = ["/usr/bin", "/bin"].iter().map(|d| std::path::PathBuf::from(d).join(tool)).find(|p| p.exists()).expect(tool);
            std::os::unix::fs::symlink(p, dir.join(tool)).unwrap();
        }
        let marks = dir.join("marks");
        for (name, body) in stubs {
            let f = dir.join(name);
            std::fs::write(&f, format!("#!/bin/sh\necho \"$0 $*\" >> '{}'\n{body}\n", marks.display())).unwrap();
            std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        let root = dir.join("root");
        let stamp = root.join("var/lib/apt/periodic/update-success-stamp");
        if let Some(age) = seed {
            std::fs::create_dir_all(stamp.parent().unwrap()).unwrap();
            std::fs::write(&stamp, "").unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
            std::fs::File::options().write(true).open(&stamp).unwrap().set_modified(t).unwrap();
        }
        let out = std::process::Command::new(dir.join("sh")).args(["-c", PROBE_SH, "pxu-probe", refresh, "0"]).env_clear().env("PATH", &dir).env("PXU_PROBE_ROOT", &root).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let p = parse_probe(&text.lines().map(String::from).collect::<Vec<_>>());
        let calls = std::fs::read_to_string(&marks).unwrap_or_default().lines().map(|l| l.rsplit('/').next().unwrap().to_string()).collect();
        let stamp_age = std::fs::metadata(&stamp).ok().and_then(|m| m.modified().ok()).map(|t| t.elapsed().map(|d| d.as_secs()).unwrap_or(0));
        let _ = std::fs::remove_dir_all(&dir);
        (p, calls, stamp_age)
    }

    #[test]
    fn probe_sh_apt_read_only_and_security_marks() {
        let apt = "case \"$*\" in *dist-upgrade*) cat <<'EOT'\nInst libssl3 [3.0.15-1] (3.0.16-1 Debian-Security:12/stable-security [amd64])\nInst curl [8.5.0-2] (8.6.0-1 Debian:12.7/stable [amd64]) []\nInst newdep (1.0 Debian:12/stable [amd64])\nConf curl (8.6.0-1 Debian:12 [amd64])\nEOT\n;; esac";
        let (p, calls) = run_probe("apt", &[("apt-get", apt), ("apt-mark", "echo docker-ce")], "0");
        assert_eq!(p.pm, "apt");
        assert_eq!(p.end, Some(0));
        assert_eq!(p.pkgs.len(), 3, "{p:?}");
        assert_eq!((p.pkgs[0].name.as_str(), p.pkgs[0].cur.as_str(), p.pkgs[0].new.as_str(), p.pkgs[0].sec), ("libssl3", "3.0.15-1", "3.0.16-1", true));
        assert!(!p.pkgs[1].sec);
        assert_eq!((p.pkgs[2].cur.as_str(), p.pkgs[2].new.as_str()), ("-", "1.0"), "new dependency has no current version");
        assert_eq!(p.held, ["docker-ce"]);
        assert_eq!(p.refresh, "skipped");
        // solo se llamó a `apt-get -s … dist-upgrade` y `apt-mark showhold`; ninguna escritura
        assert!(calls.iter().all(|c| c.contains("-s -o Debug::NoLocking=1 dist-upgrade") || c.starts_with("apt-mark showhold")), "{calls:?}");
        assert!(!calls.iter().any(|c| c.contains("update")), "{calls:?}");
    }

    #[test]
    fn probe_sh_apk_pacman_dnf_zypper_and_none() {
        let (p, calls) = run_probe("apk", &[("apk", "echo 'curl-8.5.0-r0 x86_64 {curl} (MIT) [upgradable from: curl-8.4.0-r1]'")], "0");
        assert_eq!((p.pm.as_str(), p.pkgs.len()), ("apk", 1));
        assert_eq!((p.pkgs[0].name.as_str(), p.pkgs[0].cur.as_str(), p.pkgs[0].new.as_str()), ("curl", "8.4.0-r1", "8.5.0-r0"));
        assert_eq!(calls, ["apk --no-network list -u"]);

        let (p, calls) = run_probe("pacman", &[("pacman", "echo 'linux 6.10.1-1 -> 6.10.2-1'; exit 1")], "0");
        assert_eq!((p.pm.as_str(), p.pkgs.len(), p.end), ("pacman", 1, Some(0)));
        assert_eq!(p.pkgs[0].new, "6.10.2-1");
        assert_eq!(calls, ["pacman -Qu"], "never -Sy");

        let (p, calls) = run_probe("dnf", &[("dnf", "printf 'openssl-libs.x86_64   3.2.2-2   updates\\nbash.x86_64  5.2-2  baseos\\n\\nObsoleting Packages\\nold.x86_64 1 x\\n'; exit 100")], "0");
        assert_eq!((p.pm.as_str(), p.pkgs.len(), p.end), ("dnf", 2, Some(0)), "exit 100 means updates, not an error");
        assert_eq!(p.pkgs[0].name, "openssl-libs");
        assert_eq!(calls, ["dnf -q -C check-update"]);

        let (p, calls) = run_probe("zypper", &[("zypper", "echo 'v | repo-oss | glibc | 2.39-1 | 2.39-2 | x86_64'")], "0");
        assert_eq!((p.pm.as_str(), p.pkgs.len()), ("zypper", 1));
        assert_eq!((p.pkgs[0].name.as_str(), p.pkgs[0].cur.as_str(), p.pkgs[0].new.as_str()), ("glibc", "2.39-1", "2.39-2"));
        assert_eq!(calls, ["zypper --no-refresh -n -q list-updates"]);

        let (p, _) = run_probe("none", &[], "0");
        assert_eq!((p.pm.as_str(), p.pkgs.len(), p.end), ("none", 0, Some(0)));
    }

    #[test]
    fn probe_sh_refresh_only_when_asked_and_stale() {
        let apt = "case \"$*\" in *update*) ;; *) echo 'Inst curl [1] (2 Debian:12 [amd64])';; esac";
        let (p, calls) = run_probe("refresh", &[("apt-get", apt)], "1");
        assert_eq!(p.refresh, "ok");
        assert!(calls[0].contains("update -qq"), "{calls:?}");
        assert_eq!(p.pkgs.len(), 1, "check still runs after the refresh");
    }

    /// La edad del índice es la de DESPUÉS del refresco: ok => ≈0 y stamp de apt tocado; fallo => la previa; sin refresco => sin escribir.
    #[test]
    fn probe_sh_cache_age_is_post_refresh() {
        let ok = "case \"$*\" in *update*) ;; *) ;; esac";
        let (p, _, stamp) = run_probe_root("age-ok", &[("apt-get", ok)], "1", Some(20 * 3600));
        assert_eq!(p.refresh, "ok");
        assert!(p.cache_age.is_some_and(|a| a <= 5), "{:?}", p.cache_age);
        assert!(stamp.is_some_and(|a| a <= 5), "stamp touched: {stamp:?}");
        let (p, _, stamp) = run_probe_root("age-ok-noseed", &[("apt-get", ok)], "1", None);
        assert!(p.cache_age.is_some_and(|a| a <= 5) && stamp.is_some(), "stamp created when missing");

        let fail = "case \"$*\" in *update*) exit 100;; esac";
        let (p, _, stamp) = run_probe_root("age-fail", &[("apt-get", fail)], "1", Some(20 * 3600));
        assert!(p.refresh.starts_with("failed"), "{}", p.refresh);
        assert!(p.cache_age.is_some_and(|a| (20 * 3600..20 * 3600 + 5).contains(&a)), "previous age kept: {:?}", p.cache_age);
        assert!(stamp.is_some_and(|a| a >= 20 * 3600), "stamp untouched on failure");

        let (p, calls, stamp) = run_probe_root("age-ro", &[("apt-get", ok)], "0", Some(20 * 3600));
        assert_eq!(p.refresh, "skipped");
        assert!(p.cache_age.is_some_and(|a| a >= 20 * 3600), "{:?}", p.cache_age);
        assert!(stamp.is_some_and(|a| a >= 20 * 3600), "read-only does not touch the stamp");
        assert!(!calls.iter().any(|c| c.contains("update -qq")), "{calls:?}");

        let apk = "case \"$*\" in *update*) ;; esac";
        let (p, _, _) = run_probe_root("age-apk", &[("apk", apk)], "1", None);
        assert_eq!((p.refresh.as_str(), p.cache_age), ("ok", Some(0)));
    }

    /// Regresión de solo lectura: un `check` completo no emite escrituras ni refrescos, y con `refresh` sin opt-in todo CT falla.
    #[test]
    fn check_path_is_read_only() {
        let h = fx("basic-37");
        let rep = run_all(&h, &[]);
        assert_eq!(rep.summary().with_updates, 13);
        assert_eq!(rep.summary().errors, 0, "read-only wrapper must not refuse the check path");
        let cfg = Config::load(&h).unwrap();
        let inv = guests::load(&h);
        let o = CheckOpts::from_config(&cfg, Some(true));
        let r = run(&h, &inv, &cfg, &o, &[103], &AtomicBool::new(false), &mut |_| {});
        assert_eq!(r.results[0].status, Status::Error, "refresh without allow_refresh must be refused: {:?}", r.results[0].detail);
        assert!(r.results[0].detail.contains("read-only"));
    }

    /// `--no-refresh` / `C`: solo lectura estricta; `None` sigue la config (default refresca); un `[[ct]] refresh` no lo cruza.
    #[test]
    fn refresh_mode_follows_config_and_read_only_overrides_everything() {
        let cfg = Config::default();
        assert!(CheckOpts::from_config(&cfg, None).refresh, "default config refreshes");
        let ro = CheckOpts::from_config(&cfg, Some(false));
        assert!(!ro.refresh && ro.read_only);
        let mut off = Config::default();
        off.check.refresh = false;
        assert!(!CheckOpts::from_config(&off, None).refresh);
        assert!(CheckOpts::from_config(&off, Some(true)).refresh, "--refresh still forces it");
        let h = fx("basic-37");
        let cfg: Config = toml::from_str("[check]\nrefresh = true\n[[ct]]\nvmid = 103\nrefresh = true\n").unwrap();
        let inv = guests::load(&h);
        let r = run(&h, &inv, &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[103], &AtomicBool::new(false), &mut |_| {});
        assert_ne!(r.results[0].status, Status::Error, "read-only probe is accepted by the read-only host: {:?}", r.results[0].detail);
        assert!(!r.refresh);
    }

    #[test]
    fn check_stops_probing_after_its_own_deadline() {
        let h = ReadOnlyHost::new(World::demo().build());
        let cfg = Config::load(&h).unwrap();
        let inv = guests::load(&h);
        let o = CheckOpts::from_config(&cfg, Some(false));
        let first = targets(&inv, &[]).first().map(|g| g.vmid).expect("a CT");
        let rep = run(&h, &inv, &cfg, &o, &[], &AtomicBool::new(false), &mut |r| {
            if r.vmid == first {
                h.inner().advance_clock(CHECK_DEADLINE_S + 1);
            }
        });
        assert!(rep.results.len() > 1);
        assert!(rep.results.iter().skip(1).all(|r| r.status == Status::Timeout && r.detail.contains("check time limit")), "{:?}", rep.results.iter().map(|r| (&r.vmid, &r.status)).collect::<Vec<_>>());
        assert!(rep.host.is_none(), "the host check is skipped too");
    }

    #[test]
    fn a_probe_that_does_not_fit_in_the_time_left_is_not_started() {
        // queda menos que el peor caso de una sonda (con refresco, 3 × timeout_s): ni la sonda ni el chequeo del host empiezan
        let h = ReadOnlyHost::new(World::demo().build());
        let cfg = Config::load(&h).unwrap();
        let inv = guests::load(&h);
        let o = CheckOpts::from_config(&cfg, Some(true));
        let first = targets(&inv, &[]).first().map(|g| g.vmid).expect("a CT");
        let rep = run(&h, &inv, &cfg, &o, &[], &AtomicBool::new(false), &mut |r| {
            if r.vmid == first {
                h.inner().advance_clock(CHECK_DEADLINE_S - probe_worst_s(&o) + 1);
            }
        });
        assert!(rep.results.iter().skip(1).all(|r| r.status == Status::Timeout), "{:?}", rep.results.iter().map(|r| (&r.vmid, &r.status)).collect::<Vec<_>>());
        assert!(rep.host.is_none());
        // con los máximos de la config un chequeo con refresco sigue cabiendo en su plazo
        assert!(probe_worst_s(&CheckOpts { timeout_s: crate::share::config::MAX_CHECK_TIMEOUT_S, ..o.clone() }) < CHECK_DEADLINE_S);
    }
}
