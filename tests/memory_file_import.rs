//! Restoring an entry file must be a supported operation, not a raw SQL job.
//!
//! Story 017-a378: `mdkb memory add` always stamps `created_at`/`updated_at`
//! with now() and has no flag to preserve them, so restoring 265 orphaned entry
//! files would have flattened a June–August timeline into one day and destroyed
//! recency ranking. The only alternative was a raw `sqlite3 INSERT` against
//! index.sqlite, which bypasses the connection pragmas (busy_timeout, WAL,
//! synchronous) and the `.mutation.lock` protocol. Doing exactly that against a
//! live store corrupted `memory_fts_data` — `Rowid out of order`,
//! `2nd reference to page 12862` — and recovery needed a pre-write file copy.
//!
//! So the tests here are about the *timeline* and the *connection*: an import
//! that rewrites timestamps is useless for a restore, and one that opens its own
//! connection is the thing that corrupted the store.

use mdkb::cli::handlers::{handle_init, handle_memory_import_file, handle_update};
use mdkb::core::Context;

const CREATED: i64 = 1_718_000_000; // June
const UPDATED: i64 = 1_722_000_000; // July

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    (dir, root)
}

/// An orphaned entry file as it actually exists on disk: pre-split frontmatter,
/// carrying the local telemetry that `to_markdown` no longer writes.
fn write_entry_file(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(format!("{id}.md"));
    std::fs::write(
        &path,
        format!(
            "---\nid: {id}\ntitle: Restored entry\nentry_type: decision\n\
             source_type: official_docs\nstatus: active\ntags: [restore]\n\
             created_at: {CREATED}\nupdated_at: {UPDATED}\n\
             access_count: 17\nconfirmations: 4\nexpires_at: 1900000000\n\
             ---\n\nThe decision body.\n"
        ),
    )
    .unwrap();
    path
}

/// The whole point of the story: the timeline survives.
///
/// `mdkb memory add` would stamp both timestamps with now(), collapsing months
/// of history into the moment of the restore and destroying recency ranking for
/// every entry brought back.
#[test]
fn import_preserves_the_authored_timeline_and_provenance() {
    let (dir, root) = store();
    let file = write_entry_file(&dir.path().join("incoming"), "restored-decision");

    let ctx = Context::open(&root).expect("open");
    handle_memory_import_file(&ctx, &file).expect("import");

    let e = mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "restored-decision")
        .unwrap()
        .expect("entry must exist");
    assert_eq!(e.created_at, CREATED, "created_at must be verbatim");
    assert_eq!(e.updated_at, UPDATED, "updated_at must be verbatim");
    assert_eq!(
        e.source_type,
        mdkb::store::memory::SourceType::OfficialDocs,
        "source_type drives the confidence multiplier and must not be defaulted"
    );
    assert_eq!(e.expires_at, Some(1_900_000_000));
    assert_eq!(
        e.access_count, 17,
        "an explicit restore preserves the counters verbatim — unlike a git sync, \
         where a colleague's counters are theirs and start fresh here"
    );
    assert_eq!(e.confirmations, 4);
}

/// Idempotence, stated as an explicit conflict rather than a silent overwrite.
/// A restore that quietly clobbers a live entry is worse than one that refuses.
#[test]
fn a_second_import_of_the_same_id_is_refused_not_duplicated() {
    let (dir, root) = store();
    let file = write_entry_file(&dir.path().join("incoming"), "twice");

    let ctx = Context::open(&root).expect("open");
    handle_memory_import_file(&ctx, &file).expect("first import");

    let err = handle_memory_import_file(&ctx, &file).expect_err("second import must be refused");
    assert!(
        err.to_string().contains("twice"),
        "the conflict must name the id: {err}"
    );

    let count: i64 = ctx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_entries WHERE id = 'twice'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "never a duplicate row");
}

/// A file whose frontmatter disagrees with its name is ambiguous, and guessing
/// which one is authoritative is how a restore silently writes the wrong id.
#[test]
fn an_id_that_disagrees_with_the_filename_is_refused() {
    let (dir, root) = store();
    let incoming = dir.path().join("incoming");
    write_entry_file(&incoming, "declared-id");
    let renamed = incoming.join("different-name.md");
    std::fs::rename(incoming.join("declared-id.md"), &renamed).unwrap();

    let ctx = Context::open(&root).expect("open");
    let err = handle_memory_import_file(&ctx, &renamed).expect_err("mismatch must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("declared-id") && msg.contains("different-name"),
        "the error must name both spellings so the operator can pick: {msg}"
    );
    assert!(
        mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "declared-id")
            .unwrap()
            .is_none(),
        "nothing may be written when the id is ambiguous"
    );
}

/// The connection is the other half of the story. A raw `sqlite3 INSERT` skipped
/// busy_timeout, WAL and the mutation lock, and corrupted `memory_fts_data`.
/// Going through `Context` means the entry is in FTS the moment it lands.
#[test]
fn an_imported_entry_is_immediately_searchable_and_then_embedded() {
    let (dir, root) = store();
    let file = write_entry_file(&dir.path().join("incoming"), "searchable-entry");

    let ctx = Context::open(&root).expect("open");
    handle_memory_import_file(&ctx, &file).expect("import");

    let hits: i64 = ctx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_fts WHERE memory_fts MATCH 'decision'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        hits >= 1,
        "the FTS triggers must have fired — this is what the raw INSERT bypassed"
    );

    // Embedding backfill is `mdkb update`'s job and must need no manual step.
    handle_update(&ctx, &root).expect("update");
    let pending = mdkb::store::memory::count_pending_embeddings(&ctx.conn).unwrap();
    assert_eq!(
        pending, 0,
        "the next update must leave no imported entry pending an embedding"
    );
}
