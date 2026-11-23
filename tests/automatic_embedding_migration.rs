//! Writable encounters regenerate model-incompatible stores; discovery stays read-only.
//!
//! Unix only: the recording worker is a `/bin/sh` script, and the lowered-child
//! check reads the priority with `getpriority`.
#![cfg(unix)]
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use mdkb::{
    config::Config, core::Context, daemon::registry::RepoHandle, llm::embeddings::Model,
    mcp::dispatch::spawn_embedding_backfill, store::vectors,
};
use tokio::sync::Mutex;

/// Route regeneration children to the built `mdkb` through a wrapper that
/// records each run in `<repo>/.mdkb/worker.log`: the test harness is not `mdkb`.
fn use_recording_worker() {
    static WORKER: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    WORKER.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("worker.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nlog=.mdkb/worker.log\necho \"ppid=$PPID no_daemon=$MDKB_NO_DAEMON ort=$MDKB_ORT_THREADS args=$*\" >> \"$log\"\n\"{}\" \"$@\" 2>> \"$log\"\n",
                env!("CARGO_BIN_EXE_mdkb")
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: set once, before any regeneration child reads it, to the same value.
        unsafe { std::env::set_var("MDKB_EMBED_WORKER", &script) };
        dir
    });
}

fn fixture() -> (tempfile::TempDir, Arc<RepoHandle>) {
    use_recording_worker();
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::init(dir.path()).unwrap();
    ctx.conn.execute_batch("INSERT INTO collections(name,path,pattern,created_at,updated_at) VALUES('notes','.','*.md',0,0);
        INSERT INTO content(hash,body,created_at) VALUES('hash','sqlite recovery instructions',0);
        INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(1,'notes','recovery.md','hash',0,0);
        INSERT INTO memory_entries(id,title,content,entry_type,tags,status,created_at,updated_at) VALUES('memory','SQLite recovery','sqlite recovery instructions','topic','[]','active',0,0);").unwrap();
    vectors::store_embedding(&ctx.conn, 1, &[0.25; 384], Model::MiniLm.identity()).unwrap();
    vectors::store_memory_embedding(&ctx.conn, 1, &[0.5; 384], Model::MiniLm.identity()).unwrap();
    let handle = Arc::new(RepoHandle::from_shared(
        ctx.root().to_path_buf(),
        Arc::new(Mutex::new(Some(ctx))),
        Arc::new(Mutex::new(None)),
        Config::default(),
        vec![],
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ));
    (dir, handle)
}

fn persisted_vector(conn: &rusqlite::Connection) -> Vec<u8> {
    conn.query_row(
        "SELECT embedding FROM embeddings WHERE document_id=1",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

fn select(handle: &RepoHandle, model: &str) {
    std::fs::write(
        handle.root.join(".mdkb/config.toml"),
        format!("[embedding]\nmodel='{model}'\n"),
    )
    .unwrap();
}

async fn encounter(handle: &Arc<RepoHandle>) {
    let dctx = mdkb::mcp::dispatch::DispatchContext {
        metrics: Arc::new(mdkb::metrics::UsageMetrics::new()),
        session_id: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        persistent_call_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        optimize_interval_calls: 200,
        hook_dedup: Arc::new(std::sync::Mutex::new(Default::default())),
        background: None,
    };
    mdkb::mcp::dispatch::dispatch_call("get", serde_json::json!({"id":"1"}), handle.clone(), &dctx)
        .await
        .unwrap();
}

async fn settled(handle: &RepoHandle) {
    // A watchdog, not a latency assertion. The observable oracle is persisted
    // data after the automatic encounter's flight releases its guard.
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        while handle.backfill_in_flight.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("background generation did not settle");
}

async fn lexical(handle: &RepoHandle) {
    let slot = handle.ctx.lock().await;
    let ctx = slot.as_ref().unwrap();
    let rows = mdkb::core::search::hybrid_search_fts_labelled(ctx, "sqlite", None, 5, None, false)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, 1);
}

// Catches automatic regeneration silently accepting malformed config or deleting
// a recoverable generation before the code archive's sources are available.
#[tokio::test]
async fn automatic_migration_invalid_config_and_orphan_archive_preserve_vectors() {
    for config in [
        "[embedding]\nmodel='granite107m'\n[search]\nauto_embed_memory='yes'\n",
        "[embedding]\nmodel='unknown'\n",
        "[embedding]\nmodel='granite107m'\n",
    ] {
        let (_dir, handle) = fixture();
        std::fs::write(handle.root.join(".mdkb/config.toml"), config).unwrap();
        std::fs::write(handle.root.join(".mdkb/vectors.bin"), b"orphan archive").unwrap();
        let (db, marker) = {
            let slot = handle.ctx.lock().await;
            let ctx = slot.as_ref().unwrap();
            mdkb::store::heal::verify_and_mark(&ctx.conn, &ctx.db_path).unwrap();
            (
                ctx.db_path.clone(),
                std::path::PathBuf::from(format!("{}.integrity-ok", ctx.db_path.display())),
            )
        };
        let before = std::fs::metadata(&marker).unwrap().modified().unwrap();
        if let Some(task) = spawn_embedding_backfill(handle.clone()) {
            assert_eq!(task.await.unwrap(), 0);
        }
        assert_eq!(
            std::fs::metadata(&marker).unwrap().modified().unwrap(),
            before,
            "no-write precondition failure changed health marker for {}",
            db.display()
        );
        let slot = handle.ctx.lock().await;
        let conn = &slot.as_ref().unwrap().conn;
        let (model, pending): (String, Option<String>) = conn
            .query_row(
                "SELECT current_model,pending_model FROM embedding_identity",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(model, Model::MiniLm.identity());
        assert_eq!(pending, None);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(slot);
        if !config.contains("unknown") {
            lexical(&handle).await;
        }
        assert!(!handle.backfill_in_flight.load(Ordering::Acquire));
    }
}

// Catches fresh/current stores being reset or fetching weights on every request.
#[tokio::test]
async fn automatic_migration_matching_generation_is_unchanged() {
    let (_dir, handle) = fixture();
    let db = handle.ctx.lock().await.as_ref().unwrap().db_path.clone();
    let writer = mdkb::store::mutation_lock::acquire_writer(
        &db,
        "already current must not open writable context",
    )
    .unwrap();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        encounter(&handle).await;
        settled(&handle).await;
    })
    .await;
    drop(writer);
    outcome.expect("current-store encounter queued a writable open");
    let slot = handle.ctx.lock().await;
    let conn = &slot.as_ref().unwrap().conn;
    assert_eq!(persisted_vector(conn), 0.25f32.to_le_bytes().repeat(384));
    assert!(!vectors::identity_status(conn).unwrap().stale);
}

// Catches a supposedly read-only encounter reopening the database for migration.
#[tokio::test]
async fn automatic_migration_readonly_handle_does_not_start_work() {
    let (dir, handle) = fixture();
    select(&handle, "granite107m");
    let readonly = Context::open_read_only(dir.path()).unwrap();
    *handle.ctx.lock().await = Some(readonly);
    encounter(&handle).await;
    assert!(!handle.backfill_in_flight.load(Ordering::Acquire));
    assert!(spawn_embedding_backfill(handle.clone()).is_none());
    let slot = handle.ctx.lock().await;
    let label: String = slot
        .as_ref()
        .unwrap()
        .conn
        .query_row("SELECT model FROM embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(label, Model::MiniLm.identity());
    drop(slot);
    lexical(&handle).await;
}

// Catches duplicate flights after config reload creates a new RepoHandle, and a
// worker retaining the serving mutex while waiting for another writer.
#[tokio::test]
async fn automatic_migration_shared_store_deduplicates_without_holding_serving_slot() {
    let (_dir, handle) = fixture();
    select(&handle, "granite107m");
    // Fail deterministically before any inference/download after admission.
    std::fs::write(handle.root.join(".mdkb/vectors.bin"), b"orphan archive").unwrap();
    let other = Arc::new(RepoHandle::open(&handle.root, &mdkb::DaemonConfig::default()).unwrap());
    let slot = handle.ctx.lock().await;
    let writer = mdkb::store::mutation_lock::acquire_writer(
        &slot.as_ref().unwrap().db_path,
        "migration test writer",
    )
    .unwrap();
    drop(slot);
    let first = spawn_embedding_backfill(handle.clone()).unwrap();
    assert!(spawn_embedding_backfill(other).is_none());
    // Even while worker admission is blocked, serving can take its own slot.
    lexical(&handle).await;
    drop(writer);
    assert_eq!(first.await.unwrap(), 0);
    assert!(
        spawn_embedding_backfill(handle.clone()).is_none(),
        "failed same-config encounter must back off"
    );
}

// Catches a failed model fetch deleting existing vectors, or a failed flight
// never releasing its guard for a subsequent automatic attempt. The child has
// an isolated empty cache and an observed reject-only endpoint. A loopback
// proxy trap also prevents the pre-fix loader from reaching a public endpoint.
#[test]
fn automatic_migration_failed_model_fetch_preserves_generation_and_retries() {
    if std::env::var_os("MDKB_MIGRATION_FAILURE_CHILD").is_none() {
        use std::io::{BufRead, Write};
        use std::sync::atomic::{AtomicBool, Ordering};

        let cache = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let endpoint = format!("http://{address}");
        let stopping = Arc::new(AtomicBool::new(false));
        let receiver_stopping = stopping.clone();
        let receiver = std::thread::spawn(move || {
            let mut requests = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                if receiver_stopping.load(Ordering::Acquire) {
                    break;
                }
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut request_line = String::new();
                std::io::BufReader::new(&stream)
                    .read_line(&mut request_line)
                    .unwrap();
                requests.push(request_line);
                // This owned receiver rejects requests; it does not emulate a
                // model provider, forward traffic, or retain authorization headers.
                let _ = stream.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
            requests
        });
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "automatic_migration_failed_model_fetch_preserves_generation_and_retries",
                "--nocapture",
            ])
            .env("MDKB_MIGRATION_FAILURE_CHILD", "1")
            .env("FASTEMBED_CACHE_DIR", cache.path())
            .env("HOME", cache.path())
            .env("HF_HOME", cache.path())
            .env("HF_TOKEN", "")
            .env("HF_ENDPOINT", &endpoint)
            .env("HTTP_PROXY", &endpoint)
            .env("HTTPS_PROXY", &endpoint)
            .env("http_proxy", &endpoint)
            .env("https_proxy", &endpoint)
            .env("ALL_PROXY", &endpoint)
            .env("all_proxy", &endpoint)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .output();
        stopping.store(true, Ordering::Release);
        let _ = std::net::TcpStream::connect(address);
        let requests = receiver.join().unwrap();
        let output = output.unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            requests.len() >= 2,
            "initial fetch and later retry must reach the owned receiver"
        );
        assert!(
            requests.iter().all(|line| {
                line.starts_with("GET ")
                    && line.contains(
                        "/ibm-granite/granite-embedding-107m-multilingual/resolve/d6cffd338414d6a1c1f5decfad5fec62eebc90d5/",
                    )
            }),
            "model fetch bypassed the configured endpoint (proxy CONNECT is not an endpoint request)"
        );
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (_dir, handle) = fixture();
        select(&handle, "granite107m");
        encounter(&handle).await;
        settled(&handle).await;
        lexical(&handle).await;
        {
            let slot = handle.ctx.lock().await;
            let conn = &slot.as_ref().unwrap().conn;
            let label: String = conn
                .query_row("SELECT model FROM embeddings", [], |r| r.get(0))
                .unwrap();
            assert_eq!(label, Model::MiniLm.identity());
            assert!(vectors::identity_status(conn).unwrap().stale);
        }
        assert!(
            spawn_embedding_backfill(handle.clone()).is_none(),
            "failure must not retry on every encounter"
        );
        tokio::time::sleep(std::time::Duration::from_secs(31)).await;
        let retry = spawn_embedding_backfill(handle.clone())
            .expect("same config must become eligible again");
        assert_eq!(retry.await.unwrap(), 0);
        // A new encounter can retry. The independent current-store oracle
        // also proves that failure never deleted its existing vector values.
        select(&handle, "minilm");
        encounter(&handle).await;
        settled(&handle).await;
        let slot = handle.ctx.lock().await;
        let conn = &slot.as_ref().unwrap().conn;
        assert!(!vectors::identity_status(conn).unwrap().stale);
        assert_eq!(persisted_vector(conn), 0.25f32.to_le_bytes().repeat(384));
    });
}

