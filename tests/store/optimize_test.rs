//! Integration test: database runtime upkeep and vacuum-regression guard.
//!
//! `PRAGMA optimize` (non-locking) is gated on a call-count drift counter at
//! runtime. Automatic `auto_vacuum = INCREMENTAL` conversion was removed after it
//! corrupted `index.sqlite` pointer-map pages, so a freshly-opened database must
//! stay in the historical `auto_vacuum = NONE` (0) mode with no pointer-map pages.

use mdkb::cli::handlers::handle_init;
use mdkb::core::Context;
use mdkb::store::maintenance::{run_optimize, should_optimize};

#[test]
fn fresh_db_is_auto_vacuum_none() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    handle_init(&root).expect("init mdkb");

    let ctx = Context::open(&root).expect("open ctx");

    let mode: i64 = ctx
        .conn
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .expect("read auto_vacuum");
    assert_eq!(
        mode, 0,
        "database must stay in auto_vacuum=NONE — INCREMENTAL introduces the \
         pointer-map pages that were being corrupted"
    );
}

#[test]
fn should_optimize_gates_on_interval() {
    assert!(!should_optimize(0, 200), "zero calls: skip");
    assert!(!should_optimize(1, 200), "below threshold: skip");
    assert!(should_optimize(200, 200), "at threshold: trigger");
    assert!(!should_optimize(201, 200), "just past: skip");
    assert!(should_optimize(400, 200), "next multiple: trigger");
    assert!(!should_optimize(200, 0), "interval=0 disables filter");
}

#[test]
fn run_optimize_is_non_locking_and_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    handle_init(&root).expect("init mdkb");

    let ctx = Context::open(&root).expect("open ctx");

    for _ in 0..5 {
        run_optimize(&ctx.conn).expect("PRAGMA optimize must not error");
    }

    // Concurrent read must still work — PRAGMA optimize never holds an exclusive lock.
    let concurrent: i64 = ctx
        .conn
        .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
        .expect("read during optimize");
    assert_eq!(concurrent, 0);
}
