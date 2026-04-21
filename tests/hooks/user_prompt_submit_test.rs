//! Integration tests for `mdkb hook user-prompt-submit` — auto-recall injection.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use mdkb::cli::handlers::{Context, handle_init};
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use tempfile::TempDir;

fn mdkb_bin() -> Command {
    let bin = env!("CARGO_BIN_EXE_mdkb");
    Command::new(bin)
}

fn run_user_prompt_submit_in(dir: &Path, stdin_json: &str) -> (i32, String) {
    let mut child = mdkb_bin()
        .args(["hook", "user-prompt-submit"])
        .current_dir(dir)
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
