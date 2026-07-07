//! Integration tests for `mdkb hook user-prompt-submit` — auto-recall injection.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use mdkb::cli::handlers::{Context, handle_init};
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use mdkb::store::memory_graph::{self, MemoryRelation, TargetKind};
use tempfile::TempDir;

fn mdkb_bin() -> Command {
    let bin = env!("CARGO_BIN_EXE_mdkb");
    Command::new(bin)
}

fn run_user_prompt_submit_in(dir: &Path, stdin_json: &str) -> (i32, String) {
    // Recall injection is gated behind the `*` sigil by default; these tests
    // exercise recall mechanics with plain (un-prefixed) prompts, so turn the gate
    // off in the project config when it exists. The gate itself is covered by the
    // unit test `require_sigil_gates_injection_to_star_prefixed_prompts`.
    let cfg = dir.join(".mdkb/config.toml");
    if let Ok(body) = fs::read_to_string(&cfg) {
        let patched = body.replace(
            "user_prompt_submit_require_sigil = true",
            "user_prompt_submit_require_sigil = false",
        );
        if patched != body {
            let _ = fs::write(&cfg, patched);
        }
    }

    let mut child = mdkb_bin()
        .args(["hook", "user-prompt-submit"])
        .current_dir(dir)
        .env("MDKB_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mdkb");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Run an arbitrary `mdkb` subcommand in `dir`, asserting success.
fn run_mdkb(args: &[&str], dir: &Path) {
    let out = mdkb_bin()
        .args(args)
        .current_dir(dir)
        .env("MDKB_NO_DAEMON", "1")
        .stderr(Stdio::null())
        .output()
        .expect("run mdkb");
    assert!(
        out.status.success(),
        "mdkb {args:?} failed: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn additional_context(stdout: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Init a project and index two docs under `docs/`: `a.md` carries a `related`
/// frontmatter edge to `b.md`. Returns the project root.
fn seed_doc_graph() -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    run_mdkb(&["init"], root);

    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(
        docs.join("a.md"),
        "---\nrelated:\n  - b.md\n---\n# Doc A\n\nThe architecture overview for the system.\n",
    )
    .unwrap();
    fs::write(
        docs.join("b.md"),
        "# Doc B\n\nThe data model details for the system.\n",
    )
    .unwrap();

    run_mdkb(&["update"], root);
    tmp
}

fn seed_project_with_memory(id: &str, title: &str, content: &str, tags: &[&str]) -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    handle_init(&root).expect("init");

    let ctx = Context::open(&root).expect("open ctx");
    let now = chrono::Utc::now().timestamp();

    let entry = MemoryEntry {
        id: id.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        entry_type: EntryType::Decision,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 5,
        last_accessed: Some(now),
        source_path: None,
        confirmations: 1,
        last_confirmed_at: Some(now),
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };
    add_entry(&ctx.conn, &entry).expect("add entry");

    tmp
}

#[test]
fn user_prompt_submit_recall_snippet_strips_frontmatter() {
    // Handoff entries persist YAML frontmatter inside `content`; the recall
    // snippet must show the body, never `---`/`session_id:` YAML.
    let tmp = seed_project_with_memory(
        "handoff-frontmatter",
        "Kafka rebalance handoff",
        "---\nsession_id: deadbeef\ndone: [x]\n---\nInvestigated kafka consumer rebalance storm mitigation",
        &["kafka", "rebalance"],
    );

    let event = r#"{"prompt":"kafka consumer rebalance storm mitigation investigation"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);
    assert_eq!(code, 0);

    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let ctx_block = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .expect("recall context present");

    assert!(
        ctx_block.contains("handoff-frontmatter"),
        "handoff entry must be recalled: {ctx_block}"
    );
    assert!(
        ctx_block.contains("Investigated kafka"),
        "snippet must show the body: {ctx_block}"
    );
    assert!(
        !ctx_block.contains("session_id:"),
        "snippet must NOT leak frontmatter: {ctx_block}"
    );
    // The recall line title still legitimately uses the entry title; ensure the
    // leaked YAML fence specifically is gone from the snippet portion.
    let recall_line = ctx_block
        .lines()
        .find(|l| l.contains("handoff-frontmatter"))
        .expect("recall line");
    assert!(
        !recall_line.contains("---"),
        "recall line must not contain a YAML fence: {recall_line}"
    );
}

#[test]
fn user_prompt_submit_injects_relevant_memory() {
    let tmp = seed_project_with_memory(
        "jwt-refresh-strategy",
        "JWT refresh token rotation",
        "sliding expiry with refresh token rotation handles token expiration safely",
        &["jwt", "auth", "token"],
    );

    let event = r#"{"prompt":"how did we handle jwt token expiration refresh rotation"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0, "hook must always exit 0");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");

    let ctx_block = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .expect("additionalContext must be a string when relevant memory exists");

    assert!(
        ctx_block.contains("## mdkb: relevant context"),
        "context must contain recall header, got: {ctx_block}"
    );
    assert!(
        ctx_block.contains("jwt-refresh-strategy"),
        "matching memory id must appear, got: {ctx_block}"
    );
    assert!(
        ctx_block.contains("(just now)"),
        "recall lines must show relative age so stale context is visible, got: {ctx_block}"
    );
    assert_eq!(
        parsed
            .get("hookSpecificOutput")
            .and_then(|h| h.get("hookEventName"))
            .and_then(|v| v.as_str()),
        Some("UserPromptSubmit"),
        "hookEventName must be UserPromptSubmit"
    );
}

#[test]
fn user_prompt_submit_no_injection_when_no_match() {
    let tmp = seed_project_with_memory(
        "ci-pipeline",
        "CI pipeline caching",
        "use buildkit layer cache for docker builds",
        &["ci", "docker"],
    );

    let event = r#"{"prompt":"completely unrelated quantum physics question"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "no injection when no matches, got: {stdout}"
    );
}

