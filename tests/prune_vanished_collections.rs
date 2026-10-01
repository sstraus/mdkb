//! `mdkb update` must unregister an auto-detected collection whose directory is
//! gone, or its documents stay searchable and recall keeps injecting them.
//!
//! Story 214-1b93: orchestrator deleted `archive/`; the `archive` collection kept
//! serving `archive:orchestration-2026-09-26.md` into coordinator turns.

use mdkb::cli::handlers::{handle_collection_add, handle_init, handle_update};
use mdkb::core::Context;
use mdkb::core::ops::handle_search;

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    (dir, root)
}

fn write_docs(root: &std::path::Path, dir: &str) {
    let d = root.join(dir);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("old.md"), "# Old\n\nzebracrossing marker\n").unwrap();
}

fn hits(ctx: &Context) -> usize {
    handle_search(ctx, "zebracrossing", 10, None)
        .expect("search")
        .len()
}

/// Catches: update walks only existing collection dirs, reports the missing path
/// and never prunes, so the deleted directory's documents stay searchable.
#[test]
fn deleting_a_convention_directory_drops_its_collection_and_documents() {
    let (_dir, root) = store();
    write_docs(&root, "archive");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    assert_eq!(
        hits(&ctx),
        1,
        "precondition: the archive document is indexed"
    );

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert_eq!(
        hits(&ctx),
        0,
        "documents of a deleted directory must be gone"
    );
    assert_eq!(result.collections_pruned, vec!["archive".to_string()]);
    assert!(
        result.collections.iter().all(|c| c.name != "archive"),
        "the pruned collection must leave the per-collection report"
    );
}

/// Catches: pruning reuses the loss signal, so a deliberate removal is reported
/// as a collection that "vanished" and the run is flagged as failing.
#[test]
fn a_pruned_collection_is_not_reported_as_vanished() {
    let (_dir, root) = store();
    write_docs(&root, "archive");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert!(result.collections_vanished.is_empty());
    assert!(
        !result.errors.iter().any(|e| e.contains("archive")),
        "no error may name the pruned collection: {:?}",
        result.errors
    );
}

/// Catches: pruning every collection with a missing path, which would wipe a
/// hand-registered collection on an unmounted volume.
#[test]
fn a_manual_collection_with_a_missing_directory_keeps_its_documents() {
    let (_dir, root) = store();
    write_docs(&root, "notes");
    let ctx = Context::open(&root).expect("open");
    handle_collection_add(&ctx, "notes", "notes", "**/*.md").expect("add");
    handle_update(&ctx, &root).expect("first update");
    assert_eq!(hits(&ctx), 1);

    std::fs::remove_dir_all(root.join("notes")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert_eq!(
        hits(&ctx),
        1,
        "a manual registration is the user's, not pruned"
    );
    assert!(result.collections_pruned.is_empty());
}

/// Catches: an unmounted volume under the root makes every convention directory
/// look deleted, and the prune wipes all of their documents in one run.
#[test]
fn losing_every_convention_directory_at_once_prunes_none() {
    let (_dir, root) = store();
    write_docs(&root, "archive");
    write_docs(&root, "docs");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::remove_dir_all(root.join("docs")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert!(result.collections_pruned.is_empty());
    assert!(
        result.errors.iter().any(|e| e.contains("none pruned")),
        "the skipped prune must be reported: {:?}",
        result.errors
    );
}

/// Catches: counting `_root` (always present when a README exists) toward the
/// mass-loss guard, which disables it in every repo that has a README.
#[test]
fn losing_every_convention_directory_prunes_none_when_a_root_readme_exists() {
    let (_dir, root) = store();
    std::fs::write(root.join("README.md"), "# Readme\n\nreadmemarker\n").unwrap();
    write_docs(&root, "archive");
    write_docs(&root, "docs");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::remove_dir_all(root.join("docs")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert!(result.collections_pruned.is_empty());
    assert_eq!(hits(&ctx), 2, "both directories' documents must survive");
}
