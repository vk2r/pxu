//! Revisión del host Proxmox (fase 1, SOLO LECTURA): updates pendientes, repos, reinicio pendiente y GPUs (driver NVIDIA/DKMS).
//! Los parsers son puros (texto → datos). Nada instala, quita ni actualiza paquetes del host: los únicos comandos son
//! `apt-get update` (solo con refresco), `apt list --upgradable`, `apt-get -s … full-upgrade`, `lspci -nn` y `dkms status`.
use crate::share::host::{ExecOpts, Host, IoPrio};
use crate::share::model::{Guest, GuestKind, RunState};
use crate::share::proxmox::conf::Conf;
use crate::share::proxmox::guests::Inventory;
use crate::share::state::{CheckInfo, Pkg};
use crate::share::update::check::CheckOpts;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Comandos del host de esta fase (lista exacta; `host_cmd_is_read_only` solo deja pasar estos argv).
pub const APT_UPDATE: &[&str] = &["apt-get", "update"];
pub const APT_LIST: &[&str] = &["apt", "list", "--upgradable"];
pub const APT_SIM: &[&str] = &["apt-get", "-s", "-o", "Debug::NoLocking=1", "full-upgrade"];
pub const LSPCI: &[&str] = &["lspci", "-nn"];
pub const DKMS: &[&str] = &["dkms", "status"];
/// Dentro de un CT en marcha (solo lectura): versión del driver NVIDIA que ve el CT.
pub const NVIDIA_SMI: &[&str] = &["nvidia-smi", "--query-gpu=driver_version", "--format=csv,noheader"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PkgClass {
    Kernel,
    Firmware,
    Pve,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostPkg {
    pub name: String,
    pub cur: String,
    pub new: String,
    #[serde(default)]
    pub sec: bool,
    pub class: PkgClass,
}

impl HostPkg {
    /// Grupo de la vista de detalle: `kernel` · `firmware` · `pve` · `security` (seguridad que no es de los anteriores) · `other`.
    pub fn group(&self) -> &'static str {
        match self.class {
            PkgClass::Kernel => "kernel",
            PkgClass::Firmware => "firmware",
            PkgClass::Pve => "pve",
            PkgClass::Other if self.sec => "security",
            PkgClass::Other => "other",
        }
    }
}

pub fn classify(name: &str) -> PkgClass {
    if name == "pve-firmware" {
        PkgClass::Firmware
    } else if name.starts_with("proxmox-kernel-") || name.starts_with("proxmox-headers-") || name.starts_with("pve-kernel-") || name.starts_with("pve-headers-") || name == "proxmox-default-kernel" || name == "proxmox-default-headers" {
        PkgClass::Kernel
    } else if name.starts_with("pve-") || name.starts_with("libpve-") || name == "proxmox-ve" || name == "qemu-server" || name == "lxc-pve" || name == "proxmox-widget-toolkit" {
        PkgClass::Pve
    } else {
        PkgClass::Other
    }
}

/// `apt list --upgradable`: `name/origen[,origen] versión arch [upgradable from: actual]`. Ignora `Listing...` y avisos.
pub fn parse_upgradable(text: &str) -> Vec<HostPkg> {
    let mut v = Vec::new();
    for l in text.lines() {
        let Some((name, rest)) = l.split_once('/') else { continue };
        if name.is_empty() || name.contains(char::is_whitespace) {
            continue;
        }
        let mut f = rest.split_whitespace();
        let (Some(origins), Some(new)) = (f.next(), f.next()) else { continue };
        let cur = rest.split_once("[upgradable from:").and_then(|(_, c)| c.split(']').next()).map(|c| c.trim().to_string()).unwrap_or_default();
        let sec = origins.split(',').any(|o| o.ends_with("-security") || o.ends_with("/security"));
        v.push(HostPkg { name: name.into(), cur, new: new.into(), sec, class: classify(name) });
    }
    v
}

/// `apt-get -s full-upgrade`: bloqueado si quitaría `proxmox-ve` o `pve-manager` (o si apt no logra resolverlo).
pub fn parse_sim(text: &str) -> Option<String> {
    let mut err = None;
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix("Remv ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            if name == "proxmox-ve" || name == "pve-manager" {
                return Some(format!("full-upgrade would remove {name}"));
            }
        } else if err.is_none() && l.starts_with("E: ") {
            err = Some(format!("apt cannot resolve the upgrade ({})", l.trim_start_matches("E: ").trim()));
        }
    }
    err
}

// ---------------------------------------------------------------- repos

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repo {
    pub file: String,
    pub uri: String,
    pub suites: String,
    pub components: String,
    pub enabled: bool,
}

impl Repo {
    pub fn is_enterprise(&self) -> bool {
        self.uri.contains("enterprise.proxmox.com")
    }
    /// pve-no-subscription · pve-enterprise · ceph… · "debian" para los demás.
    pub fn label(&self) -> String {
        if self.uri.contains("proxmox.com") {
            let kind = if self.is_enterprise() { "enterprise" } else if self.components.contains("test") { "test" } else { "no-subscription" };
            let who = if self.uri.contains("/ceph") { "ceph" } else { "proxmox" };
            format!("{who} {kind}")
        } else {
            "other".into()
        }
    }
}

/// `.sources` (deb822): estrofas separadas por línea en blanco; `Enabled: no` la desactiva.
pub fn parse_deb822(file: &str, text: &str) -> Vec<Repo> {
    let mut out = Vec::new();
    for stanza in text.split("\n\n") {
        let (mut uris, mut suites, mut comps, mut enabled, mut types) = (String::new(), String::new(), String::new(), true, String::new());
        for l in stanza.lines() {
            if l.trim_start().starts_with('#') {
                continue;
            }
            let Some((k, v)) = l.split_once(':') else { continue };
            let v = v.trim();
            match k.trim().to_ascii_lowercase().as_str() {
                "uris" => uris = v.into(),
                "suites" => suites = v.into(),
                "components" => comps = v.into(),
                "types" => types = v.into(),
                "enabled" => enabled = !matches!(v.to_ascii_lowercase().as_str(), "no" | "false" | "0"),
                _ => {}
            }
        }
        if !uris.is_empty() && (types.is_empty() || types.split_whitespace().any(|t| t == "deb")) {
            for u in uris.split_whitespace() {
                out.push(Repo { file: file.into(), uri: u.into(), suites: suites.clone(), components: comps.clone(), enabled });
            }
        }
    }
    out
}