#[test]
fn user_prompt_submit_skips_on_wrapup_marker() {
    let tmp = seed_project_with_memory("any-memory", "Anything", "matching content here", &["any"]);

    let event = r#"{"prompt":"/wrapup session is ending"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "wrap-up markers must suppress all output, got: {stdout}"
    );
}

#[test]
fn user_prompt_submit_respects_mdkbignore_hooks_marker() {
    let tmp = seed_project_with_memory(
        "jwt-strategy",
        "JWT strategy",
        "matching content about jwt",
        &["jwt"],
    );
    fs::write(tmp.path().join(".mdkbignore-hooks"), "").expect("write marker");

    let event = r#"{"prompt":"how did we handle jwt"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "opt-out marker must suppress all output, got: {stdout}"
    );
}

#[test]
fn user_prompt_submit_on_uninitialized_project_returns_silence() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let event = r#"{"prompt":"anything"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0, "hook must never block");
    assert!(
        stdout.trim().is_empty(),
        "no .mdkb/ means no output, got: {stdout}"
    );
}

#[test]
fn user_prompt_submit_no_prompt_field_returns_silence() {
    let tmp = seed_project_with_memory("x", "x", "x", &[]);
    let event = r#"{}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "missing prompt must produce no output, got: {stdout}"
    );
}

/// Seed a project with two memory entries that differ only in access_count/last_accessed.
/// Returns a TempDir with the project root.
fn seed_two_entries_with_access_counts(
    low_id: &str,
    high_id: &str,
    shared_keyword: &str,
) -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open ctx");

    let now = chrono::Utc::now().timestamp();

    // Low-access entry: accessed once, 30 days ago.
    let low = MemoryEntry {
        id: low_id.to_string(),
        title: format!("{shared_keyword} low-access entry"),
        content: format!("{shared_keyword} implementation detail rarely accessed by anyone"),
        entry_type: EntryType::Decision,
        tags: vec![shared_keyword.to_string()],
        status: EntryStatus::Active,
        created_at: now - 40 * 86400,
        updated_at: now - 40 * 86400,
        superseded_by: None,
        access_count: 1,
        last_accessed: Some(now - 30 * 86400),
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };

    // High-access entry: accessed 50 times, 1 hour ago — should rank first.
    let high = MemoryEntry {
        id: high_id.to_string(),
        title: format!("{shared_keyword} high-access entry"),
        content: format!("{shared_keyword} implementation detail frequently accessed by the team"),
        entry_type: EntryType::Decision,
        tags: vec![shared_keyword.to_string()],
        status: EntryStatus::Active,
        created_at: now - 5 * 86400,
        updated_at: now - 5 * 86400,
        superseded_by: None,
        access_count: 50,
        last_accessed: Some(now - 3600),
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };

    add_entry(&ctx.conn, &low).expect("add low entry");
    add_entry(&ctx.conn, &high).expect("add high entry");

    tmp
}

