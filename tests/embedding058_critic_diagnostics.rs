//! Story 058 diagnostics regressions: code-archive identity in fresh processes,
//! and SessionStart recovery advice while the daemon owns model migration.
//!
//! Every case that depends on process-global state (sqlite-vec registration,
//! model cache and Hugging Face endpoint) runs in a fresh child process of this
//! test binary, so another test in the same process cannot mask it.
use std::process::Command;
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
};

use mdkb::{
    code::semantic::VectorStore,
    config::Config,
    core::Context,
    daemon::registry::RepoHandle,
    llm::embeddings::Model,
    mcp::dispatch::{DispatchContext, dispatch_call},
    store::vectors,
};

/// Re-run exactly `test` in a fresh process with `env`, and require success.
fn run_child(test: &str, marker: &str, env: &[(&str, &std::ffi::OsStr)]) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .env(marker, "1");
    // Daemon eligibility is decided by these variables: never inherit them.
    for key in [
        "MDKB_NO_DAEMON",
        "MDKB_NAMESPACE",
        "NODE_TEST_CONTEXT",
        "VITEST",
        "JEST_WORKER_ID",
        "PYTEST_CURRENT_TEST",
    ] {
        command.env_remove(key);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{test} child failed\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Environment that makes any model acquisition fail locally and at once.
fn offline_env(cache: &std::path::Path) -> Vec<(&'static str, std::ffi::OsString)> {
    let closed = std::ffi::OsString::from("http://127.0.0.1:9");
    vec![
        ("FASTEMBED_CACHE_DIR", cache.as_os_str().to_owned()),
        ("HF_HOME", cache.as_os_str().to_owned()),
        ("HF_ENDPOINT", closed.clone()),
        ("HTTP_PROXY", closed.clone()),
        ("HTTPS_PROXY", closed.clone()),
        ("http_proxy", closed.clone()),
        ("https_proxy", closed.clone()),
        ("ALL_PROXY", closed.clone()),
        ("all_proxy", closed),
    ]
}

// ---------------------------------------------------------------------------
// F3: the code-archive identity check must work in a process that has not
// opened a document store before.
// ---------------------------------------------------------------------------

// Catches: `VectorStore::require_identity` querying the vec0 tables of
// index.sqlite on a raw connection opened before sqlite-vec is registered, so
// a fresh process (direct `mdkb code index`: Windows, MDKB_NO_DAEMON,
// namespaces) fails every archive load/count/write/clear with
// "no such module: vec0". No model is needed: identity is checked first.
#[test]
fn fresh_process_code_archive_reads_and_writes_without_prior_store_open() {
    const MARKER: &str = "MDKB_058_F3_ARCHIVE_CHILD";
    if let Some(dir) = std::env::var_os(MARKER) {
        let archive = std::path::PathBuf::from(dir).join("vectors.bin");
        let store = VectorStore::open_for_model(&archive, Model::MiniLm).unwrap();
        assert_eq!(store.count().expect("fresh-process count"), 0);
        assert!(store.load().expect("fresh-process load").is_empty());
        store
            .write_all(&[(7, vec![0.25; 384])])
            .expect("fresh-process write");
        assert_eq!(
            store.load().expect("fresh-process reload"),
            vec![(7, vec![0.25; 384])]
        );
        store.clear().expect("fresh-process clear");
        assert_eq!(store.count().unwrap(), 0);
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Created by THIS process; the child that reads it registers nothing first.
    let ctx = Context::init(tmp.path()).unwrap();
    let dir = ctx.db_path.parent().unwrap().to_path_buf();
    drop(ctx);
    VectorStore::open_for_model(dir.join("vectors.bin"), Model::MiniLm).unwrap();
    run_child(
        "fresh_process_code_archive_reads_and_writes_without_prior_store_open",
        MARKER,
        &[(MARKER, dir.as_os_str())],
    );
}

// Catches the same defect at the consumer boundary the probe observed: a
// direct `mdkb code index` exits 0 while every semantic clear, sweep and
// generation fails on the missing vec0 module.
#[test]
fn direct_code_index_does_not_fail_on_missing_sqlite_vec_module() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("repo");
    let cache = tmp.path().join("model-cache");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let mdkb = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mdkb"));
        command
            .args(args)
            .current_dir(&repo)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("MDKB_NO_DAEMON", "1")
            .env("MDKB_NO_SPAWN", "1")
            .env("RUST_LOG", "mdkb=warn");
        for (key, value) in offline_env(&cache) {
            command.env(key, value);
        }
        command.output().unwrap()
    };
    let init = mdkb(&["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(repo.join("a.rs"), "pub fn alpha() {}\n").unwrap();
    let index = mdkb(&["update", "--code"]);
    let stderr = String::from_utf8_lossy(&index.stderr);
    assert!(index.status.success(), "{stderr}");
    assert!(
        String::from_utf8_lossy(&index.stdout).contains("Symbols written"),
        "code index never reached symbol indexing: {stderr}"
    );
    assert!(
        !stderr.contains("no such module"),
        "direct code index opened index.sqlite without sqlite-vec: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// F2: SessionStart advice must match who owns recovery.
// ---------------------------------------------------------------------------

/// A store whose document and memory vectors carry `label`, selecting `model`.
fn store_with_vectors(
    model: &str,
    memory_label: Option<&str>,
) -> (tempfile::TempDir, Arc<RepoHandle>) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Context::init(dir.path()).unwrap();
    ctx.conn.execute_batch("INSERT INTO collections(name,path,pattern,created_at,updated_at) VALUES('notes','.','*.md',0,0);
        INSERT INTO content(hash,body,created_at) VALUES('hash','sqlite recovery instructions',0);
        INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(1,'notes','recovery.md','hash',0,0);
        INSERT INTO memory_entries(id,title,content,entry_type,tags,status,created_at,updated_at) VALUES('memory','SQLite recovery','sqlite recovery instructions','topic','[]','active',0,0);").unwrap();
    vectors::store_embedding(&ctx.conn, 1, &[0.25; 384], Model::MiniLm.identity()).unwrap();
    vectors::store_memory_embedding(&ctx.conn, 1, &[0.5; 384], Model::MiniLm.identity()).unwrap();
    if let Some(label) = memory_label {
        ctx.conn
            .execute("DELETE FROM embedding_identity", [])
            .unwrap();
        ctx.conn
            .execute("UPDATE memory_embeddings SET model=?1", [label])
            .unwrap();
    }
    // Code intelligence off: SessionStart must not start an unrelated code
    // refresh in this fixture.
    std::fs::write(
        &ctx.config_path,
        format!("[embedding]\nmodel = '{model}'\n[code]\nenabled = false\n"),
    )
    .unwrap();
    let config = Config::load(&ctx.config_path).unwrap();
    let handle = Arc::new(RepoHandle::from_shared(
        ctx.root().to_path_buf(),
        Arc::new(tokio::sync::Mutex::new(Some(ctx))),
        Arc::new(tokio::sync::Mutex::new(None)),
        config,
        vec![],
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ));
    (dir, handle)
}

/// SessionStart through the real dispatcher. `daemon` selects the long-lived
/// route (`background: None`) or the one-shot in-process route.
async fn session_start(handle: &Arc<RepoHandle>, daemon: bool) -> String {
    let background = (!daemon).then(|| Arc::new(StdMutex::new(Vec::new())));
    let dctx = DispatchContext {
        metrics: Arc::new(mdkb::metrics::UsageMetrics::new()),
        session_id: Arc::new(AtomicI64::new(0)),
        persistent_call_count: Arc::new(AtomicU64::new(0)),
        optimize_interval_calls: 200,
        hook_dedup: Arc::new(StdMutex::new(Default::default())),
        background: background.clone(),
    };
    let value = dispatch_call(
        "hook.session_start",
        serde_json::json!({"session_id": "embedding058-diagnostics"}),
        handle.clone(),
        &dctx,
    )
    .await
    .unwrap();
    // Join what the one-shot route parked, and let a daemon-scheduled flight
    // (which fails at once on the closed endpoint) settle before teardown.
    if let Some(parked) = background {
        let tasks: Vec<_> = parked.lock().unwrap().drain(..).collect();
        for task in tasks {
            let _ = task.await;
        }
    }
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while handle.backfill_in_flight.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("embedding flight did not settle");
    value.to_string()
}

// Catches: while the daemon owns an automatic migration of a plainly
// model-mismatched store, SessionStart injecting an Error that tells every
// agent to run `mdkb embed --reembed` (a second full rebuild queued behind the
// automatic one). The state stays visible as a warning. Unix only: Windows
// has no daemon, so manual recovery is the correct advice there.
#[cfg(unix)]
#[test]
fn daemon_session_start_does_not_ask_for_manual_reembed_during_automatic_migration() {
    const MARKER: &str = "MDKB_058_F2_DAEMON_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let cache = tempfile::tempdir().unwrap();
        let env = offline_env(cache.path());
        let env: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        run_child(
            "daemon_session_start_does_not_ask_for_manual_reembed_during_automatic_migration",
            MARKER,
            &env,
        );
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (_dir, handle) = store_with_vectors("granite107m", None);
        let text = session_start(&handle, true).await;
        assert!(
            text.contains("[warning] embedding.identity"),
            "migration state disappeared from SessionStart: {text}"
        );
        assert!(
            !text.contains("[error] embedding.identity"),
            "daemon-owned migration reported as an error: {text}"
        );
        assert!(
            !text.contains("update --reembed"),
            "daemon-owned migration asks agents for a manual rebuild: {text}"
        );
    });
}

// Catches (review B3): an ambiguous store, which the daemon rebuilds like a
// stale one, reported as an Error that tells agents to queue a manual
// `embed --reembed` behind the automatic rebuild.
#[cfg(unix)]
#[test]
fn daemon_session_start_reports_ambiguous_store_as_background_rebuild() {
    const MARKER: &str = "MDKB_058_B3_DAEMON_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let cache = tempfile::tempdir().unwrap();
        let env = offline_env(cache.path());
        let env: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        run_child(
            "daemon_session_start_reports_ambiguous_store_as_background_rebuild",
            MARKER,
            &env,
        );
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (_dir, handle) = store_with_vectors("minilm", Some("unknown-encoder"));
        let text = session_start(&handle, true).await;
        assert!(
            text.contains("[warning] embedding.identity")
                && text.contains("automatic re-embedding"),
            "ambiguous store does not say a background rebuild runs: {text}"
        );
        assert!(
            !text.contains("update --reembed"),
            "daemon-owned rebuild of an ambiguous store asks for a manual one: {text}"
        );
    });
}

