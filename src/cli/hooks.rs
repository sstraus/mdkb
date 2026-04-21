//! Hook dispatch handlers for Claude Code / Codex lifecycle events.
//!
//! Contract: every handler reads event JSON from stdin (best-effort, may be empty
//! or malformed), optionally writes a JSON response to stdout, and exits 0.
//! When there is nothing to report, the handler stays silent (no stdout output).
//! The host CLI must never be blocked by mdkb — internal failures are logged
//! to stderr and swallowed.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

use crate::cli::HookEvent;
use crate::cli::handlers::Context;
use crate::cli::hook_logic::{
    REINDEX_TOOLS, build_recall_query, canonicalize_under_cwd, classify_definition_search,
    classify_grep_pattern, mdkbignore_hooks_present, prompt_is_wrapup, tool_input_path,
};
use crate::config::{Config, HooksConfig};
use crate::store::memory::{access_recency_score, get_warmup_index, search_entries_fts};

/// Entry point for `mdkb hook <event>`.
pub fn dispatch(event: HookEvent) {
    let start = Instant::now();
    let input = read_stdin_best_effort();
    let event_json = parse_event(&input);

    let response = match event {
        HookEvent::SessionStart => handle_session_start(event_json),
        HookEvent::UserPromptSubmit => handle_user_prompt_submit(event_json),
        HookEvent::PostToolUse => handle_post_tool_use(event_json),
        HookEvent::PreToolUse => handle_pre_tool_use(event_json),
    };

    emit_response(&response);
    log_hook_event(&event, &response, start.elapsed().as_millis());
}

fn log_hook_event(event: &HookEvent, response: &Value, elapsed_ms: u128) {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return,
    };
    let log_path = cwd.join(".mdkb").join("hook-events.jsonl");

    let outcome = if response.get("hookSpecificOutput").is_some() {
        "fired"
    } else {
        "skipped"
    };

    let event_name = match event {
        HookEvent::SessionStart => "SessionStart",
        HookEvent::UserPromptSubmit => "UserPromptSubmit",
        HookEvent::PostToolUse => "PostToolUse",
        HookEvent::PreToolUse => "PreToolUse",
    };

    let line = json!({
        "event": event_name,
        "outcome": outcome,
        "elapsed_ms": elapsed_ms,
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
    match value {
        Value::Null => return,
        Value::Object(o) if o.is_empty() => return,
        _ => {}
    }
    let serialized = match serde_json::to_string(value) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("emit_response: serialization failed: {}", e);
            return;
        }
    };
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{}", serialized);
    let _ = handle.flush();
}

fn hooks_config(root: &std::path::Path) -> HooksConfig {
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
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", line);
        }
        Err(e) => {
            tracing::warn!("log_slow_hook: failed to open {:?}: {}", log_path, e);
        }
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

    let limit = cfg.warmup_limit.max(1);
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

    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "mdkb".to_string());

    let mut body = String::from("## mdkb memory warmup\n\n");
    for line in &lines {
        body.push_str("- ");
        body.push_str(line);
        body.push('\n');
    }

    body.push_str(&format!(
        "\n**mdkb CLI** (semantic search — not available via Grep/Glob). Run `{0} cheatsheet` for full syntax.\n",
        bin
    ));

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

