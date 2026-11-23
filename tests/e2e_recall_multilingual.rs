//! Bilingual recall end to end, deterministic: no model weights, so no test
//! depends on inference. Every process runs in a [`recall::Sandbox`] (its own
//! `HOME`, project, store and empty model cache, offline).
//!
//! Without a model the store runs on BM25 and admits only a strong lexical
//! match (`store::memory::search_entries_hybrid_fts`, `admit_doc_hits`): the
//! queries here quote titles and identifiers. Natural-language retrieval in the
//! four language directions is in `e2e_recall_acceptance.rs`.

#[path = "common/cli.rs"]
mod cli;
#[path = "common/recall.rs"]
mod recall;

use recall::{Model, Sandbox};

/// F01 in both languages: documents as `docs/<id>.<lang>.md`, memories as
/// `<id>-<lang>` entries.
fn seed_f01(s: &Sandbox) -> (usize, usize) {
    let (docs, _) = recall::fixture(&["F01-ingest"]);
    let (mut files, mut memories) = (0, 0);
    for d in &docs {
        for (lang, (title, text)) in [("en", &d.en), ("it", &d.it)] {
            if d.kind == "doc" {
                s.write_doc(&format!("docs/{}.{lang}.md", d.id), title, text);
                files += 1;
            } else {
                s.add_memory(&format!("{}-{lang}", d.id), title, text, &d.entry_type, &[]);
                memories += 1;
            }
        }
    }
    s.ok(&["update"]);
    (files, memories)
}

// Scenarios 1 and 2. Catches: a writer that drops non-ASCII text or one
// language variant, or a memory write that never reaches the index.
#[test]
fn bilingual_documents_and_memories_are_indexed_and_found_in_both_languages() {
    let s = Sandbox::new(Model::Absent);
    let (files, memories) = seed_f01(&s);
    assert_eq!(s.count("SELECT COUNT(*) FROM documents") as usize, files);
    assert_eq!(
        s.count("SELECT COUNT(*) FROM memory_entries") as usize,
        memories
    );

    let docs = s.search("Backpressure dell'ingest", "docs");
    assert_eq!(
        docs.first().map(|h| h.0.as_str()),
        Some("ingest-backpressure.it.md"),
        "{docs:?}"
    );
    let docs = s.search("Ingest backpressure", "docs");
    assert_eq!(
        docs.first().map(|h| h.0.as_str()),
        Some("ingest-backpressure.en.md"),
        "{docs:?}"
    );
    let (fixture, _) = recall::fixture(&["F01-ingest"]);
    let memory = fixture
        .iter()
        .find(|d| d.kind == "memory")
        .expect("a memory in F01");
    for (lang, title) in [("it", &memory.it.0), ("en", &memory.en.0)] {
        let hits = s.search(title, "memory");
        let want = format!("{}-{lang}", memory.id);
        assert!(
            hits.iter().any(|h| h.0 == want),
            "{title} must find {want}: {hits:?}"
        );
    }
}

// Scenario 5. Catches: recall that injects the nearest BM25 hit for any prompt
// sharing one common word.
#[test]
fn an_unrelated_prompt_injects_no_context() {
    let s = Sandbox::new(Model::Absent);
    seed_f01(&s);
    for prompt in [
        "* what is a good recipe for pizza dough?",
        "* qual è la capitale del Portogallo?",
    ] {
        let (context, stderr, code) = s.hook(prompt);
        assert_eq!(code, 0, "{stderr}");
        assert!(context.is_empty(), "{prompt}: injected {context}");
    }
}

// Scenario 6. Catches: an update that keeps the old body in the FTS index.
#[test]
fn an_edited_document_is_found_by_its_new_text_only() {
    let s = Sandbox::new(Model::Absent);
    s.write_doc(
        "docs/limits.md",
        "Upload limits",
        "The cap is set by zq_alpha_limit.",
    );
    s.ok(&["update"]);
    assert_eq!(s.search("zq_alpha_limit", "docs").len(), 1);

    s.write_doc(
        "docs/limits.md",
        "Upload limits",
        "The cap is set by zq_beta_limit.",
    );
    // `update` compares whole-second mtimes with `indexed_at`: an edit in the
    // second of the last index is skipped (reported as a defect). A later
    // mtime is what a human edit has; setting it keeps this test exact.
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(s.root.join("docs/limits.md"))
        .and_then(|f| f.set_modified(later))
        .expect("set mtime");
    s.ok(&["update"]);
    assert!(
        s.search("zq_alpha_limit", "docs").is_empty(),
        "old text must be gone"
    );
    assert_eq!(
        s.search("zq_beta_limit", "docs").len(),
        1,
        "new text must be found"
    );
}

