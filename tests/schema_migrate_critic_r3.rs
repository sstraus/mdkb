//! Round-3 critic cases for the version read inside `BEGIN IMMEDIATE` (story 219-9d67).
//!
//! `Context::open` serialises `init_schema` behind an exclusive writer lock, so
//! the race the in-transaction version read closes is only reachable by callers
//! that go straight to `init_schema` on their own connection. These tests do that.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use mdkb::store::schema::{SCHEMA_VERSION, init_schema};
use rusqlite::Connection;

const V17_SQL: &str = include_str!("fixtures/schema_v17.sql");

fn v17_db(parent: &Path) -> PathBuf {
    let db = parent.join("index.sqlite");
    let conn = Connection::open(&db).expect("create");
    conn.execute_batch(V17_SQL).expect("v17 schema");
    conn.execute("INSERT INTO schema_version (version) VALUES (17)", [])
        .expect("record v17");
    conn.execute_batch(
        "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
         VALUES ('one', 'One', 'body', 'topic', '[]', 1, 1);",
    )
    .expect("seed");
    // WAL, as every real open: the stale reader must not block on the writer.
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal");
    db
}

fn opened(db: &Path) -> Connection {
    let conn = Connection::open(db).expect("open");
    conn.busy_timeout(Duration::from_secs(10)).expect("busy");
    conn
}

fn memory_count(db: &Path) -> i64 {
    Connection::open(db)
        .expect("open")
        .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
        .expect("count")
}

/// Runs `init_schema` on a second connection that has already read the stale
/// version and is blocked on the write lock held by `holder`; `while_held` runs
/// on the holder before it commits.
fn init_while_holding(db: &Path, while_held: impl FnOnce(&Connection)) -> Result<(), String> {
    let holder = opened(db);
    holder.execute_batch("BEGIN IMMEDIATE").expect("lock");
    let b = {
        let db = db.to_path_buf();
        std::thread::spawn(move || init_schema(&opened(&db)).map_err(|e| e.to_string()))
    };
    std::thread::sleep(Duration::from_millis(600));
    while_held(&holder);
    holder.execute_batch("COMMIT").expect("commit");
    b.join().expect("thread")
}

/// Catches: the version read before `BEGIN IMMEDIATE`. The first migrator
/// commits "current" while the second waits on the lock; the second must then do
/// nothing. A stale read re-runs the v17→current steps, and the v21 purge
/// deletes the blank-id row planted by the first.
#[test]
fn a_waiter_whose_store_was_migrated_while_it_waited_runs_no_step() {
    let parent = tempfile::tempdir().expect("parent");
    let db = v17_db(parent.path());

    init_while_holding(&db, |holder| {
        holder
            .execute_batch(&format!(
                "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
                 VALUES ('   ', 'Blank', 'blank', 'topic', '[]', 3, 3);
                 UPDATE schema_version SET version = {SCHEMA_VERSION};"
            ))
            .expect("pretend migrated");
    })
    .expect("the waiter must succeed");

    assert_eq!(memory_count(&db), 2, "the waiter re-ran a migration step");
}

/// Catches: `_ => Ok(())` swallowing a store that a NEWER binary migrated while
/// this (older) binary waited on the lock. `refuse_future_schema` ran before the
/// wait; the in-transaction read must refuse too, not carry on opening a store
/// this binary cannot understand.
#[test]
fn a_waiter_whose_store_became_newer_while_it_waited_refuses_it() {
    let parent = tempfile::tempdir().expect("parent");
    let db = v17_db(parent.path());

    let result = init_while_holding(&db, |holder| {
        holder
            .execute_batch(&format!(
                "UPDATE schema_version SET version = {};",
                SCHEMA_VERSION + 1
            ))
            .expect("pretend a newer binary migrated");
    });

    assert!(result.is_err(), "an older binary accepted a newer store");
}

/// Catches: two direct `init_schema` callers on one stale store where the loser
/// fails (duplicate column / table) or leaves the version behind.
#[test]
fn two_direct_init_schema_callers_on_one_stale_store_both_succeed() {
    for round in 0..5 {
        let parent = tempfile::tempdir().expect("parent");
        let db = v17_db(parent.path());
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (db, barrier) = (db.clone(), barrier.clone());
                std::thread::spawn(move || {
                    let conn = opened(&db);
                    barrier.wait();
                    init_schema(&conn).map_err(|e| e.to_string())
                })
            })
            .collect();
        for h in handles {
            h.join()
                .expect("thread")
                .unwrap_or_else(|e| panic!("round {round}: {e}"));
        }
        let version: i32 = Connection::open(&db)
            .expect("open")
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }
}
