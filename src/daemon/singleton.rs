//! Advisory file-lock singleton for the mdkb daemon.
//!
//! Uses `flock(LOCK_EX | LOCK_NB)` on Unix (via `fs4`) so that a second
//! `mdkb serve --daemon` invocation exits immediately instead of starting
//! a parallel instance. The kernel releases the lock when the holding
//! process dies, so stale locks from crashes are reclaimed automatically.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

/// RAII guard holding an exclusive advisory lock on a file.
///
/// Drop releases the lock (and on Unix, the kernel releases it on process
/// exit even without Drop running).
#[derive(Debug)]
pub struct LockGuard {
    file: File,
    path: PathBuf,
}

impl LockGuard {
    /// Path of the lock file this guard holds.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Overwrite the lock file contents with `pid\n`. The advisory lock
    /// remains held; on crash the kernel releases the lock but the pid
    /// stays for post-mortem inspection until the next daemon writes its own.
    pub fn write_pid(&mut self, pid: u32) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.file.set_len(0)?;
        writeln!(self.file, "{pid}")?;
        self.file.sync_all()?;
        Ok(())
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Best-effort unlock. If this fails the OS will clean up on exit.
        let _ = FileExt::unlock(&self.file);
    }
}

/// Error returned by [`acquire_singleton_lock`].
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// Another process (or the current one) already holds the lock.
    #[error("another mdkb daemon is already running (lock held)")]
    AlreadyHeld,

    /// I/O failure while creating or locking the file.
    #[error("failed to acquire singleton lock: {0}")]
    Io(#[from] std::io::Error),
}

/// Canonical path for the daemon singleton lock / pid file
/// (`~/.mdkb/daemon.pid`). Falls back to `/tmp/mdkb-daemon.pid` if the
/// home directory cannot be resolved.
pub fn default_lock_path() -> PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().join(".mdkb").join("daemon.pid"))
        .unwrap_or_else(|| PathBuf::from("/tmp/mdkb-daemon.pid"))
}

/// Read the pid stored in the lock file, if any and if parseable.
///
/// Used to format "daemon already running at <pid>" messages. Returns
/// `None` if the file is missing, empty, or doesn't contain a valid pid.
pub fn read_pid(path: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}

/// Acquire the daemon singleton lock.
///
/// Creates the lock file (and parent directories) if missing, then takes
/// an exclusive non-blocking advisory lock. Returns [`AcquireError::AlreadyHeld`]
/// if another holder exists — callers should exit with a clear message.
pub fn acquire_singleton_lock(path: &Path) -> Result<LockGuard, AcquireError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(LockGuard {
            file,
            path: path.to_path_buf(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Err(AcquireError::AlreadyHeld),
        Err(e) => Err(AcquireError::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn acquire_creates_lock_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let guard = acquire_singleton_lock(&path).unwrap();
        assert_eq!(guard.path(), path);
        assert!(path.exists());
    }

    #[test]
    fn acquire_twice_in_same_process_fails() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let _a = acquire_singleton_lock(&path).unwrap();
        let err = acquire_singleton_lock(&path).unwrap_err();
        assert!(matches!(err, AcquireError::AlreadyHeld));
    }

    #[test]
    fn drop_releases_lock() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lock");
        drop(acquire_singleton_lock(&path).unwrap());
        let _again = acquire_singleton_lock(&path).unwrap();
    }
}
