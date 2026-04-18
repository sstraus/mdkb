//! Setup command handlers for configuring mdkb integrations.

use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::error::{Error, ErrorKind, Result};

/// Result of MCP setup operation.
#[derive(Debug)]
pub struct McpSetupResult {
    /// Whether the setup was successful.
    pub success: bool,
    /// Scope of the installation (global or local).
    pub scope: McpScope,
    /// Path to the mdkb binary.
    pub binary_path: String,
    /// Working directory for the MCP server.
    pub cwd: String,
    /// Any messages from the setup process.
    pub message: String,
}

/// Scope of MCP installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpScope {
    /// Global (user-wide) installation.
    Global,
    /// Local (project-specific) installation.
    Local,
}

impl std::fmt::Display for McpScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpScope::Global => write!(f, "global"),
            McpScope::Local => write!(f, "local"),
        }
    }
}

/// Handle MCP setup for Claude Code.
///
/// Registers mdkb as an MCP server using the `claude mcp add` command.
///
/// # Arguments
/// * `cwd` - Current working directory (used for local installs and as server cwd)
/// * `global` - Whether to install globally (--scope user) or locally (--scope project)
/// * `skip_confirm` - Skip confirmation prompt
pub fn handle_setup_mcp_claude(
    cwd: &Path,
    global: bool,
    skip_confirm: bool,
) -> Result<McpSetupResult> {
    let scope = if global {
        McpScope::Global
    } else {
        McpScope::Local
    };

    // Find the mdkb binary
    let binary_path = find_mdkb_binary()?;

    // Determine the cwd for the MCP server
    let server_cwd = cwd.canonicalize().map_err(|e| {
        Error::from(ErrorKind::Io {
            path: cwd.to_path_buf(),
            operation: format!("canonicalize: {}", e),
        })
    })?;
    let server_cwd_str = server_cwd.to_string_lossy().to_string();

    // Show what we're about to do and get confirmation
    if !skip_confirm {
        println!("mdkb MCP Setup for Claude Code");
        println!("================================");
        println!();
        println!("This will register mdkb as an MCP server with the following settings:");
        println!();
        println!("  Name:    mdkb");
        println!("  Binary:  {}", binary_path);
        println!("  Args:    serve");
        println!("  Scope:   {}", scope);
        println!();

        if scope == McpScope::Local {
            println!("Note: Project-scoped installation.");
            println!("      The server will run in: {}", server_cwd_str);
            println!("      Ensure .mdkb/ directory exists in this project.");
            println!();
        } else {
            println!("IMPORTANT: Global installation limitations:");
            println!("  - The MCP server will run from wherever Claude Code starts it");
            println!("  - You must have .mdkb/ directory in your home directory OR");
            println!("  - Run Claude Code from a directory with .mdkb/ initialized");
            println!("  - Consider using --project scope instead for project-specific setups");
            println!();
        }

        print!("Proceed? [Y/n] ");
        io::stdout().flush().map_err(|e| {
            Error::from(ErrorKind::Io {
                path: cwd.to_path_buf(),
                operation: format!("flush stdout: {}", e),
            })
        })?;

        let mut input = String::new();
        io::stdin().read_line(&mut input).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: cwd.to_path_buf(),
                operation: format!("read stdin: {}", e),
            })
        })?;

        let input = input.trim().to_lowercase();
        if !input.is_empty() && input != "y" && input != "yes" {
            return Ok(McpSetupResult {
                success: false,
                scope,
                binary_path,
                cwd: server_cwd_str,
                message: "Setup cancelled by user".to_string(),
            });
        }
    }

    // Build the claude mcp add command
    // Format: claude mcp add --scope <scope> <name> -- <command> [args...]
    //
    // IMPORTANT: claude mcp add doesn't support specifying cwd for the subprocess.
    // The MCP server will run from wherever Claude Code launches it, so:
    // - For 'local' scope: runs from the current project directory
    // - For 'user' scope: runs from wherever Claude Code is started
    let scope_arg = if global { "user" } else { "local" };

    let output = Command::new("claude")
        .args([
            "mcp",
            "add",
            "--scope",
            scope_arg,
            "mdkb",
            "--",
            &binary_path,
            "serve",
        ])
        .current_dir(&server_cwd)
        .output();

    match output {
        Ok(output) => {
            if output.status.success() {
                Ok(McpSetupResult {
                    success: true,
                    scope,
                    binary_path,
                    cwd: server_cwd_str,
                    message: format!("Successfully registered mdkb MCP server (scope: {})", scope),
                })
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);

                // Check if it's already registered
                if stderr.contains("already exists") || stdout.contains("already exists") {
                    Ok(McpSetupResult {
                        success: true,
                        scope,
                        binary_path,
                        cwd: server_cwd_str,
                        message: "mdkb MCP server is already registered".to_string(),
                    })
                } else {
                    Err(Error::from(ErrorKind::Command {
                        command: "claude mcp add".to_string(),
                        message: format!(
                            "Failed to register MCP server: {}{}",
                            stderr,
                            if stdout.is_empty() {
                                String::new()
                            } else {
                                format!("\n{}", stdout)
                            }
                        ),
                    }))
                }
            }
        }
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                Err(Error::from(ErrorKind::Command {
                    command: "claude".to_string(),
                    message: "Claude CLI not found. Please install Claude Code first: https://docs.anthropic.com/en/docs/claude-code".to_string(),
                }))
            } else {
                Err(Error::from(ErrorKind::Command {
                    command: "claude mcp add".to_string(),
                    message: format!("Failed to execute command: {}", e),
                }))
            }
        }
    }
}

