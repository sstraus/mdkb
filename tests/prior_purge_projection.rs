//! A migration that deletes rows must dispose of their markdown projection.
//!
//! Story 016-a6dd: the v11 → v12 migration deletes every legacy System-B
//! behavioural prior (`prior-` + 16 hex). The delete cascades to FTS, embeddings
//! and revisions through triggers, but nothing touched
//! `.mdkb/memory/entries/<id>.md`. The files survived with `status: active`
//! frontmatter and no database row — 113 of them on one store.
//!
//! Bidirectional sync (story 014-fdf0) turned that from litter into a
//! correctness bug: a file with no row is now *imported*, so the next
//! `mdkb update` would resurrect precisely the rows the migration deleted, and
//! the purge would undo itself.

use mdkb::cli::handlers::{handle_init, handle_update};
use mdkb::core::Context;

const LEGACY_ID: &str = "prior-deadbeefdeadbeef";

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    (dir, root)
}

fn entries(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".mdkb/memory/entries")
}

fn archive(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".mdkb/memory/archive")
}

/// Write the projection a purged legacy prior leaves behind: a file with
/// `status: active` and no database row.
fn write_legacy_prior_file(root: &std::path::Path, id: &str) {
    std::fs::create_dir_all(entries(root)).unwrap();
    std::fs::write(
        entries(root).join(format!("{id}.md")),
        format!(
            "---\nid: {id}\ntitle: Legacy behavioural prior\nentry_type: prior\n\
             source_type: auto_extracted\nstatus: active\ntags: []\n\
             created_at: 1700000000\nupdated_at: 1700000000\n---\n\nAlways do the thing.\n"
        ),
    )
    .unwrap();
}

/// Rewind the store's recorded schema version so the next open replays the
/// migrations from before the purge.
fn rewind_schema_to(root: &std::path::Path, version: i32) {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.execute("UPDATE schema_version SET version = ?1", [version])
        .expect("rewind");
}

/// The purge must take the file with it — through the same disposal used by
/// `mdkb memory rm`, not a second copy of the rule.
#[test]
fn the_v12_purge_disposes_of_the_projection() {
    let (_dir, root) = store();
    {
        let ctx = Context::open(&root).expect("open");
        ctx.conn
            .execute(
                "INSERT INTO memory_entries
                 (id, title, content, entry_type, tags, status, created_at, updated_at, source_type)
                 VALUES (?1, 'Legacy', 'body', 'prior', '[]', 'active', 0, 0, 'auto_extracted')",
                [LEGACY_ID],
            )
            .expect("seed legacy prior");
    }
    write_legacy_prior_file(&root, LEGACY_ID);
    rewind_schema_to(&root, 11);

    let ctx = Context::open(&root).expect("reopen runs the v12 purge");
    assert!(
        mdkb::store::memory::get_entry_without_tracking(&ctx.conn, LEGACY_ID)
            .unwrap()
            .is_none(),
        "the purge must still delete the row"
    );
    assert!(
        !entries(&root).join(format!("{LEGACY_ID}.md")).exists(),
        "the purge must dispose of the projection, or bidirectional sync \
         re-imports the row it just deleted"
    );
    assert!(
        archive(&root).join(format!("{LEGACY_ID}.md")).exists(),
        "disposal means archived, not deleted — the same rule `mdkb memory rm` uses"
    );
}

/// The 113 files already on disk. A store that migrated before the fix is past
/// v12, so replaying that migration will never help it; the heal has to be its
/// own one-time step.
#[test]
fn a_store_already_past_v12_is_healed_of_its_leftover_files() {
    let (_dir, root) = store();
    for i in 0..3 {
        write_legacy_prior_file(&root, &format!("prior-abcdef012345678{i}"));
    }
    rewind_schema_to(&root, 19);

    let ctx = Context::open(&root).expect("reopen runs the sweep");
    for i in 0..3 {
        let id = format!("prior-abcdef012345678{i}");
        assert!(
            !entries(&root).join(format!("{id}.md")).exists(),
            "{id} must be swept off disk"
        );
        assert!(
            archive(&root).join(format!("{id}.md")).exists(),
            "{id} must be archived, not destroyed"
        );
    }

    // And the sweep must actually prevent the resurrection it exists to stop.
    let result = handle_update(&ctx, &root).expect("update");
    assert_eq!(
        result.memory_files_imported, 0,
        "no purged prior may be re-imported"
    );
}

/// The sweep must not touch anything else. A file with a database row is a live
/// entry, and a non-prior file is none of the sweep's business.
#[test]
fn the_sweep_leaves_live_entries_and_ordinary_files_alone() {
    let (_dir, root) = store();
    {
        let ctx = Context::open(&root).expect("open");
        mdkb::cli::handlers::handle_memory_add(
            &ctx,
            "live-topic",
            "Live",
            "topic",
            None,
            "body",
            None,
            None,
            None,
            None,
        )
        .expect("add");
        mdkb::cli::handlers::sync_memory_files(&ctx).expect("project");
    }
    // A hand-authored entry with no row: importable, and not a legacy prior.
    std::fs::write(
        entries(&root).join("colleagues-entry.md"),
        "---\nid: colleagues-entry\ntitle: Theirs\nentry_type: decision\n\
         source_type: user_statement\nstatus: active\ntags: []\n\
         created_at: 1700000000\nupdated_at: 1700000000\n---\n\nBody.\n",
    )
    .unwrap();
    rewind_schema_to(&root, 19);

    let ctx = Context::open(&root).expect("reopen runs the sweep");
    assert!(
        entries(&root).join("live-topic.md").exists(),
        "an entry with a database row must never be swept"
    );
    assert!(
        entries(&root).join("colleagues-entry.md").exists(),
        "a non-prior file with no row is an import, not litter"
    );

    let result = handle_update(&ctx, &root).expect("update");
    assert_eq!(
        result.memory_files_imported, 1,
        "the colleague's entry must still import normally"
    );
}
