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
/// Quien vacía la cola lo toma mientras envía (puede durar minutos).
pub const OUTBOX_LOCK: &str = "pxu-outbox.lock";
/// Protege solo las mutaciones de la cola (elegir nombre, podar, escribir): breve, nunca se mantiene durante la red.
pub const OUTBOX_FILES_LOCK: &str = "pxu-outbox-files.lock";
const OUTBOX_FILES_WAIT_MS: u64 = 5_000;
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
    /// `api_key` cifrada con `systemd-creds` (base64 en una línea). Se descifra solo al enviar (ARCHITECTURE §4).
    #[serde(default)]
    pub api_key_enc: String,
    /// `urls` cifradas con `systemd-creds` (una por línea antes de cifrar), base64 en una línea.
    #[serde(default)]
    pub urls_enc: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default = "d_on")]
    pub on: Vec<String>,
    #[serde(default = "d_timeout")]
    pub timeout_s: u64,
    /// Esperas entre reintentos (ms) de errores transitorios. Por defecto 5 s, 30 s, 2 min y 5 min (~7,5 min).
    #[serde(default = "d_retry")]
    pub retry_delays_ms: Vec<u64>,
}

/// Máximos que `config` aplica al cargar: acotan el peor caso de un aviso para que quepa en `update::RUN_MARGIN_S`.
pub const MAX_TIMEOUT_S: u64 = 30;
pub const MAX_RETRIES: usize = 6;
pub const MAX_RETRY_TOTAL_MS: u64 = 600_000;
/// Un intento de envío: `timeout_s` + 5 s de la orden + cola de SIGTERM/SIGKILL (30 s).
pub const ATTEMPT_S: u64 = MAX_TIMEOUT_S + 5 + 30;
/// Secretos que se descifran como máximo (`api_key_enc` y `urls_enc`).
pub const MAX_SECRETS: u64 = 2;
/// Descifrar `secrets` secretos: 20 s de `systemd-creds` + 30 s de cola cada uno (con 0 no se descifra nada).
/// Se hace una sola vez por envío (con todos sus reintentos) y una sola vez por pasada de la cola.
pub const fn reveal_s(secrets: u64) -> u64 {
    secrets * (20 + 30)
}
/// Peor caso con los dos secretos cifrados.
pub const REVEAL_S: u64 = reveal_s(MAX_SECRETS);
/// Avisos que se reenvían de la cola en una pasada (el resto espera a la siguiente).
pub const MAX_FLUSH: usize = 10;

/// Peor caso de un aviso con `retries` reintentos (con los máximos de arriba) y `secrets` secretos cifrados (1 o 2; 0 = en claro).
pub const fn worst_send_s_with(retries: usize, secrets: u64) -> u64 {
    reveal_s(secrets) + (1 + retries as u64) * ATTEMPT_S + MAX_RETRY_TOTAL_MS / 1000
}
/// Igual, con los dos secretos cifrados (el peor caso que se usa para los plazos).
pub const fn worst_send_s(retries: usize) -> u64 {
    worst_send_s_with(retries, MAX_SECRETS)
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
    vec![5_000, 30_000, 120_000, 300_000]
}

impl Default for AppriseCfg {
    fn default() -> Self {
        AppriseCfg { transport: d_transport(), api_url: String::new(), api_key: String::new(), urls: vec![], api_key_enc: String::new(), urls_enc: String::new(), tag: String::new(), on: d_on(), timeout_s: d_timeout(), retry_delays_ms: d_retry() }
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
    /// ¿Hay clave de la API (en claro o cifrada)?
    pub fn has_key(&self) -> bool {
        !self.api_key.is_empty() || !self.api_key_enc.is_empty()
    }
    /// ¿Hay URLs de destino (en claro o cifradas)?
    pub fn has_urls(&self) -> bool {
        !self.urls.is_empty() || !self.urls_enc.is_empty()
    }
    /// Secretos cifrados que `reveal` tendría que descifrar (0, 1 o 2): marca el tiempo máximo de cada aviso.
    pub fn encrypted_secrets(&self) -> u64 {
        u64::from(self.api_key.is_empty() && !self.api_key_enc.is_empty()) + u64::from(self.urls.is_empty() && !self.urls_enc.is_empty())
    }
    /// Secretos guardados en claro en el config (conviene guardarlos de nuevo en la TUI para cifrarlos).
    pub fn has_plain_secret(&self) -> bool {
        !self.api_key.is_empty() || !self.urls.is_empty()
    }
    /// Copia con los secretos cifrados ya descifrados (`systemd-creds decrypt`, entrada y salida por stdin/stdout).
    /// Un valor en claro que ya esté en memoria (config en claro o `APPRISE_URLS`) tiene prioridad y no se descifra nada.
    /// Falla con «cannot decrypt apprise secret: …» si no se puede; nunca cae a texto plano en silencio.
    pub fn reveal(&self, host: &dyn Host) -> Result<AppriseCfg, String> {
        let mut c = self.clone();
        if c.api_key.is_empty() && !c.api_key_enc.is_empty() {
            c.api_key = unseal(host, &c.api_key_enc)?;
        }
        if c.urls.is_empty() && !c.urls_enc.is_empty() {
            c.urls = unseal(host, &c.urls_enc)?.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
        }
        Ok(c)
    }
}

/// Nombre de la credencial: el texto cifrado solo se descifra con este mismo nombre.
pub const CREDS_NAME: &str = "pxu-apprise";
/// Cifrado: TPM2 si hay; si no, la llave del host (`/var/lib/systemd/credential.secret`, la crea systemd).
pub const CREDS_ENCRYPT: &[&str] = &["systemd-creds", "encrypt", "--name=pxu-apprise", "--with-key=auto", "-", "-"];
pub const CREDS_DECRYPT: &[&str] = &["systemd-creds", "decrypt", "--name=pxu-apprise", "-", "-"];

/// Credencial cifrada para el aviso de fallo del servicio (`pxu-update-notify-failed.service`): la lee systemd, no pxu.
pub const FAILURE_CRED_PATH: &str = "/etc/pxu/failure-notice.cred";

/// stdout (el dato) y stderr (avisos de systemd-creds) van por separado: un aviso nunca se mezcla con el resultado.
#[derive(Default)]
struct CredsOut {
    out: Vec<String>,
    err: Vec<String>,
}
impl LineSink for CredsOut {
    fn line(&mut self, l: &str) {
        self.out.push(l.to_string());
    }
    fn err_line(&mut self, l: &str) {
        self.err.push(l.to_string());
    }
}

fn creds(host: &dyn Host, argv: &[&str], stdin: &str, timeout_s: u64) -> Result<Vec<String>, String> {
    let opts = ExecOpts { timeout_s: Some(timeout_s), stdin: Some(stdin.as_bytes().to_vec()), split_stderr: true, ..Default::default() };
    let mut o = CredsOut::default();
    match host.host_cmd(argv, &opts, &mut o) {
        Ok(e) if e.code == 0 => Ok(o.out),
        // solo en el error y solo de stderr: es el mensaje de systemd-creds, nunca el secreto
        Ok(e) => Err(format!("systemd-creds exited with {}{}", e.code, o.err.first().map(|l| format!(": {}", l.chars().take(100).collect::<String>())).unwrap_or_default())),
        Err(e) => Err(format!("systemd-creds: {e}")),
    }
}

/// Cifra `plain` con `systemd-creds`; devuelve el texto base64 en una sola línea.
/// Valida que sea base64 y lo descifra de vuelta: si no da el mismo valor, error y nada se guarda.
pub fn seal(host: &dyn Host, plain: &str) -> Result<String, String> {
    let lines = creds(host, CREDS_ENCRYPT, plain, 20).map_err(|e| format!("cannot encrypt the apprise secret: {e}"))?;
    let enc: String = lines.iter().flat_map(|l| l.split_whitespace()).collect();
    if enc.is_empty() {
        return Err("cannot encrypt the apprise secret: systemd-creds returned nothing".into());
    }
    if !enc.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')) {
        return Err("cannot encrypt the apprise secret: systemd-creds output is not base64".into());
    }
    match unseal(host, &enc) {
        Ok(back) if back == plain => Ok(enc),
        _ => Err("cannot encrypt the apprise secret: round-trip check failed".into()),
    }
}

