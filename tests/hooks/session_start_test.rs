//! Integration tests for `mdkb hook session-start` — warmup injection contract.

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

fn run_session_start_in(dir: &Path, stdin_json: &str) -> (i32, String) {
    let mut child = mdkb_bin()
        .args(["hook", "session-start"])
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

fn setup_project_with_entries() -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    handle_init(&root).expect("init");

    let ctx = Context::open(&root).expect("open ctx");
    let now = chrono::Utc::now().timestamp();

    let entry = MemoryEntry {
        id: "auth-flow-decision".to_string(),
        title: "OAuth2 refresh token rotation".to_string(),
        content: "use refresh tokens with sliding expiry".to_string(),
        entry_type: EntryType::Decision,
        tags: vec!["auth".to_string(), "oauth2".to_string()],
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 10,
        last_accessed: Some(now),
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };
    add_entry(&ctx.conn, &entry).expect("add entry");

    tmp
}

#[test]
fn session_start_emits_warmup_with_stored_entries() {
    let tmp = setup_project_with_entries();
    let (code, stdout) = run_session_start_in(tmp.path(), "");

    assert_eq!(code, 0, "hook must always exit 0");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");

    let ctx_block = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .expect("hookSpecificOutput.additionalContext must be a string");

    assert!(
        ctx_block.contains("## mdkb memory warmup"),
        "context must contain warmup header, got: {ctx_block}"
    );
    assert!(
        ctx_block.contains("auth-flow-decision"),
        "stored memory id must appear in warmup, got: {ctx_block}"
    );
    assert_eq!(
        parsed
            .get("hookSpecificOutput")
            .and_then(|h| h.get("hookEventName"))
            .and_then(|v| v.as_str()),
        Some("SessionStart"),
        "hookEventName must be SessionStart"
    );
}

#[test]
fn session_start_respects_mdkbignore_hooks_marker() {
    let tmp = setup_project_with_entries();
    fs::write(tmp.path().join(".mdkbignore-hooks"), "").expect("write marker");

    let (code, stdout) = run_session_start_in(tmp.path(), "");

    assert_eq!(code, 0);
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");

    assert!(
        parsed.get("hookSpecificOutput").is_none()
            || parsed
                .get("hookSpecificOutput")
                .and_then(|h| h.get("additionalContext"))
                .is_none(),
        "opt-out marker must suppress context injection, got: {parsed}"
    );
}

#[test]
fn session_start_on_uninitialized_project_returns_empty_object() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (code, stdout) = run_session_start_in(tmp.path(), "");

    assert_eq!(code, 0, "hook must never block, even without .mdkb/");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");
    assert!(parsed.is_object());
    assert!(
        parsed.get("hookSpecificOutput").is_none(),
        "no .mdkb/ means no injection, got: {parsed}"
    );
}
