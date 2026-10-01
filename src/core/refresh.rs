//! Deliberate migration of stores older than this binary (story 219-9d67).
//!
//! A read-only command does not migrate, so a store nobody writes stays on its
//! old schema and `root="*"` skips it. This is the explicit way to bring such a
//! store forward.
//!
//! A migration is not a schema bump alone: v21 deletes memory entries with
//! unreadable ids, v22 dates mined priors, v23 and v26 archive prior clusters,
//! v27 regroups prior candidates. So each store is copied first, the copy is
//! verified, and only then is the store opened for writing. Anything that goes
//! wrong leaves the store at its old schema: the migration itself is one SQLite
//! transaction, and a store with no verified copy is never touched.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::core::Context;
use crate::error::{Error, ErrorKind, Result};
use crate::store::heal::{Soundness, is_structurally_sound};
use crate::store::{mutation_lock, namespace, schema, vectors};

/// What happened to one store that did not fail.
#[derive(Debug)]
pub enum RefreshStatus {
    /// Already at this binary's schema; not touched.
    Current,
    /// Written by a newer binary; not touched, and no migration here could help.
    Newer { found: i32 },
    /// Copied, verified, then migrated.
    Migrated(Migrated),
}

#[derive(Debug)]
pub struct Migrated {
    pub from: i32,
    /// The verified copy taken before the migration.
    pub backup: PathBuf,
    pub memory_before: i64,
    /// Differs from `memory_before` only by what a migration step deletes on
    /// purpose (v21: entries whose id is unreadable).
    pub memory_after: i64,
}

/// A store left at its old schema, and why.
#[derive(Debug)]
pub struct RefreshFailure {
    pub reason: String,
    /// The verified copy, when the failure came after it was taken.
    pub backup: Option<PathBuf>,
    /// The schema the store is at NOW, read after the failure. A failure after
    /// the migration committed (a later initialization step) leaves a migrated
    /// store, and saying "unchanged" about it would be false. `None` when the
    /// store could not be read.
    pub schema_after: Option<i32>,
}

#[derive(Debug)]
pub struct RefreshReport {
    pub root: PathBuf,
    pub outcome: std::result::Result<RefreshStatus, RefreshFailure>,
}

/// Bring every store in `roots` that is older than this binary up to date, one
/// at a time. A failure on one store is reported and does not stop the others.
pub fn refresh_outdated(roots: &[PathBuf]) -> Vec<RefreshReport> {
    let stamp = chrono::Utc::now().timestamp().to_string();
    roots
        .iter()
        .map(|root| RefreshReport {
            root: root.clone(),
            outcome: refresh_store(root, &stamp),
        })
        .collect()
}

/// Refresh one store. `stamp` names its backup, so a run's backups sort together.
pub fn refresh_store(
    root: &Path,
    stamp: &str,
) -> std::result::Result<RefreshStatus, RefreshFailure> {
    let db_path = index_path(root).map_err(|e| RefreshFailure {
        reason: e.to_string(),
        backup: None,
        schema_after: None,
    })?;
    let failed = |e: Error| RefreshFailure {
        reason: e.to_string(),
        backup: None,
        schema_after: read_version(&db_path).ok(),
    };

    // Unlocked first, so a store that is already current costs no lock.
    if let Some(status) = settled(read_version(&db_path).map_err(failed)?) {
        return Ok(status);
    }

    // One writer for the whole copy-then-migrate: a write landing between the
    // two would make the copy older than the state the migration starts from.
    let _writer = mutation_lock::acquire_writer(&db_path, "repos-refresh").map_err(failed)?;
    let from = read_version(&db_path).map_err(failed)?;
    if let Some(status) = settled(from) {
        return Ok(status);
    }

    vectors::init_sqlite_vec();
    let backup = backup_path(&db_path, from, stamp);
    let memory_before = copy_and_verify(&db_path, &backup, from).map_err(failed)?;

    let with_backup = |e: Error| RefreshFailure {
        reason: e.to_string(),
        backup: Some(backup.clone()),
        schema_after: read_version(&db_path).ok(),
    };
    let ctx = Context::open_writer_admitted(root).map_err(with_backup)?;
    let now = schema::get_schema_version(&ctx.conn).map_err(with_backup)?;
    if now != Some(schema::SCHEMA_VERSION) {
        return Err(with_backup(Error::other(format!(
            "store is at schema {now:?} after the migration, expected v{}",
            schema::SCHEMA_VERSION
        ))));
    }
    let memory_after = count_memory(&ctx.conn).map_err(with_backup)?;
    Ok(RefreshStatus::Migrated(Migrated {
        from,
        backup: backup.clone(),
        memory_before,
        memory_after,
    }))
}

