//! Integration tests for memory embedding on every write path, the
//! `--source-type` provenance flag, and the `mdkb update` backfill.
//!
//! Tests that assert an embedding vector was actually produced require the
//! ~100MB ONNX model and are `#[ignore]`d (run with `-- --ignored`). The
//! model-free tests below exercise the pending-count query, backfill row
//! selection, and source-type persistence deterministically without the model.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mdkb::cli::handlers::{
    handle_init, handle_memory_add, handle_memory_import, handle_memory_show, handle_update,
};
use mdkb::config::Config;
use mdkb::core::Context;
use mdkb::daemon::registry::RepoHandle;
use mdkb::mcp::dispatch::spawn_embedding_backfill;
use mdkb::store::memory::{self, EntryStatus, EntryType, MemoryEntry, SourceType};
use rusqlite::Connection;
use tempfile::TempDir;
use tokio::sync::Mutex as TokioMutex;

struct Env {
    _dir: TempDir,
    root: PathBuf,
    ctx: Context,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        handle_init(&root).expect("init");
        let ctx = Context::open(&root).expect("open");
        Self {
            _dir: dir,
            root,
            ctx,
        }
    }
}

/// A store-layer entry with a deterministic title/content and no embedding row.
fn raw_entry(id: &str) -> MemoryEntry {
    let now = chrono::Utc::now().timestamp();
    MemoryEntry {
        id: id.to_string(),
        title: format!("Title {id}"),
        content: format!("Content for {id}."),
        entry_type: EntryType::Topic,
        tags: vec!["test".to_string()],
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 0,
        last_accessed: None,
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    }
}

/// Insert an entry straight through the store layer, bypassing the embed-on-write
/// path so pending/backfill counts are deterministic without the model.
fn add_raw_conn(conn: &Connection, id: &str) {
    memory::add_entry(conn, &raw_entry(id)).expect("add_entry");
}

fn add_raw(env: &Env, id: &str) {
    add_raw_conn(&env.ctx.conn, id);
}

/// Manually mark an entry as embedded (an empty-vector row is enough for the
/// LEFT JOIN in `count_pending_embeddings`).
fn mark_embedded_conn(conn: &Connection, id: &str) {
    let rowid = memory::get_rowid(conn, id)
        .expect("get_rowid")
        .expect("entry exists");
    conn.execute(
        "INSERT INTO memory_embeddings (memory_rowid, embedding, model, created_at)
             VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![rowid, Vec::<u8>::new(), "AllMiniLML6V2", 0i64],
    )
    .expect("insert embedding row");
}

fn mark_embedded(env: &Env, id: &str) {
    mark_embedded_conn(&env.ctx.conn, id);
}

// ---------------------------------------------------------------------------
// source_type flag — model-free
// ---------------------------------------------------------------------------

#[test]
fn memory_add_default_source_type_is_user_statement() {
    let env = Env::new();
    handle_memory_add(
        &env.ctx,
        "st-default",
        "T",
        "topic",
        None,
        "body",
        None,
        None,
        None,
        None,
    )
    .expect("add");
    let e = handle_memory_show(&env.ctx, "st-default")
        .unwrap()
        .expect("exists");
    assert_eq!(e.source_type, SourceType::UserStatement);
}

#[test]
fn memory_add_persists_explicit_source_type() {
    let env = Env::new();
    handle_memory_add(
        &env.ctx,
        "st-docs",
        "T",
        "topic",
        None,
        "body",
        None,
        None,
        None,
        Some("official_docs"),
    )
    .expect("add");
    let e = handle_memory_show(&env.ctx, "st-docs")
        .unwrap()
        .expect("exists");
    assert_eq!(e.source_type, SourceType::OfficialDocs);
}

#[test]
fn memory_add_rejects_invalid_source_type() {
    let env = Env::new();
    let err = handle_memory_add(
        &env.ctx,
        "st-bad",
        "T",
        "topic",
        None,
        "body",
        None,
        None,
        None,
        Some("gospel"),
    )
    .expect_err("invalid source_type must be rejected");
    assert!(err.to_string().contains("source_type"), "{err}");
}

#[test]
fn rewrite_preserves_source_type_when_flag_absent() {
    let env = Env::new();
    handle_memory_add(
        &env.ctx,
        "st-keep",
        "T",
        "topic",
        None,
        "body",
        None,
        None,
        None,
        Some("official_docs"),
    )
    .expect("add");
    // Re-write WITHOUT --source-type: a defaulted re-write must not downgrade.
    handle_memory_add(
        &env.ctx, "st-keep", "T2", "topic", None, "body2", None, None, None, None,
    )
    .expect("rewrite");
    let e = handle_memory_show(&env.ctx, "st-keep")
        .unwrap()
        .expect("exists");
    assert_eq!(
        e.source_type,
        SourceType::OfficialDocs,
        "re-write without flag must preserve provenance"
    );
}