// Catches encounter-only code warning about a mismatch without ever scheduling
// the complete document+memory generation, or treating pending work as current.
#[tokio::test]
#[ignore = "requires cached MiniLM and Granite weights"]
async fn automatic_migration_encounter_rebuilds_documents_memories_and_pending_generation() {
    for interrupted in [false, true] {
        let (_dir, handle) = fixture();
        select(&handle, "granite107m");
        if interrupted {
            vectors::begin_reembed(&handle.ctx.lock().await.as_ref().unwrap().conn).unwrap();
        }
        encounter(&handle).await;
        settled(&handle).await;
        lexical(&handle).await;
        let slot = handle.ctx.lock().await;
        let conn = &slot.as_ref().unwrap().conn;
        let status = vectors::identity_status(conn).unwrap();
        assert!(!status.stale);
        assert_eq!(
            status.current_model.as_deref(),
            Some(Model::Granite107M.identity())
        );
        assert!(status.pending_model.is_none());
        let labels: Vec<String> = conn
            .prepare("SELECT model FROM embeddings UNION ALL SELECT model FROM memory_embeddings")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(labels, vec![Model::Granite107M.identity(); 2]);
        assert_ne!(persisted_vector(conn), 0.25f32.to_le_bytes().repeat(384));
    }
}