/// Descifra el texto de `seal`. El resultado es un secreto: no se imprime ni se registra.
/// Solo stdout; solo se pierde el salto de línea final, los espacios del secreto se conservan.
pub fn unseal(host: &dyn Host, enc: &str) -> Result<String, String> {
    let lines = creds(host, CREDS_DECRYPT, enc, 20).map_err(|e| format!("cannot decrypt apprise secret: {e}"))?;
    let plain = lines.join("\n");
    if plain.is_empty() {
        return Err("cannot decrypt apprise secret: systemd-creds returned nothing".into());
    }
    Ok(plain)
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
        "cli" if c.has_urls() => Transport::Cli,
        "auto" if !c.api_url.is_empty() => Transport::Api,
        "auto" if c.has_urls() && apprise_installed(host) => Transport::Cli,
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
        "url = {}\nrequest = \"POST\"\nheader = \"Content-Type: application/json\"\ndata = {}\nsilent\nshow-error\nfail\nproto = \"=http,https\"\nproto-redir = \"=http,https\"\nmax-filesize = 65536\nmax-time = {}\nconnect-timeout = {}\n",
        curl_q(&url),
        curl_q(&body.to_string()),
        c.timeout_s,
        c.timeout_s.min(5)
    )
}

/// Desescapa un valor entre comillas de `curl --config` (solo `\\ \" \n \r \t`); `None` si está mal formado.
fn curl_unq(v: &str) -> Option<String> {
    let inner = v.strip_prefix('"')?.strip_suffix('"')?;
    let mut o = String::new();
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        match c {
            '"' => return None,
            '\\' => o.push(match it.next()? {
                '\\' => '\\',
                '"' => '"',
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                _ => return None,
            }),
            c => o.push(c),
        }
    }
    Some(o)
}

