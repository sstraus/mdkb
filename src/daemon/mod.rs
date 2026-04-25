//! Multi-repo daemon: manages N repositories through a single Unix domain socket.

pub mod config;
pub mod ipc_server;
pub mod registry;
pub mod singleton;
pub mod spawn;
