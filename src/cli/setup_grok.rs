//! `mdkb setup mcp|hooks grok`: register mdkb with the Grok CLI.
//!
//! grok reads MCP servers from `~/.grok/config.toml` (`[mcp_servers.<name>]`,
//! the same table Codex uses) and hooks from every `~/.grok/hooks/*.json`, in
//! the Claude Code hook format. mdkb owns `~/.grok/hooks/mdkb.json`.
//!
//! Measured on grok 1.0.89 (2026-11-23): context from `SessionStart` and
//! `UserPromptSubmit` never reaches the model, so warmup and prompt recall
//! cannot work through grok hooks; `PreToolUse`/`PostToolUse` context does.
//! grok also runs the hooks in `~/.claude/settings.json` by default
//! (`[compat.claude] hooks`), so mdkb hooks registered for Claude Code already
//! fire in grok.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::setup::{claude_profile_dir, find_mdkb_binary, hook_command_line, is_mdkb_hook_entry};
use crate::error::{Error, ErrorKind, Result};

/// The grok events mdkb registers, with the `mdkb hook` event each one runs.
/// `SessionStart` and `UserPromptSubmit` are left out: grok drops their context.
const GROK_EVENTS: &[(&str, &str)] = &[
    ("PreToolUse", "pre-tool-use"),
    ("PostToolUse", "post-tool-use"),
    ("PostToolUseFailure", "post-tool-use"),
    ("Stop", "stop"),
];

/// Result of a grok setup: the file written and its new content.
#[derive(Debug)]
pub struct GrokSetupResult {
    pub path: PathBuf,
    pub content: String,
    pub message: String,
}

/// `~/.grok`, refusing when it is absent: grok is not installed.
fn grok_home(cmd: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| command_error(cmd, "HOME environment variable not set"))?;
    let dir = PathBuf::from(home).join(".grok");
    if !dir.exists() {
        return Err(command_error(
            cmd,
            &format!(
                "{} does not exist — the Grok CLI is not installed",
                dir.display()
            ),
        ));
    }
    Ok(dir)
}

fn command_error(cmd: &str, message: &str) -> Error {
    Error::from(ErrorKind::Command {
        command: cmd.to_string(),
        message: message.to_string(),
    })
}

fn io_error(path: &Path, operation: &str, e: std::io::Error) -> Error {
    Error::from(ErrorKind::Io {
        path: path.to_path_buf(),
        operation: format!("{operation}: {e}"),
    })
}

/// The content of `~/.grok/hooks/mdkb.json`.
fn hooks_file(binary_path: &str) -> Value {
    let mut hooks = json!({});
    for (event, cli_event) in GROK_EVENTS {
        hooks[*event] = json!([{
            "hooks": [{ "type": "command", "command": hook_command_line(binary_path, cli_event, false) }]
        }]);
    }
    json!({ "hooks": hooks })
}

/// Whether grok will also run the mdkb hooks of `claude_settings`: it reads
/// that file unless `[compat.claude] hooks = false` in its config.
fn claude_hooks_already_run(claude_settings: &Value, grok_config: &str) -> bool {
    let compat_on = grok_config
        .parse::<toml_edit::DocumentMut>()
        .ok()
        .and_then(|doc| doc.get("compat")?.get("claude")?.get("hooks")?.as_bool())
        .unwrap_or(true);
    let has_mdkb = claude_settings
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|events| {
            GROK_EVENTS.iter().any(|(event, cli_event)| {
                events
                    .get(*event)
                    .and_then(Value::as_array)
                    .is_some_and(|groups| groups.iter().any(|g| is_mdkb_hook_entry(g, cli_event)))
            })
        });
    compat_on && has_mdkb
}

/// Write `~/.grok/hooks/mdkb.json`. Refused, unless `force`, when grok already
/// runs mdkb hooks from `~/.claude/settings.json`: each event would run twice.
pub fn handle_setup_hooks_grok(dry_run: bool, force: bool) -> Result<GrokSetupResult> {
    let cmd = "setup hooks grok";
    let home = grok_home(cmd)?;
    if !force {
        let claude = super::setup::read_settings_lenient(
            claude_profile_dir().map(|dir| dir.join("settings.json")),
        );
        let grok_config = std::fs::read_to_string(home.join("config.toml")).unwrap_or_default();
        if claude_hooks_already_run(&claude, &grok_config) {
            return Err(command_error(
                cmd,
                "grok already runs the mdkb hooks in ~/.claude/settings.json ([compat.claude] hooks); \
                 registering them again would run each event twice. Set `[compat.claude] hooks = false` \
                 in ~/.grok/config.toml, or pass --force",
            ));
        }
    }
    let path = home.join("hooks").join("mdkb.json");
    let content =
        serde_json::to_string_pretty(&hooks_file(&find_mdkb_binary()?)).unwrap_or_default();
    if dry_run {
        println!("{content}");
    } else {
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|e| io_error(&path, "create hooks dir", e))?;
        std::fs::write(&path, format!("{content}\n")).map_err(|e| io_error(&path, "write", e))?;
    }
    Ok(GrokSetupResult {
        message: if dry_run {
            "Dry run: no changes written".to_string()
        } else {
            format!(
                "mdkb hooks registered in {} (PreToolUse, PostToolUse, Stop). Grok drops SessionStart \
                 and UserPromptSubmit context, so use the mdkb MCP tools for recall",
                path.display()
            )
        },
        path,
        content,
    })
}

