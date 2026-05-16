//! Unified file system walker used by both code and document indexing.
//!
//! Uses the `ignore` crate for fast directory traversal with support for
//! `.gitignore`, `.mdkbignore`, custom glob exclusions, and the `# mdkb:index`
//! force-include annotation.
//!
//! # Ignore semantics
//!
//! Two modes, controlled by [`WalkOptions::respect_gitignore`]:
//!
//! - `true` — honor `.gitignore`, `.git/info/exclude`, and the global gitignore.
//!   The `# mdkb:index` annotation inside `.gitignore` still force-includes
//!   matching patterns.  `.mdkbignore` is NOT read in this mode.
//! - `false` — gitignore files are fully ignored. `.mdkbignore` is read instead
//!   (same syntax as `.gitignore`, with `!pattern` to re-include).
//!
//! Hidden files and directories (names starting with `.`) are always skipped
//! in both modes. The `# mdkb:index` force-include path uses a separate walker
//! with `hidden(false)` to reach files inside hidden directories.
//!
//! Additionally, [`WalkOptions::ignore_patterns`] (glob strings, e.g.
//! `**/node_modules/**`) are applied as exclusions in both modes.

use crate::code::parsing::language::Language;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Name of the mdkb-specific ignore file, read when `respect_gitignore` is false.
const MDKB_IGNORE_FILENAME: &str = ".mdkbignore";

/// Options controlling how a directory is walked for indexing.
#[derive(Debug)]
pub struct WalkOptions<'a> {
    /// Root directory to walk.
    pub root: &'a Path,
    /// Additional glob patterns to exclude (e.g. `**/node_modules/**`).
    pub ignore_patterns: &'a [String],
    /// When true, honor `.gitignore` (and enable `# mdkb:index`). When false,
    /// read `.mdkbignore` instead.
    pub respect_gitignore: bool,
}

/// Parse `.gitignore` at `root` for patterns preceded by a `# mdkb:index` comment line.
///
/// Git does not support inline comments, so the annotation must appear on its
/// own line immediately before the pattern:
///
/// ```text
/// # mdkb:index
/// generated/
/// ```
///
/// Returns the patterns that should be force-included in the index despite
/// being gitignored.
fn parse_mdkb_index_annotations(root: &Path) -> Vec<String> {
    let gitignore_path = root.join(".gitignore");
    let Ok(content) = std::fs::read_to_string(&gitignore_path) else {
        return Vec::new();
    };

    let mut results = Vec::new();
    let mut next_is_forced = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("# mdkb:index") {
            next_is_forced = true;
        } else if next_is_forced {
            // Skip blank lines without losing the annotation — users may put
            // a blank between the comment and the pattern for readability.
            if trimmed.is_empty() {
                continue;
            }
            // A non-mdkb comment resets the annotation (the intent was likely
            // for the immediately following pattern, which we just skipped).
            next_is_forced = false;
            if !trimmed.starts_with('#') {
                results.push(trimmed.to_string());
            }
        }
    }

    results
}

/// Walk `root` collecting only files that match `force_patterns`, ignoring
/// `.gitignore` rules. Still applies the `accept` filter and skips hidden files.
fn discover_forced_files(
    root: &Path,
    force_patterns: &[String],
    accept: &dyn Fn(&Path) -> bool,
) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .require_git(false);

    // Only yield files matching the forced patterns
    let mut overrides = OverrideBuilder::new(root);
    for pattern in force_patterns {
        if let Err(e) = overrides.add(pattern) {
            tracing::warn!("Invalid mdkb:index pattern '{pattern}': {e}");
        }
    }
    match overrides.build() {
        Ok(built) => {
            builder.overrides(built);
        }
        Err(e) => {
            // If the override set itself fails to compile, force-include would
            // yield EVERY file under `root` — which silently pollutes the index.
            // Skip the forced scan entirely and surface the error.
            tracing::error!(
                "Failed to build mdkb:index overrides: {e}. Forced files will NOT be included."
            );
            return Vec::new();
        }
    }

    builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
        .filter_map(|entry| {
            let path = entry.into_path();
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                return None;
            }
            if !accept(&path) {
                return None;
            }
            Some(path)
        })
        .collect()
}