// Guards the other side of the same boundary, so the fix cannot simply drop
// the diagnostic: where no daemon can own the rebuild (MDKB_NO_DAEMON), an
// ambiguous store stays an error and keeps its recovery command, as does the
// one-shot route, which never runs a full migration.
#[test]
fn session_start_keeps_ambiguous_error_and_one_shot_recovery_command() {
    const MARKER: &str = "MDKB_058_F2_GUARD_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let cache = tempfile::tempdir().unwrap();
        let env = offline_env(cache.path());
        let mut env: Vec<_> = env.iter().map(|(k, v)| (*k, v.as_os_str())).collect();
        env.push(("MDKB_NO_DAEMON", std::ffi::OsStr::new("1")));
        run_child(
            "session_start_keeps_ambiguous_error_and_one_shot_recovery_command",
            MARKER,
            &env,
        );
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (_ambiguous_dir, ambiguous) = store_with_vectors("minilm", Some("unknown-encoder"));
        let text = session_start(&ambiguous, true).await;
        assert!(
            text.contains("[error] embedding.identity"),
            "ambiguous vectors no longer reported as an error: {text}"
        );

        let (_one_shot_dir, one_shot) = store_with_vectors("granite107m", None);
        let text = session_start(&one_shot, false).await;
        assert!(
            text.contains("update --reembed"),
            "one-shot route lost the only recovery command: {text}"
        );
    });
}