#[test]
fn user_prompt_submit_access_recency_reranks_higher_access_first() {
    // Both entries share a unique keyword so both will match FTS.
    // The high-access entry should float to the top after re-ranking.
    let keyword = "cacherefreshpolicy";
    let low_id = "cache-low-access";
    let high_id = "cache-high-access";

    let tmp = seed_two_entries_with_access_counts(low_id, high_id, keyword);

    let event = format!(r#"{{"prompt":"how does the {keyword} work in our system"}}"#);
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), &event);

    assert_eq!(code, 0, "hook must always exit 0");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");

    let ctx_block = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .expect("additionalContext must be present when both entries match");

    // Both entries must appear in the output.
    assert!(
        ctx_block.contains(high_id),
        "high-access entry must appear in output, got:\n{ctx_block}"
    );
    assert!(
        ctx_block.contains(low_id),
        "low-access entry must appear in output, got:\n{ctx_block}"
    );

    // High-access entry must appear BEFORE the low-access entry.
    let high_pos = ctx_block.find(high_id).expect("high_id must be present");
    let low_pos = ctx_block.find(low_id).expect("low_id must be present");

    assert!(
        high_pos < low_pos,
        "high-access entry (pos {high_pos}) must appear before low-access (pos {low_pos}) in:\n{ctx_block}"
    );
}

#[test]
fn user_prompt_submit_injects_frontmatter_neighbor() {
    let tmp = seed_doc_graph();

    // Path-only prompt: no FTS keywords survive, so this exercises the
    // doc-graph leg independently of memory recall.
    let event = r#"{"prompt":"open docs/a.md please"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0, "hook must always exit 0");
    let ctx = additional_context(&stdout)
        .unwrap_or_else(|| panic!("expected related-docs block, got: {stdout}"));

    assert!(
        ctx.contains("## mdkb: related docs"),
        "must emit the related-docs header, got: {ctx}"
    );
    assert!(
        ctx.contains("b.md") && ctx.contains("(related)"),
        "must list the frontmatter neighbor with its relation label, got: {ctx}"
    );
}

#[test]
fn user_prompt_submit_doc_graph_flag_off_silences_related() {
    let tmp = seed_doc_graph();
    fs::write(
        tmp.path().join(".mdkb/config.toml"),
        "[hooks]\ndoc_graph_in_recall = false\n",
    )
    .unwrap();

    let event = r#"{"prompt":"open docs/a.md please"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0);
    assert!(
        !stdout.contains("related docs"),
        "flag off must suppress the related-docs block, got: {stdout}"
    );
}

