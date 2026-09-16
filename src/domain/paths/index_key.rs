//! The key a file is known by inside an index.
//!
//! Everything the index stores about a file hangs off one string: the file's
//! path relative to the root it was indexed under. That string is a database
//! key, a glob-match subject, an MCP resource address, and the base a Rust or
//! Python module path is derived from — so it has to mean the same thing on
//! every platform.
//!
//! It did not. Four call sites each wrote `strip_prefix(root)` followed by
//! `to_string_lossy()`, which keeps whatever separator the host uses. The same
//! repository therefore indexed as `src/lib.rs` on Linux and `src\lib.rs` on
//! Windows, and three things broke on Windows only:
//!
//! 1. A lookup by root-relative path returned nothing, because the caller
//!    spells the path with `/` and the stored key used `\`.
//! 2. `module_path_for` derived addresses like `crate::src\store\db`, since it
//!    documents a `/`-separated input and was handed a `\`-separated one.
//! 3. A collection's glob (`**/*.md`) was matched against a `\`-separated
//!    relative path, where globset reads `\` as an escape rather than a
//!    separator.
//!
//! So the key is defined here, once, as a normalised form rather than a
//! by-product of whichever call site needed it: **relative to the root, `/`
//! separated, no leading `./`**.

use std::path::Path;

/// The index key for `path` under `root`: relative, `/` separated.
///
/// A path outside `root` keeps its own spelling, normalised the same way. The
/// caller that must reject an outside path checks containment itself — this
/// function answers "what is this file called", not "is this file allowed".
pub fn rel_key(path: &Path, root: &Path) -> String {
    normalize(path.strip_prefix(root).unwrap_or(path))
}

/// The `/`-separated spelling of `path`, with any leading `./` removed.
///
/// Separate from [`rel_key`] because some call sites have already stripped the
/// prefix (a glob subject built from a canonical base, say) and need only the
/// normalisation half.
pub fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text = text.trim_start_matches("./").trim_start_matches(".\\");
    if std::path::MAIN_SEPARATOR == '/' {
        text.to_string()
    } else {
        text.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The property the whole module exists for: one repository, one key, on
    /// every host. Written with `components` so the expectation is not itself
    /// spelled in the separator under test.
    #[test]
    fn a_nested_file_keys_the_same_on_every_platform() {
        let root: PathBuf = ["a", "b"].iter().collect();
        let file: PathBuf = ["a", "b", "src", "store", "db.rs"].iter().collect();
        assert_eq!(rel_key(&file, &root), "src/store/db.rs");
    }

    #[test]
    fn a_file_directly_under_the_root_has_no_separator() {
        let root: PathBuf = ["a", "b"].iter().collect();
        let file: PathBuf = ["a", "b", "lib.rs"].iter().collect();
        assert_eq!(rel_key(&file, &root), "lib.rs");
    }

    /// Outside-root paths are normalised, never rewritten. Containment is the
    /// caller's question, and a key that silently changed shape here would hide
    /// the answer from it.
    #[test]
    fn a_path_outside_the_root_keeps_its_own_spelling() {
        let root: PathBuf = ["a", "b"].iter().collect();
        let file: PathBuf = ["x", "y", "z.rs"].iter().collect();
        assert_eq!(rel_key(&file, &root), "x/y/z.rs");
    }

    /// `module_path_for` trims `./` itself; doing it here too means every
    /// consumer sees one spelling rather than each trimming defensively.
    #[test]
    fn a_leading_dot_slash_is_dropped_in_either_spelling() {
        assert_eq!(normalize(Path::new("./src/lib.rs")), "src/lib.rs");
        assert_eq!(
            normalize(&PathBuf::from(".").join("src").join("lib.rs")),
            "src/lib.rs"
        );
    }

    #[test]
    fn an_already_normalized_key_is_unchanged() {
        assert_eq!(normalize(Path::new("src/lib.rs")), "src/lib.rs");
    }

    /// The root itself relativises to nothing. Callers that treat the empty key
    /// as "the root" rather than as a failure need it to stay empty.
    #[test]
    fn the_root_itself_yields_an_empty_key() {
        let root: PathBuf = ["a", "b"].iter().collect();
        assert_eq!(rel_key(&root, &root), "");
    }
}
