//! Avisos Apprise (ARCHITECTURE §4): Apprise API vía `curl` (config por stdin) o CLI `apprise` como respaldo.
//! Ningún secreto (URL, clave, URLs de destino) viaja en argv: todo va por stdin.
use crate::share::update::check::{CtResult, Report, Status};
use crate::share::host::{ExecOpts, Host, LineSink};
use crate::share::state;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const OUTBOX_DIR: &str = "/var/lib/pxu/outbox";
const OUTBOX_MAX: usize = 50;
const OUTBOX_MAX_AGE_S: u64 = 7 * 86400;

#[derive(Debug, Clone, Deserialize)]
pub struct AppriseCfg {
    /// "auto" | "api" | "cli" | "none"
    #[serde(default = "d_transport")]
    pub transport: String,
    #[serde(default)]
    pub api_url: String,
    /// Modo stateful: clave de la configuración guardada en la API (`/notify/<key>`).
    #[serde(default)]
    pub api_key: String,
    /// Modo stateless (o CLI): URLs de destino de Apprise. También `APPRISE_URLS` (espacios o comas).
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub tag: String,
    #[serde(default = "d_on")]
    pub on: Vec<String>,
    #[serde(default = "d_timeout")]
    pub timeout_s: u64,
    /// Esperas entre reintentos (ms). Por defecto 2 s y 8 s.
    #[serde(default = "d_retry")]
    pub retry_delays_ms: Vec<u64>,
}

/// Eventos que pueden avisar (`[apprise] on`).
pub const EVENTS: &[&str] = &["updates_found", "update_failed", "run_summary", "check_error"];

fn d_transport() -> String {
    "auto".into()
}
fn d_on() -> Vec<String> {
    EVENTS.iter().map(|s| s.to_string()).collect()
}
fn d_timeout() -> u64 {
    10
}
fn d_retry() -> Vec<u64> {
    vec![2000, 8000]
}

impl Default for AppriseCfg {
    fn default() -> Self {
        AppriseCfg { transport: d_transport(), api_url: String::new(), api_key: String::new(), urls: vec![], tag: String::new(), on: d_on(), timeout_s: d_timeout(), retry_delays_ms: d_retry() }
    }
}

impl AppriseCfg {
    /// `APPRISE_URLS` (si existe y no está vacía) sustituye a `urls` del config.
    pub fn apply_env(&mut self, env_urls: Option<&str>) {
        let v: Vec<String> = env_urls.unwrap_or("").split([' ', ',', '\n']).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
        if !v.is_empty() {
            self.urls = v;
        }
    }
    pub fn wants(&self, event: &str) -> bool {
        self.on.iter().any(|e| e == event)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Success,
    Warning,
    Failure,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub title: String,
    pub body: String,
    pub level: Level,
    pub event: String,
    #[serde(default)]
    pub created: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Api,
    Cli,
    None,
}

impl Transport {
    pub fn label(self) -> &'static str {
        match self {
            Transport::Api => "apprise-api (curl)",
            Transport::Cli => "apprise cli",
            Transport::None => "none",
        }
    }
}

fn apprise_installed(host: &dyn Host) -> bool {
    ["/usr/local/bin/apprise", "/usr/bin/apprise", "/bin/apprise", "/root/.local/bin/apprise"].iter().any(|p| host.exists(Path::new(p)))
}

pub fn resolve(host: &dyn Host, c: &AppriseCfg) -> Transport {
    match c.transport.as_str() {
        "api" if !c.api_url.is_empty() => Transport::Api,
        "cli" if !c.urls.is_empty() => Transport::Cli,
        "auto" if !c.api_url.is_empty() => Transport::Api,
        "auto" if !c.urls.is_empty() && apprise_installed(host) => Transport::Cli,
        _ => Transport::None,
    }
}