/// Remove `~/.grok/hooks/mdkb.json`.
pub fn handle_remove_hooks_grok() -> Result<String> {
    let path = grok_home("setup remove hooks grok")?
        .join("hooks")
        .join("mdkb.json");
    if !path.exists() {
        return Ok(format!(
            "{} does not exist — nothing to remove",
            path.display()
        ));
    }
    std::fs::remove_file(&path).map_err(|e| io_error(&path, "remove", e))?;
    Ok(format!("Removed {}", path.display()))
}

fn read_config(path: &Path, cmd: &str) -> Result<toml_edit::DocumentMut> {
    if !path.exists() {
        return Ok(toml_edit::DocumentMut::new());
    }
    let raw = std::fs::read_to_string(path).map_err(|e| io_error(path, "read config.toml", e))?;
    raw.parse::<toml_edit::DocumentMut>()
        .map_err(|e| command_error(cmd, &format!("failed to parse {}: {e}", path.display())))
}

/// Register mdkb in `[mcp_servers.mdkb]` of `~/.grok/config.toml`, keeping
/// every other key and comment.
pub fn handle_setup_mcp_grok(dry_run: bool) -> Result<GrokSetupResult> {
    let cmd = "setup mcp grok";
    let path = grok_home(cmd)?.join("config.toml");
    let mut doc = read_config(&path, cmd)?;
    if !doc.contains_key("mcp_servers") {
        doc["mcp_servers"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let servers = doc["mcp_servers"]
        .as_table_mut()
        .ok_or_else(|| command_error(cmd, "`mcp_servers` must be a TOML table"))?;
    let mut server = toml_edit::Table::new();
    server.insert("command", toml_edit::value(find_mdkb_binary()?));
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    server.insert("args", toml_edit::value(args));
    servers.insert("mdkb", toml_edit::Item::Table(server));
    let content = doc.to_string();
    if dry_run {
        println!("{content}");
    } else {
        std::fs::write(&path, &content).map_err(|e| io_error(&path, "write config.toml", e))?;
    }
    Ok(GrokSetupResult {
        message: if dry_run {
            "Dry run: no changes written".to_string()
        } else {
            format!("mdkb MCP server registered in {}", path.display())
        },
        path,
        content,
    })
}

/// Remove `[mcp_servers.mdkb]` from `~/.grok/config.toml`.
pub fn handle_remove_mcp_grok() -> Result<String> {
    let cmd = "setup remove mcp grok";
    let path = grok_home(cmd)?.join("config.toml");
    let mut doc = read_config(&path, cmd)?;
    let removed = doc
        .get_mut("mcp_servers")
        .and_then(|v| v.as_table_mut())
        .is_some_and(|servers| servers.remove("mdkb").is_some());
    if !removed {
        return Ok(format!("mdkb was not registered in {}", path.display()));
    }
    std::fs::write(&path, doc.to_string()).map_err(|e| io_error(&path, "write config.toml", e))?;
    Ok(format!(
        "Removed [mcp_servers.mdkb] from {}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claude_with_mdkb() -> Value {
        json!({"hooks": {"PreToolUse": [{"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "'/b/mdkb' hook pre-tool-use"}]}]}})
    }

    /// Catches: registering grok hooks while grok already runs the Claude
    /// copies, which runs every mdkb event twice per tool call.
    #[test]
    fn claude_hooks_count_unless_grok_compat_is_off() {
        assert!(claude_hooks_already_run(&claude_with_mdkb(), ""));
        assert!(claude_hooks_already_run(
            &claude_with_mdkb(),
            "[compat.claude]\nskills = false\n"
        ));
        assert!(!claude_hooks_already_run(
            &claude_with_mdkb(),
            "[compat.claude]\nhooks = false\n"
        ));
        assert!(!claude_hooks_already_run(&json!({"hooks": {}}), ""));
    }

    /// Catches: registering the prompt and session events grok drops, which
    /// costs a recall per prompt for text the model never sees.
    #[test]
    fn hooks_file_registers_only_events_grok_delivers() {
        let file = hooks_file("/b/mdkb");
        let events: Vec<_> = file["hooks"].as_object().unwrap().keys().cloned().collect();
        assert!(!events.contains(&"SessionStart".to_string()));
        assert!(!events.contains(&"UserPromptSubmit".to_string()));
        assert_eq!(
            file["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "'/b/mdkb' hook pre-tool-use"
        );
    }
}
