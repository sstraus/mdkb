//! Setup command handlers for configuring mdkb integrations.

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use fs4::fs_std::FileExt;

use crate::config::Config;
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
        println!("  Args:    mcp");
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

    // Remove any existing registration at this scope first. `claude mcp add`
    // reports "already exists" and refuses to overwrite, so a stale entry (e.g.
    // a legacy `mdkb serve` command from before the daemon-proxy switch) would
    // never be replaced by the current `mdkb mcp` proxy. Best-effort: ignore
    // failures (most commonly "no such server"), then add the fresh entry.
    let _ = Command::new("claude")
        .args(["mcp", "remove", "--scope", scope_arg, "mdkb"])
        .current_dir(&server_cwd)
        .output();

    let output = Command::new("claude")
        .args([
            "mcp",
            "add",
            "--scope",
            scope_arg,
            "mdkb",
            "--",
            &binary_path,
            "mcp",
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
    #[cfg(debug_assertions)]
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
    /// True when the Codex `codex_hooks = true` flag was detected in config.toml.
    /// Always false for the Claude variant (Claude has no analogous flag).
    pub codex_hooks_flag_present: bool,
}

/// Lifecycle events emitted by `mdkb hook`.
/// Tuple: (event_name, cli_event, optional matcher for settings.json).
pub const HOOK_EVENTS: &[(&str, &str, Option<&str>)] = &[
    ("SessionStart", "session-start", None),
    ("UserPromptSubmit", "user-prompt-submit", None),
    (
        "PostToolUse",
        "post-tool-use",
        Some("Edit|Write|NotebookEdit|MultiEdit"),
    ),
    ("PreToolUse", "pre-tool-use", Some("Grep|Bash")),
    ("Stop", "stop", None),
];

/// Single-quote shell escaping: wraps `s` in single quotes, escaping any
/// embedded single quotes with the `'\''` pattern.
fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

/// Shell command written into Claude/Codex settings for a lifecycle event.
///
/// Primary path goes through the daemon via `mdkb hook <event>`. If that
/// call fails (daemon missing or a transient auto-spawn error), the shell
/// falls through to `MDKB_NO_DAEMON=1 mdkb hook <event>` which runs the
/// same dispatch in-process. Both branches exit 0, so the host CLI is
/// never blocked by mdkb.
///
/// When `daemon_required` is true, the fallback is omitted — the hook
/// runs daemon-only and silently returns `{}` if the daemon is down.
pub fn hook_command_line(binary_path: &str, cli_event: &str, daemon_required: bool) -> String {
    // One command, no shell conditional. The old wiring was
    // `if ! mdkb hook <event>; then MDKB_NO_DAEMON=1 mdkb hook <event>; fi`,
    // which could never fire: `run_hook` returns `Ok(())` on every failure
    // because the host hook must exit 0, so the `if !` branch was unreachable
    // and the settings file advertised a rail that did not exist (021-0636).
    //
    // The fallback now lives inside the process. `run_hook` reads
    // `hooks.daemon_required` from the project config and only dispatches
    // in-process when policy allows it, so both modes use the same shell-safe
    // command line.
    let _ = daemon_required;
    format!(
        "{bin} hook {event}",
        bin = shell_quote(binary_path),
        event = cli_event
    )
}

/// Resolve the Claude Code settings path for the given scope.
/// - `local`: `<cwd>/.claude/settings.local.json`
/// - `user`:  `<profile_dir>/settings.json` (default profile_dir: `$HOME/.claude`)
pub fn claude_settings_path(
    cwd: &Path,
    scope: &str,
    profile_dir: Option<&Path>,
) -> Result<std::path::PathBuf> {
    match scope {
        "local" | "project" => Ok(cwd.join(".claude").join("settings.local.json")),
        "user" | "global" => {
            let dir = if let Some(p) = profile_dir {
                p.to_path_buf()
            } else {
                let home = crate::home::dir().ok_or_else(|| {
                    Error::from(ErrorKind::Command {
                        command: "setup hooks claude".to_string(),
                        message: "no home directory: neither HOME nor USERPROFILE is set"
                            .to_string(),
                    })
                })?;
                home.join(".claude")
            };
            Ok(dir.join("settings.json"))
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
            // DEFERRED (2026-04-20) — "pretooluse"/"pre-tool-use" not in HOOK_EVENTS yet;
            // alias kept so --disable pre-tool-use is silently accepted without error.
            "pretooluse" | "pre-tool-use" => "PreToolUse".to_string(),
            "stop" => "Stop".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// Run `f` under an exclusive advisory lock on `target.lock`, then write the
/// returned JSON value to `target` atomically (temp-file + rename).
///
/// The lock is acquired **before** `f` is called, so `f` can safely read
/// `target` from disk knowing no other process is concurrently writing it.
/// The lock is released when this function returns (the `lock_file` drops).
///
/// The lock file (`target.lock`) is never removed — it is a stable sentinel
/// reused across invocations, consistent with the daemon-singleton pattern.
fn locked_read_modify_write<F>(target: &Path, f: F) -> Result<()>
where
    F: FnOnce() -> Result<serde_json::Value>,
{
    // Ensure parent directory exists before opening the lock file.
    let parent_dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent_dir).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: parent_dir.to_path_buf(),
            operation: format!("create parent dir: {e}"),
        })
    })?;

    // Build the lock-file path: same name with ".lock" appended.
    let lock_path: PathBuf = {
        let mut p = target.as_os_str().to_owned();
        p.push(".lock");
        PathBuf::from(p)
    };
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| {
            Error::from(ErrorKind::Io {
                path: lock_path.clone(),
                operation: format!("open lock file: {e}"),
            })
        })?;
    // Blocking exclusive lock — waits until any concurrent holder releases it.
    lock_file.lock_exclusive().map_err(|e| {
        Error::from(ErrorKind::Io {
            path: lock_path.clone(),
            operation: format!("lock_exclusive: {e}"),
        })
    })?;

    // Run the caller's read+modify logic while holding the lock.
    let value = f()?;

    // Write to a temp file in the same directory so the rename is atomic
    // (same filesystem — no cross-device move).
    let mut tmp = tempfile::NamedTempFile::new_in(parent_dir).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: parent_dir.to_path_buf(),
            operation: format!("create temp file: {e}"),
        })
    })?;
    let serialized = serde_json::to_string_pretty(&value).map_err(|e| {
        Error::from(ErrorKind::Command {
            command: "setup hooks".to_string(),
            message: format!("serialize JSON: {e}"),
        })
    })?;
    tmp.write_all(serialized.as_bytes()).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: parent_dir.to_path_buf(),
            operation: format!("write temp file: {e}"),
        })
    })?;
    tmp.flush().map_err(|e| {
        Error::from(ErrorKind::Io {
            path: parent_dir.to_path_buf(),
            operation: format!("flush temp file: {e}"),
        })
    })?;
    tmp.persist(target).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: target.to_path_buf(),
            operation: format!("rename temp -> target: {e}"),
        })
    })?;

    // `lock_file` drops here, releasing the advisory lock.
    Ok(())
}

