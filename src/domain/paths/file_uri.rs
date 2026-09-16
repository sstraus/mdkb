//! Converting between a filesystem path and a `file:` URI.
//!
//! Three places in the program need this, and each wrote its own half inline.
//! All three were wrong on Windows, in two different ways:
//!
//! - **Reading.** The MCP resource surface turned `file:///C:/dir` into a path
//!   by trimming `file://`, which leaves `/C:/dir` — not an absolute path, so
//!   every Windows resource URI was rejected.
//! - **Writing, twice.** The corruption salvage and the damage diagnosis each
//!   opened the quarantined database with `format!("file:{path}?immutable=1")`.
//!   A Windows path is `C:\dir\index.sqlite`, which is not a URI: SQLite's
//!   parser wants `/` separators and a leading slash before the drive letter.
//!   `ATTACH` and `open` failed, and because salvage is best-effort the failure
//!   was a log line while the memory it exists to rescue was lost.
//!
//! The module is three layers, so each answers one question:
//!
//! | Layer | Question | Items |
//! |---|---|---|
//! | Transformation | How is a path spelled as a URI, and back? | [`to_uri`], [`from_uri`] |
//! | Policy | How should a *possibly corrupt* database be opened? | [`read_only_uri`] |
//! | Rule | Is a leading slash part of the path or part of the URI? | `strip_drive_slash` |
//!
//! The policy layer matters most for what went wrong. Appending
//! `?immutable=1` is a decision about how to treat a torn file, and it was
//! spelled out at each call site as string concatenation. Naming it means the
//! decision is made once, both readers of a quarantined file make it the same
//! way, and neither has to know that a URI takes its parameters after a `?`.

use std::path::{Path, PathBuf};

// ── Transformation: path <-> URI ─────────────────────────────────────────────

/// The `file:` URI for `path`, in the form SQLite and MCP both accept.
///
/// Windows `C:\dir\db` becomes `file:///C:/dir/db`; Unix `/dir/db` becomes
/// `file:///dir/db`.
pub fn to_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    // A Windows path starts at the drive letter; the URI form puts a slash
    // before it. A Unix path already carries one.
    let leading = if text.starts_with('/') { "" } else { "/" };
    format!("file://{leading}{}", encode_uri_syntax(&text))
}

/// The filesystem path a `file:` URI names, when it names an absolute one.
///
/// `None` for a URI with no `file://` scheme, and for one whose path is
/// relative — or, on Windows, merely drive-relative. `/Users/me` names a
/// location on whichever drive happens to be current, and every caller here
/// asked for an unambiguous location.
pub fn from_uri(uri: &str) -> Option<PathBuf> {
    let path = PathBuf::from(strip_drive_slash(uri.strip_prefix("file://")?));
    path.is_absolute().then_some(path)
}

// ── Policy: how to open a file that may be torn ──────────────────────────────

/// The URI that opens `path` as a read-only, no-locking database.
///
/// `immutable=1` tells SQLite the file will not change, so it skips locking and
/// hot-journal rollback. That is the only safe way to read a possibly-corrupt
/// file: taking a lock on a torn database can block on a stale hot journal, and
/// rolling one back can destroy the very pages the salvage is trying to read.
///
/// Named rather than concatenated at each call site because it is a decision,
/// not a format. Both readers of a quarantined file — the salvage and the
/// diagnosis — must make the same one.
pub fn read_only_uri(path: &Path) -> String {
    format!("{}?immutable=1", to_uri(path))
}

// ── Rules ────────────────────────────────────────────────────────────────────

/// Percent-encode the three characters SQLite reads as URI syntax.
///
/// Without this a directory containing `?` truncates the path and turns the
/// rest into query parameters, so a different file (or none) is opened.
fn encode_uri_syntax(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '?' => out.push_str("%3f"),
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            _ => out.push(ch),
        }
    }
    out
}

/// `/C:/dir` -> `C:/dir`; anything else unchanged.
///
/// The guard matters: a bare `/c/data` on Unix is a real path whose leading
/// slash must survive, so the rule fires only on a genuine drive specifier.
fn strip_drive_slash(path_str: &str) -> &str {
    let Some(rest) = path_str.strip_prefix('/') else {
        return path_str;
    };
    let mut chars = rest.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(drive), Some(':'), Some('/' | '\\')) if drive.is_ascii_alphabetic() => rest,
        _ => path_str,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property both directions exist for: a path survives the round trip.
    /// Compared on components so the fixture is not spelled in the separator
    /// under test.
    #[test]
    fn a_path_survives_the_round_trip() {
        let path = if cfg!(windows) {
            PathBuf::from("C:\\dir\\sub\\index.sqlite")
        } else {
            PathBuf::from("/dir/sub/index.sqlite")
        };
        let back = from_uri(&to_uri(&path)).expect("round trip");
        assert_eq!(
            back.components().collect::<Vec<_>>(),
            path.components().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_windows_path_gains_a_slash_before_the_drive() {
        assert_eq!(
            to_uri(Path::new("C:\\dir\\index.sqlite")),
            "file:///C:/dir/index.sqlite"
        );
    }

    #[test]
    fn a_unix_path_keeps_its_single_leading_slash() {
        assert_eq!(
            to_uri(Path::new("/dir/index.sqlite")),
            "file:///dir/index.sqlite"
        );
    }

    /// An unencoded `?` would make SQLite read the rest of the path as URI
    /// parameters, attaching a different file or none.
    #[test]
    fn uri_syntax_characters_are_encoded() {
        assert_eq!(
            to_uri(Path::new("/a?b/c#d/e%f")),
            "file:///a%3fb/c%23d/e%25f"
        );
    }

    /// The policy adds its parameter after the encoded path, so the `?` it
    /// introduces is the only unencoded one in the string.
    #[test]
    fn the_read_only_policy_appends_one_parameter() {
        let uri = read_only_uri(Path::new("/dir/db"));
        assert_eq!(uri, "file:///dir/db?immutable=1");
        assert_eq!(uri.matches('?').count(), 1);
    }

    /// A `?` in the path must not be mistaken for the policy's own parameter.
    #[test]
    fn a_question_mark_in_the_path_stays_encoded_under_the_policy() {
        let uri = read_only_uri(Path::new("/we?rd/db"));
        assert_eq!(uri, "file:///we%3frd/db?immutable=1");
        assert_eq!(uri.matches('?').count(), 1);
    }

    #[test]
    fn a_uri_without_the_scheme_is_not_a_path() {
        assert_eq!(from_uri("/dir/x"), None);
        assert_eq!(from_uri("https://example.com/x"), None);
    }

    /// The drive rule must not fire on a path that merely starts with a letter,
    /// or a Unix path like `/c/data` would lose its leading slash.
    #[test]
    fn the_drive_rule_leaves_other_paths_alone() {
        assert_eq!(strip_drive_slash("/Users/me"), "/Users/me");
        assert_eq!(strip_drive_slash("/c/data"), "/c/data");
        assert_eq!(strip_drive_slash("C:/already"), "C:/already");
        assert_eq!(strip_drive_slash("/C:/drive"), "C:/drive");
    }

    #[cfg(windows)]
    #[test]
    fn a_drive_relative_uri_is_refused_on_windows() {
        assert_eq!(from_uri("file:///Users/me/project"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_absolute_uri_resolves() {
        assert_eq!(
            from_uri("file:///Users/me/project"),
            Some(PathBuf::from("/Users/me/project"))
        );
    }
}