/// Find the mdkb binary path.
///
/// Tries in order:
/// 1. Current executable path (if running from cargo or installed binary)
/// 2. PATH lookup
fn find_mdkb_binary() -> Result<String> {
    // Test / CI override — lets integration tests point at the built `mdkb`
    // bin under target/debug without polluting PATH.
    if let Some(override_path) = env::var_os("MDKB_BINARY_OVERRIDE") {
        let s = override_path.to_string_lossy().to_string();
        if !s.is_empty() {
            return Ok(s);
        }
    }

    // Try current executable first
    if let Ok(exe) = env::current_exe() {
        let exe_name = exe.file_name().map(|n| n.to_string_lossy().to_string());
        if exe_name.as_deref() == Some("mdkb") {
            return Ok(exe.to_string_lossy().to_string());
        }
    }

    // Try which/where command
    let which_cmd = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(which_cmd).arg("mdkb").output();

    if let Ok(output) = output {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout)
                .trim()
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            if !path.is_empty() {
                return Ok(path);
            }
        }
    }

    // Check common cargo install location
    if let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) {
        let cargo_bin = home.join(".cargo/bin/mdkb");
        if cargo_bin.exists() {
            return Ok(cargo_bin.to_string_lossy().to_string());
        }
    }

    Err(Error::from(ErrorKind::Command {
        command: "mdkb".to_string(),
        message: "Could not find mdkb binary. Please ensure mdkb is installed and in PATH, or run from the mdkb project directory.".to_string(),
    }))
}

/// Result of hooks setup operation.
#[derive(Debug)]
pub struct HooksSetupResult {
    pub success: bool,
    pub settings_path: std::path::PathBuf,
    pub events_registered: Vec<String>,
    pub events_skipped: Vec<String>,
    pub dry_run: bool,
    pub merged_json: serde_json::Value,
    pub message: String,
}

/// Three events currently emitted by `mdkb hook`.
pub const HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PostToolUse", "post-tool-use"),
];

/// Resolve the Claude Code settings path for the given scope.
/// - `local`: `<cwd>/.claude/settings.local.json`
/// - `user`:  `$HOME/.claude/settings.json`
pub fn claude_settings_path(cwd: &Path, scope: &str) -> Result<std::path::PathBuf> {
    match scope {
        "local" | "project" => Ok(cwd.join(".claude").join("settings.local.json")),
        "user" | "global" => {
            let home = env::var_os("HOME").ok_or_else(|| {
                Error::from(ErrorKind::Command {
                    command: "setup hooks claude".to_string(),
                    message: "HOME environment variable not set".to_string(),
                })
            })?;
            Ok(std::path::PathBuf::from(home)
                .join(".claude")
                .join("settings.json"))
        }
        other => Err(Error::from(ErrorKind::Command {
            command: "setup hooks claude".to_string(),
            message: format!("Invalid scope '{other}'. Must be 'local' or 'user'."),
        })),
    }
}