/// Walk a directory and collect file paths, applying ignore rules and the
/// caller-supplied `accept` filter.
///
/// # Filter
///
/// `accept` decides whether a discovered file belongs in the result set. For
/// code indexing this is a language filter; for document/collection indexing
/// this is a glob match against the collection's pattern.
///
/// # Ignore rules
///
/// See [`WalkOptions`] for gitignore vs `.mdkbignore` semantics. The
/// `ignore_patterns` globs are applied as additional exclusions in both modes.
/// Hidden files and directories (starting with `.`) are always skipped.
pub fn walk_files(opts: WalkOptions<'_>, accept: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let WalkOptions {
        root,
        ignore_patterns,
        respect_gitignore,
    } = opts;

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore)
        .git_exclude(respect_gitignore)
        .follow_links(false)
        .require_git(false); // respect .gitignore even outside git repos

    if !respect_gitignore {
        // .mdkbignore substitutes for .gitignore in "ignore-gitignore" mode.
        builder.add_custom_ignore_filename(MDKB_IGNORE_FILENAME);
    }

    // Apply custom ignore patterns via overrides (negated globs = exclusions)
    if !ignore_patterns.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        let mut added = 0usize;
        for pattern in ignore_patterns {
            // Negate the pattern: "!pattern" tells the override to exclude matches
            match overrides.add(&format!("!{pattern}")) {
                Ok(_) => added += 1,
                Err(e) => tracing::warn!("Invalid ignore pattern '{pattern}': {e}"),
            }
        }
        if added > 0 {
            match overrides.build() {
                Ok(built) => {
                    builder.overrides(built);
                }
                Err(e) => {
                    // Silently dropping the override set would let excluded
                    // directories (node_modules, target, etc.) slip into the
                    // index. Log loudly so the misconfiguration is visible.
                    tracing::error!(
                        "Failed to build ignore overrides from {added} pattern(s): {e}. \
                         Custom exclusions will NOT be applied."
                    );
                }
            }
        }
    }

    let mut files: Vec<PathBuf> = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
        .filter_map(|entry| {
            let path = entry.into_path();
            if !accept(&path) {
                return None;
            }
            Some(path)
        })
        .collect();

    // Force-include files annotated with `# mdkb:index` in .gitignore.
    // Only applies when gitignore is being read in the first place.
    if respect_gitignore {
        let force_patterns = parse_mdkb_index_annotations(root);
        if !force_patterns.is_empty() {
            let forced = discover_forced_files(root, &force_patterns, &accept);
            let existing: HashSet<&PathBuf> = files.iter().collect();
            let new: Vec<PathBuf> = forced
                .into_iter()
                .filter(|p| !existing.contains(p))
                .collect();
            files.extend(new);
        }
    }

    files
}

