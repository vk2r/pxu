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

/// Lock de la transacción leer-modificar-guardar de `state.json` (A-07): check, update, hostupdate y avisos.
pub const STATE_LOCK: &str = "pxu-state.lock";
/// Espera máxima por el lock: la sección crítica es solo leer + escribir un JSON pequeño (nunca red ni comandos).
const STATE_LOCK_WAIT_MS: u64 = 10_000;

/// Lee `state.json`, aplica `f` y lo guarda, todo bajo `pxu-state.lock`: dos procesos nunca se pisan los cambios.
/// Sin el lock (ocupado más allá de la espera, o error al abrirlo) no escribe nada: falla en cerrado.
/// `f` no debe tomar este lock otra vez ni hacer trabajo lento.
pub fn update_state(host: &dyn Host, f: impl FnOnce(&mut State)) -> std::io::Result<()> {
    update_state_wait(host, STATE_LOCK_WAIT_MS, f)
}

/// Toma el lock propio `name` (de `lock_dir`) esperando hasta `wait_ms`. Un error al abrirlo no se ignora: nunca se sigue sin lock.
pub fn lock_wait(host: &dyn Host, name: &str, wait_ms: u64) -> std::io::Result<crate::share::host::LockGuard> {
    let path = host.lock_dir().join(name);
    let t0 = std::time::Instant::now();
    loop {
        match host.try_lock(&path, true)? {
            Some(g) => return Ok(g),
            None if t0.elapsed().as_millis() as u64 >= wait_ms => return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, format!("{name} is held by another pxu process"))),
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

fn update_state_wait(host: &dyn Host, wait_ms: u64, f: impl FnOnce(&mut State)) -> std::io::Result<()> {
    let _lock = lock_wait(host, STATE_LOCK, wait_ms)?;
    let mut st = load_state(host);
    f(&mut st);
    save_state(host, &st)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::share::host::ReadOnlyHost;
    use crate::share::mock::scenario::World;

    #[test]
    fn concurrent_updates_never_lose_each_others_changes() {
        let h = ReadOnlyHost::new(World::demo().build());
        std::thread::scope(|s| {
            for t in 0..4 {
                let h = &h;
                s.spawn(move || {
                    for i in 0..10 {
                        update_state(h, |st| {
                            st.notified.insert(format!("{t}-{i}"), "fp".into());
                        })
                        .unwrap();
                    }
                });
            }
        });
        assert_eq!(load_state(&h).notified.len(), 40, "every read-modify-write kept the others' keys");
    }

    #[test]
    fn update_state_fails_closed_while_the_state_lock_is_held() {
        let h = ReadOnlyHost::new(World::demo().build());
        update_state(&h, |st| {
            st.notified.insert("1".into(), "a".into());
        })
        .unwrap();
        let g = h.try_lock(&h.lock_dir().join(STATE_LOCK), true).unwrap().expect("allowed in read-only mode, free");
        let e = update_state_wait(&h, 0, |st| {
            st.notified.insert("2".into(), "b".into());
        })
        .unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(load_state(&h).notified.len(), 1, "nothing written without the lock");
        drop(g);
        update_state(&h, |st| {
            st.notified.insert("2".into(), "b".into());
        })
        .unwrap();
        assert_eq!(load_state(&h).notified.len(), 2);
    }
}
