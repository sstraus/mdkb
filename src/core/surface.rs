//! One inventory of the program's surface, shared by MCP and the CLI.
//!
//! The two expose overlapping capability through independent code paths, and
//! before this nothing asserted they agreed. The symptom that started story
//! 024-0c7e: `mdkb memory-write` does not exist as a CLI command — it is
//! `mdkb hook memory-write`, reachable only if you already know — while the MCP
//! tool is `memory_write`. A caller who knows one surface cannot guess the
//! other, and nothing anywhere said so.
//!
//! This table is the answer to "what is the other name for this?", in both
//! directions, and it is checked against reality rather than trusted:
//! `tests/surface_parity.rs` asserts every entry names a tool the MCP server
//! actually advertises and a command clap actually parses, and that no tool on
//! either side is missing from the table. Adding a tool without mapping it
//! fails the suite.
//!
//! A `None` equivalent is legitimate — the two surfaces serve different callers
//! and need not mirror each other exactly — but it must carry a reason, because
//! "no equivalent" and "nobody got round to it" look identical otherwise, and
//! that ambiguity is what let the reported gap survive.

use clap::CommandFactory;

/// One capability, under both of its names.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceEntry {
    /// The MCP tool name, as advertised to a client.
    pub mcp_tool: &'static str,
    /// The equivalent CLI command, space-separated as typed after `mdkb`.
    /// `None` when there deliberately is none — see `note`.
    pub cli_command: Option<&'static str>,
    /// Why the pair differs, or why there is no CLI equivalent. Required when
    /// `cli_command` is `None`.
    pub note: &'static str,
}

/// The inventory. Ordered as the MCP server advertises its tools.
pub const SURFACE_MAP: &[SurfaceEntry] = &[
    SurfaceEntry {
        mcp_tool: "search",
        cli_command: Some("search"),
        note: "",
    },
    SurfaceEntry {
        mcp_tool: "get",
        cli_command: Some("get"),
        note: "",
    },
    SurfaceEntry {
        mcp_tool: "status",
        cli_command: Some("stats"),
        note: "Named differently on purpose and worth knowing: the MCP tool \
               returns the index status an agent needs to decide whether to \
               reindex, while `mdkb stats` renders the full operator report.",
    },
    SurfaceEntry {
        mcp_tool: "update",
        cli_command: Some("update"),
        note: "",
    },
    SurfaceEntry {
        mcp_tool: "memory_write",
        cli_command: Some("memory add"),
        note: "`add` is an upsert, which is what `memory_write` means. \
               `mdkb hook memory-write` is a THIRD spelling of the same thing on \
               the daemon hook path; it exists for hook wiring, not for humans.",
    },
    SurfaceEntry {
        mcp_tool: "memory_write_batch",
        cli_command: Some("memory import"),
        note: "The CLI batch path takes a file or directory rather than an \
               inline array, because a shell argument list is the wrong place \
               for a hundred entries.",
    },
    SurfaceEntry {
        mcp_tool: "memory_delete",
        cli_command: Some("memory rm"),
        note: "",
    },
    SurfaceEntry {
        mcp_tool: "memory_confirm",
        cli_command: Some("memory confirm"),
        note: "",
    },
    SurfaceEntry {
        mcp_tool: "memory_list",
        cli_command: Some("memory list"),
        note: "",
    },
    SurfaceEntry {
        mcp_tool: "graph",
        cli_command: Some("graph"),
        note: "The MCP tool takes the traversal kind as a parameter; the CLI \
               splits it into subcommands (links, backlinks, neighbors, path, \
               hubs, dangling).",
    },
    SurfaceEntry {
        mcp_tool: "code_graph",
        cli_command: Some("code"),
        note: "Same split as `graph`: one MCP tool, several CLI subcommands \
               (calls, callers, impact, find, info).",
    },
    SurfaceEntry {
        mcp_tool: "usage",
        cli_command: Some("metrics show"),
        note: "",
    },
];

/// The CLI command for an MCP tool name.
pub fn mcp_to_cli(tool: &str) -> Option<&'static str> {
    SURFACE_MAP
        .iter()
        .find(|e| e.mcp_tool == tool)
        .and_then(|e| e.cli_command)
}

/// The MCP tool for a CLI command, the other way round.
pub fn cli_to_mcp(command: &str) -> Option<&'static str> {
    SURFACE_MAP
        .iter()
        .find(|e| e.cli_command == Some(command))
        .map(|e| e.mcp_tool)
}

/// Does clap actually define this space-separated command path?
///
/// Asked of the parser rather than of a list, so a renamed subcommand is caught
/// by the map's tests instead of by a user typing it.
pub fn cli_command_exists(path: &str) -> bool {
    let mut cmd = crate::cli::Cli::command();
    for word in path.split_whitespace() {
        let Some(next) = cmd
            .get_subcommands()
            .find(|s| s.get_name() == word || s.get_all_aliases().any(|a| a == word))
            .cloned()
        else {
            return false;
        };
        cmd = next;
    }
    true
}

/// Render the inventory for `mdkb surface`.
///
/// The point is that the answer is a command, not a grep: an agent holding one
/// name can get the other without reading the source.
pub fn render() -> String {
    let mut out = String::from("MCP tool -> CLI command\n\n");
    for e in SURFACE_MAP {
        let cli = e.cli_command.unwrap_or("(none)");
        out.push_str(&format!("  {:<20} {}\n", e.mcp_tool, cli));
        if !e.note.is_empty() {
            // Collapse the source's line-continuation whitespace.
            let note: Vec<&str> = e.note.split_whitespace().collect();
            out.push_str(&format!("  {:<20} note: {}\n", "", note.join(" ")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nested_command_path_resolves() {
        assert!(cli_command_exists("memory add"));
        assert!(cli_command_exists("graph links"));
    }

    #[test]
    fn an_invented_command_does_not_resolve() {
        assert!(!cli_command_exists("memory teleport"));
        assert!(!cli_command_exists("definitely not a command"));
    }

    /// The reported gap, as a unit test: `memory-write` is not a top-level CLI
    /// command, which is exactly why the map has to exist.
    #[test]
    fn the_reported_gap_is_real() {
        assert!(
            !cli_command_exists("memory-write"),
            "if this ever resolves, the map's note about the three spellings \
             needs revisiting"
        );
        assert!(cli_command_exists("hook memory-write"));
    }
}