/// Read and parse `path` as a JSON object. Missing or empty file yields `{}`.
fn read_json_file(path: &Path, cmd: &str) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read_to_string(path).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: path.to_path_buf(),
            operation: format!("read: {e}"),
        })
    })?;
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        Error::from(ErrorKind::Command {
            command: cmd.to_string(),
            message: format!("failed to parse {}: {e}", path.display()),
        })
    })?;
    if !v.is_object() {
        return Err(Error::from(ErrorKind::Command {
            command: cmd.to_string(),
            message: "settings file root must be a JSON object".to_string(),
        }));
    }
    Ok(v)
}

/// True when a hook-array `item` is an mdkb registration for `cli_event`.
///
/// Two cases: our own `_managedBy: "mdkb"` tag, and legacy untagged installs
/// (pre-tag) whose command still invokes `mdkb hook <cli_event>`. Foreign hooks
/// (rtk's `hook claude`, other tools) do not match — the marker is event-specific.
fn is_mdkb_hook_entry(item: &serde_json::Value, cli_event: &str) -> bool {
    if item.get("_managedBy").and_then(|v| v.as_str()) == Some("mdkb") {
        return true;
    }
    let event_marker = format!("hook {cli_event}");
    item.get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(&event_marker))
            })
        })
}

/// Count mdkb registrations for one event in a single settings object.
fn count_mdkb_entries(settings: &serde_json::Value, event_name: &str, cli_event: &str) -> usize {
    settings
        .get("hooks")
        .and_then(|h| h.get(event_name))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|i| is_mdkb_hook_entry(i, cli_event))
                .count()
        })
        .unwrap_or(0)
}

/// Divergence of on-disk hook registrations from the canonical mdkb set
/// (exactly one entry per `HOOK_EVENTS` event). Both conditions cause real
/// misbehavior: `duplicated` events double-fire (the audit's warmup/recall
/// ran twice per turn); `missing` events silently never run (Stop → prior
/// mining dead).
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct HookDrift {
    /// Events registered more than once (double-fire).
    pub duplicated: Vec<String>,
    /// Canonical events with no registration (e.g. Stop never installed).
    pub missing: Vec<String>,
}

