//! Multi-repo daemon: manages N repositories through a single Unix domain socket.

pub mod config;
#[cfg(unix)]
pub mod ipc_server;
pub mod registry;
#[cfg(unix)]
pub mod singleton;
#[cfg(unix)]
pub mod spawn;
