//! DATA-B1 (story 068): the production DB-open path (`Context::open`) must set
//! `busy_timeout` so that ordinary write-lock contention between the daemon and
//! one-shot CLI processes resolves by waiting briefly, not by an immediate
//! `SQLITE_BUSY` error.

use std::time::{Duration, Instant};

use mdkb::cli::handlers::{Context, handle_init};
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
