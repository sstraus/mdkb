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

/// Target agent framework for rules setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RulesTarget {
    /// Claude Code — references via CLAUDE.md
    Claude,
    /// Generic agents — references via AGENTS.md
    Agents,
    /// OpenAI Codex — references via CODEX.md
    Codex,
}

impl RulesTarget {
    /// The config filename for this target.
    pub fn config_filename(&self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE.md",
            Self::Agents => "AGENTS.md",
            Self::Codex => "CODEX.md",
        }
    }
}

/// Result of rules setup operation.
#[derive(Debug)]
pub struct RulesSetupResult {
    /// Whether the setup was successful.
    pub success: bool,
    /// Path to the generated MDKB.md.
    pub mdkb_md_path: std::path::PathBuf,
    /// Path to the target config file that was updated.
    pub config_path: std::path::PathBuf,
    /// Descriptive message.
    pub message: String,
}

/// The MDKB.md content — usage rules for AI agents.
const MDKB_MD_CONTENT: &str = "\
# mdkb — Project Knowledge Base

mdkb indexes this project's docs, code, symbols, and persistent memory.

## Rules

1. **Search before explore.** Before using Grep, Glob, or reading files to understand \
the codebase, call `search(query)`. mdkb already indexed the code and docs — manual \
exploration is the fallback, not the default.

2. **Save what you learned.** After solving a non-trivial problem, finding a non-obvious \
pattern, or making an architectural decision, write it to memory: \
`memory_write(id, title, content)`. Search memory first to avoid duplicates.

3. **Use code intelligence.** To find a function, struct, or type: \
`search(query, scope=\"symbols\")`. To trace callers or impact: `code_graph(name)`. \
These are faster and more complete than grepping.

4. **Memory types matter.** Use `entry_type`:
   - `problem` — bugs, gotchas, failure modes
   - `decision` — architectural choices with rationale
   - `topic` — patterns, conventions, domain knowledge

5. **Check memory on complex tasks.** Before starting multi-step work, search memory \
for prior context: `search(query, scope=\"memory\")`. Past sessions may have solved \
related problems.
";

