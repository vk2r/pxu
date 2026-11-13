use crate::share::proxmox::conf::Conf;
use crate::share::state::CheckInfo;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GuestKind {
    Lxc,
    Vm,
    /// Fila sintética del propio host Proxmox (vmid 0): solo existe si hay una revisión del host guardada.
    Host,
}

impl GuestKind {
    pub fn label(self) -> &'static str {
        match self {
            GuestKind::Lxc => "LXC",
            GuestKind::Vm => "VM",
            GuestKind::Host => "HOST",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Stopped,
    /// Guest de otro nodo del clúster: estado no consultable sin pvesh.
    Remote,
}

#[derive(Debug, Clone, Serialize)]
pub struct Guest {
    pub vmid: u32,
    pub kind: GuestKind,
    pub node: String,
    pub name: String,
    pub state: RunState,
    pub lock: Option<String>,
    pub template: bool,
    pub tags: Vec<String>,
    pub ostype: String,
    /// "debian 12" si hay chequeo; si no, el `ostype` del .conf.
    pub os_label: String,
    pub cores: Option<u64>,
    pub memory_mb: Option<u64>,
    pub mem_used_bytes: Option<u64>,
    /// Tiempo de CPU acumulado del cgroup (µs); el % requiere dos muestras.
    pub cpu_usec: Option<u64>,
    /// Suma de discos asignados (rootfs + mp / discos de VM), en bytes.
    pub disk_bytes: Option<u64>,
    pub onboot: bool,
    pub auto_update: bool,
    /// `[update] host_nvidia_auto`: las actualizaciones programadas incluyen el driver NVIDIA (en un CT con GPU NVIDIA, sus librerías: siguen al host).
    pub nvidia_auto: bool,
    /// Modo de backup efectivo antes del update (`[[ct]] backup` o `[backup] mode`): auto | snapshot | vzdump | none.
    pub backup: String,
    pub conf_path: String,
    pub conf: Conf,
    pub check: Option<CheckInfo>,
    /// Dispositivos de GPU que toca según su `.conf` (columna GPU).
    pub gpu: crate::share::proxmox::hostcheck::GpuUse,
}

impl Guest {
    pub fn is_running(&self) -> bool {
        self.state == RunState::Running
    }
    pub fn updates(&self) -> usize {
        self.check.as_ref().map_or(0, |c| c.packages.len())
    }
    pub fn security(&self) -> usize {
        self.check.as_ref().map_or(0, |c| c.packages.iter().filter(|p| p.sec).count())
    }
}