fn handle_user_prompt_submit(event: Value) -> Value {
    let start = Instant::now();
    let cwd: PathBuf = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return json!({}),
    };

    let prompt = match event.get("prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.to_string(),
        _ => return json!({}),
    };

    if prompt_is_wrapup(&prompt) {
        return json!({});
    }

    if mdkbignore_hooks_present(&cwd) {
        return json!({});
    }

    if !cwd.join(".mdkb").is_dir() {
        return json!({});
    }

    let cfg = hooks_config(&cwd);
    if !cfg.user_prompt_submit_enabled {
        return json!({});
    }

    let ctx = match Context::open(&cwd) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("user-prompt-submit hook: Context::open failed: {e}");
            return json!({});
        }
    };

    let fts_query = match build_recall_query(&prompt) {
        Some(q) => q,
        None => return json!({}),
    };

    let limit = cfg.recall_limit.max(1);
    let mut results = match search_entries_fts(&ctx.conn, &fts_query, limit) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("user-prompt-submit hook: search_entries_fts failed: {e}");
            return json!({});
        }
    };

    if results.is_empty() {
        return json!({});
    }

    // Re-rank by combining BM25 position with access-recency boost.
    // BM25 rank is encoded as a descending score (position 0 = highest BM25).
    // access_recency_score is added as a tiebreaker/boost signal.
    if cfg.recall_half_life_secs > 0 {
        let now = chrono::Utc::now().timestamp();
        let n = results.len() as f64;
        let mut scored: Vec<(f64, usize)> = results
            .iter()
            .enumerate()
            .map(|(rank, entry)| {
                let bm25_score = (n - rank as f64) / n; // normalised: 1.0 → 1/n
                let recency = access_recency_score(
                    entry.access_count,
                    entry.last_accessed,
                    now,
                    cfg.recall_half_life_secs,
                );
                (bm25_score + recency, rank)
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let orig = results.clone();
        results = scored.into_iter().map(|(_, i)| orig[i].clone()).collect();
    }

    let mut body = String::from("## mdkb: relevant context\n\n");
    for entry in &results {
        let snippet_raw = entry.content.trim().replace('\n', " ");
        let snippet: String = snippet_raw.chars().take(160).collect();
        body.push_str(&format!("- [{}] {} — {}\n", entry.id, entry.title, snippet));
    }

    let elapsed_ms = start.elapsed().as_millis();
    if elapsed_ms > cfg.latency_budget_ms as u128 {
        log_slow_hook(
            &cwd,
            "user-prompt-submit",
            elapsed_ms,
            cfg.latency_budget_ms,
        );
    }

    json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": body,
        }
    })
}

fn handle_post_tool_use(event: Value) -> Value {
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
    if !cfg.post_tool_use_enabled {
        return json!({});
    }

    let tool_name = match event.get("tool_name").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return json!({}),
    };
    if !REINDEX_TOOLS.contains(&tool_name) {
        return json!({});
    }

    let raw_path = match event.get("tool_input").and_then(tool_input_path) {
        Some(p) => p,
        None => return json!({}),
    };

    // Security: canonicalize and verify the path is under cwd before enqueuing.
    // This prevents path traversal (e.g. "../../etc/passwd") from reaching the queue.
    let path = match canonicalize_under_cwd(&cwd, &raw_path) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "post-tool-use hook: rejected path outside cwd: {}",
                raw_path
            );
            return json!({});
        }
    };

    let queue_path = mdkb_dir.join("reindex-queue.jsonl");
    let line = json!({
        "path": path,
        "tool": tool_name,
        "at": chrono::Utc::now().timestamp(),
    });

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&queue_path)
    {
        if let Err(e) = writeln!(f, "{}", line) {
            tracing::warn!(
                "post-tool-use hook: failed to write reindex queue {}: {}",
                queue_path.display(),
                e
            );
        }
    } else {
        tracing::warn!(
            "post-tool-use hook: failed to open reindex queue {}",
            queue_path.display()
        );
    }

    json!({})
}

fn handle_pre_tool_use(event: Value) -> Value {
    let cwd: PathBuf = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return json!({}),
    };

    if mdkbignore_hooks_present(&cwd) {
        return json!({});
    }

    if !cwd.join(".mdkb").is_dir() {
        return json!({});
    }

    let cfg = hooks_config(&cwd);
    if !cfg.pre_tool_use_enabled {
        return json!({});
    }

    let tool_name = match event.get("tool_name").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return json!({}),
    };

    if tool_name != "Grep" {
        return json!({});
    }

    let tool_input = match event.get("tool_input") {
        Some(v) => v,
        None => return json!({}),
    };

    let pattern = match tool_input.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return json!({}),
    };

    let path = tool_input.get("path").and_then(|v| v.as_str());

    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "mdkb".to_string());

    // Try classifiers in order: definition search, callsite, then pure symbol.
    let suggestion = classify_definition_search(pattern, &bin)
        .or_else(|| classify_grep_pattern(pattern, path, &bin));

    match suggestion {
        Some(text) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "additionalContext": text,
            }
        }),
        None => json!({}),
    }
}
