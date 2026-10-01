//! Critic cases for story 214-1b93: attacks on the pruning of auto-detected
//! collections whose directory is gone.

use mdkb::cli::handlers::{handle_collection_add, handle_collection_update, handle_init, handle_update};
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

/// Catches: `collection update --path` on a convention collection keeps
/// `source = convention` (only `--pattern` flips it), so a collection the user
/// deliberately pointed at another directory is pruned, with its documents,
/// the day that directory is absent (unmounted volume, branch switch).
#[test]
fn a_convention_collection_the_user_repointed_is_not_pruned() {
    let (_dir, root) = store();
    write_doc(&root, "docs", "a.md", "# A\n\nconventiondoc\n");
    write_doc(&root, "notes", "n.md", "# N\n\nrepointeddoc\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    handle_collection_update(&ctx, "docs", Some("notes"), None).expect("repoint docs at notes");
    handle_update(&ctx, &root).expect("second update");
    assert_eq!(hits(&ctx, "repointeddoc"), 1, "precondition: repointed docs indexed");

    std::fs::remove_dir_all(root.join("notes")).unwrap();
    let result = handle_update(&ctx, &root).expect("third update");

    assert!(
        result.collections_pruned.is_empty(),
        "a collection the user pointed somewhere by hand must not be pruned: {:?}",
        result.collections_pruned
    );
}

/// Catches: the prune is committed outside the update transaction and the
/// sidecar snapshot is only rewritten on success, so an update that fails
/// after pruning leaves the old snapshot behind. The next healthy update then
/// reports the pruned collection as `vanished` with an error (a false loss
/// alarm for a deliberate removal).
#[cfg(unix)]
#[test]
fn a_failed_update_after_a_prune_does_not_leave_a_false_vanished_report() {
    let (_dir, root) = store();
    let outside = tempfile::tempdir().expect("outside");
    write_doc(&root, "archive", "old.md", "# Old\n\narchivemarker\n");
    write_doc(&root, "ext", "e.md", "# E\n\next marker\n");
    let ctx = Context::open(&root).expect("open");
    handle_collection_add(&ctx, "ext", "ext", "**/*.md").expect("add ext");
    handle_update(&ctx, &root).expect("first update");
    assert_eq!(hits(&ctx, "archivemarker"), 1);

    // Run 2: archive is deleted (prune) and ext now escapes the root (hard error).
    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::remove_dir_all(root.join("ext")).unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("ext")).unwrap();
    let failed = handle_update(&ctx, &root);
    assert!(failed.is_err(), "precondition: the escaping collection fails the update");

    // Run 3: healthy again.
    std::fs::remove_file(root.join("ext")).unwrap();
    write_doc(&root, "ext", "e.md", "# E\n\next marker\n");
    let result = handle_update(&ctx, &root).expect("third update");

    assert!(
        result.collections_vanished.is_empty(),
        "a pruned collection was reported as vanished after a failed run: {:?}",
        result.collections_vanished
    );
    assert!(
        !result.errors.iter().any(|e| e.contains("archive")),
        "no error may name the pruned collection: {:?}",
        result.errors
    );
}

/// Catches: pruning that is not reversible. The directory returns after a
/// prune; `apply_conventions` must register it again and index its documents.
#[test]
fn a_pruned_directory_that_returns_is_registered_and_indexed_again() {
    let (_dir, root) = store();
    write_doc(&root, "archive", "old.md", "# Old\n\nfirstgeneration\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    std::fs::remove_dir_all(root.join("archive")).unwrap();
    handle_update(&ctx, &root).expect("prune update");

    write_doc(&root, "archive", "new.md", "# New\n\nsecondgeneration\n");
    let result = handle_update(&ctx, &root).expect("restore update");

    assert_eq!(hits(&ctx, "secondgeneration"), 1, "the returned directory is indexed");
    assert_eq!(hits(&ctx, "firstgeneration"), 0, "the old generation stays gone");
    assert!(result.collections_pruned.is_empty());
    assert!(result.collections_vanished.is_empty());
}

/// Catches: pruning by path prefix or by "anything covering that directory":
/// deleting `docs/` must not touch `_root` (path `.`), whose own files stay.
#[test]
fn pruning_docs_leaves_the_root_collection_and_its_documents() {
    let (_dir, root) = store();
    std::fs::write(root.join("README.md"), "# Readme\n\nrootmarker\n").unwrap();
    write_doc(&root, "docs", "a.md", "# A\n\ndocsmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    assert_eq!(hits(&ctx, "rootmarker"), 1);

    std::fs::remove_dir_all(root.join("docs")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert_eq!(result.collections_pruned, vec!["docs".to_string()]);
    assert_eq!(hits(&ctx, "rootmarker"), 1, "_root keeps its document");
    assert_eq!(hits(&ctx, "docsmarker"), 0);
    assert!(result.errors.is_empty(), "{:?}", result.errors);
}
