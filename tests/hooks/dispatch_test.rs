//! Integration tests for `mdkb hook` dispatch skeleton.

use std::io::Write;
use std::process::{Command, Stdio};

fn mdkb_bin() -> Command {
    let bin = env!("CARGO_BIN_EXE_mdkb");
    Command::new(bin)
}

/// Invoke `mdkb hook <event>` with the given stdin JSON and return (exit_code, stdout).
fn run_hook(event: &str, stdin_json: &str) -> (i32, String) {
    let mut child = mdkb_bin()
        .args(["hook", event])
        .env("MDKB_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mdkb");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .expect("failed to write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    (code, stdout)
}

fn assert_valid_hook_output(stdout: &str) {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return;
    }
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("non-empty stdout must be valid JSON");
    assert!(parsed.is_object(), "output must be a JSON object");
}

#[test]
fn session_start_empty_stdin_exits_zero_and_emits_valid_json() {
    let (code, stdout) = run_hook("session-start", "");
    assert_eq!(code, 0, "exit code must be 0 (non-blocking)");
    assert_valid_hook_output(&stdout);
}

#[test]
fn user_prompt_submit_empty_stdin_exits_zero_and_emits_valid_json() {
    let (code, stdout) = run_hook("user-prompt-submit", "");
    assert_eq!(code, 0, "exit code must be 0 (non-blocking)");
    assert_valid_hook_output(&stdout);
}

#[test]
fn post_tool_use_empty_stdin_exits_zero_and_emits_valid_json() {
    let (code, stdout) = run_hook("post-tool-use", "");
    assert_eq!(code, 0, "exit code must be 0 (non-blocking)");
    assert_valid_hook_output(&stdout);
}

#[test]
fn session_start_with_valid_json_input_exits_zero() {
    let input = r#"{"session_id":"test-123","transcript_path":"/tmp/test.jsonl"}"#;
    let (code, stdout) = run_hook("session-start", input);
    assert_eq!(code, 0);
    assert_valid_hook_output(&stdout);
}

#[test]
fn user_prompt_submit_with_valid_json_input_exits_zero() {
    let input = r#"{"session_id":"test-123","transcript_path":"/tmp/test.jsonl","prompt":"how does auth work?"}"#;
    let (code, stdout) = run_hook("user-prompt-submit", input);
    assert_eq!(code, 0);
    assert_valid_hook_output(&stdout);
}

#[test]
fn post_tool_use_with_valid_json_input_exits_zero() {
    let input = r#"{"session_id":"test-123","tool_name":"Edit","tool_input":{"file_path":"src/main.rs"},"tool_output":"ok"}"#;
    let (code, stdout) = run_hook("post-tool-use", input);
    assert_eq!(code, 0);
    assert_valid_hook_output(&stdout);
}
