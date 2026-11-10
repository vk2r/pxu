//! Módulos que usan a la vez la CLI y la TUI. Todo habla con `&dyn Host`; nada aquí toca la terminal.
pub mod config;
pub mod fmt;
pub mod host;
pub mod labels;
#[cfg(any(test, debug_assertions, feature = "mock"))]
pub mod mock;
pub mod model;
pub mod notify;
pub mod proxmox;
pub mod schedule;
pub mod state;
pub mod update;
