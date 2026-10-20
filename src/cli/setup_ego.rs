//! `mdkb setup hooks ego` — register mdkb as ego's prompt-context source.

use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::setup::find_installed_mdkb_binary;
use crate::error::{Error, ErrorKind, Result};

const KEY: &str = "mdkb_prompt_command";

/// What `mdkb setup hooks ego` did to the `mdkb_prompt_command` key.
#[derive(Debug, PartialEq, Eq)]
pub enum EgoSetupOutcome {
    /// The key was absent, or replaced under `--force`.
    Written,
    /// The key already held this binary path.
    Unchanged,
    /// The key held another value and `--force` was not given.
    KeptExisting(String),
}

/// Result of `mdkb setup hooks ego`.
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

/// Set the top-level `mdkb_prompt_command` key in ego's user `config.toml`.
///
/// Edits through `toml_edit`, so unrelated tables, keys and comments survive
/// byte-for-byte. A different existing value is kept unless `force` is set.
pub fn handle_setup_hooks_ego(force: bool, dry_run: bool) -> Result<EgoSetupResult> {
    let config_path = ego_config_path()?;
    let binary_path = find_installed_mdkb_binary()?;

    let raw = match std::fs::read_to_string(&config_path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(io_error(&config_path, format!("read config.toml: {e}"))),
    };
    let mut doc: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| cmd_error(format!("failed to parse {}: {e}", config_path.display())))?;

    let outcome = match doc.get(KEY) {
        Some(item) if item.as_str() == Some(binary_path.as_str()) => EgoSetupOutcome::Unchanged,
        Some(_) if !force => {
            let existing = doc[KEY]
                .as_value()
                .map(|v| v.to_string().trim().trim_matches('"').to_string())
                .unwrap_or_default();
            EgoSetupOutcome::KeptExisting(existing)
        }
        _ => EgoSetupOutcome::Written,
    };

    if outcome == EgoSetupOutcome::Written {
        let mut new_value = toml_edit::Value::from(binary_path.as_str());
        if let Some(old) = doc.get(KEY).and_then(|i| i.as_value()) {
            *new_value.decor_mut() = old.decor().clone();
        }
        doc[KEY] = toml_edit::Item::Value(new_value);
    }
    let merged_toml = doc.to_string();

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
