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

use mdkb::code::indexing::IndexFacade;
use mdkb::core::Context;
use mdkb::domain::Collection;
use mdkb::mcp::server::{
    CODE_REINDEX_COUNT, DOC_REINDEX_COUNT, WATCHER_SPAWN_COUNT, run_file_watcher_inner,
};
use mdkb::store::collections::add_collection;
use mdkb::store::memory::{self, EntryStatus};

/// `WATCHER_SPAWN_COUNT`, `DOC_REINDEX_COUNT` and `CODE_REINDEX_COUNT` are
/// process-global, so two watchers alive at once in this binary make every
/// "exactly one" assertion meaningless — and, worse, intermittently true. Every
/// test here spawns a watcher, so they take this guard and run one at a time.
///
/// Assertions about *this* repo's state should still prefer polling the repo's
/// own tables over any of those counters: the guard removes the race, it does
/// not make a global counter a statement about a particular store.
static WATCHER_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let _serial = WATCHER_TESTS.lock().await;
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
        assert!(
            tokio::time::Instant::now() < deadline,
            "DOC_REINDEX_COUNT did not increment within 10s (before={doc_before}, now={current})"
        );
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
    let _serial = WATCHER_TESTS.lock().await;
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
        assert!(
            tokio::time::Instant::now() < deadline,
            "CODE_REINDEX_COUNT did not increment within 5s (before={code_before}, now={current})"
        );
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
    let _serial = WATCHER_TESTS.lock().await;
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
            50, // debounce_ms — fast for test
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

    // The buffered path must now be reindexed. Poll the symbol itself rather
    // than the process-global flush counter: these tests run concurrently, so a
    // different watcher may increment that counter first and create a false
    // positive wake-up.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let indexed = code_index
            .lock()
            .await
            .as_ref()
            .is_some_and(|facade| facade.get_symbol_by_name("late_ctx_fn").is_some());
        if indexed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "late-ctx injected path was never reindexed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Prove the actual symbol remains queryable after the completed flush.
    let guard = code_index.lock().await;
    let facade = guard.as_ref().unwrap();
    assert!(
        facade.get_symbol_by_name("late_ctx_fn").is_some(),
        "the file edited before ctx was ready must be indexed once ctx arrives"
    );

    watcher_handle.abort();
}

/// Spawn a watcher over `root` with a fast flush, and wait until it is ready.
///
/// `code_enabled` is threaded through because it is exactly the parameter that
/// used to gate the root registration — the tests below need to vary it.
async fn spawn_ready_watcher(
    root: &std::path::Path,
    ctx: &Arc<Mutex<Option<Context>>>,
    code_enabled: bool,
) -> tokio::task::JoinHandle<()> {
    let watcher_root = root.to_path_buf();
    let watcher_ctx = Arc::clone(ctx);
    let watcher_code: Arc<Mutex<Option<IndexFacade>>> = Arc::new(Mutex::new(None));
    let ready = Arc::new(tokio::sync::Notify::new());
    let ready_clone = Arc::clone(&ready);
    let handle = tokio::spawn(async move {
        let _ = run_file_watcher_inner(
            watcher_root,
            watcher_ctx,
            watcher_code,
            code_enabled,
            vec![],
            true, // respect_gitignore
            50,   // debounce_ms — fast for test
            200,  // fast flush for test
            Some(ready_clone),
            None,
        )
        .await;
    });
    tokio::time::timeout(Duration::from_secs(10), ready.notified())
        .await
        .expect("watcher did not become ready within 10s");
    handle
}