impl HookDrift {
    /// No drift — registrations match the canonical set exactly.
    pub fn is_clean(&self) -> bool {
        self.duplicated.is_empty() && self.missing.is_empty()
    }

    /// One-line actionable warning, or `None` when clean.
    pub fn warning(&self) -> Option<String> {
        if self.is_clean() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.duplicated.is_empty() {
            parts.push(format!(
                "{} duplicated ({})",
                self.duplicated.len(),
                self.duplicated.join(", ")
            ));
        }
        if !self.missing.is_empty() {
            parts.push(format!(
                "{} missing ({})",
                self.missing.len(),
                self.missing.join(", ")
            ));
        }
        Some(format!(
            "stale mdkb hook registrations: {} — run: mdkb setup hooks",
            parts.join("; ")
        ))
    }
}

/// Detect drift by summing mdkb registrations across every provided settings
/// object. Claude Code fires hooks from all scopes (user + local), so an event
/// present in two scopes double-fires — summing across files is the correct
/// double-fire semantic.
pub fn detect_hook_drift(settings: &[&serde_json::Value]) -> HookDrift {
    let mut drift = HookDrift::default();
    for (event_name, cli_event, _) in HOOK_EVENTS {
        let total: usize = settings
            .iter()
            .map(|s| count_mdkb_entries(s, event_name, cli_event))
            .sum();
        if total == 0 {
            drift.missing.push((*event_name).to_string());
        } else if total > 1 {
            drift.duplicated.push((*event_name).to_string());
        }
    }
    drift
}

/// Detect hook drift for a repo by reading both the user-scope
/// (`~/.claude/settings.json`) and local-scope (`<cwd>/.claude/settings.local.json`)
/// settings. Missing or unparseable files contribute nothing (treated as `{}`)
/// so drift detection never fails a caller — it degrades to "no mdkb hooks seen".
pub fn detect_hook_drift_for_repo(cwd: &Path, profile_dir: Option<&Path>) -> HookDrift {
    let read = |path: Result<PathBuf>| -> serde_json::Value {
        path.ok()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .filter(|raw| !raw.trim().is_empty())
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| serde_json::json!({}))
    };
    let user = read(claude_settings_path(cwd, "user", profile_dir));
    let local = read(claude_settings_path(cwd, "local", None));
    detect_hook_drift(&[&user, &local])
}

/// Upsert mdkb hook entries into `settings` in-place, replacing any existing
/// `_managedBy: "mdkb"` entries and skipping events in `disabled`.
/// Returns `(registered_events, skipped_events)`.
fn upsert_hook_entries(
    settings: &mut serde_json::Value,
    binary_path: &str,
    disabled: &std::collections::HashSet<String>,
    daemon_required: bool,
) -> (Vec<String>, Vec<String>) {
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

    for (event_name, cli_event, matcher) in HOOK_EVENTS {
        if disabled.contains(*event_name) {
            skipped.push((*event_name).to_string());
            continue;
        }
        let command = hook_command_line(binary_path, cli_event, daemon_required);
        let mut mdkb_entry = serde_json::json!({
            "_managedBy": "mdkb",
            "hooks": [{"type": "command", "command": command}]
        });
        if let Some(m) = matcher {
            mdkb_entry["matcher"] = serde_json::json!(m);
        }
        let entry_list = hooks_root
            .as_object_mut()
            .unwrap()
            .entry((*event_name).to_string())
            .or_insert_with(|| serde_json::json!([]));
        let arr = if let Some(a) = entry_list.as_array_mut() {
            a
        } else {
            *entry_list = serde_json::json!([]);
            entry_list.as_array_mut().unwrap()
        };
        // Drop any prior mdkb entry for this event so re-running setup replaces
        // rather than duplicates. See `is_mdkb_hook_entry` for the two cases
        // (tagged + legacy untagged). rtk's `hook claude` and other hooks don't
        // match, so they survive.
        arr.retain(|item| !is_mdkb_hook_entry(item, cli_event));
        arr.push(mdkb_entry);
        registered.push((*event_name).to_string());
    }

    (registered, skipped)
}

