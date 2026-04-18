//! Hook dispatch handlers for Claude Code / Codex lifecycle events.
//!
//! Contract: every handler reads event JSON from stdin (best-effort, may be empty
//! or malformed), writes a JSON object to stdout, and the process exits 0.
//! The host CLI must never be blocked by mdkb — internal failures are logged
//! to stderr and swallowed. Actual recall / warmup / reindex logic lands in
//! the event-specific stories (015 SessionStart, 016 UserPromptSubmit, 017 PostToolUse).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

use crate::cli::HookEvent;
use crate::cli::handlers::Context;
use crate::config::{Config, HooksConfig};
use crate::store::memory::get_warmup_index;

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

/// Walk ancestors looking for `.mdkbignore-hooks` marker. Stops at the user's
/// home directory (never walks above it) to avoid picking up unrelated markers.
fn mdkbignore_hooks_present(start: &Path) -> bool {
    let home: Option<PathBuf> = std::env::var_os("HOME").map(PathBuf::from);
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        if dir.join(".mdkbignore-hooks").exists() {
            return true;
        }
        if let Some(h) = home.as_deref() {
            if dir == h {
                return false;
            }
        }
        current = dir.parent();
    }
    false
}

fn hooks_config(root: &Path) -> HooksConfig {
    let cfg_path = root.join(".mdkb").join("config.toml");
    Config::load_or_default(&cfg_path).hooks
}

fn log_slow_hook(root: &Path, event: &str, elapsed_ms: u128, budget_ms: u64) {
    let log_path = root.join(".mdkb").join("hook-slow.jsonl");
    let line = json!({
        "event": event,
        "elapsed_ms": elapsed_ms,
        "budget_ms": budget_ms,
        "ts": chrono::Utc::now().timestamp(),
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        let _ = writeln!(f, "{}", line);
    }
}

fn handle_session_start(_event: Value) -> Value {
    let start = Instant::now();
    let cwd: PathBuf = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return json!({}),
    };

    if mdkbignore_hooks_present(&cwd) {
        return json!({});
    }

    let mdkb_dir = cwd.join(".mdkb");
    if !mdkb_dir.is_dir() {
        return json!({});
    }

    let cfg = hooks_config(&cwd);
    if !cfg.session_start_enabled {
        return json!({});
    }

    let ctx = match Context::open(&cwd) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("session-start hook: Context::open failed: {e}");
            return json!({});
        }
    };

    let limit = if cfg.recall_limit > 0 {
        cfg.recall_limit.max(20)
    } else {
        50
    };
    let lines = match get_warmup_index(&ctx.conn, limit) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("session-start hook: get_warmup_index failed: {e}");
            return json!({});
        }
    };

    if lines.is_empty() {
        return json!({});
    }

    let mut body = String::from("## mdkb memory warmup\n\n");
    for line in &lines {
        body.push_str("- ");
        body.push_str(line);
        body.push('\n');
    }

    let elapsed_ms = start.elapsed().as_millis();
    if elapsed_ms > cfg.latency_budget_ms as u128 {
        log_slow_hook(&cwd, "session-start", elapsed_ms, cfg.latency_budget_ms);
        body.push_str(
            "\n_(truncated: hook exceeded latency budget — run `mdkb memory list` for full view)_\n",
        );
    }

    json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": body,
        }
    })
}

fn handle_user_prompt_submit(_event: Value) -> Value {
    json!({})
}

fn handle_post_tool_use(_event: Value) -> Value {
    json!({})
}
