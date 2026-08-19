//! What autoheal must carry across a quarantine.
//!
//! Story 012-19e7: a store reported "2046 docs indexed", and a later run in the
//! same directory reported 3 docs with the `map` collection absent from
//! `mdkb collection list`. No `collection remove` was ever issued. The suspected
//! cause was a `.mdkb/config.toml` edit; it was not. Autoheal quarantines a
//! corrupt index, rebuilds an empty one, and salvages `memory_entries` and
//! `memory_edges` out of the old file — and nothing else. `collections` is
//! wiped, so the next `mdkb update` finds no collection registered, indexes only
//! the root, prints a success line and exits 0.
//!
//! The distinction that matters: a table reconstructible from source files
//! (`documents`, `content`, `edges`) may be dropped, because `mdkb update`
//! rebuilds it. A table holding a decision nobody can re-derive — which
//! directories are collections, what an entry used to say — must survive, or the
//! quarantine turns a recoverable corruption into permanent data loss.

use mdkb::cli::handlers::{handle_collection_add, handle_init};
// Only the quarantine tests write memory entries, and those are unix-only
// (they need the daemon's file layout), so this import has no user on Windows.
#[cfg(unix)]
use mdkb::cli::handlers::handle_memory_add;
use mdkb::core::Context;
use mdkb::store::collections;

/// Build a store, populate it, then tear the database file in half so the next
/// open must quarantine and rebuild.
///
/// Gated with its callers: every test that corrupts a store is unix-only, so
/// on Windows this helper would be dead code rather than a missing case.
#[cfg(unix)]
fn corrupt_after(setup: impl FnOnce(&Context)) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    {
        let ctx = Context::open(&root).expect("open");
        setup(&ctx);
    }

    let db = root.join(".mdkb").join("index.sqlite");
    {
        let conn = rusqlite::Connection::open(&db).expect("open for filler");
        // Grow the file with rows nobody is asserting on, so the torn band lands
        // in derivable data rather than on the handful of pages under test. A
        // store this small would otherwise keep its only collection row in the
        // tail, and the test would be measuring where the scribble landed
        // instead of what salvage carries.
        conn.execute_batch(
            "BEGIN;
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i < 2000)
             INSERT INTO content (hash, body, created_at)
             SELECT 'filler-' || i, hex(randomblob(256)), 0 FROM n;
             COMMIT;",
        )
        .expect("seed filler");
        // Checkpoint: under WAL the seeded rows may live entirely in `-wal`, and
        // scribbling only the main file would corrupt nothing.
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("checkpoint into the main file");
    }

    // Tear a band of pages off the TAIL, not the whole file. The point of these
    // tests is what survives salvage, and salvage has to be able to read the
    // quarantined file: truncating it in half destroys the schema, which proves
    // only that a totally destroyed file yields nothing.
    const PAGE: u64 = 4096;
    let len = std::fs::metadata(&db).expect("stat").len();
    let span = (8 * PAGE).min(len / 3);
    let start = (len - span) / PAGE * PAGE;
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&db)
            .expect("open db");
        f.seek(SeekFrom::Start(start)).expect("seek");
        f.write_all(&vec![0xA5_u8; span as usize])
            .expect("scribble");
        f.sync_all().expect("sync");
    }

    // The integrity probe is throttled by a marker sidecar, so a store checked
    // moments ago is trusted on the next open. Corrupting behind its back is a
    // test artifact; drop the marker so the probe actually runs.
    let _ = std::fs::remove_file(root.join(".mdkb/index.sqlite.integrity-ok"));

    (dir, root)
}

/// The reported failure, as a regression test: a registered collection must
/// survive the rebuild.
///
/// Nothing else can re-register it. `documents` is reindexed from the files on
/// disk, but the *fact* that `map/` is a collection exists only in this table —
/// so losing it silently downgrades every later `mdkb update` to indexing the
/// root alone, which is exactly what shipped 3 docs where 2307 were expected.
/// Unix-only: blocked by the Windows live-lock probe defect — see the note
/// on `corrupt_db_is_left_in_place_while_a_connection_is_live` in
/// `src/store/heal.rs`.
#[cfg(unix)]
#[test]
fn a_registered_collection_survives_a_quarantine() {
    let (_dir, root) = corrupt_after(|ctx| {
        std::fs::create_dir_all(ctx.root().join("map")).unwrap();
        handle_collection_add(ctx, "map", "map", "**/*.md").expect("add collection");
    });

    let ctx = Context::open(&root).expect("reopen after corruption");
    assert!(
        ctx.rebuilt_from_corruption,
        "the fixture must actually have triggered a quarantine, or this test \
         proves nothing"
    );

    let names: Vec<String> = collections::list_collections(&ctx.conn)
        .expect("list")
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert!(
        names.contains(&"map".to_string()),
        "the collection registration must survive the rebuild — nothing else \
         can re-derive it; got {names:?}"
    );
}

/// Memory edit history — which now also holds the losing side of every file/DB
/// conflict (story 014-fdf0) — is the other thing no reindex can rebuild.
/// Unix-only: blocked by the Windows live-lock probe defect — see the note
/// on `corrupt_db_is_left_in_place_while_a_connection_is_live` in
/// `src/store/heal.rs`.
#[cfg(unix)]
#[test]
fn memory_revisions_survive_a_quarantine() {
    let (_dir, root) = corrupt_after(|ctx| {
        handle_memory_add(
            ctx,
            "revised",
            "Revised",
            "topic",
            None,
            "first body",
            None,
            None,
            None,
            None,
        )
        .expect("add");
        // A second write of the same id records a revision.
        handle_memory_add(
            ctx,
            "revised",
            "Revised",
            "topic",
            None,
            "second body",
            None,
            None,
            None,
            None,
        )
        .expect("rewrite");
        assert!(
            !mdkb::store::memory::get_revisions(&ctx.conn, "revised")
                .expect("revisions")
                .is_empty(),
            "the fixture must produce a revision to salvage"
        );
    });

    let ctx = Context::open(&root).expect("reopen after corruption");
    assert!(ctx.rebuilt_from_corruption, "fixture must have quarantined");

    let revs = mdkb::store::memory::get_revisions(&ctx.conn, "revised").expect("revisions");
    assert!(
        !revs.is_empty(),
        "edit history must survive the rebuild — it holds conflict losers that \
         exist nowhere else"
    );
}

/// The counterpart: a healthy store must not be disturbed. A salvage that fired
/// on every open would be a silent second writer on the hot path.
#[test]
fn a_healthy_store_is_not_rebuilt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    {
        let ctx = Context::open(&root).expect("open");
        std::fs::create_dir_all(ctx.root().join("map")).unwrap();
        handle_collection_add(&ctx, "map", "map", "**/*.md").expect("add collection");
    }

    let ctx = Context::open(&root).expect("reopen");
    assert!(
        !ctx.rebuilt_from_corruption,
        "a sound index must never be quarantined"
    );
    assert_eq!(collections::list_collections(&ctx.conn).unwrap().len(), 1);
}
