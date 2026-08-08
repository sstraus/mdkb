//! Multi-repo daemon: manages N repositories through a single Unix domain socket.

use std::path::{Path, PathBuf};

pub mod config;
#[cfg(unix)]
pub mod ipc_server;
pub mod registry;
#[cfg(unix)]
pub mod singleton;
#[cfg(unix)]
pub mod spawn;

/// The executable a daemon was launched from, as it looked at launch.
///
/// The daemon is long-lived; the binary it was launched from is rebuilt
/// underneath it. Measured on one machine (story 020-9824): a daemon up since
/// 08-05 17:11 while `target/release/mdkb` was rebuilt 08-07 09:22, with schema
/// v18 and v19 landing in between. The running process is then a different build
/// from every one-shot CLI writer, and the two disagree about the schema they
/// share.
///
/// Identity is `(len, mtime)`, not a content hash: this is checked periodically
/// for the life of the process, and hashing a release binary on a timer to catch
/// an event that happens at most a few times a day is the wrong trade. A rebuild
/// always moves the mtime, and `cargo` writes a fresh file rather than patching
/// one in place. The failure mode of the cheap check is a spurious retirement
/// after a `touch`, which costs one respawn — the failure mode of not checking
/// is silent corruption.
#[derive(Debug, Clone)]
pub struct ExeIdentity {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl ExeIdentity {
    /// Record how `path` looks now.
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            path: path.to_path_buf(),
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    /// Record the currently-running executable, if it can be located.
    pub fn of_current() -> Option<Self> {
        Self::of(&std::env::current_exe().ok()?)
    }

    /// Has the file been replaced since [`ExeIdentity::of`] recorded it?
    ///
    /// A file that has since disappeared counts as changed: the daemon is then
    /// running code no file on disk corresponds to, which is skew of the worst
    /// kind — nothing can even say what version is serving the store.
    pub fn changed(&self) -> bool {
        match std::fs::metadata(&self.path) {
            Ok(meta) => meta.len() != self.len || meta.modified().ok() != self.modified,
            Err(_) => true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
