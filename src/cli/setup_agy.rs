//! `mdkb setup mcp|hooks agy`: register mdkb with Antigravity CLI (`agy`).
//!
//! agy reads MCP servers from `~/.gemini/config/mcp_config.json`
//! (`mcpServers.<name>`) and hooks from `~/.gemini/config/hooks.json`, an object
//! of named groups. mdkb owns the `mdkb` entry in each file and leaves every
//! other entry as it was. The hook commands call `mdkb hook agy <event>`, which
//! speaks agy's hook format (see [`crate::cli::host_agy`]).

use std::path::PathBuf;

use serde_json::{Value, json};

use super::host_agy::AgyEvent;
use super::setup::{find_mdkb_binary, hook_command_line, locked_read_modify_write, read_json_file};
use crate::error::{Error, ErrorKind, Result};

/// The key mdkb owns in both agy files.
const ENTRY: &str = "mdkb";

/// Result of an agy setup or removal: the file and its content after the change.
#[derive(Debug)]
pub struct AgySetupResult {
    pub path: PathBuf,
    pub merged: Value,
    pub message: String,
}

/// `~/.gemini/config`, refusing when `~/.gemini` is absent: agy is not installed.
fn agy_config_dir(cmd: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| {
        Error::from(ErrorKind::Command {
            command: cmd.to_string(),
            message: "HOME environment variable not set".to_string(),
        })
    })?;
    let gemini = PathBuf::from(home).join(".gemini");
    if !gemini.exists() {
        return Err(Error::from(ErrorKind::Command {
            command: cmd.to_string(),
            message: format!(
                "{} does not exist — Antigravity CLI (agy) is not installed",
                gemini.display()
            ),
        }));
    }
    Ok(gemini.join("config"))
}

/// The `mdkb` hook group: one handler per [`AgyEvent`]. Tool events take a
/// matcher list; the others take handlers directly under the event.
fn hook_group(binary_path: &str) -> Value {
    let mut group = json!({});
    for (event, agy_name) in AgyEvent::ALL {
        let handler = json!({
            "type": "command",
            "command": hook_command_line(binary_path, &format!("agy {}", event.cli_name()), false),
            "timeout": 30,
        });
        group[agy_name] = if event == AgyEvent::PostToolUse {
            json!([{ "matcher": ".*", "hooks": [handler] }])
        } else {
            json!([handler])
        };
    }
    group
}

/// Replace the `mdkb` entry under `parent` (or at the top level when `parent`
/// is `None`) of a JSON file, or remove it when `value` is `None`.
fn upsert_entry(
    path: &PathBuf,
    parent: Option<&str>,
    value: Option<Value>,
    cmd: &str,
    dry_run: bool,
) -> Result<(Value, bool)> {
    let apply = |mut doc: Value| -> (Value, bool) {
        if !doc.is_object() {
            doc = json!({});
        }
        let target = match parent {
            Some(key) => {
                let slot = doc.as_object_mut().unwrap().entry(key).or_insert(json!({}));
                if !slot.is_object() {
                    *slot = json!({});
                }
                slot
            }
            None => &mut doc,
        };
        let map = target.as_object_mut().unwrap();
        let changed = match &value {
            Some(v) => map.insert(ENTRY.to_string(), v.clone()).as_ref() != Some(v),
            None => map.remove(ENTRY).is_some(),
        };
        (doc, changed)
    };
    if dry_run {
        let (doc, changed) = apply(read_json_file(path, cmd)?);
        println!("{}", serde_json::to_string_pretty(&doc).unwrap_or_default());
        return Ok((doc, changed));
    }
    let mut changed = false;
    let mut written = serde_json::Value::Null;
    // Return the written value: a read after the lock races a concurrent
    // writer's rename, which Windows refuses while the target is open.
    locked_read_modify_write(path, || {
        let (doc, did) = apply(read_json_file(path, cmd)?);
        changed = did;
        written = doc.clone();
        Ok(doc)
    })?;
    Ok((written, changed))
}

