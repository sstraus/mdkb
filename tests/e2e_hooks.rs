//! End-to-end test for the `mdkb hook <event>` dispatcher.
//!
//! Drives the full hook→stdin→stdout→reindex-queue lifecycle by spawning
//! the real `mdkb` binary so the assertions cover the actual production
//! path Claude Code / Codex would exercise. Covers:
//!
//! 1. `session-start` surfaces a memory warmup block.
//! 2. `user-prompt-submit` injects relevant memory when the prompt
//!    matches indexed content.
//! 3. `post-tool-use` enqueues the edited path into
//!    `.mdkb/reindex-queue.jsonl`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use mdkb::cli::handlers::{Context, handle_init, handle_memory_add};
use serde_json::Value;

fn mdkb_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mdkb")
}

/// Run `mdkb hook <event>` with the given stdin payload and return stdout.
fn run_hook(event: &str, cwd: &Path, stdin_payload: &str) -> (String, String, i32) {
    let mut child = Command::new(mdkb_bin())
        .args(["hook", event])
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mdkb hook");

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_payload.as_bytes())
            .expect("write stdin");
    }

    let output = child.wait_with_output().expect("wait mdkb hook");
    let stdout = String::from_utf8(output.stdout).unwrap_or_default();
    let stderr = String::from_utf8(output.stderr).unwrap_or_default();
    (stdout, stderr, output.status.code().unwrap_or(-1))
}

#[test]
fn hooks_e2e_warmup_recall_and_reindex() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();

    handle_init(&root).expect("init mdkb");

    // Seed a memory entry whose tokens will match a later user prompt.
    let ctx = Context::open(&root).expect("open context");
    handle_memory_add(
        &ctx,
        "hooks-e2e-topic",
        "Hook dispatcher architecture",
        "topic",
        Some("hooks,architecture"),
        "The mdkb hook dispatcher reads stdin and writes JSON to stdout. \
         It supports session-start, user-prompt-submit, and post-tool-use events.",
        None,
        None,
    )
    .expect("seed memory");
    drop(ctx);

    // --- SessionStart: warmup block must appear -------------------------
    let (stdout, stderr, code) = run_hook("session-start", &root, "{}");
    assert_eq!(
        code, 0,
        "session-start must exit 0. stderr={stderr} stdout={stdout}"
    );
    let v: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("session-start stdout must be JSON; err={e} stdout={stdout}"));
    let warmup = v
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        warmup.contains("mdkb memory warmup"),
        "session-start must emit warmup header. Got: {warmup}"
    );
    assert!(
        warmup.contains("Hook dispatcher architecture") || warmup.contains("hooks-e2e-topic"),
        "warmup must reference the seeded entry. Got: {warmup}"
    );

    // --- UserPromptSubmit: relevant memory injection --------------------
    let prompt_payload = r#"{"prompt": "Explain the hook dispatcher architecture and events"}"#;
    let (stdout, stderr, code) = run_hook("user-prompt-submit", &root, prompt_payload);
    assert_eq!(
        code, 0,
        "user-prompt-submit must exit 0. stderr={stderr} stdout={stdout}"
    );
    let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("user-prompt-submit stdout must be JSON; err={e} stdout={stdout}")
    });
    let recall = v
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        recall.contains("relevant context"),
        "user-prompt-submit must include relevant-context header. Got: {recall}"
    );
    assert!(
        recall.contains("Hook dispatcher architecture") || recall.contains("hooks-e2e-topic"),
        "user-prompt-submit must surface the seeded entry. Got: {recall}"
    );

    // --- PostToolUse: reindex queue must gain an entry ------------------
    let edited_file = root.join("notes.md");
    std::fs::write(&edited_file, "hello").expect("seed edited file");
    let tool_payload = serde_json::json!({
        "tool_name": "Edit",
        "tool_input": { "file_path": edited_file.to_string_lossy() }
    });
    let (stdout, stderr, code) = run_hook(
        "post-tool-use",
        &root,
        &serde_json::to_string(&tool_payload).unwrap(),
    );
    assert_eq!(
        code, 0,
        "post-tool-use must exit 0. stderr={stderr} stdout={stdout}"
    );

    let queue_path = root.join(".mdkb").join("reindex-queue.jsonl");
    assert!(
        queue_path.exists(),
        "post-tool-use must create reindex-queue.jsonl"
    );
    let queue_raw = std::fs::read_to_string(&queue_path).expect("read queue");
    let last_line = queue_raw
        .lines()
        .last()
        .expect("queue must have at least one line");
    let entry: Value = serde_json::from_str(last_line).expect("queue line is JSON");
    assert_eq!(
        entry.get("tool").and_then(|v| v.as_str()),
        Some("Edit"),
        "queue entry tool must be Edit"
    );
    let queued_path = entry
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        queued_path.ends_with("notes.md"),
        "queue entry path must point at the edited file. Got: {queued_path}"
    );
}

#[test]
fn hooks_e2e_respects_mdkbignore_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    handle_init(&root).expect("init mdkb");

    // Seed a memory entry so warmup would otherwise produce output.
    let ctx = Context::open(&root).expect("open ctx");
    handle_memory_add(
        &ctx,
        "ignored-topic",
        "Ignored topic",
        "topic",
        None,
        "this should not surface because the project opted out of hooks",
        None,
        None,
    )
    .expect("seed memory");
    drop(ctx);

    // Opt-out marker stops all hook output.
    std::fs::write(root.join(".mdkbignore-hooks"), "").expect("write marker");

    let (stdout, _stderr, code) = run_hook("session-start", &root, "{}");
    assert_eq!(code, 0);
    let v: Value = serde_json::from_str(stdout.trim()).expect("json");
    assert!(
        v.get("hookSpecificOutput").is_none(),
        ".mdkbignore-hooks must suppress warmup. Got: {v}"
    );
}
