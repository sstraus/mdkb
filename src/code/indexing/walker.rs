//! File system walker for discovering source files to index.
//!
//! Uses the `ignore` crate for fast, parallel directory traversal
//! that respects `.gitignore` rules and custom ignore patterns.

use crate::code::parsing::language::Language;
use ignore::WalkBuilder;
use std::path::{Path, PathBuf};

/// Walk a directory and collect paths of source files with supported languages.
///
/// Respects `.gitignore`, `.git/info/exclude`, and global gitignore. Skips
/// hidden files (starting with `.`) and files whose extension doesn't map
/// to a [`Language`].
pub fn discover_files(root: &Path) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false) // enter hidden dirs (let gitignore handle filtering)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(false)
        .require_git(false); // respect .gitignore even outside git repos

    builder
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
        .collect()
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

        let files = discover_files(root);

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

        let files = discover_files(root);
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

        let files = discover_files(root);
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

        let files = discover_files(root);
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let files = discover_files(dir.path());
        assert!(files.is_empty());
    }

    #[test]
    fn test_gitignore_directory_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/build.rs"), "fn build() {}").unwrap();
        fs::write(root.join("src.rs"), "fn src() {}").unwrap();

        let files = discover_files(root);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("src.rs"));
    }
}