/// Shared core: read `settings_path`, upsert mdkb hook entries idempotently,
/// and either print (dry-run) or write the result via `locked_read_modify_write`.
///
/// For the real (non-dry-run) path the read happens **inside** the lock closure
/// so the full read-modify-write is atomic with respect to other processes.
///
/// `cmd_name` appears in error messages. Returns `(registered, skipped, merged_json)`.
fn write_hook_entries(
    settings_path: &Path,
    binary_path: &str,
    disabled: &std::collections::HashSet<String>,
    cmd_name: &str,
    dry_run: bool,
    daemon_required: bool,
) -> Result<(Vec<String>, Vec<String>, serde_json::Value)> {
    if dry_run {
        // Dry-run: read without locking (no write will happen).
        let mut settings = read_json_file(settings_path, cmd_name)?;
        let (registered, skipped) =
            upsert_hook_entries(&mut settings, binary_path, disabled, daemon_required);
        println!(
            "{}",
            serde_json::to_string_pretty(&settings).unwrap_or_default()
        );
        return Ok((registered, skipped, settings));
    }

    // Non-dry-run: hold the advisory lock across the full read-modify-write so
    // concurrent invocations serialize and each sees the other's changes.
    let mut registered_out = Vec::new();
    let mut skipped_out = Vec::new();
    let registered_ref = &mut registered_out;
    let skipped_ref = &mut skipped_out;
    locked_read_modify_write(settings_path, || {
        let mut settings = read_json_file(settings_path, cmd_name)?;
        let (registered, skipped) =
            upsert_hook_entries(&mut settings, binary_path, disabled, daemon_required);
        *registered_ref = registered;
        *skipped_ref = skipped;
        Ok(settings)
    })?;

    let merged_json = read_json_file(settings_path, cmd_name)?;
    Ok((registered_out, skipped_out, merged_json))
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
    profile_dir: Option<&Path>,
) -> Result<HooksSetupResult> {
    let settings_path = claude_settings_path(cwd, scope, profile_dir)?;
    let binary_path = find_mdkb_binary()?;
    let disabled = parse_disabled_events(disable);
    let cfg_path = cwd.join(".mdkb").join("config.toml");
    let hooks_cfg = Config::load_or_default(&cfg_path).hooks;

    let (registered, skipped, merged_json) = write_hook_entries(
        &settings_path,
        &binary_path,
        &disabled,
        "setup hooks claude",
        dry_run,
        hooks_cfg.daemon_required,
    )?;

    Ok(HooksSetupResult {
        success: true,
        settings_path,
        events_registered: registered,
        events_skipped: skipped,
        dry_run,
        merged_json,
        message: if dry_run {
            "Dry run: no changes written".to_string()
        } else {
            "Hooks registered".to_string()
        },
        codex_hooks_flag_present: false,
    })
}

/// Resolve the Codex hooks file path: `$HOME/.codex/hooks.json`.
pub fn codex_hooks_path() -> Result<std::path::PathBuf> {
    let home = crate::home::dir().ok_or_else(|| {
        Error::from(ErrorKind::Command {
            command: "setup hooks codex".to_string(),
            message: "no home directory: neither HOME nor USERPROFILE is set".to_string(),
        })
    })?;
    Ok(home.join(".codex").join("hooks.json"))
}

/// Best-effort probe for `codex_hooks = true` in `$HOME/.codex/config.toml`.
/// Missing file or missing flag both return `false`. Parse errors return `false`.
fn probe_codex_hooks_flag() -> bool {
    let Some(home) = crate::home::dir() else {
        return false;
    };
    let cfg = home.join(".codex").join("config.toml");
    let Ok(raw) = std::fs::read_to_string(&cfg) else {
        return false;
    };
    // Minimal TOML-aware scan: avoid pulling in a full parser.
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(rhs) = trimmed.strip_prefix("codex_hooks") {
            let rhs = rhs.trim_start();
            if let Some(val) = rhs.strip_prefix('=') {
                let v = val
                    .trim()
                    .trim_end_matches(|c: char| c == ',' || c.is_whitespace());
                if v.eq_ignore_ascii_case("true") {
                    return true;
                }
            }
        }
    }
    false
}

/// Register Codex CLI lifecycle hooks idempotently into `$HOME/.codex/hooks.json`.
///
/// Reuses `HOOK_EVENTS` from the Claude variant so the surface stays identical.
/// UserPromptSubmit and SessionStart are attempted optimistically; if Codex
/// rejects them at runtime it surfaces in Codex's own logs, not here.
/// The `codex_hooks = true` flag in `~/.codex/config.toml` is probed and
/// reported; the caller decides whether to warn the user.
pub fn handle_setup_hooks_codex(disable: &str, dry_run: bool) -> Result<HooksSetupResult> {
    let settings_path = codex_hooks_path()?;
    let binary_path = find_mdkb_binary()?;
    let disabled = parse_disabled_events(disable);
    let codex_hooks_flag_present = probe_codex_hooks_flag();
    let daemon_required = std::env::current_dir()
        .ok()
        .map(|cwd| {
            let cfg_path = cwd.join(".mdkb").join("config.toml");
            Config::load_or_default(&cfg_path).hooks.daemon_required
        })
        .unwrap_or(false);

    let (registered, skipped, merged_json) = write_hook_entries(
        &settings_path,
        &binary_path,
        &disabled,
        "setup hooks codex",
        dry_run,
        daemon_required,
    )?;

    Ok(HooksSetupResult {
        success: true,
        settings_path,
        events_registered: registered,
        events_skipped: skipped,
        dry_run,
        merged_json,
        message: if dry_run {
            "Dry run: no changes written".to_string()
        } else if codex_hooks_flag_present {
            "Hooks registered".to_string()
        } else {
            "Hooks registered. Warning: `codex_hooks = true` not found in ~/.codex/config.toml — Codex CLI will not invoke these hooks until the flag is set.".to_string()
        },
        codex_hooks_flag_present,
    })
}