#[test]
fn user_prompt_submit_excludes_non_doc_frontmatter_targets() {
    // a.md relates to a real doc (b.md) AND carries entity relations whose
    // targets are tags, not docs (themes: platform, owner: security-team). Only
    // the doc neighbor must surface under "related docs".
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    run_mdkb(&["init"], root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(
        docs.join("a.md"),
        "---\nrelated:\n  - b.md\nthemes:\n  - platform\nowner: security-team\n---\n# A\n\nbody\n",
    )
    .unwrap();
    fs::write(docs.join("b.md"), "# B\n\nbody\n").unwrap();
    run_mdkb(&["update"], root);

    let event = r#"{"prompt":"open docs/a.md"}"#;
    let (code, stdout) = run_user_prompt_submit_in(root, event);

    assert_eq!(code, 0);
    let block = additional_context(&stdout)
        .unwrap_or_else(|| panic!("expected related-docs block, got: {stdout}"));
    assert!(
        block.contains("b.md (related)"),
        "real doc neighbor must surface, got: {block}"
    );
    assert!(
        !block.contains("platform") && !block.contains("security-team"),
        "entity-tag frontmatter targets (themes/owner) must NOT surface as related docs, got: {block}"
    );
}

#[test]
fn user_prompt_submit_resolves_wikilink_token() {
    // A bare `[[guide]]` wikilink (no `/` or `.md`) must resolve to its neighbor.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    run_mdkb(&["init"], root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(
        docs.join("guide.md"),
        "---\nrelated:\n  - spec.md\n---\n# Guide\n\nbody\n",
    )
    .unwrap();
    fs::write(docs.join("spec.md"), "# Spec\n\nbody\n").unwrap();
    run_mdkb(&["update"], root);

    let event = r#"{"prompt":"check the [[guide]] note"}"#;
    let (code, stdout) = run_user_prompt_submit_in(root, event);

    assert_eq!(code, 0);
    let ctx = additional_context(&stdout)
        .unwrap_or_else(|| panic!("expected related-docs via wikilink, got: {stdout}"));
    assert!(
        ctx.contains("## mdkb: related docs") && ctx.contains("spec.md (related)"),
        "wikilink token must resolve to its frontmatter neighbor, got: {ctx}"
    );
}

#[test]
fn user_prompt_submit_caps_neighbors_at_three() {
    // a.md relates to four real docs; the injected block must cap at 3.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    run_mdkb(&["init"], root);
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(
        docs.join("a.md"),
        "---\nrelated:\n  - b.md\n  - c.md\n  - d.md\n  - e.md\n---\n# A\n\nbody\n",
    )
    .unwrap();
    for name in ["b", "c", "d", "e"] {
        fs::write(
            docs.join(format!("{name}.md")),
            format!("# {name}\n\nbody\n"),
        )
        .unwrap();
    }
    run_mdkb(&["update"], root);

    let event = r#"{"prompt":"open docs/a.md"}"#;
    let (code, stdout) = run_user_prompt_submit_in(root, event);

    assert_eq!(code, 0);
    let ctx = additional_context(&stdout)
        .unwrap_or_else(|| panic!("expected related-docs block, got: {stdout}"));
    let neighbor_lines = ctx
        .lines()
        .filter(|l| l.starts_with("- ") && l.contains("(related)"))
        .count();
    assert_eq!(
        neighbor_lines, 3,
        "must cap doc-graph neighbors at 3, got {neighbor_lines} in:\n{ctx}"
    );
}

#[test]
fn user_prompt_submit_no_related_block_without_path_token() {
    let tmp = seed_doc_graph();

    // No path/.md token → the doc-graph leg must stay silent.
    let event = r#"{"prompt":"what is the meaning of life"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0);
    assert!(
        !stdout.contains("related docs"),
        "no path token must produce no related-docs block, got: {stdout}"
    );
}

#[test]
fn user_prompt_submit_dedups_neighbor_against_memory_id() {
    let tmp = seed_doc_graph();

    // A memory whose id collides with the neighbor's canonical path. When the
    // prompt both matches this memory (FTS) and names a.md, the neighbor must
    // NOT be repeated in the related-docs block.
    let ctx = Context::open(tmp.path()).expect("open ctx");
    let now = chrono::Utc::now().timestamp();
    let entry = MemoryEntry {
        id: "b.md".to_string(),
        title: "Frobnicator memory".to_string(),
        content: "frobnicatorxyz design decision for the system".to_string(),
        entry_type: EntryType::Decision,
        tags: vec!["frobnicatorxyz".to_string()],
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 5,
        last_accessed: Some(now),
        source_path: None,
        confirmations: 1,
        last_confirmed_at: Some(now),
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };
    add_entry(&ctx.conn, &entry).expect("add entry");

    let event = r#"{"prompt":"review docs/a.md and the frobnicatorxyz decision"}"#;
    let (code, stdout) = run_user_prompt_submit_in(tmp.path(), event);

    assert_eq!(code, 0);
    let block =
        additional_context(&stdout).unwrap_or_else(|| panic!("expected output, got: {stdout}"));

    // The memory is injected (matches FTS)...
    assert!(
        block.contains("[b.md]"),
        "memory with id b.md must be injected, got: {block}"
    );
    // ...so b.md must NOT also appear as a related-doc line.
    assert!(
        !block.contains("b.md (related)"),
        "neighbor already injected as a memory must be deduped, got: {block}"
    );
}

