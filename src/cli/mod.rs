//! CLI layer - command parsing and execution with clap.

use clap::{Parser, Subcommand};

/// mdkb - Local markdown knowledge base with semantic search.
#[derive(Parser, Debug)]
#[command(name = "mdkb")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Verbose output (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Output format
    #[arg(long, global = true, default_value = "text")]
    pub format: OutputFormat,

    #[command(subcommand)]
    pub command: Command,
}

/// Output format for CLI commands.
#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Csv,
    Markdown,
}

/// CLI subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize .mdkb/ directory in current project
    Init,

    /// Manage collections
    #[command(subcommand)]
    Collection(CollectionCommand),

    /// Search documents
    Search {
        /// Search query
        query: String,

        /// Maximum number of results
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Filter by collection
        #[arg(short, long)]
        collection: Option<String>,
    },

    /// Retrieve a document by ID or path
    Get {
        /// Document ID or relative path
        id: String,

        /// Line range (e.g., 10:50)
        #[arg(long)]
        lines: Option<String>,
    },

    /// Show index status
    Status,

    /// Trigger differential reindex
    Update,

    /// Start MCP server
    Serve,
}

/// Collection management subcommands.
#[derive(Subcommand, Debug)]
pub enum CollectionCommand {
    /// Add a new collection
    Add {
        /// Collection name
        name: String,

        /// Path to directory
        path: String,

        /// Glob pattern for files
        #[arg(short, long, default_value = "**/*.md")]
        pattern: String,
    },

    /// Remove a collection
    Remove {
        /// Collection name
        name: String,
    },

    /// List all collections
    List,

    /// Rename a collection
    Rename {
        /// Current name
        old_name: String,

        /// New name
        new_name: String,
    },
}

impl Cli {
    /// Parse CLI arguments.
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