/// Result of `mdkb setup mcp codex`.
#[derive(Debug)]
pub struct McpCodexSetupResult {
    pub success: bool,
    pub dry_run: bool,
    pub config_path: std::path::PathBuf,
    pub binary_path: String,
    pub merged_toml: String,
    pub message: String,
}

/// Resolve the Codex CLI config.toml path: `$HOME/.codex/config.toml`.
pub fn codex_config_path() -> Result<std::path::PathBuf> {
    let home = crate::home::dir().ok_or_else(|| {
        Error::from(ErrorKind::Command {
            command: "setup mcp codex".to_string(),
            message: "no home directory: neither HOME nor USERPROFILE is set".to_string(),
        })
    })?;
    Ok(home.join(".codex").join("config.toml"))
}

/// Register mdkb as an MCP server in Codex CLI's `~/.codex/config.toml`.
///
/// Idempotent: re-runs replace only the `[mcp_servers.mdkb]` table, preserving
/// other servers, top-level keys, and comments (via `toml_edit`).
/// Errors if `~/.codex` does not exist (Codex CLI not installed).
pub fn handle_setup_mcp_codex(dry_run: bool) -> Result<McpCodexSetupResult> {
    let config_path = codex_config_path()?;

    // Codex-not-installed check: parent dir must exist.
    let parent = config_path.parent().ok_or_else(|| {
        Error::from(ErrorKind::Command {
            command: "setup mcp codex".to_string(),
            message: "cannot resolve parent of config.toml".to_string(),
        })
    })?;
    if !parent.exists() {
        return Err(Error::from(ErrorKind::Command {
            command: "setup mcp codex".to_string(),
            message: format!(
                "{} does not exist — Codex CLI is not installed. Install Codex first: https://github.com/openai/codex",
                parent.display()
            ),
        }));
    }

    let binary_path = find_mdkb_binary()?;

    let mut doc: toml_edit::DocumentMut = if config_path.exists() {
        let raw = std::fs::read_to_string(&config_path).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: config_path.clone(),
                operation: format!("read config.toml: {e}"),
            })
        })?;
        raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
            Error::from(ErrorKind::Command {
                command: "setup mcp codex".to_string(),
                message: format!("failed to parse {}: {e}", config_path.display()),
            })
        })?
    } else {
        toml_edit::DocumentMut::new()
    };

    // Ensure [mcp_servers] exists as a table.
    if !doc.contains_key("mcp_servers") {
        doc["mcp_servers"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let servers = doc["mcp_servers"].as_table_mut().ok_or_else(|| {
        Error::from(ErrorKind::Command {
            command: "setup mcp codex".to_string(),
            message: "`mcp_servers` must be a TOML table".to_string(),
        })
    })?;

    // Overwrite [mcp_servers.mdkb] — idempotent replace of our managed entry.
    let mut mdkb = toml_edit::Table::new();
    mdkb.insert("command", toml_edit::value(binary_path.clone()));
    let mut args = toml_edit::Array::new();
    args.push("mcp");
    mdkb.insert("args", toml_edit::value(args));
    servers.insert("mdkb", toml_edit::Item::Table(mdkb));

    let merged_toml = doc.to_string();

    if dry_run {
        println!("{merged_toml}");
        return Ok(McpCodexSetupResult {
            success: true,
            dry_run: true,
            config_path,
            binary_path,
            merged_toml,
            message: "Dry run: no changes written".to_string(),
        });
    }

    std::fs::write(&config_path, &merged_toml).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: config_path.clone(),
            operation: format!("write config.toml: {e}"),
        })
    })?;

    Ok(McpCodexSetupResult {
        success: true,
        dry_run: false,
        config_path,
        binary_path,
        merged_toml,
        message: "mdkb MCP server registered in ~/.codex/config.toml".to_string(),
    })
}

// ── Removal handlers ──────────────────────────────────────────────

