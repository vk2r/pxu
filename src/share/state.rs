//! Estado persistido (`/var/lib/pxu/state.json`): último chequeo por CT + huellas ya notificadas.
use crate::share::host::Host;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const STATE_PATH: &str = "/var/lib/pxu/state.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pkg {
    pub name: String,
    pub cur: String,
    pub new: String,
    #[serde(default)]
    pub sec: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckInfo {
    pub checked_at: u64,
    pub pm: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub index_age_s: u64,
    #[serde(default)]
    pub reboot_required: bool,
    #[serde(default)]
    pub packages: Vec<Pkg>,
    #[serde(default)]
    pub held: Vec<String>,
    /// FNV-1a de `nombre=versión_nueva` ordenado (dedup de avisos, ARCHITECTURE §2.4).
    #[serde(default)]
    pub fingerprint: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub checks: BTreeMap<String, CheckInfo>,
    /// vmid → huella ya avisada (la clave "0" es la revisión del host).
    #[serde(default)]
    pub notified: BTreeMap<String, String>,
    /// Última revisión del host Proxmox (solo lectura).
    #[serde(default)]
    pub host: Option<crate::share::proxmox::hostcheck::HostInfo>,
}

/// Archivo ausente o ilegible ⇒ estado vacío (nunca es un error para el listado).
pub fn load_state(host: &dyn Host) -> State {
    let Ok(bytes) = host.read_file(Path::new(STATE_PATH)) else { return State::default() };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

pub fn save_state(host: &dyn Host, st: &State) -> std::io::Result<()> {
    let mut st = st.clone();
    st.schema = 1;
    let json = serde_json::to_vec_pretty(&st).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    host.write_file(Path::new(STATE_PATH), &json)
}

pub fn load(host: &dyn Host) -> BTreeMap<u32, CheckInfo> {
    load_state(host).checks.into_iter().filter_map(|(k, v)| Some((k.parse().ok()?, v))).collect()
}

pub fn fingerprint(pkgs: &[Pkg]) -> String {
    let mut v: Vec<String> = pkgs.iter().map(|p| format!("{}={}", p.name, p.new)).collect();
    v.sort();
    let mut h: u64 = 0xcbf29ce484222325;
    for b in v.join("\n").bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}