/// Handle `mdkb setup claude|agents|codex` commands.
///
/// Generates `.mdkb/MDKB.md` if it doesn't exist, then adds a reference
/// (`@.mdkb/MDKB.md`) to the target config file (CLAUDE.md, AGENTS.md, or CODEX.md).
/// Both operations are idempotent.
pub fn handle_setup_rules(root: &Path, target: RulesTarget) -> Result<RulesSetupResult> {
    let mdkb_dir = root.join(".mdkb");
    if !mdkb_dir.exists() {
        return Err(Error::other(
            "mdkb not initialized. Run `mdkb init` first.",
        ));
    }

    let mdkb_md_path = mdkb_dir.join("MDKB.md");
    let config_path = root.join(target.config_filename());
    let reference = "@.mdkb/MDKB.md";

    // Step 1: Generate MDKB.md if missing
    let generated = if !mdkb_md_path.exists() {
        std::fs::write(&mdkb_md_path, MDKB_MD_CONTENT).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: mdkb_md_path.clone(),
                operation: format!("write MDKB.md: {}", e),
            })
        })?;
        true
    } else {
        false
    };

    // Step 2: Add reference to target config file if not already present
    let config_content = if config_path.exists() {
        std::fs::read_to_string(&config_path).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: config_path.clone(),
                operation: format!("read {}: {}", target.config_filename(), e),
            })
        })?
    } else {
        String::new()
    };

    let already_referenced = config_content.contains(reference);
    if !already_referenced {
        let new_content = if config_content.is_empty() {
            format!("{}\n", reference)
        } else if config_content.ends_with('\n') {
            format!("{}{}\n", config_content, reference)
        } else {
            format!("{}\n{}\n", config_content, reference)
        };
        std::fs::write(&config_path, new_content).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: config_path.clone(),
                operation: format!("write {}: {}", target.config_filename(), e),
            })
        })?;
    }

    let message = match (generated, already_referenced) {
        (true, false) => format!(
            "Created .mdkb/MDKB.md and added reference to {}",
            target.config_filename()
        ),
        (true, true) => "Created .mdkb/MDKB.md (reference already present)".to_string(),
        (false, false) => format!(
            "Added reference to {} (MDKB.md already existed)",
            target.config_filename()
        ),
        (false, true) => "Already set up (nothing to do)".to_string(),
    };

    Ok(RulesSetupResult {
        success: true,
        mdkb_md_path,
        config_path,
        message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_initialized_dir() -> TempDir {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".mdkb")).unwrap();
        temp
    }

    #[test]
    fn test_mcp_scope_display() {
        assert_eq!(format!("{}", McpScope::Global), "global");
        assert_eq!(format!("{}", McpScope::Local), "local");
    }

    #[test]
    fn test_rules_target_config_filename() {
        assert_eq!(RulesTarget::Claude.config_filename(), "CLAUDE.md");
        assert_eq!(RulesTarget::Agents.config_filename(), "AGENTS.md");
        assert_eq!(RulesTarget::Codex.config_filename(), "CODEX.md");
    }

    #[test]
    fn test_setup_rules_fails_if_not_initialized() {
        let temp = tempfile::tempdir().unwrap();
        let result = handle_setup_rules(temp.path(), RulesTarget::Claude);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mdkb init"));
    }

    #[test]
    fn test_setup_rules_creates_mdkb_md() {
        let temp = setup_initialized_dir();
        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        assert!(temp.path().join(".mdkb/MDKB.md").exists());
    }

    #[test]
    fn test_setup_rules_mdkb_md_content() {
        let temp = setup_initialized_dir();
        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        let content = std::fs::read_to_string(temp.path().join(".mdkb/MDKB.md")).unwrap();
        assert!(content.contains("Search before explore"));
        assert!(content.contains("Save what you learned"));
        assert!(content.contains("code intelligence"));
        assert!(content.contains("Memory types matter"));
        assert!(content.contains("Check memory on complex tasks"));
    }

    #[test]
    fn test_setup_rules_creates_target_file_if_missing() {
        let temp = setup_initialized_dir();
        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        let claude_md = temp.path().join("CLAUDE.md");
        assert!(claude_md.exists());
        let content = std::fs::read_to_string(&claude_md).unwrap();
        assert!(content.contains("@.mdkb/MDKB.md"));
    }

    #[test]
    fn test_setup_rules_agents_target() {
        let temp = setup_initialized_dir();
        handle_setup_rules(temp.path(), RulesTarget::Agents).unwrap();
        let agents_md = temp.path().join("AGENTS.md");
        assert!(agents_md.exists());
        let content = std::fs::read_to_string(&agents_md).unwrap();
        assert!(content.contains("@.mdkb/MDKB.md"));
    }

    #[test]
    fn test_setup_rules_codex_target() {
        let temp = setup_initialized_dir();
        handle_setup_rules(temp.path(), RulesTarget::Codex).unwrap();
        let codex_md = temp.path().join("CODEX.md");
        assert!(codex_md.exists());
        let content = std::fs::read_to_string(&codex_md).unwrap();
        assert!(content.contains("@.mdkb/MDKB.md"));
    }

    #[test]
    fn test_setup_rules_appends_to_existing_file() {
        let temp = setup_initialized_dir();
        let claude_md = temp.path().join("CLAUDE.md");
        std::fs::write(&claude_md, "# My Project\n\nExisting content.\n").unwrap();

        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();

        let content = std::fs::read_to_string(&claude_md).unwrap();
        assert!(content.contains("# My Project"));
        assert!(content.contains("Existing content."));
        assert!(content.contains("@.mdkb/MDKB.md"));
    }

    #[test]
    fn test_setup_rules_idempotent_mdkb_md() {
        let temp = setup_initialized_dir();
        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        // Write custom content to MDKB.md — should not be overwritten
        let mdkb_md = temp.path().join(".mdkb/MDKB.md");
        std::fs::write(&mdkb_md, "# Custom content\n").unwrap();
        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        let content = std::fs::read_to_string(&mdkb_md).unwrap();
        assert_eq!(content, "# Custom content\n");
    }

    #[test]
    fn test_setup_rules_idempotent_reference() {
        let temp = setup_initialized_dir();
        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        let content = std::fs::read_to_string(temp.path().join("CLAUDE.md")).unwrap();
        // Reference must appear exactly once
        assert_eq!(content.matches("@.mdkb/MDKB.md").count(), 1);
    }

    #[test]
    fn test_setup_rules_message_all_new() {
        let temp = setup_initialized_dir();
        let result = handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        assert!(result.message.contains("Created") && result.message.contains("CLAUDE.md"));
    }

    #[test]
    fn test_setup_rules_message_already_done() {
        let temp = setup_initialized_dir();
        handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        let result = handle_setup_rules(temp.path(), RulesTarget::Claude).unwrap();
        assert!(result.message.contains("nothing to do"));
    }
}
