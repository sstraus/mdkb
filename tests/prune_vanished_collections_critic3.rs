//! Round 3 critic cases for story 214-1b93: attacks on the guard denominator
//! after `_root` was excluded from it.

use mdkb::cli::handlers::{
    handle_collection_add, handle_collection_remove, handle_init, handle_update,
};
use mdkb::core::Context;
use mdkb::core::ops::handle_search;

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    (dir, root)
}

fn write_doc(root: &std::path::Path, dir: &str, file: &str, body: &str) {
    let d = root.join(dir);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(file), body).unwrap();
}

fn hits(ctx: &Context, term: &str) -> usize {
    handle_search(ctx, term, 10, None).expect("search").len()
}

fn readme(root: &std::path::Path) {
    std::fs::write(root.join("README.md"), "# Readme\n\nreadmemarker\n").unwrap();
}

/// Catches: the exclusion of `_root` is implemented by lowering the guard
/// threshold (e.g. `vanished.len() >= 1`), so a lone deleted convention
/// directory in a repo with a README is skipped instead of pruned.
#[test]
fn a_single_deleted_convention_directory_is_pruned_when_a_root_readme_exists() {
    let (_dir, root) = store();
    readme(&root);
    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    assert_eq!(hits(&ctx, "docsmarker"), 1, "precondition");

    std::fs::remove_dir_all(root.join("docs")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert_eq!(result.collections_pruned, vec!["docs".to_string()]);
    assert_eq!(hits(&ctx, "docsmarker"), 0);
    assert_eq!(hits(&ctx, "readmemarker"), 1, "_root must survive");
}

/// Catches: a hand-registered collection is counted in the denominator, so
/// one extra manual collection (which still exists) makes the guard stop
/// firing for an unmounted volume; or the reverse, the guard touches it.
#[test]
fn a_manual_collection_neither_joins_the_denominator_nor_gets_pruned() {
    let (_dir, root) = store();
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    write_doc(&root, "notes", "n.md", "# N\n\nnotesmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_collection_add(&ctx, "notes", "notes", "**/*.md").expect("add");
    handle_update(&ctx, &root).expect("first update");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::remove_dir_all(root.join("docs")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert!(result.collections_pruned.is_empty(), "guard must fire");
    assert_eq!(hits(&ctx, "notesmarker"), 1);
    assert_eq!(hits(&ctx, "archivemarker"), 1);
    assert_eq!(hits(&ctx, "docsmarker"), 1);
}

/// Catches: the remedy named in the guard message does not work: after
/// `collection remove archive`, the next update must prune `docs` (now the only
/// convention collection) and leave no "none pruned" error behind.
#[test]
fn the_remedy_in_the_guard_message_clears_the_situation() {
    let (_dir, root) = store();
    readme(&root);
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::remove_dir_all(root.join("docs")).unwrap();
    let guarded = handle_update(&ctx, &root).expect("guarded update");
    let msg = guarded
        .errors
        .iter()
        .find(|e| e.contains("none pruned"))
        .expect("guard message");
    assert!(msg.contains("collection remove"), "remedy named: {msg}");

    assert!(handle_collection_remove(&ctx, "archive").expect("remove"));
    let result = handle_update(&ctx, &root).expect("update after remedy");

    assert_eq!(hits(&ctx, "archivemarker"), 0, "removed collection's docs");
    assert_eq!(hits(&ctx, "docsmarker"), 0);
    assert!(
        !result.errors.iter().any(|e| e.contains("none pruned")),
        "{:?}",
        result.errors
    );
    assert_eq!(hits(&ctx, "readmemarker"), 1);
}

/// Catches: the guard message lists `_root` (or omits a vanished name), which
/// would send the user to `collection remove _root`.
#[test]
fn the_guard_message_names_exactly_the_vanished_convention_collections() {
    let (_dir, root) = store();
    readme(&root);
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::remove_dir_all(root.join("docs")).unwrap();

    let result = handle_update(&ctx, &root).expect("guarded update");

    let msg = result
        .errors
        .iter()
        .find(|e| e.contains("none pruned"))
        .expect("guard message");
    assert!(msg.contains("archive") && msg.contains("docs"), "{msg}");
    assert!(!msg.contains("_root"), "{msg}");
}