/// `.list` / `sources.list`: `deb [opciones] uri suite comp…`; las líneas comentadas no cuentan.
pub fn parse_list(file: &str, text: &str) -> Vec<Repo> {
    let mut out = Vec::new();
    for l in text.lines() {
        let l = l.trim();
        let Some(rest) = l.strip_prefix("deb ").or_else(|| l.strip_prefix("deb\t")) else { continue };
        let mut t: Vec<&str> = rest.split_whitespace().collect();
        if t.first().is_some_and(|o| o.starts_with('[')) {
            while !t.is_empty() && !t[0].ends_with(']') {
                t.remove(0);
            }
            if !t.is_empty() {
                t.remove(0);
            }
        }
        let [uri, suite, comps @ ..] = t.as_slice() else { continue };
        out.push(Repo { file: file.into(), uri: (*uri).into(), suites: (*suite).into(), components: comps.join(" "), enabled: true });
    }
    out
}

/// `/etc/subscription`: el formato (`status: active`) NO está verificado contra un host real; ausente o sin `active` = sin suscripción.
pub fn subscription_active(text: &str) -> bool {
    text.lines().any(|l| l.split_once(':').is_some_and(|(k, v)| k.trim().eq_ignore_ascii_case("status") && v.trim().eq_ignore_ascii_case("active")))
}

fn read_text(host: &dyn Host, p: &Path) -> Option<String> {
    host.read_file(p).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// Lee `/etc/apt/sources.list` y `sources.list.d/*.{list,sources}`. Nunca escribe.
pub fn read_repos(host: &dyn Host) -> Vec<Repo> {
    let mut v = Vec::new();
    if let Some(t) = read_text(host, Path::new("/etc/apt/sources.list")) {
        v.extend(parse_list("/etc/apt/sources.list", &t));
    }
    let mut files = host.read_dir(Path::new("/etc/apt/sources.list.d")).unwrap_or_default();
    files.sort();
    for p in files {
        let name = p.to_string_lossy().into_owned();
        let Some(t) = read_text(host, &p) else { continue };
        if name.ends_with(".sources") {
            v.extend(parse_deb822(&name, &t));
        } else if name.ends_with(".list") {
            v.extend(parse_list(&name, &t));
        }
    }
    v
}

// ---------------------------------------------------------------- GPUs del host

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
}

impl Vendor {
    pub fn short(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NV",
            Vendor::Amd => "AMD",
            Vendor::Intel => "INT",
        }
    }
    pub fn long(self) -> &'static str {
        match self {
            Vendor::Nvidia => "NVIDIA",
            Vendor::Amd => "AMD",
            Vendor::Intel => "Intel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostGpu {
    pub slot: String,
    pub vendor: Vendor,
    pub name: String,
}

