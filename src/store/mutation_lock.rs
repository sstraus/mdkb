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

/// Sidecar advertising that at least one process holds an open connection to
/// `db_path`.
///
/// Deliberately a different file from [`lock_path`]: the mutation lock is taken
/// briefly around index-wide writes, this one is held for the whole life of a
/// connection, and the two must never contend.
pub fn live_lock_path(db_path: &Path) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(".live.lock");
    PathBuf::from(path)
}

/// Open (creating if needed) a lock sidecar without touching its contents.
fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: parent.to_path_buf(),
                operation: format!("create mutation-lock directory: {e}"),
            })
        })?;
    }

    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            Error::from(ErrorKind::Io {
                path: path.to_path_buf(),
                operation: format!("open mutation lock: {e}"),
            })
        })
}

/// Announce a live connection to `db_path`; hold the guard for as long as that
/// connection stays open.
///
/// Shared, so every process holds it at once and nobody ever waits. It exists
/// only so [`try_acquire_live_exclusive`] can answer one question: is it safe to
/// rename the database files? Renaming under an open connection recycles the
/// path onto a second inode, and SQLite derives `-wal`/`-shm` from the path —
/// so a surviving connection can end up writing its frames into the WAL of the
/// *replacement* database, which is how a quarantine seeds the next corruption.
pub fn acquire_live_shared(db_path: &Path) -> Result<MutationGuard> {
    let path = live_lock_path(db_path);
    let file = open_lock_file(&path)?;
    FileExt::lock_shared(&file).map_err(|e| {
        Error::from(ErrorKind::Io {
            path,
            operation: format!("acquire live-connection lock: {e}"),
        })
    })?;
    Ok(MutationGuard { file })
}

/// Take the live lock exclusively, without waiting.
///
/// `Ok(None)` means some process holds an open connection to `db_path`, so its
/// files must be left where they are.
pub fn try_acquire_live_exclusive(db_path: &Path) -> Result<Option<MutationGuard>> {
    let path = live_lock_path(db_path);
    let file = open_lock_file(&path)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(MutationGuard { file })),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(Error::from(ErrorKind::Io {
            path,
            operation: format!("probe live-connection lock: {e}"),
        })),
    }
}

/// Acquire the blocking, exclusive mutation lock for an index.
///
/// The lock file is deliberately retained after release. Its contents are
/// diagnostic only; correctness comes from the OS advisory lock.
pub fn acquire(db_path: &Path, operation: &str) -> Result<MutationGuard> {
    let path = lock_path(db_path);
    let mut file = open_lock_file(&path)?;

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
    fn live_holders_coexist_and_veto_the_exclusive_probe() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");

        let first = acquire_live_shared(&db).unwrap();
        let second = acquire_live_shared(&db).unwrap();
        assert!(
            try_acquire_live_exclusive(&db).unwrap().is_none(),
            "an open connection must veto renaming the database files"
        );

        drop(first);
        assert!(
            try_acquire_live_exclusive(&db).unwrap().is_none(),
            "the remaining holder still vetoes it"
        );

        drop(second);
        assert!(
            try_acquire_live_exclusive(&db).unwrap().is_some(),
            "with the last holder gone the files can be renamed"
        );
    }

    #[test]
    fn live_lock_and_mutation_lock_never_contend() {
        // Different sidecars on purpose: the mutation lock is exclusive and
        // short, the live lock is shared and lasts a whole connection. Sharing
        // one file would let a live connection block every index-wide write.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        assert_ne!(lock_path(&db), live_lock_path(&db));

        let _live = acquire_live_shared(&db).unwrap();
        let (tx, rx) = mpsc::channel();
        let db2 = db.clone();
        let waiter = std::thread::spawn(move || {
            let _mutation = acquire(&db2, "update").unwrap();
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("an index-wide mutation must not wait on live connections");
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
