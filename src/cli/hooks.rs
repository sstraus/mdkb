//! Hook dispatch handlers for Claude Code / Codex lifecycle events.
//!
//! Contract: every handler reads event JSON from stdin (best-effort, may be empty
//! or malformed), writes a JSON object to stdout, and the process exits 0.
//! The host CLI must never be blocked by mdkb — internal failures are logged
//! to stderr and swallowed. Actual recall / warmup / reindex logic lands in
//! the event-specific stories (015 SessionStart, 016 UserPromptSubmit, 017 PostToolUse).

use std::io::{self, Read, Write};

use serde_json::{Value, json};

use crate::cli::HookEvent;

/// Entry point for `mdkb hook <event>`.
///
/// Always returns after writing a JSON object to stdout and exits 0-equivalent
/// (caller in `main` propagates the `Ok(())`).
pub fn dispatch(event: HookEvent) {
    let input = read_stdin_best_effort();
    let event_json = parse_event(&input);

    let response = match event {
        HookEvent::SessionStart => handle_session_start(event_json),
        HookEvent::UserPromptSubmit => handle_user_prompt_submit(event_json),
        HookEvent::PostToolUse => handle_post_tool_use(event_json),
    };

    emit_response(&response);
}

fn read_stdin_best_effort() -> String {
    let mut buf = String::new();
    let _ = io::stdin().read_to_string(&mut buf);
    buf
}

fn parse_event(input: &str) -> Value {
    if input.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(input).unwrap_or(Value::Null)
}

fn emit_response(value: &Value) {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{}", serialized);
    let _ = handle.flush();
}

fn handle_session_start(_event: Value) -> Value {
    json!({})
}

fn handle_user_prompt_submit(_event: Value) -> Value {
    json!({})
}

fn handle_post_tool_use(_event: Value) -> Value {
    json!({})
}
