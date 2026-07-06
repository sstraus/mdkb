//! Integration test for story 017-91cb: single watcher → single reindex
//! regardless of how many clients share the same `RepoHandle`.
//!
//! Proves the daemon contract: multiple clients connected via the proxy all
//! resolve to the same `RepoHandle` (with one watcher). A file change in a
//! collection triggers exactly one doc reindex — not N reindexes for N clients.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::Mutex;

use mdkb::cli::handlers::Context;
use mdkb::code::indexing::IndexFacade;
use mdkb::domain::Collection;
use mdkb::mcp::server::{
    CODE_REINDEX_COUNT, DOC_REINDEX_COUNT, WATCHER_SPAWN_COUNT, run_file_watcher_inner,
};
use mdkb::store::collections::add_collection;

/// On macOS /tmp → /private/tmp; FSEvents reports canonical paths.
fn canonical_tempdir() -> TempDir {
    let base = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    tempfile::tempdir_in(base).expect("tempdir")
}

fn make_collection(name: &str, path: &str) -> Collection {
    let now = chrono::Utc::now().timestamp();
    Collection {
        name: name.to_string(),
        path: path.to_string(),
        pattern: "**/*.md".to_string(),
        source: "manual".to_string(),
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_single_reindex() {
    let tmp = canonical_tempdir();
    let root = tmp.path().to_path_buf();

    // 1. Initialize mdkb repo with a "docs" collection.
    let ctx = Context::init(&root).expect("Context::init");
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&docs_dir).unwrap();
    add_collection(&ctx.conn, &make_collection("docs", "docs")).unwrap();
    std::fs::write(docs_dir.join("hello.md"), "# Hello\nOriginal content").unwrap();

    // Run initial update so hello.md is indexed.
    mdkb::cli::handlers::handle_update(&ctx, &root).unwrap();

    // 2. Wrap context in the same Arc<Mutex<Option>> shape the daemon uses.
    let ctx_arc: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(Some(ctx)));
    let code_index: Arc<Mutex<Option<mdkb::code::indexing::IndexFacade>>> =
        Arc::new(Mutex::new(None));

    // 3. Simulate two clients holding clones of the same shared state.
    let _client1_ctx = Arc::clone(&ctx_arc);
    let _client2_ctx = Arc::clone(&ctx_arc);

    // 4. Record baseline counters.
    let watcher_before = WATCHER_SPAWN_COUNT.load(Ordering::Relaxed);
    let doc_before = DOC_REINDEX_COUNT.load(Ordering::Relaxed);

    // 5. Spawn the watcher with a fast batch idle (500ms instead of 30s).
    let watcher_root = root.clone();
    let watcher_ctx = Arc::clone(&ctx_arc);
    let watcher_code = Arc::clone(&code_index);
    let ready = Arc::new(tokio::sync::Notify::new());
    let ready_clone = Arc::clone(&ready);
    let watcher_handle = tokio::spawn(async move {
        let _ = run_file_watcher_inner(
            watcher_root,
            watcher_ctx,
            watcher_code,
            true, // needed to watch root recursively
            vec![],
            true, // respect_gitignore
            50,   // debounce_ms — fast for test
            500,  // 500ms batch idle for fast test flush
            Some(ready_clone),
            None,
        )
        .await;
    });

    // Wait for the watcher to finish registering watches.
    tokio::time::timeout(Duration::from_secs(10), ready.notified())
        .await
        .expect("watcher did not become ready within 10s");

    // 6. Exactly one watcher should have spawned.
    assert_eq!(
        WATCHER_SPAWN_COUNT.load(Ordering::Relaxed) - watcher_before,
        1,
        "exactly one watcher must spawn"
    );
    assert!(
        !watcher_handle.is_finished(),
        "watcher should still be running"
    );

    // 7. Modify a doc file to trigger the watcher.
    std::fs::write(docs_dir.join("hello.md"), "# Hello\nModified content").unwrap();

    // 8. Wait for the doc reindex flush (500ms idle + debounce + margin).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let current = DOC_REINDEX_COUNT.load(Ordering::Relaxed);
        if current > doc_before {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "DOC_REINDEX_COUNT did not increment within 10s (before={doc_before}, now={current})"
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 9. Assert exactly ONE reindex happened — not two (one per client).
    assert_eq!(
        DOC_REINDEX_COUNT.load(Ordering::Relaxed) - doc_before,
        1,
        "a single file change must produce exactly one doc reindex, not one per client"
    );

    watcher_handle.abort();
}

/// Story 050-cc15: injecting a path via the reindex channel triggers a code reindex.
///
/// This is the daemon-side half of the post-tool-use IPC hook: instead of writing
/// a path to reindex-queue.jsonl, the hook will send it directly into the watcher
/// via `RepoHandle::reindex_tx`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_path_triggers_code_reindex() {
    let tmp = canonical_tempdir();
    let root = tmp.path().to_path_buf();

    // Initialize mdkb repo (needed for the watcher's CTX_WAIT_SECS poll).
    let ctx = Context::init(&root).expect("Context::init");
    let ctx_arc: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(Some(ctx)));

    // Create a real IndexFacade so flush_code_batch can actually reindex.
    let db_path = root.join(".mdkb").join("code.sqlite");
    let facade = IndexFacade::create(db_path).expect("IndexFacade::create");
    let code_index: Arc<Mutex<Option<IndexFacade>>> = Arc::new(Mutex::new(Some(facade)));

    // Create the inject channel — this is what RepoHandle::reindex_tx wraps.
    let (tx, rx) = tokio::sync::mpsc::channel::<PathBuf>(64);

    let code_before = CODE_REINDEX_COUNT.load(Ordering::Relaxed);

    // Spawn watcher with the receiver.
    let watcher_root = root.clone();
    let watcher_ctx = Arc::clone(&ctx_arc);
    let watcher_code = Arc::clone(&code_index);
    let ready = Arc::new(tokio::sync::Notify::new());
    let ready_clone = Arc::clone(&ready);
    let watcher_handle = tokio::spawn(async move {
        let _ = run_file_watcher_inner(
            watcher_root,
            watcher_ctx,
            watcher_code,
            true,
            vec![],
            true, // respect_gitignore
            50,   // debounce_ms — fast for test
            200,  // fast flush for test
            Some(ready_clone),
            Some(rx),
        )
        .await;
    });

    tokio::time::timeout(Duration::from_secs(10), ready.notified())
        .await
        .expect("watcher did not become ready within 10s");

    // Inject a path — simulates what the post-tool-use IPC hook will do.
    let injected = root.join("src").join("lib.rs");
    tx.send(injected).await.expect("send injected path");

    // Wait for CODE_REINDEX_COUNT to increment (200ms idle + margin).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let current = CODE_REINDEX_COUNT.load(Ordering::Relaxed);
        if current > code_before {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "CODE_REINDEX_COUNT did not increment within 5s (before={code_before}, now={current})"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        CODE_REINDEX_COUNT.load(Ordering::Relaxed) - code_before,
        1,
        "exactly one code reindex must fire after a single injected path"
    );

    watcher_handle.abort();
}