/// Poll `check` against the shared context until it holds, re-applying `nudge`
/// each round, or fail with `what`.
///
/// `nudge` re-performs the filesystem change under test and MUST be idempotent.
/// It exists because the watcher's `ready` notify fires as soon as
/// `watcher.watch()` returns, while FSEvents arms its stream asynchronously
/// afterwards — so a single write issued immediately after `ready` can be
/// dropped on the floor. That window is invisible to a daemon, which is running
/// long before anyone edits a file, but a test writes microseconds later. It was
/// masked in the `code_enabled = true` tests only because the code-index
/// bootstrap runs before `ready` and happens to cover the gap.
///
/// Re-issuing the same change is therefore the honest fix: it makes the test
/// wait for the watcher to be genuinely live instead of assuming `ready` means
/// armed, without weakening what is being asserted.
async fn wait_for(
    ctx: &Arc<Mutex<Option<Context>>>,
    what: &str,
    nudge: impl Fn(),
    check: impl Fn(&Context) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        {
            let guard = ctx.lock().await;
            if guard.as_ref().is_some_and(&check) {
                return;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        nudge();
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A `.md` file landing in the entry projection — what a `git pull` produces —
/// must reach the database without anyone running a command.
///
/// This is the whole point of the third watcher route. Before it, a colleague's
/// entry sat on disk indefinitely: absent from `memory_entries`, from every
/// search, and from warmup, with nothing reporting it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pulled_memory_entry_is_reconciled_by_the_watcher() {
    let _serial = WATCHER_TESTS.lock().await;
    let tmp = canonical_tempdir();
    let root = tmp.path().to_path_buf();

    let ctx = Context::init(&root).expect("Context::init");
    let entries_dir = ctx.memory_dir().join("entries");
    std::fs::create_dir_all(&entries_dir).unwrap();
    let ctx_arc: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(Some(ctx)));

    let watcher = spawn_ready_watcher(&root, &ctx_arc, true).await;

    // Land the file the way a checkout would: bytes only, no DB row anywhere.
    // Rendered through the projection serializer so the frontmatter is exactly
    // what a real clone would carry, not a hand-rolled approximation.
    let now = chrono::Utc::now().timestamp();
    let pulled = mdkb::store::memory::MemoryEntry {
        id: "pulled-from-a-colleague".to_string(),
        title: "Arrived in a checkout".to_string(),
        content: "Written on another machine and pulled into this one.".to_string(),
        entry_type: mdkb::store::memory::EntryType::Decision,
        tags: vec!["sync".to_string()],
        status: EntryStatus::Active,
        created_at: now - 3600,
        updated_at: now - 3600,
        superseded_by: None,
        access_count: 0,
        last_accessed: None,
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: mdkb::store::memory::SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };
    let pulled_path = entries_dir.join("pulled-from-a-colleague.md");
    let pulled_bytes = mdkb::store::memory_file::to_markdown(&pulled);
    let land_file = || std::fs::write(&pulled_path, &pulled_bytes).unwrap();
    land_file();

    wait_for(
        &ctx_arc,
        "the pulled entry to be imported",
        land_file,
        |ctx| {
            memory::get_entry_without_tracking(&ctx.conn, "pulled-from-a-colleague")
                .ok()
                .flatten()
                .is_some()
        },
    )
    .await;

    let guard = ctx_arc.lock().await;
    let imported = memory::get_entry_without_tracking(
        &guard.as_ref().unwrap().conn,
        "pulled-from-a-colleague",
    )
    .unwrap()
    .expect("imported entry");
    assert_eq!(imported.title, pulled.title);
    assert_eq!(imported.content, pulled.content);
    assert_eq!(
        imported.created_at, pulled.created_at,
        "the authoring timestamp must survive the import — stamping it with now \
         would flatten a colleague's history into the moment we pulled it"
    );
    drop(guard);

    watcher.abort();
}

/// The bulk-loss circuit breaker must survive the per-file watcher route.
///
/// This is the constraint most at risk in the whole design. The watcher
/// delivers one event per file, but archival is a SET-level decision: twelve
/// deletions treated as twelve independent choices are each below the cap of
/// ten, so a per-file route would let a broken checkout retire the corpus one
/// entry at a time while the breaker never fires. The event must therefore be
/// only a trigger, with the flush re-reading the whole directory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_deletions_still_go_through_the_set_level_breaker() {
    let _serial = WATCHER_TESTS.lock().await;
    let tmp = canonical_tempdir();
    let root = tmp.path().to_path_buf();

    let ctx = Context::init(&root).expect("Context::init");
    let entries_dir = ctx.memory_dir().join("entries");

    // One more than the cap, so the set is over the threshold but no single
    // deletion is. Derived from the constant: raising the cap must not silently
    // turn this test into a no-op.
    let doomed = mdkb::cli::handlers::MEMORY_SYNC_BULK_ARCHIVE_CAP + 2;
    for i in 0..doomed {
        mdkb::cli::handlers::handle_memory_add(
            &ctx,
            &format!("bulk-{i:02}"),
            &format!("Entry {i}"),
            "topic",
            None,
            "Body long enough to be a real entry.",
            None,
            None,
            None,
            None,
        )
        .expect("add");
    }
    // Every entry must be projected before deletion counts as a deletion:
    // an entry that was never projected is backfilled, never archived.
    mdkb::cli::handlers::sync_memory_files(&ctx).expect("initial projection");
    for i in 0..doomed {
        assert!(
            entries_dir.join(format!("bulk-{i:02}.md")).exists(),
            "entry {i} must be projected before the test deletes it"
        );
    }

    let ctx_arc: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(Some(ctx)));
    let watcher = spawn_ready_watcher(&root, &ctx_arc, true).await;

    // The bulk loss: every projected file disappears at once, as a `git clean`
    // or a failed restore would do. Not a git repo, so history can prove
    // nothing — the whole set is suspect and the cap applies.
    for i in 0..doomed {
        std::fs::remove_file(entries_dir.join(format!("bulk-{i:02}.md"))).unwrap();
    }

    // A reconciliation pass must be OBSERVED to have completed, or "nothing was
    // archived" would also be satisfied by the watcher never firing at all.
    // This file arrives in the same directory sweep, so its import proves the
    // pass ran over the very set the deletions belong to.
    let now = chrono::Utc::now().timestamp();
    let canary = mdkb::store::memory::MemoryEntry {
        id: "sweep-canary".to_string(),
        title: "Proof the sweep ran".to_string(),
        content: "Imported in the same pass that saw the deletions.".to_string(),
        entry_type: mdkb::store::memory::EntryType::Topic,
        tags: vec![],
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 0,
        last_accessed: None,
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: mdkb::store::memory::SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };
    let canary_path = entries_dir.join("sweep-canary.md");
    let canary_bytes = mdkb::store::memory_file::to_markdown(&canary);
    let land_canary = || std::fs::write(&canary_path, &canary_bytes).unwrap();
    land_canary();

    wait_for(
        &ctx_arc,
        "the reconciliation sweep to complete",
        land_canary,
        |ctx| {
            memory::get_entry_without_tracking(&ctx.conn, "sweep-canary")
                .ok()
                .flatten()
                .is_some()
        },
    )
    .await;

    let guard = ctx_arc.lock().await;
    let conn = &guard.as_ref().unwrap().conn;
    for i in 0..doomed {
        let id = format!("bulk-{i:02}");
        let entry = memory::get_entry_without_tracking(conn, &id)
            .unwrap()
            .unwrap_or_else(|| panic!("{id} must still exist"));
        assert_eq!(
            entry.status,
            EntryStatus::Active,
            "{id} was archived: a bulk disappearance reached the DB one file at \
             a time, which is exactly what the set-level breaker exists to stop"
        );
    }
    drop(guard);

    watcher.abort();
}

