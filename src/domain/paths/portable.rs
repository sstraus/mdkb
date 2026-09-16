//! Spelling a path for anything that is not the OS itself.
//!
//! `Path::canonicalize` on Windows returns the VERBATIM form, `\\?\C:\dir`.
//! That prefix tells the Windows API to skip path parsing — it is what lifts
//! the 260-character limit — so the OS accepts it everywhere. Nothing else
//! does.
//!
//! mdkb canonicalizes early and often, because the store's identity IS its
//! canonical path: that is what stops two spellings of one directory opening
//! two lock domains over one file. So the verbatim prefix becomes the ordinary
//! currency inside the program and rides out into two places it does not
//! belong:
//!
//! 1. **Arguments to spawned programs.** They parse the string themselves.
//!    ```text
//!    git -C '\\?\C:\repo' status  ->  fatal: cannot change to … Invalid argument
//!    git clone '\\?\C:\repo' b    ->  fatal: hostname contains invalid characters
//!    ```
//!    That second message shows why it is worse than a wrong path: git reads
//!    `\\…` as a UNC or SSH host, so the error names a hostname for an argument
//!    that was a local directory. Nothing in it points at the prefix.
//!
//! 2. **Values written down.** A collection row recorded its directory as
//!    `\\?\E:\dev\notes`. A stored path is read back later — by a person, by a
//!    report, by another tool, possibly on another host, since mdkb syncs
//!    memory through git. The verbatim spelling is a Windows-API detail and has
//!    no business surviving in a database row.
//!
//! On Linux and macOS `canonicalize` returns a plain absolute path, so there is
//! nothing to remove and every function here is the identity. That is exactly
//! why the defect was invisible until Windows.
//!
//! # One transformation, two shapes
//!
//! [`plain`] answers the question; [`plain_string`] is the same answer for a
//! caller that needs to store or send it. Named by SHAPE rather than by
//! consumer on purpose: unlike `file_uri`, nothing here is a decision, so a
//! pair of consumer-named wrappers would be two names for one behaviour and a
//! test to prove they agree. The doc names the consumers instead.
//!
//! # What this is NOT for
//!
//! Never apply it before a filesystem call. The verbatim form is the one that
//! survives paths longer than 260 characters, so stripping it there would trade
//! this defect for a worse one.

use std::path::{Path, PathBuf};

/// The Windows verbatim (extended-length) path prefix.
#[cfg(windows)]
const VERBATIM_PREFIX: &str = r"\\?\";

/// The UNC form of the verbatim prefix, `\\?\UNC\server\share`.
///
/// It maps back to `\\server\share`, so the replacement is not a plain strip.
#[cfg(windows)]
const VERBATIM_UNC_PREFIX: &str = r"\\?\UNC\";

// ── Transformation ───────────────────────────────────────────────────────────

/// `path` without the Windows verbatim prefix.
///
/// Idempotent, and a no-op for any path that does not carry the prefix — so a
/// caller can apply it without first checking whether it is needed.
#[cfg(windows)]
pub fn plain(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(VERBATIM_UNC_PREFIX) {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(VERBATIM_PREFIX) {
        Some(rest) => PathBuf::from(rest),
        None => path.to_path_buf(),
    }
}

/// `path` without the Windows verbatim prefix.
///
/// POSIX canonical paths are already plain, so this is the identity. It exists
/// on every platform so callers never branch: the boundary is named the same
/// way everywhere, and the platform knowledge stays here.
#[cfg(not(windows))]
pub fn plain(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// [`plain`], for a caller that needs the answer as a string.
///
/// Used where the path is written down — a database row, a socket message, a
/// line shown to a person. What gets written outlives the process that wrote
/// it, so it carries the ordinary spelling rather than an API detail.
pub fn plain_string(path: &Path) -> String {
    plain(path).to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity case, on every platform: a path with no verbatim prefix is
    /// returned unchanged, so a caller can apply this unconditionally.
    #[test]
    fn a_plain_path_is_unchanged() {
        let p = Path::new("relative/dir");
        assert_eq!(plain(p), p);
    }

    /// The property that lets callers apply this without thinking: converting
    /// twice is the same as converting once.
    #[test]
    fn the_conversion_is_idempotent() {
        for raw in [r"\\?\C:\repo", r"C:\repo", "/plain/dir", "relative"] {
            let once = plain(Path::new(raw));
            let twice = plain(&once);
            assert_eq!(once, twice, "not idempotent for {raw}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_drive_path_loses_its_prefix() {
        assert_eq!(
            plain(Path::new(r"\\?\C:\repo\sub")),
            PathBuf::from(r"C:\repo\sub")
        );
    }

    /// The UNC form maps back to `\\server\share`, not to `UNC\server\share` —
    /// a plain strip would produce a path that names nothing.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_unc_path_becomes_an_ordinary_unc_path() {
        assert_eq!(
            plain(Path::new(r"\\?\UNC\server\share\dir")),
            PathBuf::from(r"\\server\share\dir")
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_ordinary_windows_path_is_untouched() {
        assert_eq!(plain(Path::new(r"C:\repo")), PathBuf::from(r"C:\repo"));
    }

    /// A stored value is a string, and it must not carry the prefix either.
    #[cfg(windows)]
    #[test]
    fn a_stored_path_is_written_without_the_prefix() {
        assert_eq!(
            plain_string(Path::new(r"\\?\E:\dev\notes")),
            r"E:\dev\notes"
        );
    }
}
