//! Hook protocol adapter for Antigravity CLI (`agy`).
//!
//! agy runs hooks with its own wire format, measured on agy 1.5.10
//! (2026-11-23): stdin carries `cwd`, `conversationId`, `lastUserInput` and
//! `toolCall {name, args}`, and text reaches the model only as
//! `injectSteps: [{"systemMessage": {"systemMessage": text}}]` from
//! `SessionStart` and `PreInvocation`. This module maps an agy event to the
//! event shape the mdkb dispatcher reads, and the dispatcher's reply back.

use serde_json::{Value, json};

/// The agy hook events mdkb registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum AgyEvent {
    /// A conversation starts: memory warmup.
    SessionStart,
    /// Before each model call: prompt recall on the first call of a turn.
    PreInvocation,
    /// After a tool ran: reindex of the file it wrote.
    PostToolUse,
    /// The turn ended.
    Stop,
}

impl AgyEvent {
    /// Every event, with the agy event name it is registered under.
    pub const ALL: [(AgyEvent, &'static str); 4] = [
        (AgyEvent::SessionStart, "SessionStart"),
        (AgyEvent::PreInvocation, "PreInvocation"),
        (AgyEvent::PostToolUse, "PostToolUse"),
        (AgyEvent::Stop, "Stop"),
    ];

    /// The argument `mdkb hook agy` takes for this event.
    pub fn cli_name(self) -> &'static str {
        match self {
            AgyEvent::SessionStart => "session-start",
            AgyEvent::PreInvocation => "pre-invocation",
            AgyEvent::PostToolUse => "post-tool-use",
            AgyEvent::Stop => "stop",
        }
    }
}

/// The dispatcher method and the event to send it, or `None` when this agy
/// event needs no mdkb work.
///
/// `PreInvocation` fires before every model call of a turn; `invocationNum`
/// restarts at 0 on each user prompt, so recall runs on call 0 only.
pub fn translate(event: AgyEvent, input: &Value) -> Option<(&'static str, Value)> {
    let mut out = json!({});
    if let Some(id) = input.get("conversationId").and_then(Value::as_str) {
        out["session_id"] = json!(id);
    }
    let cwd = input.get("cwd").and_then(Value::as_str).or_else(|| {
        input
            .get("workspacePaths")
            .and_then(|p| p.get(0))
            .and_then(Value::as_str)
    });
    if let Some(cwd) = cwd {
        out["cwd"] = json!(cwd);
    }
    let method = match event {
        AgyEvent::SessionStart => {
            out["hook_event_name"] = json!("SessionStart");
            out["source"] = json!("startup");
            "hook.session_start"
        }
        AgyEvent::PreInvocation => {
            if input
                .get("invocationNum")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                != 0
            {
                return None;
            }
            out["hook_event_name"] = json!("UserPromptSubmit");
            out["prompt"] = input.get("lastUserInput").cloned().unwrap_or(json!(""));
            "hook.user_prompt_submit"
        }
        AgyEvent::PostToolUse => {
            let call = input.get("toolCall")?;
            let (name, tool_input) = tool_call(call);
            out["hook_event_name"] = json!("PostToolUse");
            out["tool_name"] = json!(name);
            out["tool_input"] = tool_input;
            "hook.post_tool_use"
        }
        // No transcript_path: agy's transcript is not the format prior mining
        // reads, so the stop hook runs its other work and mines nothing.
        AgyEvent::Stop => {
            out["hook_event_name"] = json!("Stop");
            "hook.stop"
        }
    };
    Some((method, out))
}

/// agy tool call as the Claude-shaped `(tool_name, tool_input)` the dispatcher
/// knows: file writes become `Write`/`Edit` with `file_path`, shell commands
/// become `Bash` with `command`. agy's tool names differ from its own docs
/// (`read_file` with `Path` was measured, `view_file` with `AbsolutePath` is
/// documented), so the mapping keys on name fragments and argument names.
fn tool_call(call: &Value) -> (String, Value) {
    let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
    let args = call.get("args").cloned().unwrap_or(json!({}));
    let path = [
        "TargetFile",
        "Path",
        "AbsolutePath",
        "FilePath",
        "file_path",
    ]
    .iter()
    .find_map(|key| args.get(*key).and_then(Value::as_str))
    .map(str::to_string);
    if name == "run_command" {
        let command = args.get("CommandLine").cloned().unwrap_or(json!(""));
        return ("Bash".to_string(), json!({ "command": command }));
    }
    let mapped = if name.contains("write") || name.starts_with("create") {
        "Write"
    } else if name.contains("replace") || name.contains("edit") {
        "Edit"
    } else {
        name
    };
    let mut tool_input = if args.is_object() { args } else { json!({}) };
    if let Some(path) = path {
        tool_input["file_path"] = json!(path);
    }
    (mapped.to_string(), tool_input)
}