/// Valor entre comillas para `curl --config` (escapes `\\ \" \n \r \t`).
fn curl_q(s: &str) -> String {
    let mut o = String::from('"');
    for ch in s.chars() {
        match ch {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn level_str(l: Level) -> &'static str {
    match l {
        Level::Info => "info",
        Level::Success => "success",
        Level::Warning => "warning",
        Level::Failure => "failure",
    }
}

/// Configuración completa para `curl --config -` (URL, cuerpo y cabeceras: todo por stdin).
pub fn curl_config(c: &AppriseCfg, m: &Message) -> String {
    let stateless = c.api_key.is_empty();
    let mut body = json!({ "title": m.title, "body": m.body, "type": level_str(m.level), "format": "markdown" });
    if !c.tag.is_empty() {
        body["tag"] = json!(c.tag);
    }
    if stateless {
        body["urls"] = json!(c.urls.join(","));
    }
    let base = c.api_url.trim_end_matches('/');
    let url = if stateless { format!("{base}/notify/") } else { format!("{base}/notify/{}", c.api_key) };
    format!(
        "url = {}\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\ndata = {}\nsilent\nshow-error\nfail\nproto = \"=http,https\"\nmax-filesize = 65536\nmax-time = {}\nconnect-timeout = {}\n",
        curl_q(&url),
        curl_q(&body.to_string()),
        c.timeout_s,
        c.timeout_s.min(5)
    )
}

struct Out(Vec<String>);
impl LineSink for Out {
    fn line(&mut self, l: &str) {
        self.0.push(l.to_string());
    }
}

/// Un intento de envío con el transporte resuelto. `Err` = motivo corto, sin secretos.
pub fn send_once(host: &dyn Host, c: &AppriseCfg, m: &Message) -> Result<Transport, String> {
    let t = resolve(host, c);
    let (argv, stdin): (Vec<String>, String) = match t {
        Transport::None => return Err("no Apprise transport configured (set [apprise] api_url, or urls with the apprise CLI installed)".into()),
        Transport::Api => (vec!["curl".into(), "--config".into(), "-".into()], curl_config(c, m)),
        Transport::Cli => (
            ["apprise", "--config", "/dev/stdin", "-t", m.title.as_str(), "-b", m.body.as_str(), "-n", level_str(m.level)].iter().map(|s| s.to_string()).collect(),
            c.urls.join("\n") + "\n",
        ),
    };
    let av: Vec<&str> = argv.iter().map(String::as_str).collect();
    let opts = ExecOpts { nice: Some(10), timeout_s: Some(c.timeout_s + 5), stdin: Some(stdin.into_bytes()), ..Default::default() };
    let mut out = Out(Vec::new());
    match host.host_cmd(&av, &opts, &mut out) {
        Ok(e) if e.code == 0 => Ok(t),
        Ok(e) => Err(format!("{} exited with {}{}", av[0], e.code, out.0.last().map(|l| format!(": {}", l.chars().take(100).collect::<String>())).unwrap_or_default())),
        Err(e) => Err(format!("{}: {e}", av[0])),
    }
}

/// Envío con reintentos (esperas de `retry_delays_ms`).
pub fn send(host: &dyn Host, c: &AppriseCfg, m: &Message) -> Result<Transport, String> {
    let mut last = send_once(host, c, m);
    for d in &c.retry_delays_ms {
        if last.is_ok() || resolve(host, c) == Transport::None {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(*d));
        last = send_once(host, c, m);
    }
    last
}

// ---------------------------------------------------------------- cola (outbox)

fn now_s(h: &dyn Host) -> u64 {
    h.now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn outbox_files(host: &dyn Host) -> Vec<PathBuf> {
    host.read_dir(Path::new(OUTBOX_DIR)).unwrap_or_default().into_iter().filter(|p| p.extension().is_some_and(|e| e == "json")).collect()
}

pub fn outbox_len(host: &dyn Host) -> usize {
    outbox_files(host).len()
}

/// Guarda el mensaje en la cola (máx. 50 / 7 días: se descartan los más viejos).
pub fn enqueue(host: &dyn Host, m: &Message) -> std::io::Result<()> {
    let now = now_s(host);
    let mut files = outbox_files(host);
    files.sort();
    let mut keep = Vec::new();
    for f in files {
        let ts: u64 = f.file_name().and_then(|n| n.to_str()).and_then(|n| n.split('-').next()).and_then(|t| t.parse().ok()).unwrap_or(0);
        if now.saturating_sub(ts) > OUTBOX_MAX_AGE_S {
            let _ = host.remove_file(&f);
        } else {
            keep.push(f);
        }
    }
    while keep.len() >= OUTBOX_MAX {
        let _ = host.remove_file(&keep.remove(0));
    }
    let mut msg = m.clone();
    msg.created = now;
    let mut n = 0;
    let path = loop {
        let p = Path::new(OUTBOX_DIR).join(format!("{now}-{n:03}.json"));
        if !host.exists(&p) {
            break p;
        }
        n += 1;
    };
    host.write_file(&path, &serde_json::to_vec_pretty(&msg).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?)
}

/// Reintenta la cola; devuelve (enviados, pendientes). Un mensaje ilegible se descarta.
pub fn flush_outbox(host: &dyn Host, c: &AppriseCfg) -> (usize, usize) {
    let mut files = outbox_files(host);
    files.sort();
    let (mut sent, mut left) = (0, 0);
    let mut dead = false;
    for f in files {
        let msg: Option<Message> = host.read_file(&f).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let Some(msg) = msg else {
            let _ = host.remove_file(&f);
            continue;
        };
        // Si un envío falla, no se insiste con el resto en esta pasada (red caída).
        if !dead && send_once(host, c, &msg).is_ok() {
            let _ = host.remove_file(&f);
            sent += 1;
        } else {
            dead = true;
            left += 1;
        }
    }
    (sent, left)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent(&'static str),
    Queued(String),
    Disabled,
}

/// Envía; si falla, encola. Con transporte `none` no hace nada.
pub fn notify(host: &dyn Host, c: &AppriseCfg, m: &Message) -> Outcome {
    if resolve(host, c) == Transport::None {
        return Outcome::Disabled;
    }
    match send(host, c, m) {
        Ok(t) => Outcome::Sent(t.label()),
        Err(e) => {
            let _ = enqueue(host, m);
            Outcome::Queued(e)
        }
    }
}

// ---------------------------------------------------------------- eventos de check

/// Mensajes `updates_found` (solo CTs cuya huella cambió desde el último aviso) y `check_error`.
pub fn check_messages(report: &Report, c: &AppriseCfg, st: &state::State) -> Vec<(Message, Vec<(u32, String)>)> {
    let mut out = Vec::new();
    let fresh: Vec<&CtResult> = report
        .results
        .iter()
        .filter(|r| r.updates() > 0)
        .filter(|r| r.info.as_ref().is_some_and(|i| st.notified.get(&r.vmid.to_string()) != Some(&i.fingerprint)))
        .collect();
    if c.wants("updates_found") && !fresh.is_empty() {
        let pk: usize = fresh.iter().map(|r| r.updates()).sum();
        let sec: usize = fresh.iter().map(|r| r.security()).sum();
        let mut body = String::new();
        for r in &fresh {
            let names: Vec<&str> = r.info.iter().flat_map(|i| i.packages.iter().take(4).map(|p| p.name.as_str())).collect();
            let more = r.updates().saturating_sub(4);
            body += &format!("- **{}** ({}): {} updates{} — {}{}\n", r.name, r.vmid, r.updates(), if r.security() > 0 { format!(", {} security", r.security()) } else { String::new() }, names.join(", "), if more > 0 { format!(" +{more}") } else { String::new() });
        }
        out.push((
            Message { title: format!("pxu: updates in {} CT{}", fresh.len(), if fresh.len() == 1 { "" } else { "s" }), body: format!("{pk} packages ({sec} security)\n\n{body}"), level: Level::Info, event: "updates_found".into(), created: 0 },
            fresh.iter().map(|r| (r.vmid, r.info.as_ref().map(|i| i.fingerprint.clone()).unwrap_or_default())).collect(),
        ));
    }
    // Revisión del host (solo avisa; nunca instala). Mismo evento y misma deduplicación por huella (clave "0").
    if let Some(h) = report.host.as_ref().filter(|h| h.error.is_none()) {
        let fp = h.fingerprint();
        let cts = h.outdated_cts();
        let has = h.updates() > 0 || h.reboot_pending() || h.nvidia_missing_for.is_some() || !cts.is_empty();
        if c.wants("updates_found") && has && st.notified.get("0") != Some(&fp) {
            let mut body = String::new();
            if h.updates() > 0 {
                body += &format!("- {} updates ({} security)\n", h.updates(), h.security());
            }
            if h.reboot_pending() {
                body += &format!("- reboot needed: {}\n", h.reboot_reason);
            }
            if let Some(k) = &h.nvidia_missing_for {
                body += &format!("- NVIDIA: no module for kernel {k}\n");
            }
            if !cts.is_empty() {
                body += &format!("- CT with outdated NVIDIA driver: {}\n", cts.iter().map(u32::to_string).collect::<Vec<_>>().join(", "));
            }
            if let Some(b) = &h.blocked {
                body += &format!("- blocked: {b}\n");
            }
            body += "\nThe host is only reviewed: pxu does not install anything on it.";
            out.push((Message { title: "pxu: host review".into(), body, level: Level::Info, event: "updates_found".into(), created: 0 }, vec![(0, fp)]));
        }
    }
    if let Some(e) = report.host.as_ref().and_then(|h| h.error.as_ref()).filter(|_| c.wants("check_error")) {
        out.push((Message { title: "pxu: host review failed".into(), body: format!("- {e}\n"), level: Level::Warning, event: "check_error".into(), created: 0 }, vec![]));
    }
    let errs: Vec<&CtResult> = report.results.iter().filter(|r| r.status.is_error()).collect();
    if c.wants("check_error") && !errs.is_empty() {
        let body: String = errs.iter().map(|r| format!("- **{}** ({}): {:?} — {}\n", r.name, r.vmid, r.status, r.detail)).collect();
        out.push((Message { title: format!("pxu: check failed in {} CT{}", errs.len(), if errs.len() == 1 { "" } else { "s" }), body, level: Level::Warning, event: "check_error".into(), created: 0 }, vec![]));
    }
    let _ = Status::Ok;
    out
}

/// Notifica el resultado de un chequeo y recuerda las huellas avisadas. Devuelve un resumen por línea.
pub fn notify_check(host: &dyn Host, c: &AppriseCfg, report: &Report) -> Vec<String> {
    let mut st = state::load_state(host);
    let mut lines = Vec::new();
    let (sent, left) = flush_outbox(host, c);
    if sent + left > 0 {
        lines.push(format!("outbox: {sent} sent, {left} still queued"));
    }
    for (m, fps) in check_messages(report, c, &st) {
        match notify(host, c, &m) {
            Outcome::Sent(t) => lines.push(format!("{}: sent via {t}", m.event)),
            Outcome::Queued(e) => lines.push(format!("{}: send failed ({e}); queued", m.event)),
            Outcome::Disabled => {
                lines.push("notifications disabled (no Apprise transport configured)".into());
                break;
            }
        }
        for (id, fp) in fps {
            st.notified.insert(id.to_string(), fp);
        }
    }
    let _ = state::save_state(host, &st);
    lines
}

/// Aviso de `pxu notify-test` (CLI y TUI).
pub fn test_message(node: &str) -> Message {
    Message { title: "pxu notify-test".into(), body: format!("Test notification from pxu on **{node}**. If you can read this, Apprise delivery works."), level: Level::Info, event: "notify_test".into(), created: 0 }
}

/// Quita de un texto (error de curl/apprise) la clave y las URLs de destino, por si el programa las repite.
pub fn scrub(c: &AppriseCfg, s: &str) -> String {
    c.urls.iter().chain(std::iter::once(&c.api_key)).filter(|x| !x.is_empty()).fold(s.to_string(), |a, x| a.replace(x.as_str(), "•••"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::update::check::{self, CheckOpts};
    use crate::share::mock::scenario::World;
    use crate::share::config::Config;
    use crate::share::proxmox::guests;
    use crate::share::host::{Host, ReadOnlyHost, RealHost};
    use crate::share::mock::MockHost;
    use std::io;
    use std::sync::atomic::AtomicBool;

    fn msg() -> Message {
        Message { title: "T \"q\"".into(), body: "line1\nline2 \\ ok".into(), level: Level::Info, event: "test".into(), created: 0 }
    }

    fn api(key: &str) -> AppriseCfg {
        AppriseCfg { api_url: "http://apprise.example:8000/".into(), api_key: key.into(), urls: vec!["tgram://BOTTOKEN/CHAT".into()], retry_delays_ms: vec![], ..Default::default() }
    }

    #[test]
    fn curl_config_stateful_and_stateless() {
        let c = curl_config(&api("pxu-key"), &msg());
        assert!(c.contains("url = \"http://apprise.example:8000/notify/pxu-key\""));
        assert!(c.contains("max-time = 10") && c.contains("\nfail\n"));
        assert!(c.contains(r#"\"title\":\"T \\\"q\\\"\""#), "{c}");
        assert!(!c.contains("tgram://"), "stateful mode never sends target urls");
        assert_eq!(c.lines().filter(|l| l.starts_with("data = ")).count(), 1, "no raw newlines inside the body");
        let s = curl_config(&api(""), &msg());
        assert!(s.contains("/notify/\"") && s.contains("tgram://BOTTOKEN/CHAT"));
    }

    #[test]
    fn curl_config_has_no_file_writing_options_and_resists_injection() {
        let mut c = api("k\"\noutput = \"/etc/cron.d/x");
        c.api_url = "http://a.example\nupload-file = \"/etc/shadow\"".into();
        let cfg = curl_config(&c, &msg());
        for l in cfg.lines() {
            assert!(!l.starts_with("output") && !l.starts_with("upload-file") && !l.starts_with("config"), "injected line: {l}");
        }
        assert!(cfg.contains("proto = \"=http,https\"") && cfg.contains("max-filesize = 65536"));
    }

    #[test]
    fn transport_resolution() {
        let h = World::demo().build();
        assert_eq!(resolve(&h, &api("k")), Transport::Api);
        let mut c = AppriseCfg { urls: vec!["ntfy://x".into()], ..Default::default() };
        assert_eq!(resolve(&h, &c), Transport::None, "auto + urls but no apprise binary");
        c.transport = "cli".into();
        assert_eq!(resolve(&h, &c), Transport::Cli);
        c.transport = "none".into();
        assert_eq!(resolve(&h, &api("k")), Transport::Api);
        assert_eq!(resolve(&h, &c), Transport::None);
    }

    #[test]
    fn env_urls_override_config() {
        let mut c = api("");
        c.apply_env(Some("ntfy://a, tgram://b/c  \n json://d"));
        assert_eq!(c.urls, ["ntfy://a", "tgram://b/c", "json://d"]);
        c.apply_env(Some("  "));
        assert_eq!(c.urls.len(), 3, "empty env keeps the previous value");
    }

    // ---- curl simulado sobre el RealHost: prueba stdin, argv y reintentos de verdad ----

    /// Host real con PATH acotado a un directorio con un `curl` falso.
    struct PathHost(RealHost, PathBuf);
    impl Host for PathHost {
        fn try_lock(&self, _: &Path, _: bool) -> std::io::Result<Option<crate::share::host::LockGuard>> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
        fn file_meta(&self, _: &Path) -> std::io::Result<crate::share::host::FileMeta> {
            Err(std::io::ErrorKind::Unsupported.into())
        }
                fn read_file(&self, p: &Path) -> io::Result<Vec<u8>> {
            self.0.read_file(p)
        }
        fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
            self.0.read_dir(p)
        }
        fn exists(&self, p: &Path) -> bool {
            self.0.exists(p)
        }
        fn guest_exec(&self, v: u32, a: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<crate::share::host::ExitInfo> {
            self.0.guest_exec(v, a, o, s)
        }
        fn host_cmd(&self, a: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<crate::share::host::ExitInfo> {
            let mut o = o.clone();
            o.env.push(("PATH".into(), format!("{}:/usr/bin:/bin", self.1.display())));
            self.0.host_cmd(a, &o, s)
        }
        fn write_file(&self, p: &Path, d: &[u8]) -> io::Result<()> {
            self.0.write_file(p, d)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            self.0.remove_file(p)
        }
        fn lock_dir(&self) -> &Path {
            self.0.lock_dir()
        }
        fn now(&self) -> std::time::SystemTime {
            self.0.now()
        }
    }

    fn fake_curl(tag: &str, exit: i32) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pxu-fakecurl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let script = format!("#!/bin/sh\necho \"ARGV: $*\" >> '{0}'\ncat >> '{0}'\n[ {1} = 0 ] && echo '{{\"success\":true}}'\nexit {1}\n", log.display(), exit);
        let p = dir.join("curl");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        (dir, log)
    }

    #[test]
    fn secrets_go_through_stdin_never_argv() {
        let (dir, log) = fake_curl("ok", 0);
        let h = ReadOnlyHost::new(PathHost(RealHost::new(), dir.clone()));
        let t = send_once(&h, &api("SECRETKEY"), &msg()).expect("fake curl succeeds");
        assert_eq!(t, Transport::Api);
        let logged = std::fs::read_to_string(&log).unwrap();
        let argv = logged.lines().next().unwrap();
        assert_eq!(argv, "ARGV: --config -", "argv is exactly --config -");
        assert!(logged.contains("/notify/SECRETKEY") && logged.contains("fail"), "config arrived on stdin: {logged}");
        for secret in ["SECRETKEY", "apprise.example", "tgram"] {
            assert!(!argv.contains(secret));
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failure_is_queued_then_flushed_when_curl_recovers() {
        let (dir, _log) = fake_curl("fail", 7);
        let h = PathHost(RealHost::new(), dir.clone());
        // la cola real escribe en /var/lib/pxu: usamos un MockHost para el FS y el PathHost solo para curl
        struct Split<'a>(&'a PathHost, MockHost);
        impl Host for Split<'_> {
            fn try_lock(&self, _: &Path, _: bool) -> std::io::Result<Option<crate::share::host::LockGuard>> {
                Err(std::io::ErrorKind::Unsupported.into())
            }
            fn file_meta(&self, _: &Path) -> std::io::Result<crate::share::host::FileMeta> {
                Err(std::io::ErrorKind::Unsupported.into())
            }
                        fn read_file(&self, p: &Path) -> io::Result<Vec<u8>> {
                self.1.read_file(p)
            }
            fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
                self.1.read_dir(p)
            }
            fn exists(&self, p: &Path) -> bool {
                self.1.exists(p)
            }
            fn guest_exec(&self, v: u32, a: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<crate::share::host::ExitInfo> {
                self.1.guest_exec(v, a, o, s)
            }
            fn host_cmd(&self, a: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<crate::share::host::ExitInfo> {
                self.0.host_cmd(a, o, s)
            }
            fn write_file(&self, p: &Path, d: &[u8]) -> io::Result<()> {
                self.1.write_file(p, d)
            }
            fn remove_file(&self, p: &Path) -> io::Result<()> {
                self.1.remove_file(p)
            }
            fn lock_dir(&self) -> &Path {
                self.1.lock_dir()
            }
            fn now(&self) -> std::time::SystemTime {
                self.1.now()
            }
        }
        let s = Split(&h, World::demo().build());
        let c = api("k");
        let o = notify(&s, &c, &msg());
        assert!(matches!(&o, Outcome::Queued(e) if e.contains("exited with 7")), "{o:?}");
        assert_eq!(outbox_len(&s), 1);
        assert_eq!(flush_outbox(&s, &c), (0, 1), "still failing: stays queued");
        // curl vuelve a funcionar
        let (dir2, _) = fake_curl("fail", 0);
        let h2 = PathHost(RealHost::new(), dir2.clone());
        let s2 = Split(&h2, World::demo().build());
        s2.1.write_file(&Path::new(OUTBOX_DIR).join("1-000.json"), &serde_json::to_vec(&msg()).unwrap()).unwrap();
        assert_eq!(flush_outbox(&s2, &c), (1, 0));
        assert_eq!(outbox_len(&s2), 0);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn outbox_is_bounded() {
        let h = World::demo().build();
        for _ in 0..60 {
            enqueue(&h, &msg()).unwrap();
        }
        assert_eq!(outbox_len(&h), OUTBOX_MAX);
        // un mensaje de hace 8 días se descarta al encolar
        h.write_file(&Path::new(OUTBOX_DIR).join("1-000.json"), b"{}").unwrap();
        enqueue(&h, &msg()).unwrap();
        assert!(!h.exists(&Path::new(OUTBOX_DIR).join("1-000.json")));
    }

    #[test]
    fn check_dedups_by_fingerprint() {
        let h = ReadOnlyHost::new(World::demo().build());
        let inv = guests::load(&h);
        let mut cfg = Config::load(&h).unwrap();
        cfg.apprise.retry_delays_ms = vec![];
        let rep = check::run(&h, &inv, &cfg, &CheckOpts::from_config(&cfg, false), &[], &AtomicBool::new(false), &mut |_| {});
        let first = notify_check(&h, &cfg.apprise, &rep);
        assert!(first.iter().any(|l| l.starts_with("updates_found: sent")), "{first:?}");
        let second = notify_check(&h, &cfg.apprise, &rep);
        assert!(second.is_empty(), "same fingerprints ⇒ no second notice: {second:?}");
        let msgs = check_messages(&rep, &cfg.apprise, &state::State::default());
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].0.body.contains("13 CT") || msgs[0].0.title.contains("13 CT"), "{}", msgs[0].0.title);
    }

    #[test]
    fn host_review_notifies_once_per_fingerprint_and_errors_are_a_separate_notice() {
        let h = ReadOnlyHost::new(World::host_gpu().file("/exec/134/nvidia-smi.out", b"580.95.05\n").build());
        let inv = guests::load(&h);
        let mut cfg = Config::load(&h).unwrap();
        cfg.apprise.retry_delays_ms = vec![];
        let rep = check::run(&h, &inv, &cfg, &CheckOpts::from_config(&cfg, false), &[], &AtomicBool::new(false), &mut |_| {});
        let msgs = check_messages(&rep, &cfg.apprise, &state::State::default());
        let host = msgs.iter().find(|m| m.0.title == "pxu: host review").expect("host message");
        assert_eq!(host.0.event, "updates_found");
        assert!(host.0.body.contains("7 updates (2 security)"), "{}", host.0.body);
        assert!(host.0.body.contains("reboot needed"), "{}", host.0.body);
        assert!(host.0.body.contains("CT with outdated NVIDIA driver: 134"), "{}", host.0.body);
        assert!(host.0.body.contains("does not install anything"), "{}", host.0.body);
        assert_eq!(host.1.len(), 1);
        assert_eq!(host.1[0].0, 0, "dedup key is vmid 0");
        // misma huella ⇒ no se repite
        let mut st = state::State::default();
        st.notified.insert("0".into(), host.1[0].1.clone());
        assert!(!check_messages(&rep, &cfg.apprise, &st).iter().any(|m| m.0.title == "pxu: host review"));
        // GPU sin módulo para el kernel nuevo: aviso
        let h2 = ReadOnlyHost::new(World::host_gpu().host_out("dkms", "nvidia/595.112.07, 6.17.9-1-pve, x86_64: installed\n").build());
        let rep2 = check::run(&h2, &guests::load(&h2), &cfg, &CheckOpts::from_config(&cfg, false), &[], &AtomicBool::new(false), &mut |_| {});
        let m2 = check_messages(&rep2, &cfg.apprise, &state::State::default());
        assert!(m2.iter().any(|m| m.0.body.contains("no module for kernel 6.17.13-2-pve")));
        // fallo del host: aviso de error aparte; los CT no se ven afectados
        let h3 = ReadOnlyHost::new(World::host_gpu().file("/exec/0/apt-list.out.exit", b"100").build());
        let rep3 = check::run(&h3, &guests::load(&h3), &cfg, &CheckOpts::from_config(&cfg, false), &[], &AtomicBool::new(false), &mut |_| {});
        let m3 = check_messages(&rep3, &cfg.apprise, &state::State::default());
        assert!(m3.iter().any(|m| m.0.title == "pxu: host review failed" && m.0.event == "check_error"));
        assert!(!m3.iter().any(|m| m.0.title == "pxu: host review"));
        assert!(m3.iter().any(|m| m.0.title.starts_with("pxu: updates in")), "CT updates are still announced");
    }
}
