//! CLI command handlers.
//!
//! Argument handling and output formatting for the command line. The shared
//! application layer it drives lives in [`crate::core`]; the re-exports below
//! exist so a CLI-facing caller keeps one import path, and so the dependency
//! runs CLI -> core rather than the inversion this module used to create.

pub use crate::core::code::{
    CodeInfoResult, handle_code_callers, handle_code_calls, handle_code_find, handle_code_impact,
    handle_code_index, handle_code_info, handle_code_init, handle_code_parse, handle_code_reindex,
    handle_code_search,
};
pub use crate::core::graph::{
    CollectionInfo, handle_collection_add, handle_collection_list, handle_collection_remove,
    handle_collection_rename, handle_evolve_corrects, handle_evolve_extends,
    handle_evolve_retracts, handle_evolve_supersedes, handle_evolve_updates,
    handle_graph_backlinks, handle_graph_dangling, handle_graph_hubs, handle_graph_links,
    handle_graph_neighbors, handle_graph_path, handle_superseded_by,
};
pub use crate::core::indexing::{
    handle_update, handle_update_files, handle_update_files_force, handle_update_force,
};
#[cfg(feature = "llm")]
pub use crate::core::memory::handle_memory_condense;
pub use crate::core::memory::{
    ConfirmResult, ExportResult, ImportResult, handle_memory_add, handle_memory_confirm,
    handle_memory_export, handle_memory_import, handle_memory_import_dir,
    handle_memory_import_file, handle_memory_link, handle_memory_list, handle_memory_prune,
    handle_memory_rm, handle_memory_search, handle_memory_show, handle_memory_warmup,
};
pub use crate::core::memory_sync::{
    MEMORY_SYNC_BULK_ARCHIVE_CAP, MemorySyncSummary, ProjectionDrift, generate_memory_index,
    load_memory_index, projection_drift, projection_file_and_row_counts, sync_memory_files,
};
pub use crate::core::ops::{
    EmbedResult, EvolutionHistoryEntry, ExperimentCreateResult, GetResult, PruneSessionsSummary,
    handle_current, handle_embed, handle_eval_judge, handle_eval_recall, handle_experiment_cancel,
    handle_experiment_create, handle_experiment_end, handle_experiment_list,
    handle_experiment_status, handle_get, handle_history, handle_init, handle_journal_import,
    handle_journal_import_all, handle_metrics_export, handle_metrics_latency, handle_metrics_show,
    handle_prune_sessions, handle_search, handle_vsearch, parse_retention_secs,
};
pub use crate::core::search::{handle_hybrid_search, handle_mget, hybrid_search_fts};
pub use crate::core::sessions::handle_session_index;

// ==================== Memory Handlers ====================

// ==================== Memory Import ====================

// ==================== Memory Condense (LLM feature) ====================

// ==================== Evolution Handlers ====================

// ==================== Knowledge-Graph Handlers ====================