/// Story 034-62a0: a watcher whose context initializes LATE must not exit and
/// orphan `reindex_rx`. A path injected while ctx is still `None` has to survive
/// the wait and get incrementally reindexed once ctx appears — otherwise every
/// post_tool_use send fails with "channel closed" forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_ctx_still_reindexes_injected_file() {
    let tmp = canonical_tempdir();
    let root = tmp.path().to_path_buf();

    // Set up .mdkb + a real source file, but DON'T hand ctx to the watcher yet.
    let ctx = Context::init(&root).expect("Context::init");
    let src_dir = root.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(src_dir.join("lib.rs"), "pub fn late_ctx_fn() {}").unwrap();

    // ctx starts as None — the daemon lazily fills it on the first client request.
    let ctx_arc: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(None));

    let db_path = root.join(".mdkb").join("code.sqlite");
    let facade = IndexFacade::create(db_path).expect("IndexFacade::create");
    let code_index: Arc<Mutex<Option<IndexFacade>>> = Arc::new(Mutex::new(Some(facade)));

    let (tx, rx) = tokio::sync::mpsc::channel::<PathBuf>(64);
    let code_before = CODE_REINDEX_COUNT.load(Ordering::Relaxed);

    // Spawn the watcher with ctx == None. No `ready` notify: it only fires after
    // the ctx-wait completes, which is exactly what we're delaying.
    let watcher_root = root.clone();
    let watcher_ctx = Arc::clone(&ctx_arc);
    let watcher_code = Arc::clone(&code_index);
    let watcher_handle = tokio::spawn(async move {
        let _ = run_file_watcher_inner(
            watcher_root,
            watcher_ctx,
            watcher_code,
            true,
            vec![],
            true,
            50,  // debounce_ms — fast for test
            200,
            None,
            Some(rx),
        )
        .await;
    });

    // Inject a path while ctx is still None. Old behavior: watcher eventually
    // exits and this send starts failing. New behavior: it buffers, then drains.
    let injected = src_dir.join("lib.rs");
    tx.send(injected).await.expect("send injected path");

    // Give the watcher a moment stuck in the ctx-wait, proving it hasn't exited.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !watcher_handle.is_finished(),
        "watcher must keep waiting for late ctx, not exit and drop reindex_rx"
    );

    // Context becomes available late (as the first client request would do).
    *ctx_arc.lock().await = Some(ctx);

    // The buffered path must now be reindexed.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if CODE_REINDEX_COUNT.load(Ordering::Relaxed) > code_before {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("late-ctx injected path was never reindexed");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Prove the actual symbol landed in the index, not just a counter bump.
    let guard = code_index.lock().await;
    let facade = guard.as_ref().unwrap();
    assert!(
        facade.get_symbol_by_name("late_ctx_fn").is_some(),
        "the file edited before ctx was ready must be indexed once ctx arrives"
    );

    watcher_handle.abort();
}
