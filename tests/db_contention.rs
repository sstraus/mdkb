//! DATA-B1 (story 068): the production DB-open path (`Context::open`) must set
//! `busy_timeout` so that ordinary write-lock contention between the daemon and
//! one-shot CLI processes resolves by waiting briefly, not by an immediate
//! `SQLITE_BUSY` error.

use std::time::{Duration, Instant};

use mdkb::cli::handlers::{Context, handle_collection_add, handle_init, handle_update_force};
use mdkb::store::vectors;
use rusqlite::params;
use tempfile::TempDir;

/// Auto-init creates FTS/sqlite-vec virtual tables just like `Context::open`.
/// It must join the project mutation domain rather than racing another hook
/// that is opening, healing, or initializing the same store.
#[test]
fn context_init_waits_for_the_project_mutation_lock() {
    let tmp = TempDir::new().expect("tempdir");
    let mdkb_dir = tmp.path().join(".mdkb");
    std::fs::create_dir_all(&mdkb_dir).expect("create .mdkb");
    let db = mdkb_dir.join("index.sqlite");
    let blocker =
        mdkb::store::mutation_lock::acquire(&db, "test-blocker").expect("take mutation lock");

    let (tx, rx) = std::sync::mpsc::channel();
    let root = tmp.path().to_path_buf();
    let worker = std::thread::spawn(move || {
        tx.send(Context::init(root)).expect("send init result");
    });

    assert!(
        rx.recv_timeout(Duration::from_millis(200)).is_err(),
        "Context::init must wait rather than construct schemas outside the mutation lock"
    );
    drop(blocker);
    rx.recv_timeout(Duration::from_secs(5))
        .expect("init result after lock release")
        .expect("init succeeds");
    worker.join().expect("init worker");
}

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

/// SQLite 3.51.3 fixes the WAL-reset corruption race that affects concurrent
/// writers/checkpointers in every release from 3.7.0 through 3.51.2.
#[test]
fn bundled_sqlite_contains_wal_reset_corruption_fix() {
    const SQLITE_3_51_3: i32 = 3_051_003;
    assert!(
        rusqlite::version_number() >= SQLITE_3_51_3,
        "bundled SQLite {} is vulnerable to the WAL-reset corruption race",
        rusqlite::version()
    );
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

/// Exercise the write pattern used by memory indexing: an ordinary table with
/// FTS triggers plus a sqlite-vec virtual table, from several independent
/// connections. This combination is the only native virtual-table write path
/// shared by the repositories where recurring corruption was observed.
#[test]
fn concurrent_memory_and_vector_writes_leave_one_sound_index() {
    const WORKERS: usize = 4;
    const WRITES_PER_WORKER: usize = 100;

    let tmp = TempDir::new().expect("tempdir");
    handle_init(tmp.path()).expect("init mdkb");

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(WORKERS));
    let mut workers = Vec::new();
    for worker_id in 0..WORKERS {
        let root = tmp.path().to_path_buf();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            let ctx = Context::open(root).expect("open worker context");
            barrier.wait();
            let embedding = vec![worker_id as f32; vectors::EMBEDDING_DIM];

            for n in 0..WRITES_PER_WORKER {
                let id = format!("stress-{worker_id}-{n}");
                ctx.conn
                    .execute(
                        "INSERT INTO memory_entries
                         (id, title, content, entry_type, tags, status, created_at, updated_at)
                         VALUES (?1, ?2, ?3, 'topic', '[]', 'active', ?4, ?4)",
                        params![id, format!("Title {n}"), format!("Body {n}"), n as i64],
                    )
                    .expect("insert memory entry");
                let rowid = ctx.conn.last_insert_rowid();
                vectors::store_memory_embedding(&ctx.conn, rowid, &embedding, "stress")
                    .expect("store memory embedding");

                if n % 3 == 0 {
                    ctx.conn
                        .execute(
                            "UPDATE memory_entries SET content = content || ' updated' WHERE rowid = ?1",
                            [rowid],
                        )
                        .expect("update FTS-backed memory entry");
                }
                if n % 5 == 0 {
                    ctx.conn
                        .execute("DELETE FROM memory_entries WHERE rowid = ?1", [rowid])
                        .expect("delete memory entry and vector through triggers");
                }
            }
        }));
    }

    for worker in workers {
        worker.join().expect("worker thread");
    }

    let final_ctx = Context::open(tmp.path()).expect("reopen final index");
    let check: String = final_ctx
        .conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .expect("quick_check");
    assert_eq!(check, "ok");
}
