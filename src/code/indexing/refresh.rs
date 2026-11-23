//! Cross-process activity for the SessionStart background code refresh.
use std::fs::{File, OpenOptions};
use std::path::Path;

use fs4::fs_std::FileExt;

/// Keep this file open for the whole refresh; the OS releases it on exit.
pub(crate) fn try_start(root: &Path) -> std::io::Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".mdkb/code-refresh.lock"))?;
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(Some(file)),
        Err(error) if crate::store::mutation_lock::is_lock_contention(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Read-only probe. An old sidecar alone never means work is still running.
pub(crate) fn active(root: &Path) -> Option<bool> {
    let file = match File::open(root.join(".mdkb/code-refresh.lock")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    match FileExt::try_lock_shared(&file) {
        Ok(()) => Some(false),
        Err(error) if crate::store::mutation_lock::is_lock_contention(&error) => Some(true),
        Err(_) => None,
    }
}