/// Register mdkb as an MCP server in `~/.gemini/config/mcp_config.json`.
pub fn handle_setup_mcp_agy(dry_run: bool) -> Result<AgySetupResult> {
    let cmd = "setup mcp agy";
    let path = agy_config_dir(cmd)?.join("mcp_config.json");
    let server = json!({ "command": find_mdkb_binary()?, "args": ["mcp"] });
    let (merged, _) = upsert_entry(&path, Some("mcpServers"), Some(server), cmd, dry_run)?;
    Ok(AgySetupResult {
        message: if dry_run {
            "Dry run: no changes written".to_string()
        } else {
            format!("mdkb MCP server registered in {}", path.display())
        },
        path,
        merged,
    })
}

/// Register the mdkb hook group in `~/.gemini/config/hooks.json`.
pub fn handle_setup_hooks_agy(dry_run: bool) -> Result<AgySetupResult> {
    let cmd = "setup hooks agy";
    let path = agy_config_dir(cmd)?.join("hooks.json");
    let group = hook_group(&find_mdkb_binary()?);
    let (merged, _) = upsert_entry(&path, None, Some(group), cmd, dry_run)?;
    Ok(AgySetupResult {
        message: if dry_run {
            "Dry run: no changes written".to_string()
        } else {
            format!(
                "mdkb hooks registered in {} (SessionStart, PreInvocation, PostToolUse, Stop)",
                path.display()
            )
        },
        path,
        merged,
    })
}

/// Remove the mdkb MCP server from agy.
pub fn handle_remove_mcp_agy() -> Result<String> {
    remove(
        "mcp_config.json",
        Some("mcpServers"),
        "setup remove mcp agy",
        "MCP server",
    )
}

/// Remove the mdkb hook group from agy.
pub fn handle_remove_hooks_agy() -> Result<String> {
    remove("hooks.json", None, "setup remove hooks agy", "hook group")
}

fn remove(file: &str, parent: Option<&str>, cmd: &str, what: &str) -> Result<String> {
    let path = agy_config_dir(cmd)?.join(file);
    if !path.exists() {
        return Ok(format!(
            "{} does not exist — nothing to remove",
            path.display()
        ));
    }
    let (_, removed) = upsert_entry(&path, parent, None, cmd, false)?;
    Ok(if removed {
        format!("Removed the mdkb {what} from {}", path.display())
    } else {
        format!("mdkb {what} was not registered in {}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: tool events written like the others (agy then loads no
    /// matcher and never runs the reindex), or a command that calls the
    /// Claude-format hook instead of `hook agy <event>`.
    #[test]
    fn hook_group_matches_the_agy_file_shape() {
        let group = hook_group("/bin/mdkb");
        assert_eq!(
            group["PostToolUse"][0]["matcher"], ".*",
            "tool events need a matcher list: {group}"
        );
        assert_eq!(
            group["PostToolUse"][0]["hooks"][0]["command"],
            "'/bin/mdkb' hook agy post-tool-use"
        );
        assert_eq!(
            group["SessionStart"][0]["command"],
            "'/bin/mdkb' hook agy session-start"
        );
        assert_eq!(
            group["PreInvocation"][0]["command"],
            "'/bin/mdkb' hook agy pre-invocation"
        );
        assert!(
            group.get("PreToolUse").is_none(),
            "a PreToolUse reply without a decision denies the tool in agy"
        );
    }

    /// Catches: setup or removal touching another tool's hook group or MCP
    /// server in the shared agy files.
    #[test]
    fn upsert_and_remove_leave_other_entries_alone() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("mcp_config.json");
        std::fs::write(&path, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        let server = json!({"command": "/bin/mdkb", "args": ["mcp"]});
        upsert_entry(&path, Some("mcpServers"), Some(server.clone()), "t", false).unwrap();
        let doc = read_json_file(&path, "t").unwrap();
        assert_eq!(doc["mcpServers"]["mdkb"], server);
        assert_eq!(doc["mcpServers"]["other"]["command"], "x");
        let (_, removed) = upsert_entry(&path, Some("mcpServers"), None, "t", false).unwrap();
        assert!(removed);
        let doc = read_json_file(&path, "t").unwrap();
        assert!(doc["mcpServers"].get("mdkb").is_none());
        assert_eq!(doc["mcpServers"]["other"]["command"], "x");
    }
}
