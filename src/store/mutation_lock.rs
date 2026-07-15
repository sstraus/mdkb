//! Cross-process serialization for index-wide mutations.
//!
//! SQLite serializes individual write transactions, but an `mdkb update` is a
//! larger logical operation: collection discovery, document writes, embedding
//! backfills, and projection reconciliation. `compact` and corruption recovery
//! also replace or rewrite the database file. A project-scoped advisory lock
//! prevents those operations from overlapping across the daemon and one-shot
//! CLI processes.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;

use crate::error::{Error, ErrorKind, Result};

/// RAII guard for the project index mutation lock.
#[derive(Debug)]
pub struct MutationGuard {
    file: File,
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        // Best effort; the kernel also releases the advisory lock on exit.
        let _ = FileExt::unlock(&self.file);
    }
}

/// Stable sidecar path used to coordinate mutations of `db_path`.
pub fn lock_path(db_path: &Path) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(".mutation.lock");
    PathBuf::from(path)
}

/// Acquire the blocking, exclusive mutation lock for an index.
///
/// The lock file is deliberately retained after release. Its contents are
/// diagnostic only; correctness comes from the OS advisory lock.
pub fn acquire(db_path: &Path, operation: &str) -> Result<MutationGuard> {
    let path = lock_path(db_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: parent.to_path_buf(),
                operation: format!("create mutation-lock directory: {e}"),
            })
        })?;
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| {
            Error::from(ErrorKind::Io {
                path: path.clone(),
                operation: format!("open mutation lock: {e}"),
            })
        })?;

    file.lock_exclusive().map_err(|e| {
        Error::from(ErrorKind::Io {
            path: path.clone(),
            operation: format!("acquire mutation lock: {e}"),
        })
    })?;

    // Helpful when diagnosing a long wait. Failure to write metadata does not
    // invalidate the lock itself.
    let _ = file.set_len(0);
    let _ = writeln!(file, "pid={} operation={operation}", std::process::id());
    let _ = file.sync_data();

    Ok(MutationGuard { file })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn second_mutation_waits_until_first_guard_drops() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        let first = acquire(&db, "first").unwrap();

        let (tx, rx) = mpsc::channel();
        let db2 = db.clone();
        let waiter = std::thread::spawn(move || {
            let _second = acquire(&db2, "second").unwrap();
            tx.send(()).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the second mutation must remain blocked while the first holds the lock"
        );
        drop(first);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("second mutation should proceed after release");
        waiter.join().unwrap();
    }

    #[test]
    fn lock_is_scoped_to_database_path() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.sqlite");
        let b = dir.path().join("b.sqlite");
        let _ga = acquire(&a, "a").unwrap();
        let _gb = acquire(&b, "b").unwrap();
        assert_ne!(lock_path(&a), lock_path(&b));
    }
}
