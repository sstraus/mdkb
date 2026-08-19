//! Where the user's home directory is, on every platform mdkb runs on.
//!
//! `HOME` is the Unix spelling and `USERPROFILE` is the Windows one. Reading
//! only `HOME` works on Unix and fails on Windows, where the variable is
//! normally absent — so `mdkb setup hooks codex` and `mdkb setup mcp codex`
//! exited 1 with "HOME environment variable not set" for every Windows user.
//! CI never saw it, because until now no CI job ran on Windows.
//!
//! One implementation, so a new caller cannot reintroduce the Unix-only read.

use std::ffi::OsString;
use std::path::PathBuf;

/// The user's home directory, if the environment names one.
///
/// An empty value counts as absent: an empty `HOME` joined with a relative
/// path would silently resolve against the filesystem root, which is worse
/// than reporting no home at all.
pub fn dir() -> Option<PathBuf> {
    from_vars(std::env::var_os("HOME"), std::env::var_os("USERPROFILE"))
}

/// The rule itself, with the environment passed in.
///
/// Split out so the platform-independent behaviour is unit-testable without
/// mutating process environment variables, which no test can do safely while
/// its neighbours run in parallel threads.
fn from_vars(home: Option<OsString>, userprofile: Option<OsString>) -> Option<PathBuf> {
    // Filter per candidate rather than after choosing, so an empty HOME falls
    // through to USERPROFILE instead of shadowing it.
    [home, userprofile]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One environment value, as the OS hands it over.
    fn os(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn home_wins_when_both_are_set() {
        // Proves: a Unix box that also exports USERPROFILE (a shared mount, a
        // WSL profile) keeps using HOME. The fallback must not outrank it.
        assert_eq!(
            from_vars(Some(os("/home/steve")), Some(os("C:\\Users\\steve"))),
            Some(PathBuf::from("/home/steve"))
        );
    }

    #[test]
    fn userprofile_is_used_when_home_is_absent() {
        // Proves: the Windows case. This is the row that was failing — Windows
        // sets USERPROFILE and leaves HOME unset.
        assert_eq!(
            from_vars(None, Some(os("C:\\Users\\steve"))),
            Some(PathBuf::from("C:\\Users\\steve"))
        );
    }

    #[test]
    fn no_home_is_reported_rather_than_guessed() {
        // Proves: with neither set the answer is None, so callers report the
        // problem instead of joining onto a made-up path.
        assert_eq!(from_vars(None, None), None);
    }

    #[test]
    fn an_empty_value_counts_as_absent() {
        // Proves: an empty HOME does not shadow a usable USERPROFILE, and an
        // empty pair yields None rather than the filesystem root. Joining
        // ".codex/config.toml" onto "" would write to the root of the drive.
        assert_eq!(
            from_vars(Some(os("")), Some(os("C:\\Users\\steve"))),
            Some(PathBuf::from("C:\\Users\\steve"))
        );
        assert_eq!(from_vars(Some(os("")), Some(os(""))), None);
    }
}