/// Remove mdkb MCP server from Claude Code via `claude mcp remove`.
pub fn handle_remove_mcp_claude(scope: &str) -> Result<String> {
    let scope_arg = match scope {
        "local" | "project" => "local",
        "user" | "global" => "user",
        other => {
            return Err(Error::from(ErrorKind::Command {
                command: "setup remove mcp claude".to_string(),
                message: format!("Invalid scope '{other}'. Must be 'local' or 'user'."),
            }));
        }
    };

    let output = Command::new("claude")
        .args(["mcp", "remove", "-s", scope_arg, "mdkb"])
        .output();

    match output {
        Ok(output) => {
            if output.status.success() {
                Ok(format!(
                    "Removed mdkb MCP server from Claude Code (scope: {scope_arg})"
                ))
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let combined = format!("{stderr}{stdout}");
                if combined.contains("not found") && combined.contains("mdkb") {
                    Ok("mdkb MCP server was not registered in Claude Code".to_string())
                } else {
                    Err(Error::from(ErrorKind::Command {
                        command: "claude mcp remove".to_string(),
                        message: format!("Failed: {stderr}{stdout}"),
                    }))
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Ok("Claude CLI not found — nothing to remove".to_string())
        }
        Err(e) => Err(Error::from(ErrorKind::Command {
            command: "claude mcp remove".to_string(),
            message: format!("Failed to execute: {e}"),
        })),
    }
}

/// Remove mdkb MCP server from Codex CLI's `~/.codex/config.toml`.
pub fn handle_remove_mcp_codex() -> Result<String> {
    let config_path = codex_config_path()?;
    if !config_path.exists() {
        return Ok("~/.codex/config.toml does not exist — nothing to remove".to_string());
    }

    let raw = std::fs::read_to_string(&config_path).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: config_path.clone(),
            operation: format!("read config.toml: {e}"),
        })
    })?;
    let mut doc: toml_edit::DocumentMut = raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
        Error::from(ErrorKind::Command {
            command: "setup remove mcp codex".to_string(),
            message: format!("failed to parse {}: {e}", config_path.display()),
        })
    })?;

    let removed = if let Some(servers) = doc.get_mut("mcp_servers").and_then(|v| v.as_table_mut()) {
        servers.remove("mdkb").is_some()
    } else {
        false
    };

    if removed {
        std::fs::write(&config_path, doc.to_string()).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: config_path.clone(),
                operation: format!("write config.toml: {e}"),
            })
        })?;
        Ok(format!(
            "Removed [mcp_servers.mdkb] from {}",
            config_path.display()
        ))
    } else {
        Ok("mdkb was not registered in ~/.codex/config.toml".to_string())
    }
}

/// Remove mdkb-managed hook entries from a Claude/Codex settings JSON file.
/// Returns count of removed entries.
fn remove_mdkb_hook_entries(settings: &mut serde_json::Value) -> usize {
    let Some(hooks_root) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return 0;
    };

    let mut removed = 0;
    let event_keys: Vec<String> = hooks_root.keys().cloned().collect();
    for key in &event_keys {
        if let Some(arr) = hooks_root.get_mut(key).and_then(|v| v.as_array_mut()) {
            let before = arr.len();
            arr.retain(|item| {
                item.get("_managedBy")
                    .and_then(|v| v.as_str())
                    .map(|s| s != "mdkb")
                    .unwrap_or(true)
            });
            removed += before - arr.len();
        }
    }

    // Prune empty arrays.
    hooks_root.retain(|_, v| v.as_array().map(|a| !a.is_empty()).unwrap_or(true));
    // Prune empty hooks object.
    if hooks_root.is_empty() {
        if let Some(obj) = settings.as_object_mut() {
            obj.remove("hooks");
        }
    }

    removed
}

/// Remove all `_managedBy: "mdkb"` hook entries from a settings JSON file.
fn remove_hooks_from(settings_path: &Path) -> Result<String> {
    if !settings_path.exists() {
        return Ok(format!(
            "{} does not exist — no hooks to remove",
            settings_path.display()
        ));
    }

    let mut removed_count = 0;
    locked_read_modify_write(settings_path, || {
        let mut settings = read_json_file(settings_path, "setup remove hooks")?;
        removed_count = remove_mdkb_hook_entries(&mut settings);
        Ok(settings)
    })?;

    if removed_count > 0 {
        Ok(format!(
            "Removed {removed_count} mdkb hook entries from {}",
            settings_path.display()
        ))
    } else {
        Ok(format!(
            "No mdkb hooks found in {}",
            settings_path.display()
        ))
    }
}