/// A-11: ¿la configuración que llega por stdin a `curl --config -` es exactamente de la forma que genera `curl_config`?
/// Lista cerrada de opciones con valores fijos o acotados; cualquier otra (`output`, `config`, `upload-file`, `next`, `cookie-jar`…),
/// una URL que no sea http(s), un `data` que no sea JSON inline (`@archivo`) o un valor mal escapado se rechaza.
pub fn curl_config_is_safe(cfg: &str) -> bool {
    let num = |v: &str| !v.is_empty() && v.len() <= 9 && v.bytes().all(|b| b.is_ascii_digit());
    let (mut urls, mut datas) = (0, 0);
    for line in cfg.split('\n') {
        if line.is_empty() || matches!(line, "silent" | "show-error" | "fail") {
            continue;
        }
        let Some((k, v)) = line.split_once(" = ") else { return false };
        let ok = match k {
            "url" => {
                urls += 1;
                curl_unq(v).is_some_and(|u| (u.starts_with("http://") || u.starts_with("https://")) && !u.chars().any(char::is_control))
            }
            "data" => {
                datas += 1;
                curl_unq(v).is_some_and(|d| d.starts_with('{'))
            }
            "request" => v == "\"POST\"",
            "header" => v == "\"Content-Type: application/json\"",
            "proto" | "proto-redir" => v == "\"=http,https\"",
            "max-filesize" | "max-time" | "connect-timeout" => num(v),
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    urls == 1 && datas <= 1
}

/// Aviso que lanza `pxu-update-notify-failed.service` cuando el servicio falla (fijo: el script no conoce el contenido de la corrida).
fn failure_message() -> Message {
    Message {
        title: "pxu update FAILED".into(),
        body: "pxu-update.service failed or was killed: check journalctl -u pxu-update.service and /var/log/pxu; a CT may be half updated".into(),
        level: Level::Failure,
        event: "service_failed".into(),
        created: 0,
    }
}

/// Texto que se cifra para `pxu-failure-notice`: primera línea = transporte; el resto, la config lista para `curl --config -` o `apprise --config`.
/// Sin salto de línea final (el ida y vuelta de `seal` no lo conserva). `c` lleva los secretos ya descifrados.
pub fn failure_payload(c: &AppriseCfg, t: Transport) -> Option<String> {
    match t {
        Transport::Api => Some(format!("# pxu-failure-notice transport=api\n{}", curl_config(c, &failure_message()).trim_end())),
        Transport::Cli => Some(format!("# pxu-failure-notice transport=cli\n{}", c.urls.join("\n"))),
        Transport::None => None,
    }
}

/// Cifra y guarda la credencial del aviso de fallo con las mismas URLs/clave que `c`. `Ok(false)` = sin transporte (Apprise desactivado):
/// no se escribe nada y se BORRA la credencial anterior, para que no quede un destino viejo que el servicio de fallo siga usando.
pub fn write_failure_cred(host: &dyn Host, c: &AppriseCfg) -> Result<bool, String> {
    let t = resolve(host, c);
    if t == Transport::None {
        host.remove_failure_notice().map_err(|e| format!("{FAILURE_CRED_PATH}: {e}"))?;
        return Ok(false);
    }
    let revealed = c.reveal(host)?;
    let Some(plain) = failure_payload(&revealed, t) else { return Ok(false) };
    let enc = seal(host, &plain)?;
    host.write_failure_notice(enc.as_bytes()).map_err(|e| format!("{FAILURE_CRED_PATH}: {e}"))?;
    Ok(true)
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
    if t == Transport::None {
        return Err("no Apprise transport configured (set [apprise] api_url, or urls with the apprise CLI installed)".into());
    }
    // Descifrado justo antes de enviar; el valor claro solo vive aquí y se pasa por stdin.
    let rc = c.reveal(host)?;
    attempt(host, &rc, m, t)
}

/// Un intento con la config ya descifrada (`reveal`), con el error sin secretos.
fn attempt(host: &dyn Host, rc: &AppriseCfg, m: &Message, t: Transport) -> Result<Transport, String> {
    send_revealed(host, rc, m, t).map_err(|e| scrub(rc, &e))
}

fn send_revealed(host: &dyn Host, c: &AppriseCfg, m: &Message, t: Transport) -> Result<Transport, String> {
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

/// ¿El error merece otro intento? Un 4xx de Apprise (salvo 408/424/429) es de configuración: reintentar no lo arregla.
fn transient(err: &str) -> bool {
    // un secreto que no se puede (des)cifrar es un problema de la máquina: esperar no lo arregla (el aviso queda en la cola)
    if err.starts_with("cannot decrypt apprise secret") {
        return false;
    }
    let Some(i) = err.find("returned error: ") else { return true };
    let code: u32 = err[i + 16..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0);
    matches!(code, 408 | 424 | 429) || code >= 500 || code == 0
}

/// Envío con reintentos: `delays` (ms) entre intentos, solo si el fallo es transitorio.
pub fn send_with(host: &dyn Host, c: &AppriseCfg, m: &Message, delays: &[u64]) -> Result<Transport, String> {
    let t = resolve(host, c);
    if t == Transport::None {
        return send_once(host, c, m);
    }
    // Se descifra una sola vez: los reintentos reutilizan la config revelada.
    let rc = c.reveal(host)?;
    let mut last = attempt(host, &rc, m, t);
    for d in delays {
        match &last {
            Ok(_) => break,
            Err(e) if !transient(e) => break,
            _ if resolve(host, c) == Transport::None => break,
            _ => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(*d));
        last = attempt(host, &rc, m, t);
    }
    last
}

/// Envío con todas las esperas de `retry_delays_ms`.
pub fn send(host: &dyn Host, c: &AppriseCfg, m: &Message) -> Result<Transport, String> {
    send_with(host, c, m, &c.retry_delays_ms)
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
/// Elegir el nombre, podar y escribir van bajo `pxu-outbox-files.lock`; sin el lock no se escribe nada y se devuelve el error.
pub fn enqueue(host: &dyn Host, m: &Message) -> std::io::Result<()> {
    enqueue_wait(host, m, OUTBOX_FILES_WAIT_MS)
}

fn enqueue_wait(host: &dyn Host, m: &Message, wait_ms: u64) -> std::io::Result<()> {
    let _lock = state::lock_wait(host, OUTBOX_FILES_LOCK, wait_ms)?;
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
/// Si otro proceso ya la está vaciando (`pxu-outbox.lock` tomado) no envía nada y devuelve (0, pendientes).
pub fn flush_outbox(host: &dyn Host, c: &AppriseCfg) -> (usize, usize) {
    try_flush_outbox(host, c).unwrap_or_else(|| (0, outbox_len(host)))
}

/// Como `flush_outbox`, pero `None` si el lock de la cola está tomado por otro proceso (evita enviar el mismo aviso dos veces).
/// Si el lock no se puede abrir tampoco se vacía (los avisos quedan en la cola, no se pierden).
fn try_flush_outbox(host: &dyn Host, c: &AppriseCfg) -> Option<(usize, usize)> {
    let _lock = match host.try_lock(&host.lock_dir().join(OUTBOX_LOCK), true) {
        Ok(Some(g)) => g,
        Ok(None) => return None,
        Err(e) => {
            // sin exclusión mutua no se vacía (dos procesos enviarían lo mismo): los avisos siguen en la cola
            eprintln!("pxu: outbox lock unavailable: {e}; not flushing");
            return None;
        }
    };
    let mut files = outbox_files(host);
    files.sort();
    let (mut sent, mut left) = (0, 0);
    let mut dead = false;
    let mut tried = 0;
    // Una sola descifrada por pasada; si falla, no se insiste con el resto.
    let mut revealed: Option<AppriseCfg> = None;
    for f in files {
        let msg: Option<Message> = host.read_file(&f).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let Some(msg) = msg else {
            let _ = host.remove_file(&f);
            continue;
        };
        // Si un envío falla, no se insiste con el resto en esta pasada (red caída).
        if tried >= MAX_FLUSH {
            left += 1; // el resto espera a la siguiente pasada: acota la duración
        } else if !dead && {
            tried += 1;
            if revealed.is_none() {
                revealed = c.reveal(host).ok();
            }
            match (&revealed, resolve(host, c)) {
                (Some(rc), t) if t != Transport::None => attempt(host, rc, &msg, t).is_ok(),
                _ => false,
            }
        } {
            let _ = host.remove_file(&f);
            sent += 1;
        } else {
            dead = true;
            left += 1;
        }
    }
    Some((sent, left))
}

/// `pxu notify-flush`: solo vacía la cola. Devuelve (enviados, pendientes), o `None` si otro proceso ya la está vaciando.
pub fn notify_flush(host: &dyn Host, c: &AppriseCfg) -> Option<(usize, usize)> {
    if outbox_len(host) == 0 || resolve(host, c) == Transport::None {
        return Some((0, outbox_len(host)));
    }
    try_flush_outbox(host, c)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent(&'static str),
    Queued(String),
    /// El envío falló y tampoco se pudo guardar en la cola: el aviso se perdió (el texto dice por qué).
    Lost(String),
    Disabled,
}

/// Envía; si falla, encola. Con transporte `none` no hace nada.
pub fn notify(host: &dyn Host, c: &AppriseCfg, m: &Message) -> Outcome {
    notify_with(host, c, m, &c.retry_delays_ms)
}

fn notify_with(host: &dyn Host, c: &AppriseCfg, m: &Message, delays: &[u64]) -> Outcome {
    if resolve(host, c) == Transport::None {
        return Outcome::Disabled;
    }
    match send_with(host, c, m, delays) {
        Ok(t) => Outcome::Sent(t.label()),
        Err(e) => {
            match enqueue(host, m) {
                Ok(()) => Outcome::Queued(e),
                Err(q) => Outcome::Lost(format!("{e}; NOT queued: {q}")),
            }
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
    let st = state::load_state(host);
    let mut confirmed: Vec<(u32, String)> = Vec::new();
    let mut lines = Vec::new();
    let (sent, left) = flush_outbox(host, c);
    if sent + left > 0 {
        lines.push(format!("outbox: {sent} sent, {left} still queued"));
    }
    for (m, fps) in check_messages(report, c, &st) {
        match notify_with(host, c, &m, &c.retry_delays_ms[..c.retry_delays_ms.len().min(2)]) {
            Outcome::Sent(t) => lines.push(format!("{}: sent via {t}", m.event)),
            Outcome::Queued(e) => lines.push(format!("{}: send failed ({e}); queued", m.event)),
            Outcome::Lost(e) => {
                // ni enviado ni encolado: la huella no se marca, el próximo chequeo lo reintenta
                lines.push(format!("{}: send failed and NOT queued ({e}); will retry on the next check", m.event));
                continue;
            }
            Outcome::Disabled => {
                lines.push("notifications disabled (no Apprise transport configured)".into());
                break;
            }
        }
        confirmed.extend(fps);
    }
    if !confirmed.is_empty() {
        if let Err(e) = state::update_state(host, |st| {
            for (id, fp) in confirmed {
                st.notified.insert(id.to_string(), fp);
            }
        }) {
            lines.push(format!("state not saved ({e}): the next check may notify again"));
        }
    }
    lines
}

/// Aviso de `pxu notify-test` (CLI y TUI).
pub fn test_message(node: &str) -> Message {
    Message { title: "pxu notify-test".into(), body: format!("Test notification from pxu on **{node}**. If you can read this, Apprise delivery works."), level: Level::Info, event: "notify_test".into(), created: 0 }
}

/// Quita de un texto (error de curl/apprise) la clave y las URLs de destino, por si el programa las repite.
pub fn scrub(c: &AppriseCfg, s: &str) -> String {
    c.urls.iter().chain(std::iter::once(&c.api_key)).chain(std::iter::once(&c.api_key_enc)).chain(std::iter::once(&c.urls_enc)).filter(|x| !x.is_empty()).fold(s.to_string(), |a, x| a.replace(x.as_str(), "•••"))
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
    fn curl_config_carries_the_tag() {
        let c = curl_config(&AppriseCfg { tag: "system".into(), ..api("k") }, &msg());
        assert!(c.contains(r#"\"tag\":\"system\""#), "{c}");
        assert!(!curl_config(&api("k"), &msg()).contains("tag"));
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

    fn host_argv(h: &MockHost) -> Vec<Vec<String>> {
        h.recorded().into_iter().filter_map(|r| if let crate::share::mock::Recorded::Host(a) = r { Some(a) } else { None }).collect()
    }

    #[test]
    fn seal_ignores_the_stderr_warning_of_systemd_creds() {
        let h = World::demo().build();
        let enc = seal(&h, "SECRETKEY").unwrap();
        assert!(enc.bytes().all(|b| b.is_ascii_alphanumeric() || b"+/=".contains(&b)), "{enc}");
        assert!(!enc.contains("Credential") && !enc.contains("encrypted") && !enc.contains("anyway"), "{enc}");
        assert_eq!(unseal(&h, &enc).unwrap(), "SECRETKEY");
    }

    #[test]
    fn unseal_keeps_inner_spaces_of_the_secret() {
        let h = World::demo().build();
        let enc = seal(&h, " a b  c ").unwrap();
        assert_eq!(unseal(&h, &enc).unwrap(), " a b  c ");
    }

    #[test]
    fn seal_fails_when_the_round_trip_does_not_match() {
        let h = World::demo().build();
        let e = seal(&h, "xMOCK_BAD_ROUNDTRIP").unwrap_err();
        assert_eq!(e, "cannot encrypt the apprise secret: round-trip check failed");
        assert!(!e.contains("MOCK_BAD"));
    }

    fn failure_cred(h: &MockHost) -> Option<String> {
        h.read_file(Path::new(FAILURE_CRED_PATH)).ok().map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn failure_credential_for_the_api_carries_the_curl_config_encrypted() {
        let h = World::demo().build();
        let c = api("SECRETKEY");
        assert_eq!(write_failure_cred(&h, &c), Ok(true));
        let enc = failure_cred(&h).expect("credential written");
        assert!(!enc.contains("SECRETKEY") && !enc.contains("apprise.example"), "only the encrypted text is stored");
        let plain = unseal(&h, &enc).unwrap();
        assert!(plain.starts_with("# pxu-failure-notice transport=api\n"), "{plain}");
        assert!(plain.contains("/notify/SECRETKEY") && plain.contains("pxu update FAILED") && !plain.ends_with('\n'), "{plain}");
        // la clave solo viaja por stdin del cifrado, nunca por argv
        assert!(host_argv(&h).iter().flatten().all(|a| !a.contains("SECRETKEY")), "{:?}", host_argv(&h));
    }

    #[test]
    fn failure_credential_for_the_cli_carries_the_urls_and_follows_encrypted_config() {
        let h = World::demo().build();
        let urls = seal(&h, "tgram://TOK/1\nntfy://ntfy.sh/x").unwrap();
        let c = AppriseCfg { transport: "cli".into(), urls_enc: urls, retry_delays_ms: vec![], ..Default::default() };
        assert_eq!(write_failure_cred(&h, &c), Ok(true));
        let plain = unseal(&h, &failure_cred(&h).unwrap()).unwrap();
        assert_eq!(plain, "# pxu-failure-notice transport=cli\ntgram://TOK/1\nntfy://ntfy.sh/x");
    }

    #[test]
    fn without_apprise_no_failure_credential_is_written() {
        let h = World::demo().build();
        assert_eq!(write_failure_cred(&h, &AppriseCfg::default()), Ok(false));
        assert!(failure_cred(&h).is_none());
    }

    #[test]
    fn encrypted_key_is_decrypted_only_to_send_and_never_reaches_argv() {
        let h = World::demo().build();
        let enc = seal(&h, "SECRETKEY").unwrap();
        assert!(!enc.contains("SECRETKEY") && !enc.contains('\n') && !enc.is_empty());
        let c = AppriseCfg { api_url: "http://apprise.example:8000".into(), api_key_enc: enc, retry_delays_ms: vec![], ..Default::default() };
        assert!(c.api_key.is_empty() && c.has_key(), "nothing in clear in the loaded config");
        assert_eq!(resolve(&h, &c), Transport::Api);
        assert_eq!(send_once(&h, &c, &msg()), Ok(Transport::Api));
        let cmds = host_argv(&h);
        assert!(cmds.iter().any(|a| a == CREDS_DECRYPT), "{cmds:?}");
        assert!(cmds.iter().any(|a| a == &["curl", "--config", "-"]), "{cmds:?}");
        assert!(cmds.iter().flatten().all(|a| !a.contains("SECRETKEY")), "no secret in argv: {cmds:?}");
        // la clave descifrada va en la URL de curl (stdin), como en modo claro
        assert!(curl_config(&c.reveal(&h).unwrap(), &msg()).contains("/notify/SECRETKEY"));
    }

    #[test]
    fn encrypted_urls_are_decrypted_for_the_stateless_api() {
        let h = World::demo().build();
        let c = AppriseCfg { api_url: "http://apprise.example:8000".into(), urls_enc: seal(&h, "tgram://TOK/1\nntfy://ntfy.sh/x").unwrap(), retry_delays_ms: vec![], ..Default::default() };
        assert!(c.has_urls() && c.urls.is_empty());
        let r = c.reveal(&h).unwrap();
        assert_eq!(r.urls, ["tgram://TOK/1", "ntfy://ntfy.sh/x"]);
        assert!(curl_config(&r, &msg()).contains("tgram://TOK/1,ntfy://ntfy.sh/x"));
    }

    #[test]
    fn a_decrypt_failure_sends_nothing_and_never_falls_back_to_plain_text() {
        let h = World::demo().build();
        let c = AppriseCfg { api_url: "http://apprise.example:8000".into(), api_key_enc: "not-a-credential".into(), retry_delays_ms: vec![], ..Default::default() };
        let e = send_once(&h, &c, &msg()).unwrap_err();
        assert!(e.starts_with("cannot decrypt apprise secret: "), "{e}");
        assert!(host_argv(&h).iter().all(|a| a[0] != "curl"), "curl never ran");
        // no es transitorio (esperar no lo arregla) y el aviso queda en la cola de reintentos
        assert!(!transient(&e));
        assert_eq!(notify_with(&h, &c, &msg(), &[]), Outcome::Queued(e));
        assert_eq!(outbox_len(&h), 1);
    }

    #[test]
    fn a_plain_text_config_still_works_without_systemd_creds() {
        let h = World::demo().build();
        let c = AppriseCfg { api_url: "http://apprise.example:8000".into(), api_key: "PLAINKEY".into(), retry_delays_ms: vec![], ..Default::default() };
        assert!(c.has_plain_secret());
        assert_eq!(send_once(&h, &c, &msg()), Ok(Transport::Api));
        assert!(host_argv(&h).iter().all(|a| a[0] != "systemd-creds"));
    }

    #[test]
    fn scrub_also_hides_a_decrypted_value_echoed_by_curl() {
        let h = World::demo().build();
        let c = AppriseCfg { api_url: "http://a:1".into(), api_key_enc: seal(&h, "SECRETKEY").unwrap(), ..Default::default() };
        let r = c.reveal(&h).unwrap();
        assert!(!scrub(&r, "curl: (22) /notify/SECRETKEY").contains("SECRETKEY"));
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

    struct Split<'a>(&'a PathHost, MockHost);
    impl Host for Split<'_> {
        fn try_lock(&self, p: &Path, create: bool) -> std::io::Result<Option<crate::share::host::LockGuard>> {
            self.1.try_lock(p, create)
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

    #[test]
    fn failure_is_queued_then_flushed_when_curl_recovers() {
        let (dir, _log) = fake_curl("fail", 7);
        let h = PathHost(RealHost::new(), dir.clone());
        // la cola real escribe en /var/lib/pxu: usamos un MockHost para el FS y el PathHost solo para curl (`Split`)
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

    /// curl falso: las primeras `fails` llamadas salen con `exit` (y el mensaje de `--fail` si es 22), luego responde OK.
    fn flaky_curl(tag: &str, fails: u32, exit: i32, msg: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pxu-flaky-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let count = dir.join("count");
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\nn=$(cat '{0}' 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > '{0}'\nif [ $n -le {1} ]; then echo '{3}'; exit {2}; fi\necho '{{\"success\":true}}'\n",
            count.display(), fails, exit, msg
        );
        let p = dir.join("curl");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        (dir, count)
    }
    fn calls(count: &Path) -> u32 {
        std::fs::read_to_string(count).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }

    #[test]
    fn defaults_cover_about_ten_minutes() {
        let total: u64 = AppriseCfg::default().retry_delays_ms.iter().sum();
        assert_eq!(AppriseCfg::default().retry_delays_ms, vec![5_000, 30_000, 120_000, 300_000]);
        assert!((400_000..=700_000).contains(&total), "{total}");
    }

    #[test]
    fn transient_failures_are_retried_until_the_host_answers() {
        let (dir, count) = flaky_curl("424", 3, 22, "curl: (22) The requested URL returned error: 424");
        let h = ReadOnlyHost::new(PathHost(RealHost::new(), dir.clone()));
        let mut c = api("k");
        c.retry_delays_ms = vec![1, 1, 1, 1];
        assert_eq!(send(&h, &c, &msg()), Ok(Transport::Api));
        assert_eq!(calls(&count), 4, "3 failures + 1 success");
        // sin reintentos suficientes falla tras 1 + N intentos
        let (dir2, count2) = flaky_curl("conn", 9, 7, "curl: (7) Failed to connect");
        let h2 = ReadOnlyHost::new(PathHost(RealHost::new(), dir2.clone()));
        c.retry_delays_ms = vec![1, 1];
        assert!(send(&h2, &c, &msg()).is_err());
        assert_eq!(calls(&count2), 3);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(dir2);
    }

    #[test]
    fn a_400_or_401_is_not_retried() {
        for code in ["400", "401"] {
            let (dir, count) = flaky_curl(code, 9, 22, &format!("curl: (22) The requested URL returned error: {code}"));
            let h = ReadOnlyHost::new(PathHost(RealHost::new(), dir.clone()));
            let mut c = api("k");
            c.retry_delays_ms = vec![1, 1, 1];
            assert!(send(&h, &c, &msg()).is_err());
            assert_eq!(calls(&count), 1, "{code}: one attempt only");
            let _ = std::fs::remove_dir_all(dir);
        }
        assert!(transient("curl exited with 22: curl: (22) The requested URL returned error: 503"));
        assert!(!transient("curl exited with 22: curl: (22) The requested URL returned error: 404"));
    }

    #[test]
    fn no_transport_does_not_sleep_or_retry() {
        let h = ReadOnlyHost::new(World::demo().build());
        let c = AppriseCfg { retry_delays_ms: vec![60_000], ..Default::default() };
        let t0 = std::time::Instant::now();
        assert_eq!(notify(&h, &c, &msg()), Outcome::Disabled);
        assert!(send(&h, &c, &msg()).is_err());
        assert!(t0.elapsed().as_secs() < 5);
    }

    #[test]
    fn one_flush_pass_sends_at_most_max_flush_queued_notices() {
        let (dir, count) = flaky_curl("flush-cap", 0, 0, "");
        let h = PathHost(RealHost::new(), dir.clone());
        let s = Split(&h, World::demo().build());
        let c = api("k");
        for i in 0..MAX_FLUSH + 5 {
            s.1.write_file(&Path::new(OUTBOX_DIR).join(format!("{i:03}-000.json")), &serde_json::to_vec(&msg()).unwrap()).unwrap();
        }
        assert_eq!(flush_outbox(&s, &c), (MAX_FLUSH, 5), "the rest waits for the next pass");
        assert_eq!(calls(&count) as usize, MAX_FLUSH);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn flush_outbox_sends_nothing_while_the_outbox_lock_is_held() {
        let (dir, count) = flaky_curl("flush-lock", 0, 0, "");
        let h = PathHost(RealHost::new(), dir.clone());
        let s = Split(&h, World::demo().build());
        let c = api("k");
        s.1.write_file(&Path::new(OUTBOX_DIR).join("1-000.json"), &serde_json::to_vec(&msg()).unwrap()).unwrap();
        let guard = s.try_lock(&s.lock_dir().join("pxu-outbox.lock"), true).unwrap().expect("free");
        assert_eq!(flush_outbox(&s, &c), (0, 1));
        assert_eq!(notify_flush(&s, &c), None);
        assert_eq!(calls(&count), 0, "no curl while another process flushes");
        assert_eq!(outbox_len(&s), 1, "queue untouched");
        drop(guard);
        assert_eq!(flush_outbox(&s, &c), (1, 0));
        assert_eq!(calls(&count), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn flush_outbox_respects_the_outbox_lock_under_read_only_host() {
        let (dir, count) = flaky_curl("flush-ro", 0, 0, "");
        let h = PathHost(RealHost::new(), dir.clone());
        let ro = ReadOnlyHost::new(Split(&h, World::demo().build()));
        let c = api("k");
        ro.write_file(&Path::new(OUTBOX_DIR).join("1-000.json"), &serde_json::to_vec(&msg()).unwrap()).unwrap();
        let outbox_lock = ro.lock_dir().join("pxu-outbox.lock");
        let guard = ro.try_lock(&outbox_lock, true).unwrap().expect("allowed without update opt-in");
        assert!(ro.try_lock(&ro.lock_dir().join("pxu-run.lock"), true).is_err(), "other locks stay refused");
        assert_eq!(flush_outbox(&ro, &c), (0, 1));
        assert_eq!(calls(&count), 0, "no curl while the lock is held");
        assert_eq!(outbox_len(&ro), 1, "queue untouched");
        drop(guard);
        assert_eq!(flush_outbox(&ro, &c), (1, 0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn notify_flush_empties_the_queue_on_success_and_keeps_it_on_failure() {
        let (dir, count) = flaky_curl("flush-fail", 99, 7, "curl: (7) Failed to connect");
        let h = PathHost(RealHost::new(), dir.clone());
        let s = Split(&h, World::demo().build());
        let c = api("k");
        s.1.write_file(&Path::new(OUTBOX_DIR).join("1-000.json"), &serde_json::to_vec(&msg()).unwrap()).unwrap();
        s.1.write_file(&Path::new(OUTBOX_DIR).join("2-000.json"), &serde_json::to_vec(&msg()).unwrap()).unwrap();
        assert_eq!(notify_flush(&s, &c), Some((0, 2)), "failing: both stay queued");
        assert_eq!(calls(&count), 1, "stops at the first failure");
        let (dir2, count2) = flaky_curl("flush-ok", 0, 0, "");
        let h2 = PathHost(RealHost::new(), dir2.clone());
        let s2 = Split(&h2, World::demo().build());
        s2.1.write_file(&Path::new(OUTBOX_DIR).join("1-000.json"), &serde_json::to_vec(&msg()).unwrap()).unwrap();
        assert_eq!(notify_flush(&s2, &c), Some((1, 0)));
        assert_eq!(outbox_len(&s2), 0);
        // cola vacía: ni una llamada a curl
        assert_eq!(notify_flush(&s2, &c), Some((0, 0)));
        assert_eq!(calls(&count2), 1, "empty queue touches no network");
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
        let rep = check::run(&h, &inv, &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[], &AtomicBool::new(false), &mut |_| {});
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
        let rep = check::run(&h, &inv, &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[], &AtomicBool::new(false), &mut |_| {});
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
        let rep2 = check::run(&h2, &guests::load(&h2), &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[], &AtomicBool::new(false), &mut |_| {});
        let m2 = check_messages(&rep2, &cfg.apprise, &state::State::default());
        assert!(m2.iter().any(|m| m.0.body.contains("no module for kernel 6.17.13-2-pve")));
        // fallo del host: aviso de error aparte; los CT no se ven afectados
        let h3 = ReadOnlyHost::new(World::host_gpu().file("/exec/0/apt-list.out.exit", b"100").build());
        let rep3 = check::run(&h3, &guests::load(&h3), &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[], &AtomicBool::new(false), &mut |_| {});
        let m3 = check_messages(&rep3, &cfg.apprise, &state::State::default());
        assert!(m3.iter().any(|m| m.0.title == "pxu: host review failed" && m.0.event == "check_error"));
        assert!(!m3.iter().any(|m| m.0.title == "pxu: host review"));
        assert!(m3.iter().any(|m| m.0.title.starts_with("pxu: updates in")), "CT updates are still announced");
    }

    /// Host que falla a voluntad: escritura de la cola, `curl` y lock (A-06/A-07).
    struct Broken {
        inner: MockHost,
        fail_write: bool,
        fail_curl: bool,
        fail_lock: bool,
    }
    impl Broken {
        fn ok() -> Broken {
            Broken { inner: World::demo().build(), fail_write: false, fail_curl: false, fail_lock: false }
        }
    }
    impl Host for Broken {
        fn read_file(&self, p: &Path) -> io::Result<Vec<u8>> { self.inner.read_file(p) }
        fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> { self.inner.read_dir(p) }
        fn exists(&self, p: &Path) -> bool { self.inner.exists(p) }
        fn guest_exec(&self, v: u32, a: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<crate::share::host::ExitInfo> { self.inner.guest_exec(v, a, o, s) }
        fn host_cmd(&self, a: &[&str], o: &ExecOpts, s: &mut dyn LineSink) -> io::Result<crate::share::host::ExitInfo> {
            if self.fail_curl && a.first() == Some(&"curl") {
                return Ok(crate::share::host::ExitInfo { code: 7 });
            }
            self.inner.host_cmd(a, o, s)
        }
        fn write_file(&self, p: &Path, d: &[u8]) -> io::Result<()> {
            if self.fail_write && p.starts_with(OUTBOX_DIR) {
                return Err(io::Error::other("No space left on device"));
            }
            self.inner.write_file(p, d)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> { self.inner.remove_file(p) }
        fn lock_dir(&self) -> &Path { self.inner.lock_dir() }
        fn try_lock(&self, p: &Path, c: bool) -> io::Result<Option<crate::share::host::LockGuard>> {
            if self.fail_lock {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "lock dir not usable"));
            }
            self.inner.try_lock(p, c)
        }
        fn file_meta(&self, p: &Path) -> io::Result<crate::share::host::FileMeta> { self.inner.file_meta(p) }
        fn now(&self) -> std::time::SystemTime { self.inner.now() }
    }

    #[test]
    fn enqueue_two_writers_in_the_same_second_keep_both_messages() {
        let h = World::demo().build();
        enqueue(&h, &msg()).unwrap();
        enqueue(&h, &Message { title: "second".into(), ..msg() }).unwrap();
        assert_eq!(outbox_len(&h), 2, "same epoch second: distinct names, nothing overwritten");
    }

    #[test]
    fn enqueue_needs_the_files_lock_and_never_writes_without_it() {
        let h = World::demo().build();
        let g = h.try_lock(&h.lock_dir().join(OUTBOX_FILES_LOCK), true).unwrap().expect("free");
        assert_eq!(enqueue_wait(&h, &msg(), 0).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(outbox_len(&h), 0, "nothing written without the lock");
        drop(g);
        enqueue_wait(&h, &msg(), 0).unwrap();
        assert_eq!(outbox_len(&h), 1);
        let b = Broken { fail_lock: true, ..Broken::ok() };
        assert!(enqueue(&b, &msg()).is_err(), "an unusable lock dir is an error, not a silent continue");
        assert_eq!(outbox_len(&b), 0);
    }

    #[test]
    fn failed_queue_write_is_reported_as_lost_not_queued() {
        let b = Broken { fail_write: true, fail_curl: true, ..Broken::ok() };
        let c = api("k");
        match notify_with(&b, &c, &msg(), &[]) {
            Outcome::Lost(e) => assert!(e.contains("NOT queued") && e.contains("No space left"), "{e}"),
            o => panic!("{o:?}"),
        }
        assert_eq!(outbox_len(&b), 0);
    }

    #[test]
    fn check_does_not_mark_the_fingerprint_when_the_notice_was_neither_sent_nor_queued() {
        let b = Broken { fail_write: true, fail_curl: true, ..Broken::ok() };
        let inv = guests::load(&b);
        let mut cfg = Config::load(&b).unwrap();
        cfg.apprise.retry_delays_ms = vec![];
        let rep = check::run(&b, &inv, &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[], &AtomicBool::new(false), &mut |_| {});
        let lines = notify_check(&b, &cfg.apprise, &rep);
        assert!(lines.iter().any(|l| l.contains("NOT queued")), "{lines:?}");
        assert!(state::load_state(&b).notified.is_empty(), "no fingerprint without a confirmed send or queue");
        // con la cola escribible, el aviso fallido se encola y entonces sí se marca
        let q = Broken { fail_curl: true, ..Broken::ok() };
        let lines = notify_check(&q, &cfg.apprise, &rep);
        assert!(lines.iter().any(|l| l.ends_with("; queued")), "{lines:?}");
        assert!(!state::load_state(&q).notified.is_empty());
    }

    #[test]
    fn flush_does_not_run_without_the_outbox_lock() {
        let b = Broken { fail_lock: true, ..Broken::ok() };
        b.inner.write_file(&Path::new(OUTBOX_DIR).join("1-000.json"), &serde_json::to_vec(&msg()).unwrap()).unwrap();
        assert_eq!(flush_outbox(&b, &api("k")), (0, 1));
        assert_eq!(outbox_len(&b), 1, "queue intact");
        assert!(!host_argv(&b.inner).iter().any(|a| a[0] == "curl"), "nothing is sent without mutual exclusion");
    }

    #[test]
    fn curl_config_filter_accepts_only_what_pxu_generates() {
        // todo lo que genera pxu pasa, incluso con comillas, saltos y barras en los valores
        let mut weird = api("k\"quoted\\back");
        weird.tag = "a\\b".into();
        for c in [api("pxu-key"), api(""), weird, AppriseCfg { tag: "system".into(), ..api("k") }] {
            assert!(curl_config_is_safe(&curl_config(&c, &msg())), "{}", curl_config(&c, &msg()));
            assert!(curl_config_is_safe(&curl_config(&c, &failure_message())));
        }
        assert!(curl_config(&api("k"), &msg()).contains("proto-redir = \"=http,https\""));
        let good = curl_config(&api("k"), &msg());
        // una clave con salto de línea (inyección) produce una URL con caracteres de control: se rechaza entera
        assert!(!curl_config_is_safe(&curl_config(&api("k\noutput = \"/etc/cron.d/x"), &msg())));
        // opciones que escriben o leen archivos, otra config, otra transferencia o URLs que no son http(s)
        for bad in ["output = \"/etc/cron.d/x\"", "remote-name", "config = \"/tmp/c\"", "upload-file = \"/etc/shadow\"", "next", "cookie-jar = \"/tmp/j\"", "dump-header = \"/tmp/h\"", "trace = \"/tmp/t\"", "form = \"f=@/etc/shadow\"", "location", "unix-socket = \"/run/x\"", "url = \"file:///etc/shadow\"", "url = \"ftp://h/x\"", "proto = \"=file\"", "proto-redir = \"=all\"", "request = \"PUT\"", "max-time = 1; x", "data = \"@/etc/shadow\"", "data = \"{\"x\"}\"", "data = \"bad\\q\"", "url = no-quotes", "  output = \"/x\"", "output=\"/x\"", "OUTPUT = \"/x\""] {
            assert!(!curl_config_is_safe(&format!("{good}{bad}\n")), "must refuse: {bad}");
        }
        // sin url, con dos urls o vacío
        assert!(!curl_config_is_safe("silent\nfail\n") && !curl_config_is_safe(""));
        assert!(!curl_config_is_safe(&format!("{good}url = \"http://other/\"\n")));
        // un salto de línea sin escapar dentro de un valor lo rompería en otra línea: se rechaza
        assert!(!curl_config_is_safe("url = \"http://a/\noutput = \"/x\"\"\ndata = \"{}\"\n"));
    }
}
