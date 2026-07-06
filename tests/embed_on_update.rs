//! Integration tests for auto-embed-on-update (story 041).
//!
//! The `claude_sessions` exclusion is exercised with a *manually-created*
//! collection literally named `claude_sessions` — the scoping filter keys on the
//! collection name, so this faithfully tests the exclusion without needing the
//! full session-indexing machinery.
//!
//! Tests that assert a real embedding vector require the ~100MB ONNX model and
//! are `#[ignore]`d (run with `-- --ignored`). The config-default test is
//! model-free.

use std::path::PathBuf;

use mdkb::cli::handlers::{
    Context, handle_collection_add, handle_embed, handle_init, handle_update, handle_update_force,
};
use mdkb::store::{documents, vectors};
use tempfile::TempDir;

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

    /// Create a collection rooted at `<root>/<subdir>` with one markdown file.
    fn seed_collection(&self, name: &str, subdir: &str, file: &str, body: &str) {
        let dir = self.root.join(subdir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file), body).unwrap();
        handle_collection_add(&self.ctx, name, subdir, "**/*.md").expect("add collection");
    }

    /// True when the single doc in `collection` has a stored embedding.
    fn first_doc_embedded(&self, collection: &str) -> bool {
        let docs = documents::list_documents(&self.ctx.conn, collection).expect("list");
        let doc = docs.first().expect("collection has a doc");
        vectors::has_embedding(&self.ctx.conn, doc.id).expect("has_embedding")
    }
}

/// Nested-store boundary (story 052): a doc collection rooted at the repo must
/// not index markdown that lives inside a *sub-repo* owning its own `.mdkb`.
/// Model-free — asserts which documents get *indexed*, not embedded.
#[test]
fn update_prunes_docs_inside_nested_store() {
    let env = Env::new();
    std::fs::write(env.root.join("top.md"), "# Top\n\nRoot-owned doc.").unwrap();

    // A nested sub-repo with its own *initialized* store (`.mdkb/index.sqlite`)
    // owns its docs — prune the subtree. A bare `.mdkb` dir is not enough.
    std::fs::create_dir_all(env.root.join("child/.mdkb")).unwrap();
    std::fs::write(env.root.join("child/.mdkb/index.sqlite"), b"").unwrap();
    std::fs::write(env.root.join("child/nested.md"), "# Nested\n\nSub-repo doc.").unwrap();

    handle_collection_add(&env.ctx, "docs", ".", "**/*.md").expect("add collection");
    handle_update(&env.ctx, &env.root).expect("update");

    let paths: Vec<String> = documents::list_documents(&env.ctx.conn, "docs")
        .expect("list")
        .into_iter()
        .map(|d| d.relative_path)
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("top.md")),
        "root-owned doc must be indexed: {paths:?}"
    );
    assert!(
        !paths.iter().any(|p| p.ends_with("nested.md")),
        "doc inside a nested .mdkb store must be pruned: {paths:?}"
    );
}

#[test]
fn config_default_enables_doc_embed_disables_sessions() {
    let cfg = mdkb::Config::default();
    assert!(cfg.search.auto_embed_docs, "auto_embed_docs defaults on");
    assert!(
        !cfg.search.auto_embed_sessions,
        "auto_embed_sessions defaults off"
    );
    assert!(
        cfg.search.auto_embed_memory,
        "auto_embed_memory defaults on (kill switch for the ONNX cost / hermetic tests)"
    );
}

#[test]
#[ignore = "requires ONNX model download"]
fn update_embeds_docs_but_not_sessions() {
    let env = Env::new();
    env.seed_collection("docs", "docs", "a.md", "# Doc A\n\nSearchable content.");
    env.seed_collection(
        "claude_sessions",
        "sessions",
        "b.md",
        "# Session B\n\nTranscript content.",
    );

    let result = handle_update(&env.ctx, &env.root).expect("update");
    assert!(
        result.doc_embeddings_generated >= 1,
        "docs collection must be embedded"
    );

    assert!(env.first_doc_embedded("docs"), "docs doc must be embedded");
    assert!(
        !env.first_doc_embedded("claude_sessions"),
        "claude_sessions doc must NOT be embedded by default"
    );
}

#[test]
#[ignore = "requires ONNX model download"]
fn explicit_collection_flag_embeds_sessions() {
    let env = Env::new();
    env.seed_collection(
        "claude_sessions",
        "sessions",
        "b.md",
        "# Session B\n\nTranscript content.",
    );
    handle_update(&env.ctx, &env.root).expect("update");
    assert!(
        !env.first_doc_embedded("claude_sessions"),
        "not embedded by default update"
    );

    // Explicit --collection embeds it.
    let r = handle_embed(&env.ctx, Some("claude_sessions")).expect("explicit embed");
    assert!(r.generated >= 1);
    assert!(env.first_doc_embedded("claude_sessions"));
}

#[test]
#[ignore = "requires ONNX model download"]
fn unchanged_docs_are_not_reembedded() {
    let env = Env::new();
    env.seed_collection("docs", "docs", "a.md", "# Doc A\n\nSearchable content.");

    let first = handle_update(&env.ctx, &env.root).expect("first update");
    assert!(first.doc_embeddings_generated >= 1);

    // No file changes → hash gate skips everything → nothing re-embedded.
    let second = handle_update(&env.ctx, &env.root).expect("second update");
    assert_eq!(
        second.doc_embeddings_generated, 0,
        "unchanged docs must not be re-embedded"
    );
}

#[test]
#[ignore = "requires ONNX model download"]
fn changed_doc_is_reembedded() {
    let env = Env::new();
    env.seed_collection("docs", "docs", "a.md", "# Doc A\n\nOriginal.");
    handle_update(&env.ctx, &env.root).expect("first update");
    assert!(env.first_doc_embedded("docs"));

    // Change the file body, then force a reindex (an immediate rewrite lands in
    // the same wall-clock second as the first index, so the mtime gate would
    // otherwise skip it — a test-timing artifact, not the behavior under test).
    // New content → new hash → embedding invalidated → re-embedded.
    std::fs::write(env.root.join("docs/a.md"), "# Doc A\n\nRewritten body.").unwrap();
    let r = handle_update_force(&env.ctx, &env.root, true).expect("second update");
    assert!(
        r.doc_embeddings_generated >= 1,
        "changed doc must be re-embedded"
    );
    assert!(env.first_doc_embedded("docs"));
}