/// Remove mdkb hooks from Claude Code settings.
pub fn handle_remove_hooks_claude(
    cwd: &Path,
    scope: &str,
    profile_dir: Option<&Path>,
) -> Result<String> {
    remove_hooks_from(&claude_settings_path(cwd, scope, profile_dir)?)
}

/// Remove mdkb hooks from Codex CLI's hooks.json.
pub fn handle_remove_hooks_codex() -> Result<String> {
    remove_hooks_from(&codex_hooks_path()?)
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
        let p = claude_settings_path(cwd, "local", None).unwrap();
        assert_eq!(p, cwd.join(".claude").join("settings.local.json"));
    }

    #[test]
    fn test_shell_quote_plain() {
        assert_eq!(shell_quote("/usr/local/bin/mdkb"), "'/usr/local/bin/mdkb'");
    }

    #[test]
    fn test_shell_quote_spaces_and_metacharacters() {
        // Path with spaces and shell metacharacters must not cause injection.
        let path = "/home/user/my programs/md;kb$(evil)";
        let quoted = shell_quote(path);
        assert_eq!(quoted, "'/home/user/my programs/md;kb$(evil)'");
        // The quoted form must start and end with single quotes.
        assert!(quoted.starts_with('\''));
        assert!(quoted.ends_with('\''));
    }

    #[test]
    fn test_shell_quote_embedded_single_quote() {
        // Single quotes inside the path are escaped with the '\\'' pattern.
        let path = "/home/user/it's/mdkb";
        let quoted = shell_quote(path);
        assert_eq!(quoted, "'/home/user/it'\\''s/mdkb'");
    }

    #[test]
    fn test_hook_command_line_safe_with_spaces() {
        let cmd = hook_command_line("/path/with spaces/mdkb", "session-start", false);
        assert!(cmd.contains("'/path/with spaces/mdkb'"));
        assert!(!cmd.contains(" /path/with spaces/mdkb "));
    }

    #[test]
    fn test_hook_command_line_safe_with_metacharacters() {
        let cmd = hook_command_line("/usr/bin/md;kb$(rm -rf /)", "post-tool-use", false);
        assert!(cmd.contains("'/usr/bin/md;kb$(rm -rf /)'"));
        assert!(!cmd.contains("md;kb$(rm -rf /)\" "));
    }

    #[test]
    fn test_hook_command_line_daemon_required_no_fallback() {
        let cmd = hook_command_line("/usr/bin/mdkb", "session-start", true);
        assert!(
            !cmd.contains("MDKB_NO_DAEMON"),
            "daemon_required must omit fallback"
        );
        assert!(
            !cmd.contains("if !"),
            "daemon_required must omit if-then-fi wrapper"
        );
        assert!(cmd.contains("hook session-start"));
    }

    #[test]
    fn test_binary_override_honored_in_tests() {
        // MDKB_BINARY_OVERRIDE must be visible inside #[cfg(test)] code paths.
        // SAFETY: single-threaded test; no concurrent env readers.
        unsafe {
            std::env::set_var("MDKB_BINARY_OVERRIDE", "/fake/mdkb");
        }
        let result = find_mdkb_binary();
        unsafe {
            std::env::remove_var("MDKB_BINARY_OVERRIDE");
        }
        assert_eq!(result.unwrap(), "/fake/mdkb");
    }

    #[test]
    fn test_upsert_replaces_legacy_untagged_mdkb_hook() {
        // A pre-`_managedBy` install left an untagged `mdkb hook pre-tool-use`
        // entry. Re-running setup must replace it (not duplicate), while leaving
        // unrelated hooks (e.g. rtk) untouched.
        use std::collections::HashSet;
        let mut settings = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {"matcher": "Grep", "hooks": [{"type": "command", "command": "mdkb hook pre-tool-use"}]},
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]}
                ]
            }
        });
        upsert_hook_entries(&mut settings, "/usr/bin/mdkb", &HashSet::new(), false);

        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        let managed = pre
            .iter()
            .filter(|e| e.get("_managedBy").and_then(|v| v.as_str()) == Some("mdkb"))
            .count();
        assert_eq!(managed, 1, "exactly one managed mdkb PreToolUse entry");

        let legacy_remains = pre.iter().any(|e| {
            e.get("_managedBy").is_none()
                && e["hooks"].as_array().is_some_and(|h| {
                    h.iter().any(|x| {
                        x["command"]
                            .as_str()
                            .is_some_and(|c| c.contains("hook pre-tool-use"))
                    })
                })
        });
        assert!(
            !legacy_remains,
            "legacy untagged mdkb entry must be removed"
        );

        let rtk_preserved = pre.iter().any(|e| {
            e["hooks"].as_array().is_some_and(|h| {
                h.iter()
                    .any(|x| x["command"].as_str() == Some("rtk hook claude"))
            })
        });
        assert!(rtk_preserved, "unrelated rtk hook must be preserved");
    }

    /// A canonical single-scope install (one mdkb entry per event) is clean.
    #[test]
    fn test_detect_drift_clean_canonical() {
        use std::collections::HashSet;
        let mut settings = serde_json::json!({});
        upsert_hook_entries(&mut settings, "/usr/bin/mdkb", &HashSet::new(), false);
        let drift = detect_hook_drift(&[&settings]);
        assert!(
            drift.is_clean(),
            "canonical install has no drift: {drift:?}"
        );
        assert!(drift.warning().is_none());
    }

    /// The live audit shape: SessionStart/UPS/PostToolUse each registered twice
    /// (tagged + legacy untagged absolute path), Stop entirely missing.
    #[test]
    fn test_detect_drift_live_shape_duplicates_and_missing_stop() {
        let settings = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook session-start"}]},
                    {"hooks": [{"type": "command", "command": "/Users/x/.local/bin/mdkb hook session-start"}]}
                ],
                "UserPromptSubmit": [
                    {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook user-prompt-submit"}]},
                    {"hooks": [{"type": "command", "command": "/Users/x/.local/bin/mdkb hook user-prompt-submit"}]}
                ],
                "PostToolUse": [
                    {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook post-tool-use"}]},
                    {"hooks": [{"type": "command", "command": "/Users/x/.local/bin/mdkb hook post-tool-use"}]}
                ],
                "PreToolUse": [
                    {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook pre-tool-use"}]}
                ]
            }
        });
        let drift = detect_hook_drift(&[&settings]);
        assert!(!drift.is_clean());
        assert_eq!(
            drift.duplicated,
            vec!["SessionStart", "UserPromptSubmit", "PostToolUse"],
        );
        assert_eq!(drift.missing, vec!["Stop"]);
        let warning = drift.warning().expect("warning present");
        assert!(warning.contains("3 duplicated"));
        assert!(warning.contains("Stop"));
        assert!(warning.contains("mdkb setup hooks"));
    }

    /// Same event registered in user AND local scope double-fires — summing
    /// across settings objects catches the cross-scope case.
    #[test]
    fn test_detect_drift_cross_scope_double_fire() {
        let user = serde_json::json!({
            "hooks": {"SessionStart": [
                {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook session-start"}]}
            ]}
        });
        let local = serde_json::json!({
            "hooks": {"SessionStart": [
                {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook session-start"}]}
            ]}
        });
        let drift = detect_hook_drift(&[&user, &local]);
        assert!(drift.duplicated.contains(&"SessionStart".to_string()));
    }

    /// Foreign hooks (rtk `hook claude`) never count toward drift.
    #[test]
    fn test_detect_drift_ignores_foreign_hooks() {
        let settings = serde_json::json!({
            "hooks": {"PreToolUse": [
                {"hooks": [{"type": "command", "command": "rtk hook claude"}]}
            ]}
        });
        let drift = detect_hook_drift(&[&settings]);
        // rtk doesn't count as an mdkb PreToolUse entry → PreToolUse reads missing.
        assert!(drift.missing.contains(&"PreToolUse".to_string()));
        assert!(!drift.duplicated.contains(&"PreToolUse".to_string()));
    }

    #[test]
    fn test_remove_mdkb_hook_entries_removes_managed() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook session-start"}]},
                    {"_managedBy": "other", "hooks": [{"type": "command", "command": "other-tool"}]}
                ],
                "PostToolUse": [
                    {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook post-tool-use"}]}
                ]
            }
        });
        let removed = remove_mdkb_hook_entries(&mut settings);
        assert_eq!(removed, 2);
        let hooks = settings["hooks"].as_object().unwrap();
        assert_eq!(hooks["SessionStart"].as_array().unwrap().len(), 1);
        assert!(
            !hooks.contains_key("PostToolUse"),
            "empty array should be pruned"
        );
    }

    #[test]
    fn test_remove_mdkb_hook_entries_empty_hooks_pruned() {
        let mut settings = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {"_managedBy": "mdkb", "hooks": []}
                ]
            }
        });
        let removed = remove_mdkb_hook_entries(&mut settings);
        assert_eq!(removed, 1);
        assert!(
            settings.get("hooks").is_none(),
            "empty hooks object should be pruned"
        );
    }

    #[test]
    fn test_remove_mdkb_hook_entries_no_hooks() {
        let mut settings = serde_json::json!({"key": "value"});
        let removed = remove_mdkb_hook_entries(&mut settings);
        assert_eq!(removed, 0);
    }
}