/// A plain topic entry with strong FTS content, seeded directly into the store.
fn topic_entry(id: &str, title: &str, content: &str) -> MemoryEntry {
    let now = chrono::Utc::now().timestamp();
    MemoryEntry {
        id: id.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        entry_type: EntryType::Topic,
        tags: vec![],
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 5,
        last_accessed: Some(now),
        source_path: None,
        confirmations: 1,
        last_confirmed_at: Some(now),
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    }
}

#[test]
fn user_prompt_submit_expands_one_hop_memory_neighbor() {
    // Recall matches seed A on FTS; B shares no keywords with the prompt, so it
    // can ONLY surface through the one-hop `supports` edge expansion. This drives
    // the whole recall→expand path through the real `mdkb hook` binary — the
    // in-module test at dispatch.rs exercises the same logic below the hook edge.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    handle_init(&root).expect("init");
    {
        let ctx = Context::open(&root).expect("open ctx");
        add_entry(
            &ctx.conn,
            &topic_entry(
                "orbital-decoder",
                "Orbital telemetry decoder",
                "Orbital telemetry decoder frame sync algorithm",
            ),
        )
        .expect("seed A");
        add_entry(
            &ctx.conn,
            &topic_entry(
                "checksum-detail",
                "Checksum trailer",
                "CRC32 bytes appended after the payload region",
            ),
        )
        .expect("seed B");
        memory_graph::add_edge(
            &ctx.conn,
            "orbital-decoder",
            "checksum-detail",
            TargetKind::Memory,
            MemoryRelation::Supports,
        )
        .expect("add edge");
    }

    let event = r#"{"prompt":"orbital telemetry decoder frame sync"}"#;
    let (code, stdout) = run_user_prompt_submit_in(&root, event);

    assert_eq!(code, 0, "hook must always exit 0");
    let block =
        additional_context(&stdout).unwrap_or_else(|| panic!("expected recall, got: {stdout}"));
    assert!(
        block.contains("orbital-decoder"),
        "the FTS-matched seed must recall: {block}"
    );
    assert!(
        block.contains("checksum-detail") && block.contains("(via supports)"),
        "the one-hop neighbor must expand, annotated with its relation: {block}"
    );
}

#[test]
fn user_prompt_submit_marks_stale_dependency_on_superseded_base() {
    // The recalled entry derives from a base that has been superseded, so its
    // dependency is stale — the recall line must carry the [STALE-DEP] marker.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    handle_init(&root).expect("init");
    {
        let ctx = Context::open(&root).expect("open ctx");
        add_entry(
            &ctx.conn,
            &topic_entry(
                "payload-migration",
                "Payload schema migration",
                "Payload schema migration uses versioned envelopes",
            ),
        )
        .expect("seed derived");
        add_entry(
            &ctx.conn,
            &topic_entry(
                "envelope-base",
                "Envelope format base",
                "Envelope byte layout internal notes",
            ),
        )
        .expect("seed base");
        memory_graph::add_edge(
            &ctx.conn,
            "payload-migration",
            "envelope-base",
            TargetKind::Memory,
            MemoryRelation::DerivedFrom,
        )
        .expect("add edge");
        // Supersede the base → the derived entry's dependency is now stale.
        ctx.conn
            .execute(
                "UPDATE memory_entries SET status='superseded' WHERE id='envelope-base'",
                [],
            )
            .expect("supersede base");
    }

    let event = r#"{"prompt":"payload schema migration versioned envelopes"}"#;
    let (code, stdout) = run_user_prompt_submit_in(&root, event);

    assert_eq!(code, 0, "hook must always exit 0");
    let block =
        additional_context(&stdout).unwrap_or_else(|| panic!("expected recall, got: {stdout}"));
    assert!(
        block.contains("[STALE-DEP] [payload-migration]"),
        "a derived entry with a superseded base must be flagged STALE-DEP: {block}"
    );
}