/// The dispatcher's reply in agy's format. Only `SessionStart` and
/// `PreInvocation` can carry text to the model; every other reply is `{}`,
/// which agy reads as "nothing to add".
pub fn reply(event: AgyEvent, result: &Value) -> Value {
    if !matches!(event, AgyEvent::SessionStart | AgyEvent::PreInvocation) {
        return json!({});
    }
    match result
        .get("hookSpecificOutput")
        .and_then(|o| o.get("additionalContext"))
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
    {
        Some(text) => json!({ "injectSteps": [{ "systemMessage": { "systemMessage": text } }] }),
        None => json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shapes recorded from agy 1.5.10 on 2026-11-23.
    fn agy(event: &str, extra: Value) -> Value {
        let mut v = json!({
            "conversationId": "c-1",
            "cwd": "/work/repo",
            "workspacePaths": ["/work/repo"],
            "hookEventName": event,
            "lastUserInput": "how is the index built",
        });
        for (k, val) in extra.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    }

    /// Catches: recall running before every model call of a turn instead of
    /// once per prompt (agy fires PreInvocation before each call).
    #[test]
    fn recall_runs_only_on_the_first_model_call_of_a_turn() {
        let (method, event) = translate(
            AgyEvent::PreInvocation,
            &agy("PreInvocation", json!({"invocationNum": 0})),
        )
        .unwrap();
        assert_eq!(method, "hook.user_prompt_submit");
        assert_eq!(event["prompt"], "how is the index built");
        assert_eq!(event["session_id"], "c-1");
        assert_eq!(event["cwd"], "/work/repo");
        assert!(
            translate(
                AgyEvent::PreInvocation,
                &agy("PreInvocation", json!({"invocationNum": 3}))
            )
            .is_none()
        );
    }

    /// Catches: a write through agy's own tool names never reaching reindex,
    /// which keys on `Write`/`Edit` and `file_path`.
    #[test]
    fn agy_file_writes_map_to_reindexable_tools() {
        let cases = [
            ("write_to_file", "TargetFile", "Write"),
            ("replace_file_content", "TargetFile", "Edit"),
            ("multi_replace_file_content", "TargetFile", "Edit"),
        ];
        for (name, key, expected) in cases {
            let input = agy(
                "PostToolUse",
                json!({"toolCall": {"name": name, "args": {key: "/work/repo/a.md"}}}),
            );
            let (_, event) = translate(AgyEvent::PostToolUse, &input).unwrap();
            assert_eq!(event["tool_name"], expected, "{name}");
            assert_eq!(
                event["tool_input"]["file_path"], "/work/repo/a.md",
                "{name}"
            );
        }
        let input = agy(
            "PostToolUse",
            json!({"toolCall": {"name": "run_command", "args": {"CommandLine": "cargo test"}}}),
        );
        let (_, event) = translate(AgyEvent::PostToolUse, &input).unwrap();
        assert_eq!(event["tool_name"], "Bash");
        assert_eq!(event["tool_input"]["command"], "cargo test");
    }

    /// Catches: replying in Claude's shape, which agy ignores, or with the
    /// flat `systemMessage` string its docs show, which never reached the
    /// model in the 2026-11-23 probe; only the nested object did.
    #[test]
    fn context_is_injected_as_a_nested_system_message() {
        let result = json!({"hookSpecificOutput": {"hookEventName": "UserPromptSubmit", "additionalContext": "mdkb recall"}});
        assert_eq!(
            reply(AgyEvent::PreInvocation, &result),
            json!({"injectSteps": [{"systemMessage": {"systemMessage": "mdkb recall"}}]})
        );
        assert_eq!(reply(AgyEvent::PostToolUse, &result), json!({}));
        assert_eq!(reply(AgyEvent::SessionStart, &json!({})), json!({}));
    }

    /// Catches: a Stop event handing agy's transcript to prior mining, which
    /// parses only the Claude transcript format.
    #[test]
    fn stop_sends_no_transcript() {
        let input = agy(
            "Stop",
            json!({"transcriptPath": "/x/transcript_full.jsonl"}),
        );
        let (method, event) = translate(AgyEvent::Stop, &input).unwrap();
        assert_eq!(method, "hook.stop");
        assert!(event.get("transcript_path").is_none());
    }
}
