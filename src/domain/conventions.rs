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

/// Convention patterns mdkb has shipped and then corrected.
///
/// `(collection_name, pattern_mdkb_used_to_write, pattern_it_writes_now)`.
///
/// Written down rather than inferred, because the whole defect is that a
/// correction the code makes cannot reach a store that already materialised
/// the old value. `_root` was `*.md`, which indexed only the two or three
/// files beside the README and silently ignored the rest (issue #8); it is
/// `**/*.md` now. Detection skips by collection NAME, so those stores would
/// never have received the fix — 70 of them on the fleet measured 2026-09-21,
/// every one written by convention detection and not one by a human.
const SUPERSEDED_PATTERNS: &[(&str, &str, &str)] = &[("_root", "*.md", "**/*.md")];

/// A collection whose pattern mdkb wrote, and has since improved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternUpgrade {
    pub name: String,
    pub from: String,
    pub to: String,
}

/// Collections still carrying a pattern this version has superseded.
///
/// Only `source = "convention"` is eligible. mdkb may correct its own past
/// output; it may not overrule somebody who deliberately narrowed a
/// collection. That distinction is the reason `source` is stored, and it is
/// the only thing separating "fix a stale default" from "override a choice" —
/// the two are indistinguishable from the pattern alone.
pub fn detect_pattern_upgrades(existing_collections: &[Collection]) -> Vec<PatternUpgrade> {
    existing_collections
        .iter()
        .filter(|c| c.source == COLLECTION_SOURCE_CONVENTION)
        .filter_map(|c| {
            SUPERSEDED_PATTERNS
                .iter()
                .find(|(name, from, _)| *name == c.name && *from == c.pattern)
                .map(|(_, from, to)| PatternUpgrade {
                    name: c.name.clone(),
                    from: (*from).to_string(),
                    to: (*to).to_string(),
                })
        })
        .collect()
}

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

    // Check for root *.md files, then claim the whole tree from there.
    //
    // The pattern is recursive because almost every project keeps its markdown
    // in subdirectories: `*.md` indexed only the two or three files beside the
    // README and silently ignored the other few hundred, so a fresh `mdkb init`
    // produced a store that knew nothing (issue #8). `**/*.md` is already the
    // default for `mdkb collection add`; this was the one place that disagreed.
    //
    // It does not double-index what `docs`/`archive` above already hold:
    // indexing gives a file to the collection with the most specific path
    // (`core::indexing::claimed_by_a_narrower_collection`).
    if !existing_names.contains("_root") && has_root_markdown_files(root) {
        proposals.push(ProposedCollection {
            name: "_root".to_string(),
            path: ".".to_string(),
            pattern: "**/*.md".to_string(),
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
///
/// Still the trigger, even though the proposed pattern is recursive: it is one
/// `read_dir` on a path the caller is about to walk anyway, where a recursive
/// probe would walk the whole tree just to answer "is there any markdown?".
/// A project with no markdown at all beside its root has nothing `init` can
/// guess about; the user registers a collection explicitly.
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
        // Recursive since issue #8: a root-level markdown file is only the
        // trigger, and the collection it proposes covers the whole tree.
        assert_eq!(root.unwrap().pattern, "**/*.md");
        assert_eq!(root.unwrap().path, ".");
    }

    #[test]
    fn test_no_detection_on_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let proposals = detect_conventions(tmp.path(), &[]);
        assert!(proposals.is_empty());
    }

    fn with_pattern(name: &str, source: &str, pattern: &str) -> Collection {
        Collection {
            pattern: pattern.to_string(),
            ..make_existing(name, source)
        }
    }

    #[test]
    fn a_convention_root_on_the_superseded_pattern_is_upgraded() {
        // The defect this exists for: `*.md` was the shipped `_root` pattern,
        // it was fixed to `**/*.md` (issue #8), and skip-by-name meant the fix
        // could never reach the 70 stores that had already materialised it.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("README.md"), "# r").unwrap();

        let existing = vec![with_pattern("_root", "convention", "*.md")];
        let upgrades = detect_pattern_upgrades(&existing);

        assert_eq!(upgrades.len(), 1);
        assert_eq!(upgrades[0].name, "_root");
        assert_eq!(upgrades[0].from, "*.md");
        assert_eq!(upgrades[0].to, "**/*.md");
    }

    #[test]
    fn a_pattern_a_human_chose_is_never_upgraded() {
        // The hardest criterion. mdkb may correct its own past output; it may
        // not overrule somebody who deliberately narrowed a collection. On the
        // measured fleet nobody had, but the batch cannot tell the two apart
        // and neither may this.
        let existing = vec![with_pattern("_root", "manual", "*.md")];
        assert!(
            detect_pattern_upgrades(&existing).is_empty(),
            "source=manual is a choice, not a default to fix"
        );
    }

    #[test]
    fn a_pattern_that_is_neither_current_nor_superseded_is_left_alone() {
        // `*.markdown` is not the shape mdkb ever wrote. Guessing at it would
        // be inventing an intent.
        let existing = vec![with_pattern("_root", "convention", "*.markdown")];
        assert!(detect_pattern_upgrades(&existing).is_empty());
    }

    #[test]
    fn an_already_current_pattern_is_not_reported_as_an_upgrade() {
        // A notice on every update is a notice nobody reads.
        let existing = vec![with_pattern("_root", "convention", "**/*.md")];
        assert!(detect_pattern_upgrades(&existing).is_empty());
    }

    #[test]
    fn the_superseded_table_is_keyed_by_collection_name() {
        // `docs` was always recursive, so a `docs` collection on `*.md` is not
        // a superseded default — it is somebody's narrow pattern under a name
        // convention detection also uses.
        let existing = vec![with_pattern("docs", "convention", "*.md")];
        assert!(
            detect_pattern_upgrades(&existing).is_empty(),
            "only the pairs named in SUPERSEDED_PATTERNS are upgraded"
        );
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