// Scenario 7. Catches: a deleted file whose document row, chunks or FTS row
// survive the next update and keep answering.
#[test]
fn a_deleted_document_leaves_no_rows_and_no_hits() {
    let s = Sandbox::new(Model::Absent);
    s.write_doc("docs/keep.md", "Kept page", "zq_kept_token stays.");
    s.write_doc("docs/gone.md", "Gone page", "zq_gone_token goes.");
    s.ok(&["update"]);
    assert_eq!(s.search("zq_gone_token", "docs").len(), 1);

    std::fs::remove_file(s.root.join("docs/gone.md")).expect("delete doc");
    s.ok(&["update"]);
    assert_eq!(
        s.count("SELECT COUNT(*) FROM documents WHERE relative_path LIKE '%gone.md'"),
        0
    );
    assert_eq!(
        s.count(
            "SELECT COUNT(*) FROM document_chunks c JOIN documents d ON d.id = c.document_id \
                        WHERE d.relative_path LIKE '%gone.md'"
        ),
        0
    );
    assert!(s.search("zq_gone_token", "docs").is_empty());
    assert_eq!(s.search("zq_kept_token", "docs").len(), 1);
}

// Scenario 8. Catches: a superseded, refuted or expired entry injected as if
// it were current.
#[test]
fn superseded_refuted_and_expired_entries_follow_their_rules() {
    let s = Sandbox::new(Model::Absent);
    s.add_memory(
        "events-kafka",
        "zq_event_bus choice",
        "Domain events use Kafka.",
        "decision",
        &[],
    );
    s.add_memory(
        "events-nats",
        "zq_event_bus choice v2",
        "Domain events use NATS.",
        "decision",
        &["--relates", "supersedes:events-kafka:memory"],
    );
    let old = s.search("events-kafka", "knowledge");
    let hits = s.search("zq_event_bus choice", "memory");
    let kafka = hits.iter().find(|h| h.0 == "events-kafka");
    assert!(
        kafka.is_none_or(|h| h.2 == "superseded"),
        "old decision must be marked superseded: {hits:?} {old:?}"
    );
    let (context, _, _) = s.hook("* zq_event_bus choice");
    assert!(
        context.contains("events-nats") || context.contains("NATS"),
        "current decision injected: {context}"
    );
    assert!(
        !context.contains("Domain events use Kafka"),
        "superseded decision injected: {context}"
    );

    s.add_memory(
        "retry-ceiling",
        "zq_retry_ceiling value",
        "The ceiling is forty seconds.",
        "topic",
        &[],
    );
    s.ok(&["memory", "confirm", "retry-ceiling", "--outcome", "refuted"]);
    assert!(
        s.search("zq_retry_ceiling value", "memory")
            .iter()
            .any(|h| h.0 == "retry-ceiling"),
        "an explicit search still shows a refuted entry"
    );
    let (context, _, _) = s.hook("* zq_retry_ceiling value");
    assert!(
        !context.contains("forty seconds"),
        "refuted entry injected: {context}"
    );

    s.add_memory(
        "deploy-window",
        "zq_deploy_window today",
        "Deploys are paused today.",
        "topic",
        &["--ttl", "3600"],
    );
    assert_eq!(s.search("zq_deploy_window today", "memory").len(), 1);
    // Drive the persisted expiry state rather than racing a one-second TTL
    // against subprocess startup and the host's scheduling load.
    let conn = rusqlite::Connection::open(s.root.join(".mdkb/index.sqlite")).unwrap();
    let expires_at: i64 = conn
        .query_row(
            "SELECT expires_at FROM memory_entries WHERE id = 'deploy-window'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(expires_at > chrono::Utc::now().timestamp());
    conn.execute(
        "UPDATE memory_entries SET expires_at = ?1 WHERE id = 'deploy-window'",
        [chrono::Utc::now().timestamp() - 10],
    )
    .unwrap();
    drop(conn);
    assert!(
        s.search("zq_deploy_window today", "memory").is_empty(),
        "expired entry returned"
    );
    let (context, _, _) = s.hook("* zq_deploy_window today");
    assert!(
        !context.contains("paused today"),
        "expired entry injected: {context}"
    );
}

// Scenario 11. Catches: a missing model that crashes or blocks the hook, or a
// fallback that injects BM25 noise instead of only lexical evidence.
#[test]
fn missing_weights_fail_embed_clearly_and_degrade_recall_to_lexical_evidence() {
    let s = Sandbox::new(Model::Absent);
    s.write_doc(
        "docs/sessions.md",
        "zq_session_store layout",
        "Web sessions live in Redis for 30 minutes.",
    );
    let update = s.run(&["update"]);
    assert!(update.status.success(), "update must index without a model");
    let embed = s.run(&["update", "--reembed"]);
    assert!(!embed.status.success(), "embed without weights must fail");
    assert!(
        String::from_utf8_lossy(&embed.stderr).contains("embedding model"),
        "unclear embed error"
    );

    let (context, stderr, code) = s.hook("* where are the web sessions kept?");
    assert_eq!(code, 0);
    assert!(
        context.is_empty(),
        "no vector leg, no lexical evidence: nothing to inject, got {context}"
    );
    assert!(
        stderr.contains("BM25-only"),
        "the fallback must be visible: {stderr}"
    );
    let (context, _, code) = s.hook("* zq_session_store layout");
    assert_eq!(code, 0);
    assert!(
        context.contains("sessions.md"),
        "lexical evidence still injects: {context}"
    );

    s.set_hook_deadline(1);
    let (_, stderr, code) = s.hook("* zq_session_store layout");
    assert_eq!(
        code, 0,
        "a missed deadline must not fail the prompt: {stderr}"
    );
}
