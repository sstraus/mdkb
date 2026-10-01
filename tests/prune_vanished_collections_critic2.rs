//! Round 2 critic cases for story 214-1b93: attacks on the mass-loss guard and
//! on what counts as "the directory is gone".

use mdkb::cli::handlers::{handle_init, handle_update};
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

/// Catches: the guard counts `_root` (path `.`, which can never vanish) in its
/// denominator, so any repo with a README silently disables the unmounted-volume
/// protection: the same two missing directories are skipped without a README
/// and pruned with one.
#[test]
fn the_guard_does_not_depend_on_a_root_readme_existing() {
    let (_dir, root) = store();
    std::fs::write(root.join("README.md"), "# Readme\n\nreadmemarker\n").unwrap();
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    assert_eq!(hits(&ctx, "archivemarker"), 1, "precondition");
    assert_eq!(hits(&ctx, "docsmarker"), 1, "precondition");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::remove_dir_all(root.join("docs")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert!(
        result.collections_pruned.is_empty(),
        "every path-bearing convention collection went at once: {:?}",
        result.collections_pruned
    );
}

/// Catches: the guard fires on any missing directory instead of only when all
/// of them are missing, so deleting one of two convention directories keeps the
/// deleted one's stale documents searchable (the original defect).
#[test]
fn deleting_one_of_two_convention_directories_prunes_only_that_one() {
    let (_dir, root) = store();
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert_eq!(result.collections_pruned, vec!["archive".to_string()]);
    assert_eq!(hits(&ctx, "archivemarker"), 0);
    assert_eq!(
        hits(&ctx, "docsmarker"),
        1,
        "the surviving directory is untouched"
    );
}

/// Catches: the guard remembers its decision (or a skipped run is treated as a
/// prune), so once one of the two directories returns, the one still missing is
/// never pruned and keeps being injected.
#[test]
fn a_skipped_prune_is_retried_once_the_situation_is_no_longer_all_or_nothing() {
    let (_dir, root) = store();
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::remove_dir_all(root.join("docs")).unwrap();
    let skipped = handle_update(&ctx, &root).expect("guarded update");
    assert!(
        skipped.collections_pruned.is_empty(),
        "precondition: guard fired"
    );

    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    let result = handle_update(&ctx, &root).expect("third update");

    assert_eq!(result.collections_pruned, vec!["archive".to_string()]);
    assert_eq!(hits(&ctx, "archivemarker"), 0);
    assert_eq!(hits(&ctx, "docsmarker"), 1);
}

/// Catches: existence is tested with `symlink_metadata`/`is_symlink`, so a
/// directory replaced by a dangling symlink counts as present and its documents
/// stay searchable.
#[cfg(unix)]
#[test]
fn a_dangling_symlink_in_place_of_the_directory_counts_as_gone() {
    let (_dir, root) = store();
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::os::unix::fs::symlink(root.join("nowhere"), root.join("archive")).unwrap();
    let result = handle_update(&ctx, &root).expect("second update");

    assert_eq!(result.collections_pruned, vec!["archive".to_string()]);
    assert_eq!(hits(&ctx, "archivemarker"), 0);
}

/// Catches: a directory replaced by a regular file of the same name passes the
/// existence test and leaves the old documents searchable.
#[test]
fn a_regular_file_in_place_of_the_directory_does_not_keep_old_documents() {
    let (_dir, root) = store();
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");

    std::fs::remove_dir_all(root.join("archive")).unwrap();
    std::fs::write(root.join("archive"), "now a file").unwrap();
    let _ = handle_update(&ctx, &root);

    assert_eq!(hits(&ctx, "archivemarker"), 0);
}

/// Catches: the sidecar snapshot is written from the pre-prune counts, so the
/// run after a prune reports the pruned collection as vanished and flags the
/// run as failing.
#[test]
fn the_run_after_a_prune_is_clean() {
    let (_dir, root) = store();
    write_doc(&root, "archive", "a.md", "# A\n\narchivemarker\n");
    write_doc(&root, "docs", "d.md", "# D\n\ndocsmarker\n");
    let ctx = Context::open(&root).expect("open");
    handle_update(&ctx, &root).expect("first update");
    std::fs::remove_dir_all(root.join("archive")).unwrap();
    handle_update(&ctx, &root).expect("prune update");

    let result = handle_update(&ctx, &root).expect("follow-up update");

    assert!(result.collections_pruned.is_empty());
    assert!(result.collections_vanished.is_empty());
    assert!(result.errors.is_empty(), "{:?}", result.errors);
}