/// `lspci -nn`: solo clases VGA (0300) / 3D (0302) / Display (0380) de NVIDIA (10de), AMD (1002) e Intel (8086).
pub fn parse_lspci(text: &str) -> Vec<HostGpu> {
    let mut v = Vec::new();
    for l in text.lines() {
        let Some((slot, rest)) = l.split_once(' ') else { continue };
        let Some((class, desc)) = rest.split_once("]: ") else { continue };
        let Some(code) = class.rsplit_once('[').map(|(_, c)| c) else { continue };
        if !matches!(code, "0300" | "0302" | "0380") {
            continue;
        }
        let Some((name, ids)) = desc.rsplit_once(" [") else { continue };
        let ids = ids.split(']').next().unwrap_or("");
        let vendor = match ids.split(':').next() {
            Some("10de") => Vendor::Nvidia,
            Some("1002") => Vendor::Amd,
            Some("8086") => Vendor::Intel,
            _ => continue,
        };
        v.push(HostGpu { slot: slot.into(), vendor, name: name.trim().into() });
    }
    v
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dkms {
    pub module: String,
    pub version: String,
    pub kernel: String,
    pub status: String,
}

/// `dkms status`: `nvidia/595.112.07, 6.17.13-2-pve, x86_64: installed` (o el formato viejo `nvidia, 595…, 6.x, x86_64: installed`).
pub fn parse_dkms(text: &str) -> Vec<Dkms> {
    let mut v = Vec::new();
    for l in text.lines() {
        let Some((head, status)) = l.split_once(':') else { continue };
        let f: Vec<String> = head.replace('/', ",").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if let [module, version, kernel, _arch, ..] = f.as_slice() {
            v.push(Dkms { module: module.clone(), version: version.clone(), kernel: kernel.clone(), status: status.trim().into() });
        }
    }
    v
}

/// Compara versiones de kernel por tramos numéricos (`6.17.13-2-pve` > `6.17.9-1-pve`).
pub fn kernel_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let nums = |s: &str| s.split(|c: char| !c.is_ascii_digit()).filter(|t| !t.is_empty()).map(|t| t.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    nums(a).cmp(&nums(b))
}

pub fn newest_kernel(names: &[String]) -> Option<String> {
    names.iter().filter(|n| n.ends_with("-pve")).max_by(|a, b| kernel_cmp(a, b)).cloned()
}

/// `NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.112.07  Tue …` → `595.112.07`.
pub fn parse_nvrm(text: &str) -> Option<String> {
    let l = text.lines().find(|l| l.contains("NVRM version"))?;
    l.split_whitespace().find(|t| is_dotted_version(t)).map(String::from)
}

fn is_dotted_version(t: &str) -> bool {
    let parts: Vec<&str> = t.split('.').collect();
    parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Salida de `nvidia-smi --query-gpu=driver_version`: la primera línea con forma de versión.
pub fn parse_smi_version(lines: &[String]) -> Option<String> {
    lines.iter().map(|l| l.trim()).find(|l| is_dotted_version(l)).map(String::from)
}

/// `Driver/library version mismatch` en la salida de nvidia-smi: el CT trae otra versión que el módulo del host.
pub fn smi_mismatch(lines: &[String]) -> bool {
    lines.iter().any(|l| l.to_ascii_lowercase().contains("version mismatch"))
}

/// `libnvidia-ml.so.595.112.07` → `595.112.07` (ignora `libnvidia-ml.so.1`).
pub fn lib_version(file: &str) -> Option<String> {
    let v = file.strip_prefix("libnvidia-ml.so.")?;
    is_dotted_version(v).then(|| v.to_string())
}

/// Versión de `libcuda1` en `var/lib/dpkg/status` (`595.112.07-1` → `595.112.07`).
pub fn dpkg_cuda_version(status: &str) -> Option<String> {
    for stanza in status.split("\n\n") {
        if stanza.lines().any(|l| l == "Package: libcuda1") && stanza.lines().any(|l| l == "Status: install ok installed") {
            let v = stanza.lines().find_map(|l| l.strip_prefix("Version: "))?;
            let v = v.rsplit_once(':').map_or(v, |(_, r)| r);
            let v = v.split('-').next().unwrap_or(v);
            return is_dotted_version(v).then(|| v.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------- uso de GPU por guest (desde el .conf)

/// Qué dispositivos de GPU toca un guest según su `.conf` (sin ejecutar nada).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct GpuUse {
    pub nvidia: bool,
    pub dri: bool,
    pub kfd: bool,
    /// VM con `hostpciN` (passthrough).
    pub pass: bool,
}

impl GpuUse {
    pub fn any(&self) -> bool {
        self.nvidia || self.dri || self.kfd || self.pass
    }
}

/// CT OCI (`entrypoint:` / `cmd:`): no tiene sentido sondearlo como un CT de sistema.
pub fn is_oci(conf: &Conf) -> bool {
    conf.get("entrypoint").is_some() || conf.get("cmd").is_some()
}

pub fn gpu_use(conf: &Conf, kind: GuestKind) -> GpuUse {
    match kind {
        GuestKind::Vm => GpuUse { pass: !conf.indexed("hostpci").is_empty(), ..GpuUse::default() },
        GuestKind::Lxc if is_oci(conf) => GpuUse::default(),
        GuestKind::Lxc => {
            let mut u = GpuUse::default();
            let devs = conf.indexed("dev").into_iter().map(|(_, v)| v);
            let mounts = conf.main.iter().filter(|(k, _)| k == "lxc.mount.entry").map(|(_, v)| v.as_str());
            for v in devs.chain(mounts) {
                u.nvidia |= v.contains("/dev/nvidia") || v.contains("dev/nvidia");
                u.dri |= v.contains("/dev/dri") || v.contains("dev/dri");
                u.kfd |= v.contains("/dev/kfd") || v.contains("dev/kfd");
            }
            u
        }
        GuestKind::Host => GpuUse::default(),
    }
}

// ---------------------------------------------------------------- resultado

/// Versión NVIDIA vista en un CT: `version` None = sin datos (`NV ?`); `mismatch` = nvidia-smi dijo «version mismatch».
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CtNvidia {
    pub version: Option<String>,
    #[serde(default)]
    pub mismatch: bool,
    /// `smi` (CT en marcha) | `rootfs` (CT apagado, sin arrancarlo) | `none`.
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostInfo {
    pub checked_at: u64,
    /// Motivo si la revisión del host falló (fila en error). El resto de campos quedan vacíos.
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub packages: Vec<HostPkg>,
    /// `full-upgrade` quitaría proxmox-ve / pve-manager: el host queda bloqueado con este motivo.
    #[serde(default)]
    pub blocked: Option<String>,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub repo_warnings: Vec<String>,
    #[serde(default)]
    pub running_kernel: String,
    #[serde(default)]
    pub newest_kernel: String,
    /// Motivo del reinicio pendiente (vacío = no hace falta).
    #[serde(default)]
    pub reboot_reason: String,
    #[serde(default)]
    pub gpus: Vec<HostGpu>,
    /// Versión del driver NVIDIA cargado en el host (`/proc/driver/nvidia/version`).
    #[serde(default)]
    pub nvidia_driver: Option<String>,
    #[serde(default)]
    pub dkms: Vec<Dkms>,
    /// `Some(kernel)`: el host tiene GPU NVIDIA y no hay módulo para ese kernel (el más nuevo instalado).
    #[serde(default)]
    pub nvidia_missing_for: Option<String>,
    /// vmid → versión NVIDIA del CT.
    #[serde(default)]
    pub ct_nvidia: BTreeMap<String, CtNvidia>,
    /// Avisos no fatales (p. ej. `apt-get update` falló y se usó el índice anterior).
    #[serde(default)]
    pub notes: Vec<String>,
}

impl HostInfo {
    pub fn errored(now: u64, why: &str) -> HostInfo {
        HostInfo { checked_at: now, error: Some(why.into()), ..HostInfo::default() }
    }
    pub fn updates(&self) -> usize {
        self.packages.len()
    }
    pub fn security(&self) -> usize {
        self.packages.iter().filter(|p| p.sec).count()
    }
    pub fn reboot_pending(&self) -> bool {
        !self.reboot_reason.is_empty()
    }
    pub fn has_nvidia(&self) -> bool {
        self.gpus.iter().any(|g| g.vendor == Vendor::Nvidia)
    }
    /// `Some(true)` = módulo OK para el kernel más nuevo · `Some(false)` = falta (`NV ↑`) · `None` = el host no tiene NVIDIA.
    pub fn nvidia_ok(&self) -> Option<bool> {
        self.has_nvidia().then(|| self.nvidia_missing_for.is_none())
    }
    /// CTs cuyo driver NVIDIA difiere del host o dio «version mismatch».
    pub fn outdated_cts(&self) -> Vec<u32> {
        self.ct_nvidia.iter().filter(|(_, c)| ct_outdated(c, self.nvidia_driver.as_deref())).filter_map(|(k, _)| k.parse().ok()).collect()
    }
    /// Huella de los hallazgos (dedup de avisos): cambia si cambia algo que merezca un aviso nuevo.
    pub fn fingerprint(&self) -> String {
        let mut v: Vec<String> = self.packages.iter().map(|p| format!("{}={}", p.name, p.new)).collect();
        v.sort();
        v.push(format!("reboot={}", self.reboot_reason));
        v.push(format!("gpu={}", self.nvidia_missing_for.clone().unwrap_or_default()));
        v.push(format!("cts={:?}", self.outdated_cts()));
        v.push(format!("err={}", self.error.clone().unwrap_or_default()));
        crate::share::state::fingerprint(&v.into_iter().map(|s| Pkg { name: s, cur: String::new(), new: String::new(), sec: false }).collect::<Vec<_>>())
    }
    /// Entrada para el listado: el host se comporta como una fila más (UPD, OS, reinicio).
    pub fn as_check(&self) -> CheckInfo {
        let packages: Vec<Pkg> = self.packages.iter().map(|p| Pkg { name: p.name.clone(), cur: p.cur.clone(), new: p.new.clone(), sec: p.sec }).collect();
        CheckInfo { checked_at: self.checked_at, pm: "apt".into(), os: "proxmox ve".into(), index_age_s: 0, reboot_required: self.reboot_pending(), fingerprint: crate::share::state::fingerprint(&packages), packages, held: vec![] }
    }
}

fn ct_outdated(c: &CtNvidia, host: Option<&str>) -> bool {
    c.mismatch || matches!((c.version.as_deref(), host), (Some(v), Some(h)) if v != h)
}

// ---------------------------------------------------------------- etiqueta de la columna GPU

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuLabel {
    pub text: String,
    /// `↑` (amarillo): hay algo por actualizar.
    pub warn: bool,
}

const MESA_PREFIXES: &[&str] = &["mesa-", "libdrm", "rocm", "hip", "intel-media", "libva"];

fn gpu_pending(g: &Guest) -> bool {
    g.check.as_ref().is_some_and(|c| c.packages.iter().any(|p| MESA_PREFIXES.iter().any(|m| p.name.starts_with(m))))
}

/// Un solo vendedor no NVIDIA en el host ⇒ `/dev/dri` sin más datos es de ese vendedor; si hay dos (o no se sabe), `None`.
fn dri_vendor(host: Option<&HostInfo>) -> Option<Vendor> {
    let h = host?;
    let mut others: Vec<Vendor> = h.gpus.iter().map(|g| g.vendor).filter(|v| *v != Vendor::Nvidia).collect();
    others.sort_by_key(|v| v.short());
    others.dedup();
    (others.len() == 1).then(|| others[0])
}

/// Etiqueta de la columna GPU del guest, o `None` (`—`) si no usa GPU. NVIDIA gana sobre AMD en el mismo CT.
/// Con el driver incluido en las actualizaciones programadas (`nvidia_auto`) la etiqueta NVIDIA lleva `+drv`.
pub fn gpu_label(g: &Guest, host: Option<&HostInfo>) -> Option<GpuLabel> {
    let mut l = gpu_label_base(g, host)?;
    if g.nvidia_auto && l.text.starts_with("NV ") {
        l.text.push_str("+drv");
    }
    Some(l)
}

fn gpu_label_base(g: &Guest, host: Option<&HostInfo>) -> Option<GpuLabel> {
    let l = |t: &str, warn| Some(GpuLabel { text: t.into(), warn });
    if g.kind == GuestKind::Host {
        return match host?.nvidia_ok()? {
            true => l("NV ✓", false),
            false => l("NV ↑", true),
        };
    }
    let u = g.gpu;
    if g.kind == GuestKind::Vm {
        return u.pass.then(|| GpuLabel { text: "pass".into(), warn: false });
    }
    if u.nvidia {
        let Some(ct) = host.and_then(|h| h.ct_nvidia.get(&g.vmid.to_string())) else { return l("NV ?", false) };
        if ct.version.is_none() && !ct.mismatch {
            return l("NV ?", false);
        }
        return if ct_outdated(ct, host.and_then(|h| h.nvidia_driver.as_deref())) { l("NV ↑", true) } else { l("NV ✓", false) };
    }
    let vendor = if u.kfd { Some(Vendor::Amd) } else if u.dri { dri_vendor(host) } else { return None };
    let tag = vendor.map_or("GPU", |v| v.short());
    if g.check.is_none() {
        return l(&format!("{tag} ?"), false);
    }
    if gpu_pending(g) { l(&format!("{tag} ↑"), true) } else { l(&format!("{tag} ✓"), false) }
}

/// Texto de la sección `gpu` del detalle del guest (sin sección si no usa GPU).
pub fn gpu_detail(g: &Guest, host: Option<&HostInfo>) -> Option<String> {
    let u = g.gpu;
    if g.kind == GuestKind::Vm {
        return u.pass.then(|| "PCI passthrough (hostpci)".to_string());
    }
    let mut parts: Vec<String> = Vec::new();
    if u.nvidia {
        let ct = host.and_then(|h| h.ct_nvidia.get(&g.vmid.to_string()));
        let hv = host.and_then(|h| h.nvidia_driver.as_deref());
        parts.push(match (ct.and_then(|c| c.version.as_deref()), ct.is_some_and(|c| c.mismatch), hv) {
            (Some(v), _, Some(h)) if v == h => format!("NVIDIA {v} (= host)"),
            (Some(v), _, Some(h)) => format!("NVIDIA {v} (host {h})"),
            (Some(v), _, None) => format!("NVIDIA {v}"),
            (None, true, _) => "NVIDIA driver/library version mismatch".into(),
            (None, false, _) => "NVIDIA version unknown".into(),
        });
    }
    if u.kfd || (u.dri && !u.nvidia) {
        let v = if u.kfd { Some(Vendor::Amd) } else { dri_vendor(host) };
        parts.push(v.map_or("GPU", |v| v.long()).to_string());
    } else if u.dri && u.nvidia {
        parts.push("DRI".to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

// ---------------------------------------------------------------- lectura del host

struct Out(Vec<String>);
impl crate::share::host::LineSink for Out {
    fn line(&mut self, l: &str) {
        self.0.push(crate::share::fmt::clean(l));
    }
}

fn run(host: &dyn Host, argv: &[&str], o: &CheckOpts, long: bool) -> std::io::Result<(i32, Vec<String>)> {
    let opts = ExecOpts {
        nice: Some(o.nice),
        ionice: Some(IoPrio::Idle),
        timeout_s: Some(if long { o.timeout_s * 3 } else { o.timeout_s }),
        env: vec![("LC_ALL".into(), "C".into())],
        ..Default::default()
    };
    let mut out = Out(Vec::new());
    let e = host.host_cmd(argv, &opts, &mut out)?;
    Ok((e.code, out.0))
}

fn now(host: &dyn Host) -> u64 {
    host.now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// ¿Hay módulo nvidia para `kernel`? Por `dkms status` (installed) o por el archivo del módulo.
fn nvidia_module_for(host: &dyn Host, dkms: &[Dkms], kernel: &str) -> bool {
    if dkms.iter().any(|d| d.module.starts_with("nvidia") && d.kernel == kernel && d.status.starts_with("installed")) {
        return true;
    }
    let base = PathBuf::from("/lib/modules").join(kernel);
    ["updates/dkms/nvidia.ko", "updates/dkms/nvidia.ko.xz", "updates/dkms/nvidia.ko.zst", "updates/dkms/nvidia.ko.gz", "updates/nvidia.ko", "kernel/drivers/video/nvidia.ko"].iter().any(|r| host.exists(&base.join(r)))
}

/// Revisa el host: nunca instala nada. Un fallo de apt devuelve el `HostInfo` con `error` (la revisión de los CT sigue igual).
pub fn check_host(host: &dyn Host, inv: &Inventory, o: &CheckOpts) -> Option<HostInfo> {
    let at = now(host);
    let mut info = HostInfo { checked_at: at, ..HostInfo::default() };
    if o.refresh {
        match run(host, APT_UPDATE, o, true) {
            Ok((0, _)) => {}
            Ok((rc, _)) => info.notes.push(format!("apt-get update failed (rc {rc}): using the previous index")),
            Err(e) => info.notes.push(format!("apt-get update: {e}: using the previous index")),
        }
    }
    match run(host, APT_LIST, o, false) {
        Ok((0, lines)) => info.packages = parse_upgradable(&lines.join("\n")),
        Ok((rc, _)) => return Some(HostInfo::errored(at, &format!("apt list failed (rc {rc})"))),
        // sin `apt` no es un host Debian/Proxmox: no hay nada que revisar (y no se inventa una fila en error)
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => return Some(HostInfo::errored(at, &format!("apt list: {e}"))),
    }
    // La simulación solo informa: nunca llega a instalar (`-s`).
    match run(host, APT_SIM, o, false) {
        Ok((_, lines)) => info.blocked = parse_sim(&lines.join("\n")),
        Err(e) => info.notes.push(format!("apt-get -s: {e}")),
    }
    info.repos = read_repos(host);
    let subscribed = read_text(host, Path::new("/etc/subscription")).is_some_and(|t| subscription_active(&t));
    if !subscribed && info.repos.iter().any(|r| r.enabled && r.is_enterprise()) {
        info.repo_warnings.push("pve-enterprise repo is active without a subscription: updates will fail (401)".into());
    }
    info.running_kernel = read_text(host, Path::new("/proc/sys/kernel/osrelease")).map(|s| s.trim().to_string()).unwrap_or_default();
    let installed: Vec<String> = host.read_dir(Path::new("/lib/modules")).unwrap_or_default().iter().filter_map(|p| p.file_name()?.to_str().map(String::from)).collect();
    info.newest_kernel = newest_kernel(&installed).unwrap_or_default();
    info.reboot_reason = if host.exists(Path::new("/var/run/reboot-required")) {
        "reboot-required".into()
    } else if !info.newest_kernel.is_empty() && !info.running_kernel.is_empty() && info.running_kernel != info.newest_kernel {
        format!("running kernel {} is older than installed {}", info.running_kernel, info.newest_kernel)
    } else {
        String::new()
    };
    match run(host, LSPCI, o, false) {
        Ok((0, lines)) => info.gpus = parse_lspci(&lines.join("\n")),
        Ok((rc, _)) => info.notes.push(format!("lspci failed (rc {rc})")),
        Err(e) => info.notes.push(format!("lspci: {e}")),
    }
    if info.has_nvidia() {
        info.nvidia_driver = read_text(host, Path::new("/proc/driver/nvidia/version")).and_then(|t| parse_nvrm(&t));
        if let Ok((_, lines)) = run(host, DKMS, o, false) {
            info.dkms = parse_dkms(&lines.join("\n"));
        }
        if !info.newest_kernel.is_empty() && !nvidia_module_for(host, &info.dkms, &info.newest_kernel) {
            info.nvidia_missing_for = Some(info.newest_kernel.clone());
        }
        for g in inv.guests.iter().filter(|g| g.kind == GuestKind::Lxc && g.gpu.nvidia && g.state != RunState::Remote && !g.template) {
            info.ct_nvidia.insert(g.vmid.to_string(), ct_nvidia(host, g, o));
        }
    }
    Some(info)
}

/// Versión NVIDIA de un CT: en marcha → `nvidia-smi`; apagado → rootfs (NUNCA se arranca). Sin datos ⇒ `version: None`.
fn ct_nvidia(host: &dyn Host, g: &Guest, o: &CheckOpts) -> CtNvidia {
    if g.is_running() {
        let opts = ExecOpts { nice: Some(o.nice), timeout_s: Some(o.timeout_s), ..Default::default() };
        let mut out = Out(Vec::new());
        let _ = host.guest_exec(g.vmid, NVIDIA_SMI, &opts, &mut out);
        let mismatch = smi_mismatch(&out.0);
        return CtNvidia { version: parse_smi_version(&out.0), mismatch, source: "smi".into() };
    }
    match rootfs_nvidia(host, g) {
        Some(v) => CtNvidia { version: Some(v), mismatch: false, source: "rootfs".into() },
        None => CtNvidia { version: None, mismatch: false, source: "none".into() },
    }
}

/// Rutas donde puede estar montado el rootfs de un CT apagado (dir/ZFS). LVM-thin no se monta: sin datos.
fn rootfs_candidates(g: &Guest) -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from(format!("/var/lib/lxc/{}/rootfs", g.vmid))];
    if let Some(vol) = g.conf.get("rootfs").and_then(|r| r.split(',').next()).and_then(|r| r.split_once(':')).map(|(_, v)| v) {
        if vol.starts_with("subvol-") {
            v.push(PathBuf::from("/rpool/data").join(vol));
        }
    }
    v
}

fn rootfs_nvidia(host: &dyn Host, g: &Guest) -> Option<String> {
    for root in rootfs_candidates(g) {
        for lib in ["usr/lib/x86_64-linux-gnu", "usr/lib64", "usr/lib"] {
            for p in host.read_dir(&root.join(lib)).unwrap_or_default() {
                if let Some(v) = p.file_name().and_then(|n| n.to_str()).and_then(lib_version) {
                    return Some(v);
                }
            }
        }
        if let Some(t) = read_text(host, &root.join("var/lib/dpkg/status")) {
            if let Some(v) = dpkg_cuda_version(&t) {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPGRADABLE: &str = "Listing...\n\
pve-manager/stable 9.1.4 all [upgradable from: 9.1.1]\n\
proxmox-kernel-6.17/stable 6.17.13-2 amd64 [upgradable from: 6.17.9-1]\n\
proxmox-default-kernel/stable 2.0.2 all [upgradable from: 2.0.1]\n\
pve-firmware/stable 3.17-2 all [upgradable from: 3.16-1]\n\
libc6/stable-security 2.36-9+deb12u10 amd64 [upgradable from: 2.36-9+deb12u9]\n\
openssl/stable-security,stable 3.0.17-1 amd64 [upgradable from: 3.0.16-1]\n\
htop/stable 3.4.1-1 amd64 [upgradable from: 3.4.0-1]\n\
WARNING: apt does not have a stable CLI interface. Use with caution in scripts.\n";

    #[test]
    fn apt_list_is_parsed_and_classified() {
        let p = parse_upgradable(UPGRADABLE);
        assert_eq!(p.len(), 7);
        let g = |n: &str| p.iter().find(|x| x.name == n).unwrap();
        assert_eq!(g("pve-manager").class, PkgClass::Pve);
        assert_eq!(g("pve-manager").cur, "9.1.1");
        assert_eq!(g("proxmox-kernel-6.17").class, PkgClass::Kernel);
        assert_eq!(g("proxmox-default-kernel").class, PkgClass::Kernel);
        assert_eq!(g("pve-firmware").class, PkgClass::Firmware);
        assert!(g("libc6").sec && g("libc6").group() == "security");
        assert!(g("openssl").sec, "a -security origin in a list of origins counts");
        assert!(!g("htop").sec && g("htop").group() == "other");
    }

    #[test]
    fn apt_list_empty_and_noise() {
        assert!(parse_upgradable("").is_empty());
        assert!(parse_upgradable("Listing... Done\nN: something\n").is_empty());
    }

    #[test]
    fn simulation_that_removes_proxmox_ve_blocks_the_host() {
        let sim = "Inst foo [1] (2 stable)\nRemv proxmox-ve [9.1.0]\nRemv libfoo [1]\n";
        assert_eq!(parse_sim(sim).as_deref(), Some("full-upgrade would remove proxmox-ve"));
        assert!(parse_sim("Remv pve-manager [9]\n").unwrap().contains("pve-manager"));
        assert_eq!(parse_sim("Inst a [1] (2 x)\nRemv libold [1]\n"), None, "removing other packages does not block");
        assert!(parse_sim("E: Unable to correct problems, you have held broken packages.\n").unwrap().contains("held broken"));
    }

    const DEB822_ENT: &str = "Types: deb\nURIs: https://enterprise.proxmox.com/debian/pve\nSuites: trixie\nComponents: pve-enterprise\nSigned-By: /usr/share/keyrings/proxmox-archive-keyring.gpg\n";

    #[test]
    fn deb822_and_legacy_repos() {
        let r = parse_deb822("/etc/apt/sources.list.d/pve-enterprise.sources", DEB822_ENT);
        assert_eq!(r.len(), 1);
        assert!(r[0].is_enterprise() && r[0].enabled && r[0].label() == "proxmox enterprise");
        let off = parse_deb822("x.sources", &format!("{DEB822_ENT}Enabled: no\n"));
        assert!(!off[0].enabled);
        let l = parse_list("x.list", "# deb http://disabled\ndeb [arch=amd64 signed-by=/k] http://download.proxmox.com/debian/pve trixie pve-no-subscription\ndeb http://deb.debian.org/debian trixie main contrib\n");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].components, "pve-no-subscription");
        assert_eq!(l[0].label(), "proxmox no-subscription");
        assert_eq!(l[1].uri, "http://deb.debian.org/debian");
        let c = parse_list("c.list", "deb http://download.proxmox.com/debian/ceph-squid trixie no-subscription\ndeb https://enterprise.proxmox.com/debian/ceph-squid trixie enterprise\n");
        assert_eq!(c[0].label(), "ceph no-subscription");
        assert_eq!(c[1].label(), "ceph enterprise");
    }

    #[test]
    fn subscription_states() {
        assert!(subscription_active("key: pve2c-abc\nstatus: Active\n"));
        assert!(!subscription_active("status: notfound\n"));
        assert!(!subscription_active(""));
    }

    const LSPCI_TXT: &str = "00:00.0 Host bridge [0600]: Advanced Micro Devices, Inc. [AMD] Root Complex [1022:14e8]\n\
01:00.0 VGA compatible controller [0300]: NVIDIA Corporation GA104GL [RTX A4000] [10de:24b0] (rev a1)\n\
01:00.1 Audio device [0403]: NVIDIA Corporation GA104 High Definition Audio Controller [10de:228b] (rev a1)\n\
c5:00.0 VGA compatible controller [0300]: Advanced Micro Devices, Inc. [AMD/ATI] Phoenix3 [1002:1900] (rev c3)\n\
00:02.0 Display controller [0380]: Intel Corporation UHD Graphics [8086:9a60]\n\
05:00.0 VGA compatible controller [0300]: ASPEED Technology, Inc. ASPEED Graphics Family [1a03:2000]\n";

    #[test]
    fn lspci_picks_only_known_gpu_vendors() {
        let g = parse_lspci(LSPCI_TXT);
        assert_eq!(g.iter().map(|x| (x.vendor, x.slot.as_str())).collect::<Vec<_>>(), vec![(Vendor::Nvidia, "01:00.0"), (Vendor::Amd, "c5:00.0"), (Vendor::Intel, "00:02.0")]);
        assert_eq!(g[0].name, "NVIDIA Corporation GA104GL [RTX A4000]");
        assert_eq!(g[1].name, "Advanced Micro Devices, Inc. [AMD/ATI] Phoenix3");
        assert!(parse_lspci("").is_empty());
    }

    #[test]
    fn dkms_formats_and_kernel_order() {
        let d = parse_dkms("nvidia/595.112.07, 6.17.13-2-pve, x86_64: installed\nnvidia/595.112.07, 6.17.9-1-pve, x86_64: installed (WARNING! Diff between built and installed module!)\nold, 1.0, 6.1.0-pve, x86_64: built\n");
        assert_eq!(d.len(), 3);
        assert_eq!((d[0].module.as_str(), d[0].version.as_str(), d[0].kernel.as_str()), ("nvidia", "595.112.07", "6.17.13-2-pve"));
        assert!(d[1].status.starts_with("installed"));
        assert_eq!(d[2].status, "built");
        assert_eq!(newest_kernel(&["6.17.9-1-pve".into(), "6.17.13-2-pve".into(), "6.8.12-4-pve".into(), "modules.dep".into()]).as_deref(), Some("6.17.13-2-pve"));
        assert_eq!(newest_kernel(&[]), None);
    }

    #[test]
    fn nvidia_version_sources() {
        assert_eq!(parse_nvrm("NVRM version: NVIDIA UNIX x86_64 Kernel Module  595.112.07  Tue Oct 21 2025\nGCC version: gcc\n").as_deref(), Some("595.112.07"));
        assert_eq!(parse_nvrm("nothing"), None);
        assert_eq!(parse_smi_version(&["595.112.07".into()]).as_deref(), Some("595.112.07"));
        assert_eq!(parse_smi_version(&["Failed to initialize NVML: Driver/library version mismatch".into()]), None);
        assert!(smi_mismatch(&["Failed to initialize NVML: Driver/library version mismatch".into()]));
        assert_eq!(lib_version("libnvidia-ml.so.595.112.07").as_deref(), Some("595.112.07"));
        assert_eq!(lib_version("libnvidia-ml.so.1"), None);
        let st = "Package: libcuda1\nStatus: install ok installed\nVersion: 595.112.07-1\n\nPackage: other\nVersion: 1\n";
        assert_eq!(dpkg_cuda_version(st).as_deref(), Some("595.112.07"));
        assert_eq!(dpkg_cuda_version("Package: libcuda1\nStatus: deinstall ok config-files\nVersion: 5.1-1\n"), None);
    }

    fn conf(lines: &str) -> Conf {
        Conf::parse(lines)
    }

    #[test]
    fn gpu_use_from_conf() {
        let ct134 = conf("dev0: /dev/dri/renderD128,gid=104\nlxc.mount.entry: /dev/nvidia0 dev/nvidia0 none bind,optional,create=file\nlxc.cgroup2.devices.allow: c 195:* rwm\n");
        assert_eq!(gpu_use(&ct134, GuestKind::Lxc), GpuUse { nvidia: true, dri: true, kfd: false, pass: false });
        let ct131 = conf("lxc.mount.entry: /dev/dri dev/dri none bind,optional,create=dir\nlxc.mount.entry: /dev/kfd dev/kfd none bind,optional,create=file\n");
        assert_eq!(gpu_use(&ct131, GuestKind::Lxc), GpuUse { nvidia: false, dri: true, kfd: true, pass: false });
        assert!(!gpu_use(&conf("hostname: plain\nnet0: name=eth0\n"), GuestKind::Lxc).any());
        assert!(gpu_use(&conf("hostpci0: 0000:01:00,pcie=1\n"), GuestKind::Vm).pass);
        assert!(!gpu_use(&conf("cores: 2\n"), GuestKind::Vm).any());
        let oci = conf("entrypoint: /bin/app\ndev0: /dev/dri/renderD128\n");
        assert!(!gpu_use(&oci, GuestKind::Lxc).any(), "OCI CTs are excluded");
    }

    // ---- integración con el host simulado ----
    use crate::share::config::Config;
    use crate::share::host::ReadOnlyHost;
    use crate::share::mock::scenario::{Ct, Pkg as WPkg, World};
    use crate::share::mock::{MockHost, Recorded};

    const SIM_REMOVES_PVE: &str = "Remv proxmox-ve [9.1.1]\nRemv pve-manager [9.1.1]\nInst foo (2 stable [amd64])\n";
    const ENTERPRISE: &str = "Types: deb\nURIs: https://enterprise.proxmox.com/debian/pve\nSuites: trixie\nComponents: pve-enterprise\nSigned-By: /usr/share/keyrings/proxmox-archive-keyring.gpg\n";

    /// Caso `host` del simulador: A4000 + Phoenix3, kernel 6.17.9-1 en marcha y 6.17.13-2 instalado.
    fn gpu_world() -> World {
        World::host_gpu()
    }

    fn review(w: World) -> (ReadOnlyHost<MockHost>, Option<HostInfo>, Inventory) {
        let h = ReadOnlyHost::new(w.build());
        let inv = crate::share::proxmox::guests::load(&h);
        let cfg = Config::load(&h).unwrap();
        let info = check_host(&h, &inv, &CheckOpts::from_config(&cfg, Some(false)));
        (h, info, inv)
    }

    fn label(inv: &Inventory, info: &HostInfo, vmid: u32) -> Option<String> {
        let g = inv.guests.iter().find(|g| g.vmid == vmid).unwrap();
        gpu_label(g, Some(info)).map(|l| format!("{}{}", l.text, if l.warn { "!" } else { "" }))
    }

    #[test]
    fn host_review_classifies_upgradable_and_reports_gpus_and_reboot() {
        let (h, info, _) = review(gpu_world());
        let i = info.unwrap();
        assert!(i.error.is_none() && i.blocked.is_none());
        assert_eq!(i.updates(), 7);
        assert_eq!(i.security(), 2, "-security origin (libc6, openssl)");
        let g = |n: &str| i.packages.iter().find(|p| p.name == n).unwrap().group();
        assert_eq!((g("proxmox-kernel-6.17"), g("proxmox-default-kernel"), g("pve-firmware"), g("pve-manager"), g("libc6")), ("kernel", "kernel", "firmware", "pve", "security"));
        assert_eq!(i.gpus.len(), 2);
        assert_eq!(i.nvidia_driver.as_deref(), Some("595.112.07"));
        assert_eq!(i.newest_kernel, "6.17.13-2-pve");
        assert!(i.reboot_pending(), "{:?}", i.reboot_reason);
        assert!(i.reboot_reason.contains("6.17.9-1-pve"), "{}", i.reboot_reason);
        assert!(i.repo_warnings.is_empty());
        // los únicos comandos del host son de lectura
        let host_cmds: Vec<Vec<String>> = h.inner().recorded().into_iter().filter_map(|r| if let Recorded::Host(a) = r { Some(a) } else { None }).collect();
        let allowed: Vec<Vec<String>> = [APT_LIST, APT_SIM, LSPCI, DKMS].iter().map(|a| a.iter().map(|s| s.to_string()).collect()).collect();
        assert_eq!(host_cmds, allowed, "sin refresco no hay apt-get update");
    }

    #[test]
    fn reboot_pending_by_flag_file_and_not_when_kernel_is_current() {
        let (_, i, _) = review(gpu_world().file("/proc/sys/kernel/osrelease", b"6.17.13-2-pve\n"));
        assert!(!i.unwrap().reboot_pending());
        let (_, i, _) = review(gpu_world().file("/proc/sys/kernel/osrelease", b"6.17.13-2-pve\n").file("/var/run/reboot-required", b""));
        let i = i.unwrap();
        assert!(i.reboot_pending());
        assert_eq!(i.reboot_reason, "reboot-required");
    }

    #[test]
    fn simulation_that_removes_proxmox_ve_blocks_the_host_row() {
        let (_, i, inv) = review(gpu_world().host_out("apt-sim", SIM_REMOVES_PVE));
        let i = i.unwrap();
        let why = i.blocked.as_deref().unwrap();
        assert!(why.contains("proxmox-ve"), "{why}");
        assert_eq!(i.updates(), 7, "the package list is still reported");
        assert!(inv.guests.iter().any(|g| g.vmid == 134), "CT review is unaffected");
    }

    #[test]
    fn enterprise_repo_without_subscription_warns_and_with_one_does_not() {
        let w = || gpu_world().file("/etc/apt/sources.list.d/pve.sources", ENTERPRISE.as_bytes());
        let (_, i, _) = review(w());
        let i = i.unwrap();
        assert_eq!(i.repo_warnings.len(), 1, "{:?}", i.repo_warnings);
        assert!(i.repo_warnings[0].contains("enterprise"));
        let (_, i, _) = review(gpu_world());
        assert!(i.unwrap().repo_warnings.is_empty(), "no-subscription repo is fine");
    }

    #[test]
    fn dkms_module_present_or_missing_for_the_newest_kernel() {
        let (_, i, _) = review(gpu_world());
        let i = i.unwrap();
        assert_eq!(i.nvidia_missing_for, None);
        assert_eq!(i.nvidia_ok(), Some(true));
        let (_, i, _) = review(gpu_world().host_out("dkms", "nvidia/595.112.07, 6.17.9-1-pve, x86_64: installed\n"));
        let i = i.unwrap();
        assert_eq!(i.nvidia_missing_for.as_deref(), Some("6.17.13-2-pve"));
        assert_eq!(i.nvidia_ok(), Some(false));
        let hg = Guest { kind: GuestKind::Host, ..crate::share::proxmox::guests::load(&ReadOnlyHost::new(gpu_world().build())).guests[0].clone() };
        assert_eq!(gpu_label(&hg, Some(&i)).map(|l| (l.text, l.warn)), Some(("NV ↑".into(), true)));
    }

    #[test]
    fn host_without_nvidia_has_no_gpu_label() {
        let (_, i, _) = review(gpu_world().host_out("lspci", "c5:00.0 Display controller [0380]: Advanced Micro Devices, Inc. [AMD/ATI] Phoenix3 [1002:1900]\n"));
        let i = i.unwrap();
        assert!(!i.has_nvidia() && i.nvidia_ok().is_none() && i.nvidia_missing_for.is_none());
    }

    #[test]
    fn gpu_column_cases() {
        let (_, i, inv) = review(gpu_world());
        let i = i.unwrap();
        assert_eq!(label(&inv, &i, 134).as_deref(), Some("NV ✓"), "nvidia+dri, same as host");
        assert_eq!(label(&inv, &i, 131).as_deref(), Some("AMD ✓"));
        assert_eq!(label(&inv, &i, 140), None, "no GPU use: —");
        assert_eq!(label(&inv, &i, 200).as_deref(), Some("pass"));
        // NVIDIA desactualizado en el CT
        let (_, i, inv) = review(gpu_world().file("/exec/134/nvidia-smi.out", b"580.95.05\n"));
        assert_eq!(label(&inv, &i.unwrap(), 134).as_deref(), Some("NV ↑!"));
        // mesa pendiente en el CT AMD
        let (_, i, inv) = review(gpu_world().ct(Ct::new(131).conf("lxc.mount.entry: /dev/dri dev/dri none bind,optional,create=dir").conf("lxc.mount.entry: /dev/kfd dev/kfd none bind,optional,create=file").updates(vec![WPkg::new("mesa-vulkan-drivers", "1", "2")])));
        assert_eq!(label(&inv, &i.unwrap(), 131).as_deref(), Some("AMD ↑!"));
    }

    #[test]
    fn stopped_nvidia_ct_is_never_started_and_without_data_is_unknown() {
        let w = gpu_world();
        let (h, i, inv) = review(w);
        let i = i.unwrap();
        assert_eq!(label(&inv, &i, 135).as_deref(), Some("NV ?"));
        assert!(h.inner().recorded().iter().all(|r| !matches!(r, Recorded::Guest(135, _))), "no command inside the stopped CT");
        assert!(h.inner().recorded().iter().all(|r| !matches!(r, Recorded::Host(a) if a[0] == "pct")), "never pct start");
        // con datos en el rootfs: se lee la versión sin arrancar nada
        let w = gpu_world().file("/var/lib/lxc/135/rootfs/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.595.112.07", b"");
        let (_, i, inv) = review(w);
        assert_eq!(label(&inv, &i.unwrap(), 135).as_deref(), Some("NV ✓"));
    }

    #[test]
    fn oci_ct_with_gpu_devices_is_excluded() {
        let (_, i, inv) = review(gpu_world());
        assert_eq!(label(&inv, &i.unwrap(), 150), None);
    }

    #[test]
    fn host_failure_is_isolated_from_the_ct_review() {
        // `apt list` falla ⇒ la fila del host queda en error con el motivo
        let w = gpu_world().host_out("apt-list", "E: boom\n").file("/exec/0/apt-list.out.exit", b"100");
        let h = ReadOnlyHost::new(w.build());
        let inv = crate::share::proxmox::guests::load(&h);
        let cfg = Config::load(&h).unwrap();
        let r = crate::share::update::check::run(&h, &inv, &cfg, &CheckOpts::from_config(&cfg, Some(false)), &[], &std::sync::atomic::AtomicBool::new(false), &mut |_| {});
        let host = r.host.as_ref().unwrap();
        assert!(host.error.as_deref().unwrap().contains("rc 100"), "{:?}", host.error);
        assert_eq!(r.results.len(), 5, "CTs are still checked");
        assert!(r.results.iter().all(|c| c.status != crate::share::update::check::Status::Error), "{:?}", r.results.iter().map(|c| (c.vmid, c.status)).collect::<Vec<_>>());
        // sin apt (NotFound) no hay fila
        let h = ReadOnlyHost::new(World::new().ct(Ct::new(101)).build());
        let inv = crate::share::proxmox::guests::load(&h);
        assert!(check_host(&h, &inv, &CheckOpts::from_config(&Config::load(&h).unwrap(), Some(false))).is_none());
    }

    #[test]
    fn refresh_runs_only_apt_get_update_and_a_failure_is_a_soft_note() {
        let h = ReadOnlyHost::allowing_refresh(gpu_world().host_out("apt-update", "E: Failed\n").file("/exec/0/apt-update.out.exit", b"100").build());
        let inv = crate::share::proxmox::guests::load(&h);
        let cfg = Config::load(&h).unwrap();
        let i = check_host(&h, &inv, &CheckOpts::from_config(&cfg, Some(true))).unwrap();
        assert!(i.error.is_none() && i.notes.iter().any(|n| n.contains("apt-get update failed")), "{:?}", i.notes);
        assert_eq!(h.inner().recorded().first(), Some(&Recorded::Host(APT_UPDATE.iter().map(|s| s.to_string()).collect())));
    }
}
