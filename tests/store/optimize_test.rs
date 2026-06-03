//! Integration test: incremental auto_vacuum conversion + runtime upkeep.
//!
//! The one-shot conversion VACUUMs (exclusive lock) so it runs only at first
//! open, gated by a `PRAGMA user_version` marker. `PRAGMA optimize` (non-locking)
//! and `incremental_vacuum` (cheap reclaim) are gated on a call-count drift
//! counter at runtime.

use mdkb::cli::handlers::{Context, handle_init};
use mdkb::store::maintenance::{
    AUTOVAC_DONE_FLAG, ensure_incremental_autovacuum, run_optimize, should_optimize,
};

#[test]
fn incremental_autovacuum_runs_once_and_sets_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    handle_init(&root).expect("init mdkb");

    let ctx = Context::open(&root).expect("open ctx");

    let version: i64 = ctx
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("read user_version");
    assert!(
        version & AUTOVAC_DONE_FLAG != 0,
        "conversion marker must be set after first open, got user_version={version}"
    );

    let mode: i64 = ctx
        .conn
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .expect("read auto_vacuum");
    assert_eq!(mode, 2, "database must be in INCREMENTAL auto_vacuum mode");

    let ran = ensure_incremental_autovacuum(&ctx.conn).expect("second call");
    assert!(!ran, "second call must be a no-op (marker already set)");
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
