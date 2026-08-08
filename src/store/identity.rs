//! One store, one spelling.
//!
//! Every cross-process guarantee in this codebase is keyed on the database path
//! as a STRING: the `.mutation.lock` and `.live.lock` sidecars, and the
//! `-wal`/`-shm` files SQLite derives itself. Two spellings that reach the same
//! inode therefore give two lock domains and two WAL indexes over one database —
//! writers that cannot see each other, allocating the same pages twice. That is
//! the exact damage recurring in the field (`2nd reference to page N`).
//!
//! `Context::open` canonicalizes, which rules out the aliases canonicalization
//! knows about (case on APFS, symlinks, `..`). It cannot rule out the ones it
//! does not resolve — macOS firmlinks make `/Users/x` and
//! `/System/Volumes/Data/Users/x` two canonical spellings of one directory.
//!
//! So the invariant is checked instead of assumed: the first process to open a
//! store records the spelling it used, and any later process arriving with a
//! different spelling for the same inode is refused. A refusal is loud and
//! rare; the alternative is silent data loss.

use std::path::Path;

use crate::error::{Error, Result};

/// Sidecar recording the canonical spelling this store is coordinated under.
fn identity_path(db_path: &Path) -> std::path::PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(".identity");
    std::path::PathBuf::from(path)
}

/// True when both paths resolve to the same file on disk.
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// Record `db_path` as this store's spelling, or refuse the open if another
/// spelling of the same file is already in use.
///
/// A recorded path that no longer exists means the store was moved or copied,
/// which is legitimate: the record is updated and the caller proceeds. Only a
/// live second spelling of the same inode is fatal.
///
/// Best-effort on write: failing to record the spelling costs the check on the
/// next open, never the open itself.
pub fn claim(db_path: &Path) -> Result<()> {
    let marker = identity_path(db_path);
    let current = db_path.to_string_lossy().into_owned();

    let recorded = std::fs::read_to_string(&marker).ok();
    let recorded = recorded.as_deref().map(str::trim).filter(|s| !s.is_empty());

    match recorded {
        Some(recorded) if recorded == current => return Ok(()),
        Some(recorded) if same_file(Path::new(recorded), db_path) => {
            return Err(Error::other(format!(
                "store already open under a different spelling of the same file:\n  \
                 in use: {recorded}\n  requested: {current}\n\
                 Locks and WAL files are named from this path, so two spellings mean two \
                 lock domains over one database — which corrupts it. Use the recorded path."
            )));
        }
        _ => {}
    }

    let _ = std::fs::write(&marker, &current);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> std::path::PathBuf {
        let db = dir.join("index.sqlite");
        std::fs::write(&db, b"db").unwrap();
        db
    }

    #[test]
    fn first_open_records_the_spelling_and_reopens_are_silent() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());

        claim(&db).expect("first open");
        assert_eq!(
            std::fs::read_to_string(identity_path(&db)).unwrap(),
            db.to_string_lossy()
        );
        claim(&db).expect("same spelling reopens");
    }

    #[test]
    fn a_second_spelling_of_the_same_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = store(dir.path());
        claim(&db).expect("first open");

        // A hard link is a second name for one inode — exactly what a firmlink
        // or any unresolved alias looks like to the lock layer.
        let alias = dir.path().join("alias.sqlite");
        std::fs::hard_link(&db, &alias).unwrap();
        std::fs::copy(identity_path(&db), identity_path(&alias)).unwrap();

        let err = claim(&alias).expect_err("aliased spelling must be refused");
        assert!(
            err.to_string().contains("different spelling"),
            "the error must name the problem: {err}"
        );
    }

    #[test]
    fn a_moved_store_is_adopted_rather_than_refused() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old").join("index.sqlite");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"db").unwrap();
        claim(&old).expect("first open");

        let new_dir = dir.path().join("new");
        std::fs::rename(dir.path().join("old"), &new_dir).unwrap();
        let moved = new_dir.join("index.sqlite");

        claim(&moved).expect("a moved store must still open");
        assert_eq!(
            std::fs::read_to_string(identity_path(&moved)).unwrap(),
            moved.to_string_lossy(),
            "the new spelling is adopted"
        );
    }
}