/// The status of a store that needs no migration, `None` when it does.
fn settled(version: i32) -> Option<RefreshStatus> {
    match version.cmp(&schema::SCHEMA_VERSION) {
        std::cmp::Ordering::Less => None,
        std::cmp::Ordering::Equal => Some(RefreshStatus::Current),
        std::cmp::Ordering::Greater => Some(RefreshStatus::Newer { found: version }),
    }
}

/// `index.sqlite` of the store at `root`, spelled the way [`Context`] spells it
/// so the writer lock taken here is the one `Context::open` would take.
fn index_path(root: &Path) -> Result<PathBuf> {
    let dir = namespace::store_dir(root)?;
    if !dir.exists() {
        return Err(ErrorKind::DatabaseNotFound {
            path: dir.join("index.sqlite"),
        }
        .into());
    }
    let dir = crate::domain::canonicalize_plain(&dir)
        .map_err(|e| Error::other(format!("cannot canonicalize {}: {e}", dir.display())))?;
    Ok(dir.join("index.sqlite"))
}

fn read_version(db_path: &Path) -> Result<i32> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    schema::get_schema_version(&conn)?.ok_or_else(|| {
        Error::other(format!(
            "{} has no schema version; it is not an mdkb store",
            db_path.display()
        ))
    })
}

fn count_memory(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))?)
}

fn backup_path(db_path: &Path, from: i32, stamp: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(format!(".pre-migrate-v{from}-{stamp}"));
    PathBuf::from(name)
}

/// Copy the store with SQLite's online backup, so the copy is a consistent
/// snapshot even with a `-wal` beside the file, then prove it is a copy: sound,
/// same schema version, same memory entries. Returns the memory entry count.
fn copy_and_verify(db_path: &Path, dest: &Path, from: i32) -> Result<i64> {
    if dest.exists() {
        return Err(Error::other(format!(
            "backup target {} already exists; refusing to overwrite a recovery copy",
            dest.display()
        )));
    }
    let copied = write_copy(db_path, dest, from);
    if copied.is_err() {
        // A half-written or unverified copy is not a recovery copy; do not
        // leave one that looks like it. Only reached once `dest` is ours: the
        // existing-target refusal above returns before anything is created.
        let _ = std::fs::remove_file(dest);
    }
    copied
}

fn write_copy(db_path: &Path, dest: &Path, from: i32) -> Result<i64> {
    let src = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    src.busy_timeout(Duration::from_secs(5))?;
    {
        let mut dst = Connection::open(dest)?;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
        backup.run_to_completion(256, Duration::from_millis(0), None)?;
    }

    // A fresh connection on the copy: it reports what is on disk, not what a
    // page cache remembers.
    let copy = Connection::open(dest)?;
    match is_structurally_sound(&copy) {
        Soundness::Sound => {}
        Soundness::Corrupt { reason } => {
            return Err(Error::other(format!(
                "the store is not sound ({reason}); not migrating it"
            )));
        }
        Soundness::Undetermined(e) => return Err(e.into()),
    }
    let copied = schema::get_schema_version(&copy)?;
    if copied != Some(from) {
        return Err(Error::other(format!(
            "backup reports schema {copied:?}, expected v{from}"
        )));
    }
    let (original, copied) = (count_memory(&src)?, count_memory(&copy)?);
    if original != copied {
        return Err(Error::other(format!(
            "backup holds {copied} memory entries, the store {original}"
        )));
    }
    Ok(original)
}