/// Discover source files under `root` for code indexing.
///
/// Convenience wrapper over [`walk_files`] with a language filter for the
/// code index. Honors `.gitignore` (controlled by `respect_gitignore`) and
/// applies `ignore_patterns` as additional exclusions.
pub fn discover_files(
    root: &Path,
    ignore_patterns: &[String],
    respect_gitignore: bool,
) -> Vec<PathBuf> {
    walk_files(
        WalkOptions {
            root,
            ignore_patterns,
            respect_gitignore,
        },
        |path| Language::from_path(path).is_some(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn discover(root: &Path) -> Vec<PathBuf> {
        discover_files(root, &[], true)
    }

    #[test]
    fn test_discovers_supported_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("app.go"), "package main").unwrap();
        fs::write(root.join("index.ts"), "const x = 1;").unwrap();
        fs::write(root.join("script.py"), "print('hi')").unwrap();
        fs::write(root.join("README.md"), "# hello").unwrap();
        fs::write(root.join("data.json"), "{}").unwrap();

        let files = discover(root);

        // md and json are not supported languages
        assert_eq!(files.len(), 4);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(names.contains(&"main.rs"));
        assert!(names.contains(&"app.go"));
        assert!(names.contains(&"index.ts"));
        assert!(names.contains(&"script.py"));
    }

    #[test]
    fn test_skips_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".hidden.rs"), "fn hidden() {}").unwrap();
        fs::write(root.join("visible.rs"), "fn visible() {}").unwrap();

        let files = discover(root);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("visible.rs"));
    }

    #[test]
    fn test_respects_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(root.join("ignored.rs"), "fn ignored() {}").unwrap();
        fs::write(root.join("included.rs"), "fn included() {}").unwrap();

        let files = discover(root);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("included.rs"));
    }

    #[test]
    fn test_traverses_subdirectories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/util")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/util/helpers.go"), "package util").unwrap();

        let files = discover(root);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let files = discover(dir.path());
        assert!(files.is_empty());
    }

    #[test]
    fn test_ignore_patterns_exclude_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Create node_modules with a .js file (no .gitignore to cover it)
        fs::create_dir_all(root.join("node_modules/lodash")).unwrap();
        fs::write(
            root.join("node_modules/lodash/index.js"),
            "function foo() {}",
        )
        .unwrap();
        // And a normal source file
        fs::write(root.join("app.js"), "function main() {}").unwrap();

        let patterns = vec!["**/node_modules/**".to_string()];
        let files = discover_files(root, &patterns, true);

        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert_eq!(files.len(), 1, "expected only app.js, got: {names:?}");
        assert!(files[0].ends_with("app.js"));
    }

    #[test]
    fn test_multiple_ignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("vendor/lib")).unwrap();
        fs::write(root.join("vendor/lib/dep.go"), "package lib").unwrap();
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::write(root.join("dist/bundle.js"), "function bundle() {}").unwrap();
        fs::write(root.join("main.go"), "package main").unwrap();

        let patterns = vec!["**/vendor/**".to_string(), "**/dist/**".to_string()];
        let files = discover_files(root, &patterns, true);

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("main.go"));
    }

    #[test]
    fn test_empty_ignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover(root);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_mdkb_index_annotation_includes_gitignored_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "# mdkb:index\ngenerated.rs\n").unwrap();
        fs::write(root.join("generated.rs"), "fn generated() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover(root);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(
            names.contains(&"generated.rs"),
            "mdkb:index file should be included"
        );
        assert!(names.contains(&"main.rs"));
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_mdkb_index_annotation_no_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // main.rs is NOT gitignored but annotated — should not appear twice
        fs::write(root.join(".gitignore"), "# mdkb:index\nmain.rs\n").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover(root);
        let count = files
            .iter()
            .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some("main.rs"))
            .count();
        assert_eq!(count, 1, "should not have duplicates");
    }

    #[test]
    fn test_mdkb_index_annotation_glob_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "# mdkb:index\ngen/*.rs\n").unwrap();
        fs::create_dir_all(root.join("gen")).unwrap();
        fs::write(root.join("gen/types.rs"), "struct Foo {}").unwrap();
        fs::write(root.join("gen/helpers.rs"), "fn help() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover(root);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(names.contains(&"types.rs"));
        assert!(names.contains(&"helpers.rs"));
        assert!(names.contains(&"main.rs"));
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn test_mdkb_index_without_annotation_stays_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join(".gitignore"),
            "# mdkb:index\ngenerated.rs\nignored.rs\n",
        )
        .unwrap();
        fs::write(root.join("generated.rs"), "fn generated() {}").unwrap();
        fs::write(root.join("ignored.rs"), "fn ignored() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover(root);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(
            names.contains(&"generated.rs"),
            "annotated file should be included"
        );
        assert!(
            !names.contains(&"ignored.rs"),
            "non-annotated file should stay ignored"
        );
        assert!(names.contains(&"main.rs"));
    }

    #[test]
    fn test_mdkb_index_annotation_tolerates_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Users often separate the comment from the pattern with a blank line.
        fs::write(root.join(".gitignore"), "# mdkb:index\n\ngenerated.rs\n").unwrap();
        fs::write(root.join("generated.rs"), "fn gen() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover(root);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(
            names.contains(&"generated.rs"),
            "blank line after annotation should not break force-include"
        );
        assert!(names.contains(&"main.rs"));
    }

    #[test]
    fn test_parse_mdkb_index_annotations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join(".gitignore"),
            "# mdkb:index\ngenerated.rs\n\
             ignored.rs\n\
             # a comment\n\
             # MDKB:INDEX\n\
             gen/*.rs\n\
             # mdkb:index\n",
        )
        .unwrap();

        let patterns = parse_mdkb_index_annotations(root);
        assert_eq!(patterns, vec!["generated.rs", "gen/*.rs"]);
    }

    #[test]
    fn test_gitignore_directory_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/build.rs"), "fn build() {}").unwrap();
        fs::write(root.join("src.rs"), "fn src() {}").unwrap();

        let files = discover(root);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("src.rs"));
    }

    // ---- New tests: .mdkbignore and respect_gitignore=false ----

    #[test]
    fn test_mdkbignore_excludes_files_when_gitignore_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".mdkbignore"), "skip.rs\n").unwrap();
        fs::write(root.join("skip.rs"), "fn skip() {}").unwrap();
        fs::write(root.join("keep.rs"), "fn keep() {}").unwrap();

        let files = discover_files(root, &[], false);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(
            !names.contains(&"skip.rs"),
            ".mdkbignore entry should be excluded"
        );
        assert!(names.contains(&"keep.rs"));
    }

    #[test]
    fn test_mdkbignore_ignored_when_respect_gitignore_true() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // .mdkbignore tries to exclude skip.rs but is dormant in gitignore mode
        fs::write(root.join(".mdkbignore"), "skip.rs\n").unwrap();
        fs::write(root.join("skip.rs"), "fn skip() {}").unwrap();
        fs::write(root.join("keep.rs"), "fn keep() {}").unwrap();

        let files = discover_files(root, &[], true);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(
            names.contains(&"skip.rs"),
            ".mdkbignore must be dormant when respect_gitignore=true"
        );
        assert!(names.contains(&"keep.rs"));
    }

    #[test]
    fn test_gitignore_dormant_when_respect_gitignore_false() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "stories/\n").unwrap();
        fs::create_dir_all(root.join("stories")).unwrap();
        fs::write(root.join("stories/s1.rs"), "fn s1() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover_files(root, &[], false);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(
            names.contains(&"s1.rs"),
            "gitignored file should be indexed when respect_gitignore=false"
        );
        assert!(names.contains(&"main.rs"));
    }

    #[test]
    fn test_mdkbignore_negation_re_includes_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Exclude files directly inside gen/, then re-include gen/keep.rs.
        // We use `gen/*` (not `gen/`) so the directory itself isn't pruned —
        // gitignore/ignore semantics forbid re-including a file whose parent
        // directory is excluded.
        fs::write(root.join(".mdkbignore"), "gen/*\n!gen/keep.rs\n").unwrap();
        fs::create_dir_all(root.join("gen")).unwrap();
        fs::write(root.join("gen/keep.rs"), "fn keep() {}").unwrap();
        fs::write(root.join("gen/drop.rs"), "fn drop() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover_files(root, &[], false);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(names.contains(&"keep.rs"));
        assert!(names.contains(&"main.rs"));
        assert!(
            !names.contains(&"drop.rs"),
            "gen/drop.rs should remain excluded"
        );
    }

    #[test]
    fn test_walk_files_with_custom_accept_filter() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("a.md"), "# a").unwrap();
        fs::write(root.join("b.md"), "# b").unwrap();
        fs::write(root.join("c.txt"), "c").unwrap();

        let files = walk_files(
            WalkOptions {
                root,
                ignore_patterns: &[],
                respect_gitignore: false,
            },
            |p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e == "md")
            },
        );
        assert_eq!(files.len(), 2);
    }
}