#[test]
fn rewrite_overrides_source_type_when_flag_given() {
    let env = Env::new();
    handle_memory_add(
        &env.ctx,
        "st-change",
        "T",
        "topic",
        None,
        "body",
        None,
        None,
        None,
        Some("official_docs"),
    )
    .expect("add");
    handle_memory_add(
        &env.ctx,
        "st-change",
        "T2",
        "topic",
        None,
        "body2",
        None,
        None,
        None,
        Some("inference"),
    )
    .expect("rewrite");
    let e = handle_memory_show(&env.ctx, "st-change")
        .unwrap()
        .expect("exists");
    assert_eq!(e.source_type, SourceType::Inference);
}

// ---------------------------------------------------------------------------
// pending count + backfill selection — model-free
// ---------------------------------------------------------------------------

#[test]
fn pending_count_tracks_unembedded_entries() {
    let env = Env::new();
    add_raw(&env, "a");
    add_raw(&env, "b");
    add_raw(&env, "c");
    assert_eq!(
        memory::count_pending_embeddings(&env.ctx.conn).unwrap(),
        3,
        "all three entries start pending"
    );

    mark_embedded(&env, "b");
    assert_eq!(
        memory::count_pending_embeddings(&env.ctx.conn).unwrap(),
        2,
        "marking one embedded decrements the pending count"
    );
}

#[test]
fn backfill_is_noop_when_nothing_pending() {
    let env = Env::new();
    add_raw(&env, "only");
    mark_embedded(&env, "only");
    // Everything already has a row → backfill touches the model for zero rows.
    assert_eq!(
        memory::backfill_memory_embeddings(&env.ctx.conn).unwrap(),
        0
    );
}

// ---------------------------------------------------------------------------
// Real embedding round-trips — require the ONNX model (`-- --ignored`)
// ---------------------------------------------------------------------------

fn embedding_row_count(env: &Env) -> i64 {
    env.ctx
        .conn
        .query_row("SELECT COUNT(*) FROM memory_embeddings", [], |r| r.get(0))
        .expect("count embeddings")
}

#[test]
#[ignore = "requires ONNX model download"]
fn memory_add_produces_embedding_row() {
    let env = Env::new();
    handle_memory_add(
        &env.ctx,
        "emb-add",
        "Title",
        "topic",
        None,
        "searchable body",
        None,
        None,
        None,
        None,
    )
    .expect("add");
    assert_eq!(embedding_row_count(&env), 1);
    assert_eq!(memory::count_pending_embeddings(&env.ctx.conn).unwrap(), 0);
}

#[test]
#[ignore = "requires ONNX model download"]
fn import_produces_embedding_rows() {
    let env = Env::new();
    let json = r#"{"entries": [
        {"id": "imp-1", "title": "One", "content": "first"},
        {"id": "imp-2", "title": "Two", "content": "second"}
    ]}"#;
    let path = env.root.join("import.json");
    std::fs::write(&path, json).unwrap();
    let result =
        handle_memory_import(&env.ctx, path.to_str().unwrap(), false, false).expect("import");
    assert_eq!(result.imported, 2);
    assert_eq!(embedding_row_count(&env), 2);
}

#[test]
#[ignore = "requires ONNX model download"]
fn update_backfills_all_pending_embeddings() {
    let env = Env::new();
    // Seed entries through the store layer (no embed) to simulate the live DB
    // full of CLI/bridge-written entries that never got vectorized.
    for id in ["bf-1", "bf-2", "bf-3"] {
        add_raw(&env, id);
    }
    assert_eq!(memory::count_pending_embeddings(&env.ctx.conn).unwrap(), 3);

    let result = handle_update(&env.ctx, &env.root).expect("update");
    assert_eq!(result.memory_embeddings_backfilled, 3);
    assert_eq!(
        memory::count_pending_embeddings(&env.ctx.conn).unwrap(),
        0,
        "after update every entry is embedded (count == entries)"
    );
    assert_eq!(embedding_row_count(&env), 3);
}

// ---------------------------------------------------------------------------
// Background drain — spawn_embedding_backfill (daemon/hook-level, model-free)
// ---------------------------------------------------------------------------

