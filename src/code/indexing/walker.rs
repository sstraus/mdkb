//! File system walker for discovering source files to index.
//!
//! Uses the `ignore` crate for fast, parallel directory traversal
//! that respects `.gitignore` rules and custom ignore patterns.

use crate::code::parsing::language::Language;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Parse `.gitignore` at `root` for lines annotated with `# mdkb:index`.
///
/// Returns the gitignore patterns (without the comment) that should be
/// force-included in the code index despite being gitignored.
fn parse_mdkb_index_annotations(root: &Path) -> Vec<String> {
    let gitignore_path = root.join(".gitignore");
    let content = match std::fs::read_to_string(&gitignore_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    content
        .lines()
        .filter_map(|line| {
            let (pattern, comment) = line.split_once('#')?;
            if comment.trim().eq_ignore_ascii_case("mdkb:index") {
                let pattern = pattern.trim();
                if !pattern.is_empty() {
                    return Some(pattern.to_string());
                }
            }
            None
        })
        .collect()
}

/// Walk `root` collecting only files that match `force_patterns`, ignoring
/// `.gitignore` rules. Still applies the Language filter and skips hidden files.
fn discover_forced_files(root: &Path, force_patterns: &[String]) -> Vec<PathBuf> {
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
    if let Ok(built) = overrides.build() {
        builder.overrides(built);
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
            Language::from_path(&path)?;
            Some(path)
        })
        .collect()
}

/// Walk a directory and collect paths of source files with supported languages.
///
/// Respects `.gitignore`, `.git/info/exclude`, and global gitignore. Skips
/// hidden files (starting with `.`) and files whose extension doesn't map
/// to a [`Language`]. Applies `ignore_patterns` as additional exclusion rules
/// (glob syntax, e.g. `**/node_modules/**`).
///
/// Lines in `.gitignore` annotated with `# mdkb:index` are force-included
/// despite being gitignored.
pub fn discover_files(root: &Path, ignore_patterns: &[String]) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false) // enter hidden dirs (let gitignore handle filtering)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(false)
        .require_git(false); // respect .gitignore even outside git repos

    // Apply custom ignore patterns via overrides (negated globs = exclusions)
    if !ignore_patterns.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        for pattern in ignore_patterns {
            // Negate the pattern: "!pattern" tells the override to exclude matches
            if let Err(e) = overrides.add(&format!("!{pattern}")) {
                tracing::warn!("Invalid ignore pattern '{pattern}': {e}");
            }
        }
        if let Ok(built) = overrides.build() {
            builder.overrides(built);
        }
    }

    let mut files: Vec<PathBuf> = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
        .filter_map(|entry| {
            let path = entry.into_path();

            // Skip hidden files
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                return None;
            }

            // Only include files with a recognized language
            Language::from_path(&path)?;
            Some(path)
        })
        .collect();

    // Force-include files annotated with `# mdkb:index` in .gitignore
    let force_patterns = parse_mdkb_index_annotations(root);
    if !force_patterns.is_empty() {
        let forced = discover_forced_files(root, &force_patterns);
        let existing: HashSet<&PathBuf> = files.iter().collect();
        let new: Vec<PathBuf> = forced
            .into_iter()
            .filter(|p| !existing.contains(p))
            .collect();
        files.extend(new);
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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

        let files = discover_files(root, &[]);

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

        let files = discover_files(root, &[]);
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

        let files = discover_files(root, &[]);
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

        let files = discover_files(root, &[]);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let files = discover_files(dir.path(), &[]);
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
        let files = discover_files(root, &patterns);

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

        let patterns = vec![
            "**/vendor/**".to_string(),
            "**/dist/**".to_string(),
        ];
        let files = discover_files(root, &patterns);

        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("main.go"));
    }

    #[test]
    fn test_empty_ignore_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover_files(root, &[]);
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_mdkb_index_annotation_includes_gitignored_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // generated.rs is gitignored but annotated with mdkb:index
        fs::write(root.join(".gitignore"), "generated.rs # mdkb:index\n").unwrap();
        fs::write(root.join("generated.rs"), "fn generated() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover_files(root, &[]);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(names.contains(&"generated.rs"), "mdkb:index file should be included");
        assert!(names.contains(&"main.rs"));
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_mdkb_index_annotation_no_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // main.rs is NOT gitignored and also annotated — should not appear twice
        fs::write(root.join(".gitignore"), "main.rs # mdkb:index\n").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover_files(root, &[]);
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

        fs::write(root.join(".gitignore"), "gen/*.rs # mdkb:index\n").unwrap();
        fs::create_dir_all(root.join("gen")).unwrap();
        fs::write(root.join("gen/types.rs"), "struct Foo {}").unwrap();
        fs::write(root.join("gen/helpers.rs"), "fn help() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover_files(root, &[]);
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

        // Two gitignore entries: one annotated, one not
        fs::write(
            root.join(".gitignore"),
            "generated.rs # mdkb:index\nignored.rs\n",
        )
        .unwrap();
        fs::write(root.join("generated.rs"), "fn generated() {}").unwrap();
        fs::write(root.join("ignored.rs"), "fn ignored() {}").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let files = discover_files(root, &[]);
        let names: Vec<&str> = files
            .iter()
            .filter_map(|p| p.file_name()?.to_str())
            .collect();
        assert!(names.contains(&"generated.rs"), "annotated file should be included");
        assert!(!names.contains(&"ignored.rs"), "non-annotated file should stay ignored");
        assert!(names.contains(&"main.rs"));
    }

    #[test]
    fn test_parse_mdkb_index_annotations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join(".gitignore"),
            "generated.rs # mdkb:index\n\
             ignored.rs\n\
             # a comment\n\
             gen/*.rs # MDKB:INDEX\n\
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

        let files = discover_files(root, &[]);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("src.rs"));
    }
}
