//! DATA-B1 (story 068): the production DB-open path (`Context::open`) must set
//! `busy_timeout` so that ordinary write-lock contention between the daemon and
//! one-shot CLI processes resolves by waiting briefly, not by an immediate
//! `SQLITE_BUSY` error.

use std::time::{Duration, Instant};

use mdkb::cli::handlers::{
    Context, handle_collection_add, handle_init, handle_update_force,
};
use tempfile::TempDir;

/// Writer A holds the write lock for ~300ms; writer B (a second connection to
/// the same file, opened via the production `Context::open` path) must wait for
/// the lock rather than erroring immediately. Without `busy_timeout` B returns
/// `SQLITE_BUSY` in single-digit milliseconds; with it, B blocks until A commits
/// and then succeeds.
#[test]
fn concurrent_writers_wait_instead_of_erroring_immediately() {
    let tmp = TempDir::new().expect("tempdir");
    handle_init(tmp.path()).expect("init mdkb");

    let ctx_a = Context::open(tmp.path()).expect("open ctx A");
    // Acquire and hold the write lock.
    ctx_a
        .conn
        .execute_batch("BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS _contention_a(x);")
        .expect("A takes write lock");

    // Release A's lock ~300ms from now, on another thread, so a contending
    // opener that respects busy_timeout will observe the lock free within the
    // timeout window.
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        ctx_a.conn.execute_batch("COMMIT;").expect("A commits");
    });

    // Opening B runs schema migrations (writes), which contend on A's lock.
    // With busy_timeout set this call waits for A to release; without it, it
    // fails immediately with SQLITE_BUSY.
    let t0 = Instant::now();
    let ctx_b = Context::open(tmp.path());
    let waited = t0.elapsed();

    releaser.join().expect("releaser thread");

    assert!(
        ctx_b.is_ok(),
        "opening a second connection should wait for the lock and succeed, \
         not error immediately: {:?}",
        ctx_b.err()
    );
    assert!(
        waited >= Duration::from_millis(200),
        "the second open should have waited on busy_timeout for A to release \
         (waited only {waited:?}) — busy_timeout is likely not set"
    );
}

/// `Context::open` performs schema migrations and initializes FTS/vector
/// virtual tables. Concurrent server + CLI startup must serialize that phase;
/// otherwise SQLite can report `SQLITE_SCHEMA: vtable constructor failed`.
#[test]
fn concurrent_context_opens_do_not_race_virtual_table_setup() {
    let tmp = TempDir::new().expect("tempdir");
    handle_init(tmp.path()).expect("init mdkb");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let mut workers = Vec::new();
    for _ in 0..8 {
        let root = tmp.path().to_path_buf();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            let ctx = Context::open(root).expect("concurrent context open");
            ctx.conn
                .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("FTS virtual table is usable")
        }));
    }

    for worker in workers {
        assert_eq!(worker.join().expect("worker thread"), 0);
    }
}

/// Two independent mdkb processes can discover the same changed files at the
/// same time (for example the daemon watcher and a manual `mdkb update`). The
/// project mutation lock must serialize the complete logical updates, not just
/// their individual SQLite transactions, and the resulting index must pass the
/// same structural check used by production auto-heal.
#[test]
fn concurrent_full_updates_leave_one_sound_index() {
    let tmp = TempDir::new().expect("tempdir");
    handle_init(tmp.path()).expect("init mdkb");
    std::fs::create_dir_all(tmp.path().join("docs")).unwrap();
    for i in 0..40 {
        std::fs::write(
            tmp.path().join("docs").join(format!("doc-{i}.md")),
            format!("# Document {i}\n\nShared update fixture {i}.\n"),
        )
        .unwrap();
    }

    let setup = Context::open(tmp.path()).unwrap();
    handle_collection_add(&setup, "docs", "./docs", "**/*.md").unwrap();
    drop(setup);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let root = tmp.path().to_path_buf();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let ctx = Context::open(&root).expect("open worker context");
            barrier.wait();
            handle_update_force(&ctx, &root, true).expect("concurrent update")
        }));
    }
    for worker in workers {
        worker.join().expect("worker thread");
    }

    let final_ctx = Context::open(tmp.path()).expect("reopen final index");
    let check: String = final_ctx
        .conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(check, "ok");
    let docs: i64 = final_ctx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE collection = 'docs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(docs, 40, "both updates converge on one complete index");
}