/// A handle sharing a freshly opened context for an already-`handle_init`ed root.
fn make_handle(root: &Path) -> Arc<RepoHandle> {
    let ctx = Context::open(root).expect("open ctx");
    Arc::new(RepoHandle::from_shared(
        root.to_path_buf(),
        Arc::new(TokioMutex::new(Some(ctx))),
        Arc::new(TokioMutex::new(None)),
        Config::default(),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ))
}

/// Init a repo and return (`TempDir`, root). The `TempDir` must be kept alive.
fn init_repo() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    handle_init(&root).expect("init");
    (dir, root)
}

#[tokio::test]
async fn backfill_single_flight_second_call_is_noop() {
    let (_dir, root) = init_repo();
    let handle = make_handle(&root);

    // Two calls before yielding to the runtime: the first wins the guard and
    // spawns; the second sees the guard held and returns None. Deterministic
    // under the current-thread runtime `#[tokio::test]` provides (spawned tasks
    // don't run until this task awaits).
    let first = spawn_embedding_backfill(Arc::clone(&handle));
    let second = spawn_embedding_backfill(Arc::clone(&handle));
    assert!(first.is_some(), "first call wins the single-flight guard");
    assert!(
        second.is_none(),
        "a second call while a drain is in flight must be a no-op"
    );

    let drained = first.unwrap().await.unwrap();
    assert_eq!(drained, 0, "empty repo drains nothing");
    assert!(
        !handle.backfill_in_flight.load(Ordering::Acquire),
        "guard resets once the drain completes"
    );

    // Guard released → a later drain can run again.
    let third = spawn_embedding_backfill(Arc::clone(&handle));
    assert!(third.is_some(), "released guard lets a later drain run");
    third.unwrap().await.unwrap();
}

#[tokio::test]
async fn backfill_is_noop_when_nothing_pending_via_handle() {
    let (_dir, root) = init_repo();
    // Seed entries that are ALL already embedded → pending == 0, so the drain
    // short-circuits on the COUNT gate and never consults the (cold) model.
    {
        let ctx = Context::open(&root).unwrap();
        for id in ["p1", "p2"] {
            add_raw_conn(&ctx.conn, id);
            mark_embedded_conn(&ctx.conn, id);
        }
    }
    let handle = make_handle(&root);
    let drained = spawn_embedding_backfill(Arc::clone(&handle))
        .unwrap()
        .await
        .unwrap();
    assert_eq!(drained, 0, "nothing pending → nothing embedded");
}

#[tokio::test]
#[ignore = "requires ONNX model download"]
async fn backfill_drains_pending_when_model_available() {
    let (_dir, root) = init_repo();
    {
        let ctx = Context::open(&root).unwrap();
        for id in ["d1", "d2", "d3"] {
            add_raw_conn(&ctx.conn, id);
        }
        assert_eq!(memory::count_pending_embeddings(&ctx.conn).unwrap(), 3);
    }
    let handle = make_handle(&root);
    let drained = spawn_embedding_backfill(Arc::clone(&handle))
        .unwrap()
        .await
        .unwrap();
    assert_eq!(drained, 3, "all pending entries embedded");

    let guard = handle.ctx.lock().await;
    let conn = &guard.as_ref().unwrap().conn;
    assert_eq!(
        memory::count_pending_embeddings(conn).unwrap(),
        0,
        "no entries remain pending after the drain"
    );
}

#[tokio::test]
#[ignore = "requires ONNX model download"]
async fn session_start_hook_clears_pending_embeddings_end_to_end() {
    use std::time::Duration;

    let (_dir, root) = init_repo();
    {
        let ctx = Context::open(&root).unwrap();
        for id in ["e1", "e2"] {
            add_raw_conn(&ctx.conn, id);
        }
        assert_eq!(memory::count_pending_embeddings(&ctx.conn).unwrap(), 2);
    }
    let handle = make_handle(&root);

    // Fire the real session-start hook. The drain is detached; the single-flight
    // guard is our completion signal (it resets when the drain task finishes).
    let _ = mdkb::mcp::dispatch::hook_session_start_impl(&handle, None).await;
    for _ in 0..200 {
        if !handle.backfill_in_flight.load(Ordering::Acquire) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        !handle.backfill_in_flight.load(Ordering::Acquire),
        "drain did not finish within the timeout"
    );

    let guard = handle.ctx.lock().await;
    let conn = &guard.as_ref().unwrap().conn;
    assert_eq!(
        memory::count_pending_embeddings(conn).unwrap(),
        0,
        "the session-start hook cleared the pending-embedding backlog with no manual update"
    );
}
