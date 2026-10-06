//! Cross-process serialization for index-wide mutations.
//!
//! SQLite serializes individual write transactions, but an `mdkb update` is a
//! larger logical operation: collection discovery, document writes, embedding
//! backfills, and projection reconciliation. `compact` and corruption recovery
//! also replace or rewrite the database file. A project-scoped advisory lock
//! prevents those operations from overlapping across the daemon and one-shot
//! CLI processes. A broader writer-admission lock also serializes smaller
//! writes such as hook telemetry with those index-wide operations.

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

/// Sidecar shared by every process that can write the main project store.
///
/// This is deliberately broader than [`lock_path`]. The latter protects one
/// index-wide operation; this lock admits exactly one write-capable adapter at
/// a time, including daemon telemetry, watcher work, direct CLI execution and
/// schema initialization.
pub fn writer_lock_path(db_path: &Path) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(".writer.lock");
    PathBuf::from(path)
}

/// True when a `try_lock` failure means "another process holds it" rather than
/// "the lock operation itself failed".
///
/// The obvious test — `e.kind() == ErrorKind::WouldBlock` — is a Unix-ism.
/// `fs4` reports contention with whatever code the platform uses: `EWOULDBLOCK`
/// on Unix, `ERROR_LOCK_VIOLATION` (os error 33) on Windows, which Rust does
/// not map to `WouldBlock`. Windows therefore read every probe of a held lock
/// as a hard I/O error, so `try_acquire_live_exclusive` reported failure where
/// it should have reported "a connection is live". That turned "leave the files
/// in place" into "recovery failed" — one salvage run recovered 0 entries
/// (issue #5). Ask `fs4` what contention looks like here instead of hard-coding
/// one platform's answer.
pub fn is_lock_contention(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock
        || (e.raw_os_error().is_some()
            && e.raw_os_error() == fs4::lock_contended_error().raw_os_error())
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
        Err(e) if is_lock_contention(&e) => Ok(None),
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

/// Admit one writer for the complete project store.
///
/// Callers that also need [`acquire`] must always take this writer lock first.
pub fn acquire_writer(db_path: &Path, operation: &str) -> Result<MutationGuard> {
    let path = writer_lock_path(db_path);
    let file = open_lock_file(&path)?;

    file.lock_exclusive().map_err(|e| {
        Error::from(ErrorKind::Io {
            path: path.clone(),
            operation: format!("acquire writer lock: {e}"),
        })
    })?;

    Ok(announce_writer(file, operation))
}

/// Probe writer admission without leaving a blocking lock waiter at shutdown.
pub(crate) fn try_acquire_writer(db_path: &Path, operation: &str) -> Result<Option<MutationGuard>> {
    let path = writer_lock_path(db_path);
    let file = open_lock_file(&path)?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(announce_writer(file, operation))),
        Err(e) if is_lock_contention(&e) => Ok(None),
        Err(e) => Err(Error::from(ErrorKind::Io {
            path,
            operation: format!("probe writer lock: {e}"),
        })),
    }
}

fn announce_writer(mut file: File, operation: &str) -> MutationGuard {
    let _ = file.set_len(0);
    let _ = writeln!(file, "pid={} operation={operation}", std::process::id());
    let _ = file.sync_data();
    MutationGuard { file }
}

/// Admit a direct CLI writer using the same lock as daemon-owned writers.
///
/// The lock lives in the store the write goes to, so a namespaced writer never
/// contends with — or is mistaken for — a writer on the default store. A
/// namespace that does not exist yet is created here, because its first write
/// is what brings it into being.
pub fn acquire_direct_cli(root: &Path) -> Result<MutationGuard> {
    let mdkb_dir = crate::store::namespace::store_dir(root)?;
    if !mdkb_dir.is_dir() && crate::store::namespace::active()?.is_some() {
        std::fs::create_dir_all(&mdkb_dir)?;
    }
    if !mdkb_dir.is_dir() {
        return Err(ErrorKind::DatabaseNotFound {
            path: mdkb_dir.join("index.sqlite"),
        }
        .into());
    }
    let mdkb_dir = mdkb_dir.canonicalize().map_err(|e| {
        Error::other(format!(
            "cannot canonicalize {} for writer admission: {e}",
            mdkb_dir.display()
        ))
    })?;
    acquire_writer(&mdkb_dir.join("index.sqlite"), "direct-cli-mutation")
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
    fn direct_cli_and_daemon_writers_share_one_outer_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".mdkb")).unwrap();
        let first = acquire_direct_cli(dir.path()).unwrap();

        let (tx, rx) = mpsc::channel();
        let db = dir.path().join(".mdkb/index.sqlite");
        let waiter = std::thread::spawn(move || {
            let _second = acquire_writer(&db, "daemon telemetry").unwrap();
            tx.send(()).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "a second direct CLI mutation must wait for the first"
        );
        drop(first);
        rx.recv_timeout(Duration::from_secs(2))
            .expect("direct CLI mutation should proceed after release");
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
    fn contention_is_recognised_by_the_platform_code_not_by_one_os_error_kind() {
        // Issue #5: on Windows `fs4` reports a held lock with
        // ERROR_LOCK_VIOLATION (os error 33), which Rust does not map to
        // `ErrorKind::WouldBlock`. The probe therefore read "someone is
        // connected" as a hard I/O error, and heal/quarantine/salvage reported
        // failure instead of leaving the files in place — 0 entries recovered
        // in one salvage run. Asking `fs4` for the platform's own contention
        // error makes this assertion true on every platform, including the one
        // it was broken on.
        assert!(
            is_lock_contention(&fs4::lock_contended_error()),
            "the platform's own contention error must classify as contention"
        );
    }

    #[test]
    fn a_real_io_failure_is_not_mistaken_for_contention() {
        // The other half: widening the classifier must not swallow genuine
        // failures, or a broken lock file would read as "a connection is live"
        // forever and recovery would never run.
        for kind in [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::InvalidInput,
        ] {
            assert!(
                !is_lock_contention(&std::io::Error::from(kind)),
                "{kind:?} is a failure to lock, not a contended lock"
            );
        }
    }

    #[test]
    fn live_lock_and_mutation_lock_never_contend() {
        // Catches: sharing the live and mutation sidecars, so a live connection
        // blocks every index-wide write. Probe the OS lock without a scheduler
        // deadline; slow thread startup or metadata sync is not contention.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        assert_ne!(lock_path(&db), live_lock_path(&db));

        let _live = acquire_live_shared(&db).unwrap();
        let probe = open_lock_file(&lock_path(&db)).unwrap();
        FileExt::try_lock_exclusive(&probe)
            .expect("an index-wide mutation must not wait on live connections");
        FileExt::unlock(&probe).unwrap();

        let mutation = acquire(&db, "update").unwrap();
        let error = FileExt::try_lock_exclusive(&probe)
            .expect_err("the probe must observe the actual mutation guard");
        assert!(is_lock_contention(&error), "{error}");
        drop(mutation);
        FileExt::try_lock_exclusive(&probe).expect("mutation release must free the probe");
        FileExt::unlock(&probe).unwrap();
    }

    #[test]
    fn writer_admission_is_outer_to_the_index_and_live_locks() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        assert_ne!(writer_lock_path(&db), lock_path(&db));
        assert_ne!(writer_lock_path(&db), live_lock_path(&db));

        let _writer = acquire_writer(&db, "outer").unwrap();
        let _index = acquire(&db, "inner").unwrap();
        let _live = acquire_live_shared(&db).unwrap();
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
