//! CLI layer - command parsing and execution with clap.

pub mod handlers;

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

    /// Search documents (hybrid: combines BM25 + semantic with RRF fusion)
    Search {
        /// Search query
        query: String,

        /// Maximum number of results
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Filter by collection
        #[arg(short, long)]
        collection: Option<String>,

        /// Include superseded/retracted documents
        #[arg(long)]
        include_superseded: bool,
    },

    /// Retrieve a document by ID or path
    Get {
        /// Document ID or relative path
        id: String,

        /// Line range (e.g., 10:50)
        #[arg(long)]
        lines: Option<String>,
    },

    /// Retrieve multiple documents by pattern
    Mget {
        /// Glob pattern to match paths (e.g., "docs/*.md")
        pattern: String,

        /// Filter by collection
        #[arg(short, long)]
        collection: Option<String>,
    },

    /// Show index status
    Status,

    /// Trigger differential reindex
    Update,

    /// Generate embeddings for documents
    Embed,

    /// Start MCP server
    Serve,

    /// Show usage statistics
    Stats {
        /// Show last N sessions (default: 5)
        #[arg(short, long, default_value = "5")]
        sessions: usize,

        /// Show aggregate stats only
        #[arg(short, long)]
        aggregate: bool,
    },

    /// Query metrics and search quality analysis
    #[command(subcommand)]
    Metrics(MetricsCommand),

    /// Manage memory entries for AI knowledge persistence
    #[command(subcommand)]
    Memory(MemoryCommand),

    /// Manage document evolution relationships
    #[command(subcommand)]
    Evolve(EvolveCommand),

    /// Show evolution history of a document
    History {
        /// Document path or ID
        path: String,
    },

    /// Find current version of a superseded document
    Current {
        /// Document path or ID
        path: String,
    },

    /// Show what superseded this document
    SupersededBy {
        /// Document path or ID
        path: String,
    },
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

/// Metrics subcommands.
#[derive(Subcommand, Debug)]
pub enum MetricsCommand {
    /// Show query metrics summary
    Show {
        /// Period in days (default: 7)
        #[arg(short, long, default_value = "7")]
        period: u32,
    },

    /// Show latency breakdown
    Latency {
        /// Period in days (default: 7)
        #[arg(short, long, default_value = "7")]
        period: u32,
    },

    /// Show search quality metrics
    Quality {
        /// Period in days (default: 7)
        #[arg(short, long, default_value = "7")]
        period: u32,
    },

    /// Export metrics data
    Export {
        /// Period in days (default: 7)
        #[arg(short, long, default_value = "7")]
        period: u32,
    },
}

/// Memory management subcommands.
#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    /// Add a new memory entry
    Add {
        /// Entry ID (slug, e.g., "auth-oauth2-flow")
        id: String,

        /// Concise title (max 50 chars)
        #[arg(short, long)]
        title: String,

        /// Entry type
        #[arg(short = 'T', long, default_value = "topic")]
        entry_type: String,

        /// Tags (comma-separated)
        #[arg(long)]
        tags: Option<String>,

        /// Content (if not provided, reads from stdin)
        #[arg(short, long)]
        content: Option<String>,
    },

    /// Show a memory entry
    Show {
        /// Entry ID
        id: String,
    },

    /// List memory entries
    List {
        /// Maximum entries to show
        #[arg(short, long, default_value = "50")]
        limit: usize,

        /// Filter by status (active, superseded, archived)
        #[arg(short, long)]
        status: Option<String>,
    },

    /// Search memory entries
    Search {
        /// Search query
        query: String,

        /// Maximum results
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },

    /// Get warmup index (compact list for AI session start)
    Warmup {
        /// Maximum entries
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },

    /// Delete a memory entry
    Rm {
        /// Entry ID
        id: String,
    },

    /// Archive unused memory entries
    Prune {
        /// Days since last access to consider entry stale (default: 90)
        #[arg(short, long, default_value = "90")]
        days: u32,

        /// Show what would be pruned without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Consolidate related memory entries (requires --features llm)
    #[cfg(feature = "llm")]
    Condense {
        /// Filter by tag (optional, condenses all if not specified)
        #[arg(short, long)]
        tag: Option<String>,

        /// Show proposed merges without making changes
        #[arg(long)]
        dry_run: bool,

        /// Ask for confirmation before each merge
        #[arg(short, long)]
        interactive: bool,

        /// Minimum entries needed to consider condensing (default: 3)
        #[arg(long, default_value = "3")]
        min_entries: usize,
    },
}

/// Evolution management subcommands.
#[derive(Subcommand, Debug)]
pub enum EvolveCommand {
    /// Mark a document as superseding another
    Supersedes {
        /// The new document (path or ID)
        new: String,

        /// The old document (path or ID)
        old: String,

        /// Reason for supersession
        #[arg(short, long)]
        reason: Option<String>,
    },

    /// Mark a document as updating another
    Updates {
        /// The updating document (path or ID)
        new: String,

        /// The updated document (path or ID)
        old: String,

        /// Scope (e.g., section path)
        #[arg(short, long)]
        scope: Option<String>,

        /// Reason for update
        #[arg(short, long)]
        reason: Option<String>,
    },

    /// Mark a document as correcting another
    Corrects {
        /// The correcting document (path or ID)
        new: String,

        /// The corrected document (path or ID)
        old: String,

        /// Reason for correction
        #[arg(short, long)]
        reason: Option<String>,
    },

    /// Mark a document as retracting another
    Retracts {
        /// The retracting document (path or ID)
        new: String,

        /// The retracted document (path or ID)
        old: String,

        /// Reason for retraction
        #[arg(short, long)]
        reason: Option<String>,
    },

    /// Mark a document as extending another
    Extends {
        /// The extending document (path or ID)
        new: String,

        /// The extended document (path or ID)
        old: String,

        /// Reason for extension
        #[arg(short, long)]
        reason: Option<String>,
    },
}

impl Cli {
    /// Parse CLI arguments.
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