/// Parse the comma-separated --disable flag into a normalized set of event names.
/// Accepts both canonical ("SessionStart") and kebab-case ("session-start").
pub fn parse_disabled_events(raw: &str) -> std::collections::HashSet<String> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| match s.to_ascii_lowercase().as_str() {
            "sessionstart" | "session-start" => "SessionStart".to_string(),
            "userpromptsubmit" | "user-prompt-submit" => "UserPromptSubmit".to_string(),
            "posttooluse" | "post-tool-use" => "PostToolUse".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// Register Claude Code lifecycle hooks idempotently.
///
/// Writes into `.claude/settings.local.json` (local scope) or
/// `~/.claude/settings.json` (user scope). Existing non-mdkb hooks are
/// preserved; entries tagged `_managedBy: "mdkb"` are replaced rather than
/// duplicated so repeated invocation is safe.
pub fn handle_setup_hooks_claude(
    cwd: &Path,
    scope: &str,
    disable: &str,
    dry_run: bool,
) -> Result<HooksSetupResult> {
    let settings_path = claude_settings_path(cwd, scope)?;
    let binary_path = find_mdkb_binary()?;
    let disabled = parse_disabled_events(disable);

    let mut settings: serde_json::Value = if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: settings_path.clone(),
                operation: format!("read settings: {e}"),
            })
        })?;
        if raw.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&raw).map_err(|e| {
                Error::from(ErrorKind::Command {
                    command: "setup hooks claude".to_string(),
                    message: format!("failed to parse {}: {e}", settings_path.display()),
                })
            })?
        }
    } else {
        serde_json::json!({})
    };

    if !settings.is_object() {
        return Err(Error::from(ErrorKind::Command {
            command: "setup hooks claude".to_string(),
            message: "settings file root must be a JSON object".to_string(),
        }));
    }

    let hooks_root = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks_root.is_object() {
        *hooks_root = serde_json::json!({});
    }

    let mut registered = Vec::new();
    let mut skipped = Vec::new();

    for (event_name, cli_event) in HOOK_EVENTS {
        if disabled.contains(*event_name) {
            skipped.push((*event_name).to_string());
            continue;
        }

        let command = format!("{} hook {}", binary_path, cli_event);
        let mdkb_entry = serde_json::json!({
            "_managedBy": "mdkb",
            "hooks": [{
                "type": "command",
                "command": command,
            }]
        });

        let entry_list = hooks_root
            .as_object_mut()
            .unwrap()
            .entry((*event_name).to_string())
            .or_insert_with(|| serde_json::json!([]));

        let arr = match entry_list.as_array_mut() {
            Some(a) => a,
            None => {
                *entry_list = serde_json::json!([]);
                entry_list.as_array_mut().unwrap()
            }
        };

        arr.retain(|item| {
            item.get("_managedBy")
                .and_then(|v| v.as_str())
                .map(|s| s != "mdkb")
                .unwrap_or(true)
        });
        arr.push(mdkb_entry);

        registered.push((*event_name).to_string());
    }

    let merged_json = settings.clone();

    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&settings).unwrap_or_default()
        );
        return Ok(HooksSetupResult {
            success: true,
            settings_path,
            events_registered: registered,
            events_skipped: skipped,
            dry_run: true,
            merged_json,
            message: "Dry run: no changes written".to_string(),
        });
    }

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: parent.to_path_buf(),
                operation: format!("create parent dir: {e}"),
            })
        })?;
    }

    let serialized = serde_json::to_string_pretty(&settings).map_err(|e| {
        Error::from(ErrorKind::Command {
            command: "setup hooks claude".to_string(),
            message: format!("serialize settings: {e}"),
        })
    })?;
    std::fs::write(&settings_path, serialized).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: settings_path.clone(),
            operation: format!("write settings: {e}"),
        })
    })?;

    Ok(HooksSetupResult {
        success: true,
        settings_path,
        events_registered: registered,
        events_skipped: skipped,
        dry_run: false,
        merged_json,
        message: "Hooks registered".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_scope_display() {
        assert_eq!(format!("{}", McpScope::Global), "global");
        assert_eq!(format!("{}", McpScope::Local), "local");
    }

    #[test]
    fn test_parse_disabled_events_empty() {
        assert!(parse_disabled_events("").is_empty());
    }

    #[test]
    fn test_parse_disabled_events_kebab_and_canonical() {
        let set = parse_disabled_events("session-start, PostToolUse");
        assert!(set.contains("SessionStart"));
        assert!(set.contains("PostToolUse"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_claude_settings_path_local() {
        let cwd = std::path::Path::new("/tmp/project");
        let p = claude_settings_path(cwd, "local").unwrap();
        assert_eq!(p, cwd.join(".claude").join("settings.local.json"));
    }
}