// ==================== Metrics Handlers ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Context;
    use crate::core::indexing::process_graph_edges;
    use crate::core::ops::{apply_line_range, log_slow_embed, should_embed_collection};
    use crate::domain::frontmatter::parse_frontmatter;
    use crate::store::evolution::RelationshipType;
    use crate::store::memory::EntryType;
    use crate::store::{collections, memory};
    use crate::store::{documents, evolution};
    use tempfile::TempDir;

    fn setup_temp_dir() -> TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    /// `.mutation.lock`, `.live.lock` and SQLite's own `-wal`/`-shm` are all
    /// named from `db_path` as a string. Two spellings of one store therefore
    /// mean two lock domains over a single inode: neither the open guard nor
    /// the live lock excludes the other writer, and the result is the
    /// doubly-referenced pages and freelist mismatch we kept recovering from.
    /// Opening through an alias must land on the identical path.
    #[test]
    fn open_canonicalizes_the_store_so_every_lock_shares_one_identity() {
        let temp = setup_temp_dir();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).expect("mkdir");
        handle_init(&real).expect("init");

        let direct = Context::open(&real).expect("open directly");

        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink");
        let aliased = Context::open(&alias).expect("open through the alias");

        assert_eq!(
            direct.db_path, aliased.db_path,
            "aliased spelling opened a second lock domain over the same inode"
        );
        assert_eq!(
            crate::store::mutation_lock::live_lock_path(&direct.db_path),
            crate::store::mutation_lock::live_lock_path(&aliased.db_path),
            "the live lock that guards against renames must be one file, not two"
        );
    }

    /// A missing store must fail loudly rather than open something. The
    /// canonicalization error branch itself is defensive only — the existence
    /// check above it means a `.mdkb` that resolves cannot then fail to
    /// canonicalize, so this covers the reachable half.
    #[test]
    fn open_refuses_a_store_that_is_not_there() {
        let temp = setup_temp_dir();
        let root = temp.path().join("gone");
        std::fs::create_dir_all(root.join(".mdkb")).expect("mkdir");
        std::fs::remove_dir(root.join(".mdkb")).expect("rmdir");
        let err = Context::open(&root).expect_err("must not open");
        let msg = err.to_string();
        assert!(
            msg.contains("index.sqlite") || msg.contains("canonicalize"),
            "error must name the store or the canonicalization failure: {msg}"
        );
    }

    // ==================== Memory confirm ====================

    fn confirm_test_ctx() -> (TempDir, Context) {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init");
        let ctx = Context::open(temp.path()).expect("open");
        handle_memory_add(
            &ctx, "c1", "Title", "topic", None, "content", None, None, None, None,
        )
        .expect("add");
        (temp, ctx)
    }

    #[test]
    fn handle_get_resolves_path_memory_and_errors_once_on_missing() {
        // BUG-E2 correctness: after gating the collection scan to run once, a
        // path-like doc still resolves, a memory slug still resolves, and a
        // not-found path-like id returns DocumentNotFound (no double scan / panic).
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let docs = temp.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("guide.md"), "# Guide\n\nbody text").unwrap();

        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        match handle_get(&ctx, "guide.md", None).expect("path-like doc resolves") {
            GetResult::Document(doc, _) => assert!(doc.relative_path.ends_with("guide.md")),
            GetResult::Memory(entry) => panic!("expected a document, got {entry:?}"),
        }

        assert!(
            handle_get(&ctx, "missing/thing.md", None).is_err(),
            "a not-found path-like id must return an error"
        );

        handle_memory_add(
            &ctx, "slug1", "T", "topic", None, "c", None, None, None, None,
        )
        .unwrap();
        match handle_get(&ctx, "slug1", None).expect("memory slug resolves") {
            GetResult::Memory(e) => assert_eq!(e.id, "slug1"),
            GetResult::Document(document, content) => {
                panic!("expected a memory entry, got {document:?} with {content:?}")
            }
        }
    }

    #[test]
    fn confirm_increments_and_reports_count() {
        let (_t, ctx) = confirm_test_ctx();
        let r = handle_memory_confirm(&ctx, "c1", "confirmed").expect("confirm");
        assert_eq!(r.confirmations, 1);
        assert!(r.message.contains("Confirmed"));
        let r2 = handle_memory_confirm(&ctx, "c1", "confirmed").expect("confirm2");
        assert_eq!(r2.confirmations, 2, "double confirm accumulates");
    }

    #[test]
    fn confirm_refuted_floors_at_zero() {
        let (_t, ctx) = confirm_test_ctx();
        let r = handle_memory_confirm(&ctx, "c1", "refuted").expect("refute");
        assert_eq!(r.confirmations, 0, "refute below zero floors at 0");
    }

    #[test]
    fn confirm_unknown_id_errors_cleanly() {
        let (_t, ctx) = confirm_test_ctx();
        let err = handle_memory_confirm(&ctx, "ghost", "confirmed").expect_err("unknown id");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn confirm_invalid_outcome_rejected() {
        let (_t, ctx) = confirm_test_ctx();
        let err = handle_memory_confirm(&ctx, "c1", "sideways").expect_err("bad outcome");
        assert!(err.to_string().contains("outcome"), "{err}");
    }

    // ==================== Embed scoping ====================

    #[test]
    fn embed_scope_default_excludes_sessions() {
        // filter=None, include_sessions=false → docs yes, claude_sessions no.
        assert!(should_embed_collection("docs", None, false));
        assert!(should_embed_collection("plans", None, false));
        assert!(!should_embed_collection("claude_sessions", None, false));
    }

    #[test]
    fn embed_scope_include_sessions_covers_all() {
        assert!(should_embed_collection("docs", None, true));
        assert!(should_embed_collection("claude_sessions", None, true));
    }

    #[test]
    fn log_slow_embed_writes_record_to_hook_slow() {
        let dir = setup_temp_dir();
        log_slow_embed(dir.path(), "docs/big.md", 1500);
        let raw = std::fs::read_to_string(dir.path().join("hook-slow.jsonl"))
            .expect("hook-slow.jsonl written");
        let v: serde_json::Value = serde_json::from_str(raw.trim()).expect("valid json line");
        assert_eq!(v["event"], "embed");
        assert_eq!(v["doc"], "docs/big.md");
        assert_eq!(v["elapsed_ms"], 1500);
        assert!(v["ts"].as_i64().is_some(), "ts recorded for stats cutoff");
    }

    #[test]
    fn embed_scope_explicit_filter_targets_one_collection() {
        // Explicit --collection embeds exactly that one, sessions included.
        assert!(should_embed_collection(
            "claude_sessions",
            Some("claude_sessions"),
            false
        ));
        assert!(!should_embed_collection(
            "docs",
            Some("claude_sessions"),
            false
        ));
        assert!(should_embed_collection("docs", Some("docs"), false));
        assert!(!should_embed_collection(
            "claude_sessions",
            Some("docs"),
            false
        ));
    }

    // ==================== Init Tests ====================

    #[test]
    fn test_handle_init_creates_mdkb_directory() {
        let temp = setup_temp_dir();

        handle_init(temp.path()).expect("init should succeed");

        assert!(temp.path().join(".mdkb").exists());
        assert!(temp.path().join(".mdkb/config.toml").exists());
        assert!(temp.path().join(".mdkb/index.sqlite").exists());
    }

    #[test]
    fn test_handle_init_creates_memory_directories() {
        let temp = setup_temp_dir();

        handle_init(temp.path()).expect("init should succeed");

        // Memory directories should be created
        assert!(temp.path().join(".mdkb/memory").exists());
        assert!(temp.path().join(".mdkb/memory/entries").exists());
        assert!(temp.path().join(".mdkb/memory/archive").exists());
    }

    #[test]
    fn test_memory_add_creates_index_json() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        // Add a memory entry
        handle_memory_add(
            &ctx,
            "test-entry",
            "Test Entry",
            "topic",
            Some("test,example"),
            "# Test content\n\nThis is test content.",
            None,
            None,
            None,
            None,
        )
        .expect("add memory should succeed");

        // index.json should be created
        let index_path = temp.path().join(".mdkb/memory/index.json");
        assert!(index_path.exists(), "index.json should be created");

        // Entry file should be created
        let entry_path = temp.path().join(".mdkb/memory/entries/test-entry.md");
        assert!(entry_path.exists(), "entry file should be created");

        // Load and verify index
        let index = load_memory_index(&ctx).expect("load index should succeed");
        assert!(index.is_some(), "index should load");
        let index = index.unwrap();
        assert_eq!(index.entries.len(), 1);
        assert!(index.entries[0].contains("test-entry"));
    }

    #[test]
    fn test_memory_add_rejects_mechanical_prior() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        let err = handle_memory_add(
            &ctx,
            "prior-deadbeefdeadbeef",
            "fix: fix|tools:Edit->Bash",
            "prior",
            Some("prior,fix"),
            "Pattern: fix|tools:Edit->Bash->Bash|files:none|error_in:none\nOutcome: fix",
            None,
            None,
            None,
            None,
        )
        .expect_err("mechanical tool-chain prior must be rejected");
        assert!(
            err.to_string().contains("mechanical tool-chain prior"),
            "{err}"
        );

        // A non-prior entry with the same text is NOT rejected (guard is prior-scoped).
        handle_memory_add(
            &ctx,
            "topic-tools",
            "Tooling",
            "topic",
            None,
            "Pattern: fix|tools:Edit->Bash|files:none|error_in:none",
            None,
            None,
            None,
            None,
        )
        .expect("non-prior entry must not be rejected");
    }

    #[test]
    fn test_memory_link_roundtrip() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        for id in ["a", "b"] {
            handle_memory_add(
                &ctx, id, "Title", "topic", None, "content", None, None, None, None,
            )
            .expect("add should succeed");
        }

        handle_memory_link(&ctx, "a", "supports", "b", false, None).expect("link should succeed");

        let out = crate::store::memory_graph::outgoing(&ctx.conn, "a", None)
            .expect("outgoing should succeed");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_ref, "b");
        assert_eq!(out[0].relation, "supports");
        assert_eq!(out[0].target_kind, "memory");
    }

    #[test]
    fn test_memory_link_invalid_relation_lists_closed_set() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        handle_memory_add(
            &ctx, "a", "Title", "topic", None, "content", None, None, None, None,
        )
        .expect("add should succeed");

        let err = handle_memory_link(&ctx, "a", "mentions", "b", false, None)
            .expect_err("invalid relation must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("supports"),
            "error must list the closed set: {msg}"
        );
        assert!(
            msg.contains("relates_to"),
            "error must list the closed set: {msg}"
        );
    }

    #[test]
    fn test_memory_link_missing_source_errors() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        let err = handle_memory_link(&ctx, "ghost", "supports", "b", false, None)
            .expect_err("missing source must be rejected");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn test_memory_link_records_agent_provenance() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        handle_memory_add(
            &ctx, "a", "Title", "topic", None, "content", None, None, None, None,
        )
        .expect("add should succeed");

        handle_memory_link(&ctx, "a", "relates_to", "b", false, Some("scout"))
            .expect("link with agent should succeed");

        let (_, agent) = memory::get_provenance(&ctx.conn, "a").expect("provenance read");
        assert_eq!(agent.as_deref(), Some("scout"));
    }

    #[test]
    fn test_memory_link_doc_target_kind() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        handle_memory_add(
            &ctx, "a", "Title", "topic", None, "content", None, None, None, None,
        )
        .expect("add should succeed");

        handle_memory_link(&ctx, "a", "derived_from", "docs/spec.md", true, None)
            .expect("doc link should succeed");

        let out = crate::store::memory_graph::outgoing(&ctx.conn, "a", None)
            .expect("outgoing should succeed");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_kind, "doc");
    }

    #[test]
    fn test_memory_add_existing_id_upserts() {
        // Writing the same id twice must update in place, not fail with a
        // UNIQUE constraint violation (the CLI/bridge memory-write path).
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        handle_memory_add(
            &ctx,
            "dup-id",
            "First",
            "topic",
            None,
            "original content",
            None,
            None,
            None,
            None,
        )
        .expect("first write should succeed");

        // Second write with the same id — must not error.
        handle_memory_add(
            &ctx,
            "dup-id",
            "Second",
            "decision",
            Some("a,b"),
            "updated content",
            None,
            None,
            None,
            None,
        )
        .expect("re-writing an existing id must upsert, not fail");

        // Still a single entry, with the updated fields.
        let entry = handle_memory_show(&ctx, "dup-id")
            .expect("show should succeed")
            .expect("entry should exist");
        assert_eq!(entry.title, "Second");
        assert_eq!(entry.content, "updated content");
        let count: i64 = ctx
            .conn
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "upsert must not create a duplicate row");
    }

    #[test]
    fn test_memory_rm_updates_index_json() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        // Add an entry
        handle_memory_add(
            &ctx,
            "to-delete",
            "To Delete",
            "topic",
            None,
            "Content",
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // Verify it exists
        let index = load_memory_index(&ctx).unwrap().unwrap();
        assert_eq!(index.entries.len(), 1);

        // Delete it
        handle_memory_rm(&ctx, "to-delete").expect("rm should succeed");

        // index.json should be updated (empty now)
        let index = load_memory_index(&ctx).unwrap().unwrap();
        assert_eq!(index.entries.len(), 0);

        // Entry should be in archive
        let archive_path = temp.path().join(".mdkb/memory/archive/to-delete.md");
        assert!(archive_path.exists(), "entry should be archived");
    }

    #[test]
    fn test_handle_init_fails_if_already_initialized() {
        let temp = setup_temp_dir();

        handle_init(temp.path()).expect("first init should succeed");
        let result = handle_init(temp.path());

        assert!(result.is_err());
    }

    #[test]
    fn test_context_open_fails_if_not_initialized() {
        let temp = setup_temp_dir();

        let result = Context::open(temp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_context_open_succeeds_after_init() {
        let temp = setup_temp_dir();

        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        assert!(ctx.db_path.exists());
    }

    #[test]
    fn test_context_open_creates_vec_memory_on_legacy_db() {
        use rusqlite::Connection;

        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();

        // Simulate a legacy DB by dropping vec_memory
        {
            let db_path = temp.path().join(".mdkb/index.sqlite");
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("DROP TABLE IF EXISTS vec_memory")
                .unwrap();

            // Verify it's gone
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_memory')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!exists, "vec_memory should be dropped");
        }

        // Re-open — should recreate vec_memory
        let ctx = Context::open(temp.path()).expect("open should succeed");
        let exists: bool = ctx
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_memory')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "vec_memory should be recreated by Context::open");
    }

    // ==================== Collection Tests ====================

    #[test]
    fn test_handle_collection_add() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").expect("add should succeed");

        let collections = handle_collection_list(&ctx).unwrap();
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].name, "docs");
    }

    #[test]
    fn test_graph_edges_indexed_dangling_and_idempotent() {
        use crate::store::graph;

        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        std::fs::create_dir_all(temp.path().join("docs")).unwrap();
        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();

        let body = "---\nowner: alice\nthemes:\n  - growth\n---\nSee [[notes/related]].\n";
        std::fs::write(temp.path().join("docs/x.md"), body).unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        let doc = documents::get_document_by_path(&ctx.conn, "docs", "x.md")
            .unwrap()
            .expect("x.md should be indexed");

        // Frontmatter (strong) + wikilink (soft) edges, target refs stored verbatim.
        let out = graph::get_outgoing(&ctx.conn, doc.id, None).unwrap();
        assert_eq!(out.len(), 3, "owner + themes + wikilink");
        assert!(
            out.iter()
                .any(|e| e.relation == "owner" && e.target_ref == "alice")
        );
        assert!(
            out.iter()
                .any(|e| e.relation == "themes" && e.target_ref == "growth")
        );
        assert!(
            out.iter()
                .any(|e| e.source_kind == graph::KIND_WIKILINK && e.target_ref == "notes/related")
        );

        // 'alice' is dangling: no document yet, but the backlink exists.
        assert!(
            graph::resolve_ref_to_doc(&ctx.conn, "alice")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            graph::get_incoming(&ctx.conn, "alice", None).unwrap().len(),
            1
        );

        // Re-running the hook (simulating re-index) must not duplicate edges.
        let parsed = parse_frontmatter(body);
        process_graph_edges(
            &ctx.conn,
            doc.id,
            &parsed,
            &crate::config::GraphConfig::default(),
        );
        let after = graph::get_outgoing(&ctx.conn, doc.id, None).unwrap();
        assert_eq!(after.len(), 3, "re-index must be idempotent");

        // Index alice.md later: the previously dangling edge now resolves.
        std::fs::write(temp.path().join("docs/alice.md"), "# Alice\n").unwrap();
        handle_update(&ctx, temp.path()).unwrap();
        assert!(
            graph::resolve_ref_to_doc(&ctx.conn, "alice")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_update_force_reindexes_unchanged_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        std::fs::create_dir_all(temp.path().join("docs")).unwrap();
        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        std::fs::write(
            temp.path().join("docs/a.md"),
            "---\nowner: alice\n---\nbody\n",
        )
        .unwrap();

        let r1 = handle_update(&ctx, temp.path()).unwrap();
        assert_eq!(r1.added, 1);

        // Plain update: unchanged mtime → skipped, not reprocessed.
        let r2 = handle_update(&ctx, temp.path()).unwrap();
        assert_eq!(r2.unchanged, 1);
        assert_eq!(r2.updated, 0);

        // Forced update: reprocessed despite unchanged mtime (so config changes apply).
        let r3 = handle_update_force(&ctx, temp.path(), true).unwrap();
        assert_eq!(r3.updated, 1, "force must reprocess unchanged files");
        assert_eq!(r3.unchanged, 0);

        // Edges remain intact after the forced re-index (idempotent).
        let doc = documents::get_document_by_path(&ctx.conn, "docs", "a.md")
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::store::graph::get_outgoing(&ctx.conn, doc.id, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_update_docs_indexed_counts_docs_on_doc_only_store() {
        // Regression for the "Files discovered: 0" lie: on a doc-only store the
        // update summary must report the real doc total, not the code delta.
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        std::fs::create_dir_all(temp.path().join("docs")).unwrap();
        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            std::fs::write(temp.path().join("docs").join(name), "# t\nbody\n").unwrap();
        }

        let r1 = handle_update(&ctx, temp.path()).unwrap();
        assert_eq!(r1.added, 3);
        assert_eq!(r1.docs_indexed(), 3, "first run: 3 new docs indexed");

        // Re-run with no changes: delta is 0/0 but the honest total stays 3.
        let r2 = handle_update(&ctx, temp.path()).unwrap();
        assert_eq!(r2.added, 0);
        assert_eq!(r2.unchanged, 3);
        assert_eq!(
            r2.docs_indexed(),
            3,
            "unchanged re-run still reports 3 docs indexed, not 0"
        );
    }

    #[test]
    fn test_handle_collection_add_duplicate_is_idempotent() {
        // Re-adding the same collection name upserts in place (no error): this keeps
        // an explicit `collection add` from racing the served process's convention
        // auto-registration of the same name.
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        handle_collection_add(&ctx, "docs", "./other", "**/*.mdx")
            .expect("re-add must be idempotent, not error");

        let all = crate::store::collections::list_collections(&ctx.conn).unwrap();
        assert_eq!(all.len(), 1, "upsert must not create a duplicate row");
        let got = crate::store::collections::get_collection(&ctx.conn, "docs")
            .unwrap()
            .unwrap();
        assert_eq!(got.path, "./other", "path should update on re-add");
        assert_eq!(got.pattern, "**/*.mdx", "pattern should update on re-add");
    }

    #[test]
    fn test_handle_collection_add_rejects_invalid_name() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let too_long = "x".repeat(101);
        let cases = vec![
            ("My Docs", "spaces"),
            ("UPPER", "uppercase"),
            ("has/slash", "slash"),
            ("ctrl\x00char", "null byte"),
            ("", "empty"),
            (too_long.as_str(), "too long"),
        ];
        for (name, label) in cases {
            let result = handle_collection_add(&ctx, name, "./docs", "**/*.md");
            assert!(result.is_err(), "should reject {label}: {name:?}");
        }
    }

    #[test]
    fn test_handle_collection_add_accepts_valid_names() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        for name in &["docs", "my-docs", "my_docs", "docs123", "a"] {
            // Remove if exists, to allow re-adding
            let _ = handle_collection_remove(&ctx, name);
            handle_collection_add(&ctx, name, "./docs", "**/*.md")
                .unwrap_or_else(|e| panic!("should accept {name:?}: {e}"));
        }
    }

    #[test]
    fn test_handle_collection_rename_validates_new_name() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        let result = handle_collection_rename(&ctx, "docs", "Bad Name!");
        assert!(result.is_err(), "rename should reject invalid new name");
    }

    #[test]
    fn test_handle_collection_add_blocks_dotdot_traversal() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let result = handle_collection_add(&ctx, "evil", "../secret", "**/*");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("path traversal") || msg.contains("escapes root"));
    }

    #[test]
    fn test_handle_collection_add_blocks_absolute_path_outside_root() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let result = handle_collection_add(&ctx, "evil", "/etc", "**/*");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("escapes root"));
    }

    #[cfg(unix)]
    #[test]
    fn test_handle_collection_add_blocks_symlink_escape() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a symlink inside the project that points outside
        let link_path = temp.path().join("sneaky");
        std::os::unix::fs::symlink("/tmp", &link_path).unwrap();

        let result = handle_collection_add(&ctx, "evil", "sneaky", "**/*");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("escapes root"));
    }

    #[test]
    fn test_handle_collection_add_allows_valid_relative_path() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Non-existent but valid relative path — should succeed (best-effort)
        handle_collection_add(&ctx, "notes", "notes/drafts", "**/*.md")
            .expect("valid relative path should succeed");
    }

    #[test]
    fn test_handle_collection_add_allows_existing_subdir() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        std::fs::create_dir_all(temp.path().join("src/lib")).unwrap();
        handle_collection_add(&ctx, "src", "src/lib", "**/*.rs")
            .expect("existing subdir should succeed");
    }

    #[test]
    fn test_handle_collection_remove() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        let removed = handle_collection_remove(&ctx, "docs").unwrap();

        assert!(removed);
        let collections = handle_collection_list(&ctx).unwrap();
        assert!(collections.is_empty());
    }

    #[test]
    fn test_handle_collection_remove_nonexistent() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let removed = handle_collection_remove(&ctx, "nonexistent").unwrap();
        assert!(!removed);
    }

    #[test]
    fn test_handle_collection_list_empty() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let collections = handle_collection_list(&ctx).unwrap();
        assert!(collections.is_empty());
    }

    #[test]
    fn test_handle_collection_rename() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "old", "./path", "**/*.md").unwrap();
        handle_collection_rename(&ctx, "old", "new").expect("rename should succeed");

        let collections = handle_collection_list(&ctx).unwrap();
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].name, "new");
    }

    // ==================== Search Tests ====================

    #[test]
    fn test_handle_search_empty_index() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let results = handle_search(&ctx, "test", 10, None).unwrap();
        assert!(results.is_empty());
    }

    // ==================== Line Range Tests ====================

    #[test]
    fn test_apply_line_range_basic() {
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";

        let result = apply_line_range(content, "2:4").unwrap();
        assert_eq!(result, "line 2\nline 3\nline 4");
    }

    #[test]
    fn test_apply_line_range_single_line() {
        let content = "line 1\nline 2\nline 3";

        let result = apply_line_range(content, "2:2").unwrap();
        assert_eq!(result, "line 2");
    }

    #[test]
    fn test_apply_line_range_beyond_end() {
        let content = "line 1\nline 2";

        let result = apply_line_range(content, "1:100").unwrap();
        assert_eq!(result, "line 1\nline 2");
    }

    #[test]
    fn test_apply_line_range_invalid_format() {
        let result = apply_line_range("content", "invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_line_range_zero_start() {
        let result = apply_line_range("content", "0:5");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_line_range_end_before_start() {
        let result = apply_line_range("content", "5:2");
        assert!(result.is_err());
    }

    // ==================== Update Tests ====================

    #[test]
    fn test_handle_update_empty_collections() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.removed, 0);
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn test_handle_update_indexes_new_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a docs directory with a markdown file
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Test\n\nContent").unwrap();

        // Add collection
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 1);
        assert_eq!(result.updated, 0);
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn test_handle_update_skips_unchanged_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a docs directory with a markdown file
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Test\n\nContent").unwrap();

        // Add collection and index
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Run update again - should skip unchanged
        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.unchanged, 1);
    }

    #[test]
    fn test_handle_update_reindexes_modified_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a docs directory with a markdown file
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Test\n\nOld Content").unwrap();

        // Add collection and index
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Wait for filesystem mtime granularity (most systems have 1-second precision)
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Modify the file to update mtime
        let new_content = "# Test\n\nNew Content - Modified";
        std::fs::write(&file_path, new_content).unwrap();

        // Run update again - should detect modification
        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 1);
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn test_handle_update_removes_deleted_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a docs directory with a markdown file
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Test\n\nContent").unwrap();

        // Add collection and index
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Delete the file
        std::fs::remove_file(&file_path).unwrap();

        // Run update - should detect deletion
        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.removed, 1);
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn test_handle_update_multiple_collections() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create two directories
        let docs_dir = temp.path().join("docs");
        let notes_dir = temp.path().join("notes");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::create_dir(&notes_dir).unwrap();
        std::fs::write(docs_dir.join("doc.md"), "# Doc").unwrap();
        std::fs::write(notes_dir.join("note.md"), "# Note").unwrap();

        // Add two collections
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_collection_add(&ctx, "notes", "notes", "**/*.md").unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 2);
    }

    #[test]
    fn test_handle_update_indexes_gitignored_collection() {
        let temp = setup_temp_dir();

        // Initialize git repo so .gitignore is respected by git-aware tools
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .expect("git init");

        // Create stories/ dir with content, then gitignore it
        let stories_dir = temp.path().join("stories");
        std::fs::create_dir(&stories_dir).unwrap();
        std::fs::write(
            stories_dir.join("001-done.md"),
            "# Story 1\n\nCompleted work",
        )
        .unwrap();
        std::fs::write(stories_dir.join("002-done.md"), "# Story 2\n\nMore work").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "stories/\n").unwrap();

        // Init mdkb and manually register collection (as wiz would do)
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "stories", "stories", "**/*.md").unwrap();

        // Update should index both files despite gitignore
        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(
            result.added, 2,
            "gitignored directory should still be indexed by collection walker"
        );
    }

    #[test]
    fn test_handle_update_respects_gitignore_when_opted_in() {
        let temp = setup_temp_dir();

        // Use a non-conventional directory name so apply_conventions won't
        // auto-register it and clash with our explicit collection add.
        let dir = temp.path().join("knowledge");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep.md"), "# keep").unwrap();
        std::fs::write(dir.join("drop.md"), "# drop").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "knowledge/drop.md\n").unwrap();

        handle_init(temp.path()).unwrap();

        // Opt into gitignore for document indexing
        let config_path = temp.path().join(".mdkb/config.toml");
        let toml = "[indexing]\nrespect_gitignore = true\n";
        std::fs::write(&config_path, toml).unwrap();

        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "knowledge", "knowledge", "**/*.md").unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(
            result.added, 1,
            "knowledge/drop.md should be excluded when respect_gitignore=true"
        );
    }

    #[test]
    fn test_handle_update_mdkbignore_excludes_collection_files() {
        let temp = setup_temp_dir();

        let dir = temp.path().join("knowledge");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep.md"), "# keep").unwrap();
        std::fs::write(dir.join("draft.md"), "# draft").unwrap();

        // .mdkbignore excludes draft.md; collection glob still matches *.md.
        // Default respect_gitignore=false makes the doc walker read .mdkbignore.
        std::fs::write(temp.path().join(".mdkbignore"), "knowledge/draft.md\n").unwrap();

        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "knowledge", "knowledge", "**/*.md").unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 1, ".mdkbignore entry should exclude draft.md");
    }

    // ==================== Update Files Tests ====================

    #[test]
    fn test_handle_update_files_indexes_specific_file() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create docs directory with two markdown files
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("a.md"), "# File A\n\nContent A").unwrap();
        std::fs::write(docs_dir.join("b.md"), "# File B\n\nContent B").unwrap();

        // Add collection
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Update only file A
        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[docs_dir.join("a.md").to_string_lossy().to_string()],
        )
        .expect("update_files should succeed");
        assert_eq!(result.added, 1, "should index only file A");
        assert_eq!(result.updated, 0);

        // File B should not be indexed
        let doc_b = documents::get_document_by_path(&ctx.conn, "docs", "b.md").unwrap();
        assert!(doc_b.is_none(), "file B should not be in index");
    }

    #[test]
    fn test_handle_update_files_updates_existing() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Old\n\nOld content").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Wait for mtime granularity
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Modify file
        std::fs::write(&file_path, "# New\n\nNew content").unwrap();

        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .expect("update_files should succeed");
        assert_eq!(result.updated, 1, "should update modified file");
        assert_eq!(result.added, 0);
    }

    #[test]
    fn test_handle_update_files_skips_file_outside_collections() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Test").unwrap();

        // Add collection for docs/
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Try to update a file outside any collection
        let other = temp.path().join("other.md");
        std::fs::write(&other, "# Other").unwrap();

        let result = handle_update_files(&ctx, temp.path(), &[other.to_string_lossy().to_string()])
            .expect("update_files should succeed");
        assert_eq!(
            result.added, 0,
            "file outside collections should be skipped"
        );
        assert_eq!(result.errors.len(), 0, "not an error, just skipped");
    }

    #[test]
    fn test_handle_update_files_with_relative_paths() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Test\n\nContent").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Use relative path
        let result = handle_update_files(&ctx, temp.path(), &["docs/readme.md".to_string()])
            .expect("update_files should succeed");
        assert_eq!(result.added, 1, "should index file via relative path");

        // Verify stored path is the canonical relative form
        let doc = documents::get_document_by_path(&ctx.conn, "docs", "readme.md").unwrap();
        assert!(
            doc.is_some(),
            "should be retrievable by canonical relative path"
        );
    }

    #[test]
    fn test_handle_update_files_file_not_found() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        let nonexistent = docs_dir.join("does_not_exist.md");
        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[nonexistent.to_string_lossy().to_string()],
        )
        .expect("should succeed overall");
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("Cannot resolve"));
        assert_eq!(result.added, 0);
    }

    #[test]
    fn test_handle_update_files_continues_after_bad_path() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("good.md"), "# Good\n\nContent").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // First path is bad, second is good — both should be processed
        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[
                "/tmp/nonexistent_xyz.md".to_string(),
                docs_dir.join("good.md").to_string_lossy().to_string(),
            ],
        )
        .expect("should succeed overall");
        assert_eq!(result.errors.len(), 1, "bad path should produce one error");
        assert_eq!(result.added, 1, "good path should still be indexed");
    }

    #[test]
    fn test_handle_update_files_skips_unchanged() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Test\n\nContent").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Index once
        let r1 = handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .unwrap();
        assert_eq!(r1.added, 1);

        // Call again immediately — mtime hasn't changed
        let r2 = handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .unwrap();
        assert_eq!(r2.unchanged, 1);
        assert_eq!(r2.updated, 0);
        assert_eq!(r2.added, 0);
    }

    #[test]
    fn test_handle_update_files_blocks_path_traversal() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Try to index a file outside the project root
        let result = handle_update_files(&ctx, temp.path(), &["/etc/hosts".to_string()])
            .expect("should succeed overall");
        assert_eq!(result.added, 0, "file outside root should not be indexed");
        assert!(
            result.errors.iter().any(|e| e.contains("path traversal")),
            "should report path traversal error"
        );
    }

    #[test]
    fn test_handle_update_files_updates_verifies_content() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Old Title\n\nOld content").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .unwrap();

        std::thread::sleep(std::time::Duration::from_secs(1));
        std::fs::write(&file_path, "# New Title\n\nNew content").unwrap();

        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .unwrap();
        assert_eq!(result.updated, 1);

        // Verify stored content was actually updated
        let doc = documents::get_document_by_path(&ctx.conn, "docs", "readme.md")
            .unwrap()
            .expect("doc should exist");
        assert_eq!(doc.title, Some("New Title".to_string()));
    }

    // ==================== Mget Tests ====================

    #[test]
    fn test_handle_mget_empty_index() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let results = handle_mget(&ctx, "**/*.md", None).expect("mget should succeed");
        assert!(results.is_empty());
    }

    #[test]
    fn test_handle_mget_matches_pattern() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create and index docs
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# README").unwrap();
        std::fs::write(docs_dir.join("guide.md"), "# Guide").unwrap();
        std::fs::write(docs_dir.join("notes.txt"), "Notes").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Pattern matches only .md files
        let results = handle_mget(&ctx, "*.md", None).expect("mget should succeed");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_handle_mget_with_collection_filter() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create two collections
        let docs_dir = temp.path().join("docs");
        let notes_dir = temp.path().join("notes");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::create_dir(&notes_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Doc README").unwrap();
        std::fs::write(notes_dir.join("readme.md"), "# Note README").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_collection_add(&ctx, "notes", "notes", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Filter to only docs collection
        let results = handle_mget(&ctx, "*.md", Some("docs")).expect("mget should succeed");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.collection, "docs");
    }

    #[test]
    fn test_handle_mget_nested_pattern() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create nested structure
        let docs_dir = temp.path().join("docs");
        let sub_dir = docs_dir.join("api");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# README").unwrap();
        std::fs::write(sub_dir.join("endpoints.md"), "# API").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Pattern matches nested files
        let results = handle_mget(&ctx, "api/*.md", None).expect("mget should succeed");
        assert_eq!(results.len(), 1);
        assert!(results[0].0.relative_path.contains("api"));
    }

    #[test]
    fn test_handle_mget_returns_content() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Hello World\n\nContent here.").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        let results = handle_mget(&ctx, "*.md", None).expect("mget should succeed");
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("Hello World"));
    }

    #[test]
    fn root_collection_glob_is_non_recursive() {
        // The `_root` convention collection uses pattern "*.md" with the explicit
        // intent (conventions.rs) of matching ONLY root-level markdown. globset's
        // default lets `*` cross `/`, which made "*.md" swallow the whole repo and
        // duplicate every other collection's docs. Indexing must treat `*` as
        // non-separator-crossing.
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        std::fs::write(temp.path().join("README.md"), "# Root").unwrap();
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("guide.md"), "# Nested").unwrap();

        handle_collection_add(&ctx, "_root", ".", "*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        let paths: Vec<String> = documents::list_documents(&ctx.conn, "_root")
            .unwrap()
            .into_iter()
            .map(|d| d.relative_path)
            .collect();

        assert!(
            paths.iter().any(|p| p == "README.md"),
            "root-level README.md must be indexed in _root: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.contains('/')),
            "_root '*.md' must be non-recursive — nested files leaked: {paths:?}"
        );
    }

    #[test]
    fn recursive_glob_still_matches_nested() {
        // Regression guard: making `*` non-recursive must NOT break `**`, which is
        // the explicit recursive token used by docs/stories/plans/reviews.
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        let sub = docs_dir.join("api");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# R").unwrap();
        std::fs::write(sub.join("endpoints.md"), "# E").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        let paths: Vec<String> = documents::list_documents(&ctx.conn, "docs")
            .unwrap()
            .into_iter()
            .map(|d| d.relative_path)
            .collect();

        assert!(
            paths.iter().any(|p| p == "readme.md"),
            "root-level doc must stay indexed: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("api")),
            "nested doc must stay indexed under '**': {paths:?}"
        );
    }

    // ==================== Evolution Frontmatter Tests ====================

    #[test]
    fn test_frontmatter_supersedes_creates_evolution() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        // Create old document first
        std::fs::write(
            docs_dir.join("api-v1.md"),
            "---\ntitle: API v1\n---\n\n# API v1\n\nOld API docs.",
        )
        .unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Create new document that supersedes the old one
        std::fs::write(
            docs_dir.join("api-v2.md"),
            "---\ntitle: API v2\nsupersedes:\n  - path: \"api-v1.md\"\n    reason: \"Complete redesign\"\n---\n\n# API v2\n\nNew API.",
        )
        .unwrap();

        // Re-index
        handle_update(&ctx, temp.path()).unwrap();

        // Get the new document and check its evolution chain
        let v2_doc = documents::get_document_by_path(&ctx.conn, "docs", "api-v2.md")
            .unwrap()
            .expect("v2 should exist");

        let chain = evolution::get_evolution_chain(&ctx.conn, v2_doc.id).unwrap();
        assert_eq!(chain.len(), 1, "should have one evolution relationship");
        assert_eq!(chain[0].relationship, RelationshipType::Supersedes);
        assert_eq!(chain[0].reason, Some("Complete redesign".to_string()));

        // Check the old document is marked as superseded
        let (status, _) = evolution::get_document_status(&ctx.conn, chain[0].target_doc_id)
            .unwrap()
            .unwrap();
        assert_eq!(status, evolution::DocumentStatus::Superseded);
    }

    #[test]
    fn test_frontmatter_updates_with_scope() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        // Create base document
        std::fs::write(
            docs_dir.join("security.md"),
            "---\ntitle: Security Guide\n---\n\n# Security\n\nSecurity info.",
        )
        .unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Create update document
        std::fs::write(
            docs_dir.join("security-jwt.md"),
            "---\ntitle: JWT Update\nupdates:\n  - path: \"security.md\"\n    scope: \"Token Handling\"\n    reason: \"JWT support\"\n---\n\nJWT info.",
        )
        .unwrap();

        handle_update(&ctx, temp.path()).unwrap();

        let jwt_doc = documents::get_document_by_path(&ctx.conn, "docs", "security-jwt.md")
            .unwrap()
            .expect("jwt doc should exist");

        let chain = evolution::get_evolution_chain(&ctx.conn, jwt_doc.id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].relationship, RelationshipType::Updates);
        assert_eq!(chain[0].scope, Some("Token Handling".to_string()));

        // Original document should still be current (updates don't supersede)
        let (status, _) = evolution::get_document_status(&ctx.conn, chain[0].target_doc_id)
            .unwrap()
            .unwrap();
        assert_eq!(status, evolution::DocumentStatus::Current);
    }

    #[test]
    fn test_frontmatter_evolution_invalid_path_warns() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        // Create document that references non-existent file
        std::fs::write(
            docs_dir.join("new.md"),
            "---\ntitle: New Doc\nsupersedes:\n  - \"nonexistent.md\"\n---\n\nContent.",
        )
        .unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Should not fail, just warn
        let result = handle_update(&ctx, temp.path());
        assert!(
            result.is_ok(),
            "update should succeed even with invalid reference"
        );

        // Document should be indexed
        let doc = documents::get_document_by_path(&ctx.conn, "docs", "new.md")
            .unwrap()
            .expect("doc should exist");

        // But no evolution chain
        let chain = evolution::get_evolution_chain(&ctx.conn, doc.id).unwrap();
        assert!(chain.is_empty(), "no relationships for invalid references");
    }

    #[test]
    fn test_frontmatter_simple_string_supersedes() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        // Create old document
        std::fs::write(
            docs_dir.join("old.md"),
            "---\ntitle: Old\n---\n\nOld content.",
        )
        .unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Create new document with simple string supersedes
        std::fs::write(
            docs_dir.join("new.md"),
            "---\ntitle: New\nsupersedes: \"old.md\"\n---\n\nNew content.",
        )
        .unwrap();

        handle_update(&ctx, temp.path()).unwrap();

        let new_doc = documents::get_document_by_path(&ctx.conn, "docs", "new.md")
            .unwrap()
            .expect("new doc should exist");

        let chain = evolution::get_evolution_chain(&ctx.conn, new_doc.id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].relationship, RelationshipType::Supersedes);
    }

    // ==================== Session Indexing Tests ====================

    /// Build a minimal valid JSONL session file with the given number of user turns.
    fn make_session_jsonl(session_id: &str, user_turns: usize) -> String {
        let mut lines = Vec::new();
        for i in 0..user_turns {
            lines.push(format!(
                r#"{{"type":"user","sessionId":"{}","message":{{"content":"User message {}"}}}}"#,
                session_id, i
            ));
            lines.push(format!(
                r#"{{"type":"assistant","sessionId":"{}","message":{{"content":"Assistant reply {}"}}}}"#,
                session_id, i
            ));
        }
        lines.join("\n")
    }

    #[test]
    fn test_handle_session_index_creates_collection_and_indexes() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Build fake session directory matching encode_project_path for temp path
        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();

        // Write a session file with enough turns
        std::fs::write(
            session_dir.join("abc-123.jsonl"),
            make_session_jsonl("abc-123", 4),
        )
        .unwrap();

        let result =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();

        assert!(result.added > 0);
        assert_eq!(result.errors.len(), 0);

        // Collection should exist
        let coll =
            collections::get_collection(&ctx.conn, crate::domain::COLLECTION_CLAUDE_SESSIONS)
                .unwrap();
        assert!(coll.is_some());

        // Document should be searchable
        let docs = documents::list_documents(&ctx.conn, crate::domain::COLLECTION_CLAUDE_SESSIONS)
            .unwrap();
        assert!(!docs.is_empty());
    }

    #[test]
    fn test_handle_session_index_skips_short_sessions() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();

        // Only 2 user turns — below min_turns=3
        std::fs::write(
            session_dir.join("short-session.jsonl"),
            make_session_jsonl("short-session", 2),
        )
        .unwrap();

        let result =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();

        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
    }

    #[test]
    fn test_handle_session_index_dedup_unchanged_content() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();

        std::fs::write(
            session_dir.join("dedup-test.jsonl"),
            make_session_jsonl("dedup-test", 4),
        )
        .unwrap();

        // First index
        let r1 =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();
        assert!(r1.added > 0);

        // Second index without modification — content hash matches, so skip.
        let r2 =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();
        assert_eq!(r2.added, 0);
        assert!(r2.unchanged > 0);
    }

    /// Regression for story 036: an append-only transcript must re-embed only the
    /// grown tail, not its whole body. Chunk keys are stable from turn 0, so after
    /// an append the earlier chunks are byte-identical and MUST be skipped by
    /// content hash — even though the file mtime bumped. The prior mtime-based
    /// dedup skipped nothing whenever the mtime changed, re-embedding the entire
    /// multi-MB file on every growth (the leak/CPU driver this story fixes).
    #[test]
    fn test_handle_session_index_append_skips_unchanged_chunks_despite_mtime_bump() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();
        let file = session_dir.join("grow.jsonl");

        // 8 user turns = 16 records → multiple stable chunks (size 10, stride 8):
        // chunk-000 = records[0..10], chunk-001 = records[8..16].
        std::fs::write(&file, make_session_jsonl("grow", 8)).unwrap();
        let r1 =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();
        assert!(r1.added > 0, "first index writes the initial chunks");

        // Append more turns. The content is deterministic per turn index, so the
        // first 16 records are byte-identical → chunk-000 (records[0..10]) is
        // unchanged; the tail grows and adds new chunks.
        std::fs::write(&file, make_session_jsonl("grow", 14)).unwrap();
        // Force mtime forward so the OLD mtime-dedup would reprocess EVERY chunk;
        // content-hash dedup must still skip the identical earlier chunk(s).
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_mins(2);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(bumped)
            .unwrap();

        let r2 =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();

        assert!(
            r2.unchanged > 0,
            "byte-identical earlier chunks MUST be skipped by content hash despite \
             the bumped mtime (mtime dedup would give unchanged=0, re-embedding all)"
        );
        assert!(
            r2.added > 0,
            "the appended tail introduces at least one new chunk"
        );
    }

    /// Story 036 AC#3 — quantified before/after: on a large session, an append
    /// re-processes only the tail, not the whole body. `unchanged` (skipped, zero
    /// embedding cost) must dominate `added + updated` (re-embedded). Under the old
    /// mtime dedup, an mtime-bumping append gave unchanged=0 and reprocessed ALL
    /// chunks — the cost this story removes.
    #[test]
    fn test_handle_session_index_append_cost_is_delta_bounded() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();
        let file = session_dir.join("big.jsonl");

        // 50 user turns = 100 records → ~12 chunks (size 10, stride 8).
        std::fs::write(&file, make_session_jsonl("big", 50)).unwrap();
        handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();

        // Append 2 turns; force mtime forward (worst case for the old dedup).
        std::fs::write(&file, make_session_jsonl("big", 52)).unwrap();
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_mins(2);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(bumped)
            .unwrap();

        let r = handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();
        let reprocessed = r.added + r.updated;
        assert!(
            r.unchanged >= 3 * reprocessed,
            "append must skip the bulk of chunks: unchanged={} should dominate \
             reprocessed(added+updated)={} (old mtime dedup: unchanged=0)",
            r.unchanged,
            reprocessed
        );
    }

    #[test]
    fn test_handle_session_index_no_session_dir() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions_base).unwrap();

        let result =
            handle_session_index(&ctx, &sessions_base, "/nonexistent/project/path").unwrap();

        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.unchanged, 0);
    }

    // ==================== Memory Import Tests ====================

    fn write_import_json(dir: &std::path::Path, content: &str) -> String {
        let path = dir.join("import.json");
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn test_memory_import_basic() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let json = r#"{"entries": [
            {"id": "test-import", "title": "Test Import", "content": "Some content", "entryType": "decision", "tags": ["db"], "sourceType": "auto_extracted"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.skipped, 0);
        assert!(result.errors.is_empty());

        let entry = handle_memory_show(&ctx, "test-import").unwrap().unwrap();
        assert_eq!(entry.title, "Test Import");
        assert_eq!(entry.source_type, memory::SourceType::AutoExtracted);
    }

    #[test]
    fn test_memory_import_dry_run() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let json = r#"{"entries": [
            {"id": "dry-run-entry", "title": "Dry Run", "content": "Content"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, true, false).unwrap();
        assert_eq!(result.imported, 1);

        // Entry should NOT exist in DB
        let entry = handle_memory_show(&ctx, "dry-run-entry").unwrap();
        assert!(entry.is_none(), "dry-run should not create entries");
    }

    #[test]
    fn test_memory_import_skip_duplicates() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Add an entry first
        handle_memory_add(
            &ctx, "existing", "Existing", "topic", None, "Content", None, None, None, None,
        )
        .unwrap();

        let json = r#"{"entries": [
            {"id": "existing", "title": "Duplicate", "content": "Content"},
            {"id": "new-one", "title": "New One", "content": "Content"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, true).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.skipped, 1);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_memory_import_duplicate_warns() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_memory_add(
            &ctx, "existing", "Existing", "topic", None, "Content", None, None, None, None,
        )
        .unwrap();

        let json = r#"{"entries": [
            {"id": "existing", "title": "Duplicate", "content": "Content"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("already exists"));
    }

    #[test]
    fn test_memory_import_empty_entries() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let json = r#"{"entries": []}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_memory_import_malformed_json() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let path = write_import_json(temp.path(), "not json at all");

        let result = handle_memory_import(&ctx, &path, false, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_memory_import_defaults() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Minimal JSON — only required fields
        let json = r#"{"entries": [
            {"id": "minimal", "title": "Minimal Entry", "content": "Content"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 1);

        let entry = handle_memory_show(&ctx, "minimal").unwrap().unwrap();
        assert_eq!(entry.source_type, memory::SourceType::UserStatement);
        assert_eq!(entry.entry_type, EntryType::Topic);
        assert!(entry.tags.is_empty());
    }

    #[test]
    fn test_memory_import_invalid_entry_type() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let json = r#"{"entries": [
            {"id": "bad-type", "title": "Bad Type", "content": "Content", "entryType": "invalid"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 0);
        assert_eq!(result.errors.len(), 1);
    }

    /// Atomicity: when two entries share the same ID the second INSERT violates the UNIQUE
    /// constraint, causing the transaction to roll back so that *neither* entry lands in the DB.
    #[test]
    fn test_memory_import_atomic_rollback_on_duplicate_id() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Both entries share the same id.  Phase-1 sees neither in the DB (no pre-existing rows),
        // so both are queued for insert.  Phase-2 inserts "dup-id" successfully, then hits a
        // UNIQUE constraint on the second insert, triggering a rollback of the whole transaction.
        let json = r#"{"entries": [
            {"id": "dup-id", "title": "First",  "content": "Content A"},
            {"id": "dup-id", "title": "Second", "content": "Content B"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false);

        // The function must return Err (UNIQUE constraint propagated via `?`).
        assert!(
            result.is_err(),
            "expected Err from UNIQUE constraint violation"
        );

        // Neither entry must survive the rollback.
        let entry = handle_memory_show(&ctx, "dup-id").unwrap();
        assert!(
            entry.is_none(),
            "rollback must have removed the first insert"
        );
    }
}

// ==================== Experiment Handlers ====================

// ============================================================================
// Journal Import Handlers
// ============================================================================

// ---------------------------------------------------------------------------
// Session indexing handlers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Code intelligence handlers
// ---------------------------------------------------------------------------