/// Turning code indexing off must not blind the watcher to everything else.
///
/// `watcher.watch(&root)` is the only call that registers the repo root, and it
/// sat behind `if code_enabled`. With `[code] enabled = false` the daemon
/// therefore watched nothing at all — no code, no documents, no memory entries —
/// while every log line still reported a running watcher. Registering the root
/// is what makes routing possible and must not depend on any single sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn documents_are_watched_with_code_indexing_disabled() {
    let _serial = WATCHER_TESTS.lock().await;
    let tmp = canonical_tempdir();
    let root = tmp.path().to_path_buf();

    let ctx = Context::init(&root).expect("Context::init");
    let docs_dir = root.join("docs");
    std::fs::create_dir_all(&docs_dir).unwrap();
    add_collection(&ctx.conn, &make_collection("docs", "docs")).unwrap();
    std::fs::write(docs_dir.join("hello.md"), "# Hello\nOriginal content").unwrap();
    mdkb::cli::handlers::handle_update(&ctx, &root).expect("initial index");

    let original_hash = mdkb::store::documents::get_document_by_path(&ctx.conn, "docs", "hello.md")
        .unwrap()
        .expect("hello.md must be indexed")
        .hash;

    let ctx_arc: Arc<Mutex<Option<Context>>> = Arc::new(Mutex::new(Some(ctx)));

    // The whole point: code_enabled = false.
    let watcher = spawn_ready_watcher(&root, &ctx_arc, false).await;

    let doc_path = docs_dir.join("hello.md");
    let modify_doc = || std::fs::write(&doc_path, "# Hello\nModified content").unwrap();
    modify_doc();

    // Poll this repo's own row rather than the process-global reindex counter:
    // the counter cannot say WHICH store was reindexed.
    wait_for(
        &ctx_arc,
        "the document to be reindexed with code indexing disabled",
        modify_doc,
        |ctx| {
            mdkb::store::documents::get_document_by_path(&ctx.conn, "docs", "hello.md")
                .ok()
                .flatten()
                .is_some_and(|d| d.hash != original_hash)
        },
    )
    .await;

    watcher.abort();
}
