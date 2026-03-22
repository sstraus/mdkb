//! Convention-based auto-detection of document collections.
//!
//! Detects standard directory layouts (docs/, archive/, root *.md files)
//! and proposes collections that should be registered automatically.

use std::path::Path;

use crate::domain::{COLLECTION_SOURCE_CONVENTION, Collection};

/// A proposed collection detected by convention rules.
#[derive(Debug, Clone)]
pub struct ProposedCollection {
    pub name: String,
    pub path: String,
    pub pattern: String,
}

/// Built-in convention rules: (directory_name, collection_name, glob_pattern).
const BUILTIN_CONVENTIONS: &[(&str, &str, &str)] = &[
    ("docs", "docs", "**/*.md"),
    ("archive", "archive", "**/*.md"),
];

/// Detect collections based on directory conventions.
///
/// Returns proposed collections for directories that exist at `root`
/// but are not already registered. Manual collections with the same
/// name are never overwritten.
pub fn detect_conventions(
    root: &Path,
    existing_collections: &[Collection],
) -> Vec<ProposedCollection> {
    let existing_names: std::collections::HashSet<&str> = existing_collections
        .iter()
        .map(|c| c.name.as_str())
        .collect();

    let mut proposals = Vec::new();

    // Check built-in convention directories
    for &(dir, name, pattern) in BUILTIN_CONVENTIONS {
        if existing_names.contains(name) {
            continue;
        }
        if root.join(dir).is_dir() {
            proposals.push(ProposedCollection {
                name: name.to_string(),
                path: dir.to_string(),
                pattern: pattern.to_string(),
            });
        }
    }

    // Check for root *.md files (non-recursive _root collection)
    if !existing_names.contains("_root") && has_root_markdown_files(root) {
        proposals.push(ProposedCollection {
            name: "_root".to_string(),
            path: ".".to_string(),
            pattern: "*.md".to_string(),
        });
    }

    proposals
}

/// Convert a proposed collection into a domain Collection ready for storage.
pub fn proposal_to_collection(proposal: &ProposedCollection) -> Collection {
    let now = chrono::Utc::now().timestamp();
    Collection {
        name: proposal.name.clone(),
        path: proposal.path.clone(),
        pattern: proposal.pattern.clone(),
        source: COLLECTION_SOURCE_CONVENTION.to_string(),
        created_at: now,
        updated_at: now,
    }
}

/// Check if the root directory contains any .md files (not in subdirectories).
fn has_root_markdown_files(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        e.path().extension().is_some_and(|ext| ext == "md")
            && e.file_type().is_ok_and(|ft| ft.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_existing(name: &str, source: &str) -> Collection {
        Collection {
            name: name.to_string(),
            path: format!("./{name}"),
            pattern: "**/*.md".to_string(),
            source: source.to_string(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_detect_docs_directory() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("docs")).unwrap();

        let proposals = detect_conventions(tmp.path(), &[]);
        assert!(proposals.iter().any(|p| p.name == "docs"));
    }

    #[test]
    fn test_detect_archive_directory() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();

        let proposals = detect_conventions(tmp.path(), &[]);
        assert!(proposals.iter().any(|p| p.name == "archive"));
    }

    #[test]
    fn test_detect_root_markdown_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("README.md"), "# Hello").unwrap();

        let proposals = detect_conventions(tmp.path(), &[]);
        let root = proposals.iter().find(|p| p.name == "_root");
        assert!(root.is_some());
        assert_eq!(root.unwrap().pattern, "*.md");
        assert_eq!(root.unwrap().path, ".");
    }

    #[test]
    fn test_no_detection_on_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let proposals = detect_conventions(tmp.path(), &[]);
        assert!(proposals.is_empty());
    }

    #[test]
    fn test_manual_collection_not_overwritten() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("docs")).unwrap();

        let existing = vec![make_existing("docs", "manual")];
        let proposals = detect_conventions(tmp.path(), &existing);
        assert!(!proposals.iter().any(|p| p.name == "docs"));
    }

    #[test]
    fn test_convention_collection_not_duplicated() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("docs")).unwrap();

        let existing = vec![make_existing("docs", "convention")];
        let proposals = detect_conventions(tmp.path(), &existing);
        assert!(!proposals.iter().any(|p| p.name == "docs"));
    }

    #[test]
    fn test_proposal_to_collection() {
        let proposal = ProposedCollection {
            name: "docs".to_string(),
            path: "docs".to_string(),
            pattern: "**/*.md".to_string(),
        };
        let coll = proposal_to_collection(&proposal);
        assert_eq!(coll.name, "docs");
        assert_eq!(coll.source, "convention");
        assert!(coll.created_at > 0);
    }

    #[test]
    fn test_detect_multiple_conventions() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("docs")).unwrap();
        fs::create_dir(tmp.path().join("archive")).unwrap();
        fs::write(tmp.path().join("README.md"), "# Hello").unwrap();

        let proposals = detect_conventions(tmp.path(), &[]);
        assert_eq!(proposals.len(), 3);
        assert!(proposals.iter().any(|p| p.name == "docs"));
        assert!(proposals.iter().any(|p| p.name == "archive"));
        assert!(proposals.iter().any(|p| p.name == "_root"));
    }
}
