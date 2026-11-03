//! Owner ingest across daemon dispatch and independent CLI reader processes.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64};

use mdkb::daemon::config::DaemonConfig;
use mdkb::daemon::registry::RepoRegistry;
use mdkb::mcp::dispatch::{DispatchContext, dispatch_call};
use mdkb::metrics::UsageMetrics;
use serde_json::{Value, json};

#[path = "common/cli.rs"]
mod cli;

/// Catches file-only readers/reindexers losing owner content after a daemon write.
#[tokio::test]
async fn external_ingest_cli_reads_survive_reindex_and_preserve_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir_all(root.join(".mdkb")).unwrap();
    std::fs::write(
        root.join(".mdkb/config.toml"),
        "[search]\nauto_embed_docs = false\n",
    )
    .unwrap();
    let registry = RepoRegistry::new(DaemonConfig {
        whitelist_dirs: vec![temp.path().to_string_lossy().into_owned()],
        state_dir: Some(temp.path().join("daemon")),
        ..DaemonConfig::default()
    });
    let handle = registry.get_or_open(&root).unwrap();
    let dctx = DispatchContext {
        metrics: Arc::new(UsageMetrics::new()),
        session_id: Arc::new(AtomicI64::new(0)),
        persistent_call_count: Arc::new(AtomicU64::new(0)),
        optimize_interval_calls: 200,
        hook_dedup: Arc::new(std::sync::Mutex::new(Default::default())),
        background: None,
    };
    let inserted = dispatch_call(
        "external.upsert",
        json!({
            "source":"tuic-stories", "id":"2333-b498", "revision":1,
            "title":"Quasar feature", "body":"quasar original owner content",
            "metadata":{"status":"complete", "type":"feature", "updated_at":"2026-10-25"}
        }),
        Arc::clone(&handle),
        &dctx,
    )
    .await
    .unwrap();
    let id = inserted["document_id"].as_i64().unwrap().to_string();

    // The actual CLI uses a separate connection and explicit root, not a daemon cache.
    let output = cli::run(
        &[
            "--format",
            "json",
            "search",
            "quasar",
            "--scope",
            "docs",
            "--root",
            root.to_str().unwrap(),
        ],
        temp.path(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let search: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(search[0]["source"], "tuic-stories");
    assert_eq!(search[0]["read_only"], true);
    // Catches the text/Markdown/CSV renderers silently dropping read-only provenance.
    for format in ["text", "markdown", "csv"] {
        let output = cli::run(
            &["--format", format, "search", "quasar", "--scope", "docs"],
            &root,
        );
        assert!(output.status.success());
        let rendered = String::from_utf8(output.stdout).unwrap();
        assert!(rendered.contains("read-only"), "{format}: {rendered}");
        assert!(rendered.contains("tuic-stories"), "{format}: {rendered}");
        if format == "csv" {
            assert!(
                rendered.contains("\"Quasar feature [read-only, source: tuic-stories]\""),
                "{rendered}"
            );
        }
    }
    let output = cli::run(&["--format", "json", "get", &id], &root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let get: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(get["content"], "quasar original owner content");
    assert_eq!(get["document"]["metadata"]["external"]["read_only"], true);
    assert_eq!(get["document"]["metadata"]["status"], "complete");

    // A path that looks like the owner ID is still not a file owned by this collection.
    std::fs::write(root.join("2333-b498"), "filesystem impostor").unwrap();
    {
        let slot = handle.ctx.lock().await;
        let ctx = slot.as_ref().unwrap();
        let full = mdkb::core::indexing::handle_update_force(ctx, &root, true).unwrap();
        assert!(full.errors.is_empty(), "{:?}", full.errors);
        let watcher = mdkb::core::indexing::handle_update_unverified(ctx, &root).unwrap();
        assert!(watcher.errors.is_empty(), "{:?}", watcher.errors);
        let targeted = mdkb::core::indexing::handle_update_files_force(
            ctx,
            &root,
            &["2333-b498".into()],
            true,
        )
        .unwrap();
        assert_eq!(targeted.removed, 0);
    }
    let get = dispatch_call("get", json!({"id":id}), Arc::clone(&handle), &dctx)
        .await
        .unwrap();
    assert!(
        get["text"]
            .as_str()
            .unwrap()
            .contains("read-only, source: tuic-stories")
    );
    assert!(
        get["text"]
            .as_str()
            .unwrap()
            .contains("quasar original owner content")
    );
    let search = dispatch_call(
        "search",
        json!({"query":"quasar", "scope":"docs"}),
        Arc::clone(&handle),
        &dctx,
    )
    .await
    .unwrap();
    assert!(
        search["text"]
            .as_str()
            .unwrap()
            .contains("read-only, source: tuic-stories")
    );
    dispatch_call(
        "external.delete",
        json!({"source":"tuic-stories", "id":"2333-b498", "revision":2}),
        Arc::clone(&handle),
        &dctx,
    )
    .await
    .unwrap();
    let output = cli::run(
        &[
            "--format",
            "json",
            "search",
            "quasar",
            "--scope",
            "docs",
            "--root",
            root.to_str().unwrap(),
        ],
        temp.path(),
    );
    assert!(output.status.success());
    let search: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(search, json!([]));
}