// Catches a full regeneration that still runs inside the daemon at the daemon's
// priority on its two-thread ONNX pool, instead of in a child that lowers itself.
#[tokio::test]
#[ignore = "requires cached MiniLM and Granite weights"]
async fn automatic_migration_regenerates_in_a_lowered_child_process() {
    // SAFETY: getpriority on this process only.
    let before = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    let (_dir, handle) = fixture();
    select(&handle, "granite107m");
    encounter(&handle).await;
    settled(&handle).await;
    let log = std::fs::read_to_string(handle.root.join(".mdkb/worker.log")).unwrap();
    let line = log
        .lines()
        .find(|l| l.starts_with(&format!("ppid={} no_daemon=1 ort=", std::process::id())))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(
        line.ends_with(" args=--format json update --reembed"),
        "{log}"
    );
    // Measured 2026-11-21: `0` (one pool per session, one thread per core)
    // burned 2.6x the CPU of a bounded pool and finished slower than 8 threads.
    let ort: usize = line
        .split(" ort=")
        .nth(1)
        .and_then(|r| r.split(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let cores = std::thread::available_parallelism().unwrap().get();
    assert!(
        ort >= 2 && ort < cores.max(3),
        "regeneration needs a bounded ONNX pool: {line}"
    );
    assert!(log.contains("embedding at nice 15"), "{log}");
    // SAFETY: as above.
    assert_eq!(unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }, before);
    let slot = handle.ctx.lock().await;
    let status = vectors::identity_status(&slot.as_ref().unwrap().conn).unwrap();
    assert_eq!(
        status.current_model.as_deref(),
        Some(Model::Granite107M.identity())
    );
    assert!(!status.stale);
}

// Catches a process-wide selected model leaking from the first queued repository
// into a second repository, including opposite transitions with equal dimensions.
#[tokio::test]
#[ignore = "requires cached MiniLM and Granite weights"]
async fn automatic_migration_multiple_repositories_keep_their_selected_models() {
    let (_first_dir, first) = fixture();
    let (_second_dir, second) = fixture();
    select(&first, "granite107m");
    select(&second, "granite107m");
    {
        let slot = second.ctx.lock().await;
        let conn = &slot.as_ref().unwrap().conn;
        vectors::begin_reembed(conn).unwrap();
        vectors::store_embedding(conn, 1, &[0.25; 384], Model::Granite107M.identity()).unwrap();
        vectors::store_memory_embedding(conn, 1, &[0.5; 384], Model::Granite107M.identity())
            .unwrap();
        vectors::finish_reembed(conn).unwrap();
    }
    select(&second, "minilm");
    encounter(&first).await;
    encounter(&second).await;
    settled(&first).await;
    settled(&second).await;
    for (handle, expected) in [(first, Model::Granite107M), (second, Model::MiniLm)] {
        let slot = handle.ctx.lock().await;
        let conn = &slot.as_ref().unwrap().conn;
        let state = vectors::identity_status(conn).unwrap();
        assert!(!state.stale);
        assert_eq!(state.current_model.as_deref(), Some(expected.identity()));
        assert_ne!(persisted_vector(conn), 0.25f32.to_le_bytes().repeat(384));
    }
}

// Catches a successful same-model pending retry replacing vectors.bin while a
// warmed IndexFacade continues returning the old archive's symbol set.
#[tokio::test]
#[ignore = "requires cached MiniLM weights"]
async fn automatic_migration_success_refreshes_warmed_code_search() {
    use mdkb::code::{indexing::IndexFacade, semantic::SemanticSearch};
    let (_dir, handle) = fixture();
    let path = handle.root.join(".mdkb/code.sqlite");
    let facade = IndexFacade::create(&path).unwrap();
    let db = facade.db();
    let file = db
        .insert_file(
            "recovery.rs",
            "recovery.rs",
            "hash",
            Some("rust"),
            None,
            None,
        )
        .unwrap();
    let old = db
        .insert_symbol(
            "old_recovery",
            "function",
            file,
            "recovery.rs",
            0,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    SemanticSearch::reembed_at(
        handle.root.join(".mdkb/vectors.bin"),
        Model::MiniLm,
        &[(old as u32, "function old_recovery".into())],
    )
    .unwrap();
    let hits = facade.semantic_search("sqlite recovery", 5, -1.0).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.name.as_ref(), "old_recovery");
    db.insert_symbol(
        "new_recovery",
        "function",
        file,
        "recovery.rs",
        1,
        None,
        None,
        None,
        0,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    // This is the persisted state of an interrupted code-index update: source
    // symbols committed, archive/cache not yet regenerated.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM code_symbols WHERE id=?1", [old])
        .unwrap();
    *handle.code_index.lock().await = Some(facade);
    vectors::begin_reembed(&handle.ctx.lock().await.as_ref().unwrap().conn).unwrap();
    encounter(&handle).await;
    settled(&handle).await;
    let index = mdkb::mcp::dispatch::acquire_handle_code_index(&handle)
        .await
        .unwrap();
    let hits = index
        .as_ref()
        .unwrap()
        .semantic_search("sqlite recovery", 5, -1.0)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.name.as_ref(), "new_recovery");
}

// Catches: a real full rebuild publishing its captured encoder after the
// selection changes during inference. Pending is observed from SQLite, rather
// than set by the test; an interrupted rebuild must remain semantically closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires cached Granite weights"]
async fn automatic_migration_selection_changes_after_begin_preserves_pending_safety() {
    let (_dir, handle) = fixture();
    {
        let slot = handle.ctx.lock().await;
        let conn = &slot.as_ref().unwrap().conn;
        for id in 2..=1024 {
            conn.execute("INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(?1,'notes',?2,'hash',0,0)", rusqlite::params![id, format!("recovery-{id}.md")]).unwrap();
        }
    }
    select(&handle, "granite107m");
    let task = spawn_embedding_backfill(handle.clone()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let slot = handle.ctx.lock().await;
            let pending: Option<String> = slot
                .as_ref()
                .unwrap()
                .conn
                .query_row("SELECT pending_model FROM embedding_identity", [], |r| {
                    r.get(0)
                })
                .unwrap();
            if pending.as_deref() == Some(Model::Granite107M.identity()) {
                // Config is an external input and can change without acquiring
                // SQLite writer admission, exactly as in a running daemon.
                select(&handle, "minilm");
                break;
            }
            assert!(
                !task.is_finished(),
                "generation completed before its pending state could be observed"
            );
            drop(slot);
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("background worker never began its generation");
    assert_eq!(task.await.unwrap(), 0);
    let slot = handle.ctx.lock().await;
    let ctx = slot.as_ref().unwrap();
    let state = vectors::identity_status(&ctx.conn).unwrap();
    assert!(state.stale);
    // begin_reembed clears the old certificate; partial writes may identify
    // Granite, but neither outcome may certify the newly selected MiniLm.
    assert!(
        state
            .current_model
            .as_deref()
            .is_none_or(|model| model == Model::Granite107M.identity()),
        "partial rebuild falsely certified the newly selected model"
    );
    assert_eq!(
        state.pending_model.as_deref(),
        Some(Model::Granite107M.identity())
    );
    assert!(
        vectors::vector_search(&ctx.conn, &[0.5; 384], 5).is_err(),
        "partial generation became searchable"
    );
    assert!(
        !mdkb::core::search::hybrid_search_fts_labelled(ctx, "sqlite", None, 5, None, false)
            .unwrap()
            .is_empty(),
        "pending generation lost lexical retrieval"
    );
}
