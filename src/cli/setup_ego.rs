//! Register MDKB through ego's generic lifecycle hook contract.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::setup::find_installed_mdkb_binary;
use crate::error::{Error, ErrorKind, Result};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

const EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PreToolUse", "pre-tool-use"),
    ("PostToolUse", "post-tool-use"),
    ("PostToolUseFailure", "post-tool-use"),
    ("Stop", "stop"),
];

#[derive(Debug, PartialEq, Eq)]
pub enum EgoSetupOutcome {
    Written,
    Unchanged,
}

#[derive(Debug)]
pub struct EgoSetupResult {
    pub config_path: PathBuf,
    pub binary_path: String,
    pub outcome: EgoSetupOutcome,
    pub dry_run: bool,
    pub merged_toml: String,
}

/// `$EGO_HOME/config.toml`, else `$HOME/.ego/config.toml` (ego's own resolution).
fn ego_config_path() -> Result<PathBuf> {
    let dir = env::var_os("EGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".ego")))
        .ok_or_else(|| cmd_error("EGO_HOME and HOME are both unset".into()))?;
    Ok(dir.join("config.toml"))
}

fn cmd_error(message: String) -> Error {
    Error::from(ErrorKind::Command {
        command: "setup hooks ego".to_string(),
        message,
    })
}

fn io_error(path: &Path, operation: String) -> Error {
    Error::from(ErrorKind::Io {
        path: path.to_path_buf(),
        operation,
    })
}

/// Merge generic hooks, executable admission, and exact command permissions.
/// `force` is retained for CLI compatibility; no conflicting values are replaced.
pub fn handle_setup_hooks_ego(_force: bool, dry_run: bool) -> Result<EgoSetupResult> {
    edit_hooks(false, dry_run)
}

pub fn handle_remove_hooks_ego() -> Result<EgoSetupResult> {
    edit_hooks(true, false)
}

fn hook_table(binary: &str, event: &str, arg: &str) -> Table {
    let mut hook = Table::new();
    hook["event"] = value(event);
    let mut handler = Table::new();
    handler["type"] = value("command");
    handler["command"] = value(binary);
    handler["args"] = value(["hook", arg].into_iter().collect::<Array>());
    handler["timeout"] = value(10);
    hook["handler"] = Item::Table(handler);
    hook
}

// Compare values, not formatting. A user matcher, timeout, or extra field is
// not our registration and must not be removed or overwritten.
fn same_hook(left: &Table, right: &Table) -> bool {
    fn values(table: &Table) -> Option<toml::Value> {
        DocumentMut::from(table.clone()).to_string().parse().ok()
    }
    values(left).zip(values(right)).is_some_and(|(a, b)| a == b)
}

fn merge_array(item: &mut Item, values: &[String], remove: bool, name: &str) -> Result<()> {
    if item.is_none() {
        if remove {
            return Ok(());
        }
        *item = value(Array::new());
    }
    let array = item
        .as_array_mut()
        .ok_or_else(|| cmd_error(format!("{name} must be an array")))?;
    if remove {
        array.retain(|v| {
            !v.as_str()
                .is_some_and(|v| values.iter().any(|owned| owned == v))
        });
    } else {
        for entry in values {
            if !array.iter().any(|v| v.as_str() == Some(entry)) {
                array.push(entry.as_str());
            }
        }
    }
    Ok(())
}

fn edit_hooks(remove: bool, dry_run: bool) -> Result<EgoSetupResult> {
    let config_path = ego_config_path()?;
    let binary_path = find_installed_mdkb_binary()?;
    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_error(&config_path, format!("read config.toml: {e}"))),
    };
    let mut doc: DocumentMut = raw
        .parse()
        .map_err(|e| cmd_error(format!("failed to parse {}: {e}", config_path.display())))?;
    let owned: Vec<Table> = EVENTS
        .iter()
        .map(|(event, arg)| hook_table(&binary_path, event, arg))
        .collect();
    if !remove {
        // Keep owner notes attached to the obsolete assignment, not the key.
        let prefix = doc
            .as_table()
            .key("mdkb_prompt_command")
            .and_then(|key| key.leaf_decor().prefix())
            .and_then(|raw| raw.as_str())
            .unwrap_or_default();
        let suffix = doc
            .get("mdkb_prompt_command")
            .and_then(Item::as_value)
            .and_then(|value| value.decor().suffix())
            .and_then(|raw| raw.as_str())
            .unwrap_or_default();
        let mut comments = String::new();
        for text in [prefix, suffix]
            .into_iter()
            .filter(|text| text.contains('#'))
        {
            comments.push_str(text.trim());
            comments.push('\n');
        }
        if !comments.is_empty() {
            doc.set_trailing(format!(
                "{}{}",
                doc.trailing().as_str().unwrap_or_default(),
                comments
            ));
        }
        doc.remove("mdkb_prompt_command");
    }
    if doc.contains_key("hooks") || !remove {
        if !doc.contains_key("hooks") {
            doc["hooks"] = Item::ArrayOfTables(ArrayOfTables::new());
        }
        if doc["hooks"].as_array().is_some() {
            let old = doc["hooks"].clone();
            let prefix = doc
                .as_table_mut()
                .key_mut("hooks")
                .and_then(|key| {
                    key.leaf_decor()
                        .prefix()
                        .and_then(|raw| raw.as_str())
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            let suffix = old
                .as_value()
                .and_then(|v| v.decor().suffix())
                .and_then(|raw| raw.as_str())
                .unwrap_or_default()
                .to_owned();
            if let Some(mut key) = doc.as_table_mut().key_mut("hooks") {
                key.leaf_decor_mut().clear();
                key.dotted_decor_mut().clear();
            }
            let mut hooks = if old.as_array().is_some_and(Array::is_empty) {
                ArrayOfTables::new()
            } else {
                old.into_array_of_tables()
                    .map_err(|_| cmd_error("hooks must contain tables".into()))?
            };
            if !remove && hooks.is_empty() {
                let first = owned[0].clone();
                hooks.push(first);
            }
            if let Some(first) = hooks.get_mut(0) {
                first.decor_mut().set_prefix(prefix);
                first.decor_mut().set_suffix(suffix);
            }
            doc["hooks"] = Item::ArrayOfTables(hooks);
        }
        let hooks = doc["hooks"]
            .as_array_of_tables_mut()
            .ok_or_else(|| cmd_error("hooks must be an array of tables".into()))?;
        if remove {
            hooks.retain(|hook| !owned.iter().any(|entry| same_hook(hook, entry)));
        } else {
            for hook in owned {
                if !hooks.iter().any(|entry| same_hook(entry, &hook)) {
                    hooks.push(hook);
                }
            }
        }
    }
    if doc.contains_key("exec_paths") || !remove {
        merge_array(
            &mut doc["exec_paths"],
            std::slice::from_ref(&binary_path),
            remove,
            "exec_paths",
        )?;
    }
    if doc.contains_key("permissions") || !remove {
        if !doc.contains_key("permissions") {
            doc["permissions"] = Item::Table(Table::new());
        }
        let permissions = doc["permissions"]
            .as_table_like_mut()
            .ok_or_else(|| cmd_error("permissions must be a table".into()))?;
        let mut rules: Vec<String> = EVENTS
            .iter()
            .map(|(_, arg)| format!("exec({binary_path} hook {arg})"))
            .collect();
        rules.sort();
        rules.dedup();
        if !permissions.contains_key("allow") && !remove {
            permissions.insert("allow", value(Array::new()));
        }
        if let Some(allow) = permissions.get_mut("allow") {
            merge_array(allow, &rules, remove, "permissions.allow")?;
        }
    }
    let merged_toml = doc.to_string();
    merged_toml
        .parse::<toml::Value>()
        .map_err(|e| cmd_error(format!("merged config is invalid: {e}")))?;
    let outcome = if merged_toml == raw {
        EgoSetupOutcome::Unchanged
    } else {
        EgoSetupOutcome::Written
    };
    if outcome == EgoSetupOutcome::Written && !dry_run {
        write_atomic(&config_path, &merged_toml)?;
    }
    Ok(EgoSetupResult {
        config_path,
        binary_path,
        outcome,
        dry_run,
        merged_toml,
    })
}

/// Replace the file atomically; a symlinked config is written through, not replaced.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| io_error(dir, format!("create dir: {e}")))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .map_err(|e| io_error(dir, format!("create temp file: {e}")))?;
    tmp.write_all(content.as_bytes())
        .map_err(|e| io_error(dir, format!("write temp file: {e}")))?;
    tmp.persist(&target)
        .map_err(|e| io_error(&target, format!("rename temp -> config.toml: {e}")))?;
    Ok(())
}
