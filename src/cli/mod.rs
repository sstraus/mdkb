//! CLI layer - command parsing and execution with clap.

pub mod daemon;
pub mod handlers;
#[cfg(unix)]
pub mod hook_client;
pub mod hook_logic;
pub mod journal;
#[cfg(unix)]
pub mod mcp_proxy;
pub mod setup;
pub mod stats_render;
pub mod stats_render_report;
pub mod stats_report;

use std::path::PathBuf;

use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};

use crate::store::memory::{EntryType, SourceType};
use crate::store::memory_graph::MemoryRelation;

/// Value parsers for the flags whose accepted values are a closed domain set.
///
/// Built from the enums rather than repeated in a help string, so `--help`
/// prints `[possible values: ...]`, an unknown value fails as a clap *usage*
/// error naming the set, and a variant added to the enum cannot be forgotten
/// here. Each returns `String` because the handlers still parse the wire form
/// themselves — the parser's job is the accepted set, not the conversion.
fn entry_type_values() -> PossibleValuesParser {
    PossibleValuesParser::new(EntryType::ALL.map(|t| t.as_str()))
}

fn source_type_values() -> PossibleValuesParser {
    PossibleValuesParser::new(SourceType::ALL.map(|t| t.as_str()))
}

fn memory_relation_values() -> PossibleValuesParser {
    PossibleValuesParser::new(MemoryRelation::ALL.map(|r| r.as_str()))
}

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

    /// Search documents, memory, or code symbols
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

        /// Search scope: docs, memory, code, or symbols. Omit to search docs+memory
        #[arg(long)]
        scope: Option<String>,

        /// Filter by symbol kind (function, struct, method, etc.) - used with code/symbols scopes
        #[arg(short, long)]
        kind: Option<String>,

        /// Filter by file path (substring match) - used with symbols scope
        #[arg(short, long)]
        file: Option<String>,

        /// Filter by entry type - used with memory scope
        #[arg(long = "entry-type", alias = "type", value_parser = entry_type_values())]
        entry_type: Option<String>,
    },

    /// Retrieve a document by ID, path, memory slug, glob pattern, or comma-separated list
    Get {
        /// Document ID, relative path, memory slug, glob pattern (e.g., "docs/*.md"), or comma-separated list (e.g., "42,43,44")
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

    /// Reindex everything (documents and source code), or specific files with --files
    Update {
        /// Only reindex specific files (absolute or relative paths)
        #[arg(long, num_args = 1..)]
        files: Vec<String>,

        /// Reindex every file regardless of modification time (applies config changes
        /// such as graph relations to already-indexed documents)
        #[arg(long)]
        force: bool,
    },

    /// Generate embeddings for documents
    Embed {
        /// Embed only this collection. Required to embed `claude_sessions`,
        /// which is excluded from the default (all-collections) embed.
        #[arg(long)]
        collection: Option<String>,
    },

    /// Start MCP server (stdio by default, --http or --https for network transport)
    Serve {
        /// Run as HTTP server instead of stdio
        #[arg(long)]
        http: bool,

        /// Run as HTTPS server with self-signed certificate
        #[arg(long)]
        https: bool,

        /// Bind address (default: 127.0.0.1:8080 for HTTP, 127.0.0.1:8443 for HTTPS)
        #[arg(long)]
        bind: Option<String>,

        /// Bearer token for authentication (required for HTTP/HTTPS)
        #[arg(long, env = "MDKB_TOKEN")]
        token: Option<String>,

        /// Allow starting --http/--https with no token, disabling authentication.
        /// Off by default: a tokenless network server accepts every request, so
        /// starting one is a hard error unless you opt in explicitly.
        #[arg(long)]
        allow_no_auth: bool,

        /// Global mode: serve multiple repos, discover roots via MCP roots/list protocol.
        /// Config loaded from ~/.mdkb/daemon.toml (whitelist, max_active_repos).
        #[arg(long)]
        global: bool,

        /// Run as persistent singleton daemon. Acquires an exclusive advisory
        /// lock on ~/.mdkb/daemon.pid so only one instance runs at a time.
        /// A second invocation exits 0 with a message on stderr.
        #[arg(long)]
        daemon: bool,

        /// Fully detach from the controlling terminal: double-fork + setsid,
        /// redirect stdio to ~/.mdkb/logs/daemon.log, then run the daemon.
        /// Only meaningful with --daemon. The parent exits as soon as the
        /// grandchild is spawned so the shell returns immediately.
        #[arg(long)]
        detach: bool,
    },

    /// Daemon lifecycle controls: status, stop, restart.
    #[command(subcommand)]
    Daemon(DaemonCommand),

    /// Connect Claude's stdio MCP transport to the mdkb daemon over its unix
    /// socket. Auto-spawns the daemon if it isn't running. On platforms
    /// without the daemon (Windows), serves MCP in-process instead. Set
    /// `MDKB_NO_DAEMON=1` to force in-process mode anywhere.
    Mcp {
        /// Override the daemon socket path (default: ~/.mdkb/daemon.sock).
        /// Unix only, and only when the daemon proxy runs: passing it where
        /// MCP serves in-process is an error rather than a silent no-op.
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// Show diagnostic statistics (index health, memory, code, sessions, hooks)
    Stats {
        /// Disable ANSI color output
        #[arg(long)]
        no_color: bool,
    },

    /// Query metrics and search quality analysis
    #[command(subcommand)]
    Metrics(MetricsCommand),

    /// Evaluate memory retrieval quality against a fixture
    #[command(subcommand)]
    Eval(EvalCommand),

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

    /// Query the knowledge graph (typed edges from frontmatter + wikilinks)
    #[command(subcommand)]
    Graph(GraphCommand),

    /// Manage A/B experiments
    #[command(subcommand)]
    Experiment(ExperimentCommand),

    /// Import Claude Code journal entries to memory
    #[command(subcommand)]
    Journal(JournalCommand),

    /// Setup and configure mdkb integrations
    #[command(subcommand)]
    Setup(SetupCommand),

    /// Code intelligence commands
    #[command(subcommand)]
    Code(CodeCommand),

    /// Reclaim disk space by vacuuming index.sqlite and code.sqlite
    Compact {
        /// Hard-delete archived session transcripts whose source jsonl is gone.
        /// Requires --older-than. Without this flag, compact only vacuums.
        #[arg(long)]
        prune_sessions: bool,

        /// Age cutoff for --prune-sessions, e.g. 90d, 12h, 2w. Only archived
        /// sessions older than this are deleted. Required with --prune-sessions.
        #[arg(long)]
        older_than: Option<String>,

        /// Write each pruned transcript as markdown to this directory before
        /// deleting it, so the archive is never silently lost.
        #[arg(long)]
        export: Option<std::path::PathBuf>,
    },

    /// Compact command reference for AI consumption
    Cheatsheet,

    /// Map each MCP tool to its CLI equivalent, and say where they differ
    ///
    /// The two surfaces expose overlapping capability under different names —
    /// the MCP tool is `memory_write`, the command is `mdkb memory add` — so
    /// this answers "what is the other name for this?" without reading source.
    Surface,

    /// Emit machine-readable CLI schema as JSON (omit COMMAND for the full tree)
    Schema {
        /// Specific subcommand to describe (omit for the full schema)
        command: Option<String>,
    },

    /// Claude Code session indexing
    #[command(subcommand)]
    Session(SessionCommand),

    /// Lifecycle hook dispatch (SessionStart, PostToolUse, …) and one-shot
    /// JSON-RPC client for hooks (reindex, search, memory-write, status, …).
    ///
    /// Lifecycle events read JSON from stdin and always exit 0. One-shot
    /// client methods connect the daemon hook socket (auto-spawning the
    /// daemon if needed), issue one JSON-RPC call, and print the result.
    /// Host CLIs must never be blocked by mdkb — failures log to stderr and
    /// still exit 0.
    #[command(subcommand)]
    Hook(HookCommand),
}

/// `mdkb daemon <cmd>` subcommands. See `cli::daemon` for behavior.
#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Print daemon pid, socket state, and uptime. Always exits 0.
    Status,

    /// Send SIGTERM to the running daemon and wait for shutdown.
    Stop,

    /// Stop (if running) and re-spawn the daemon detached.
    Restart,
}

/// `mdkb hook <cmd>` subcommands.
///
/// The first three variants are Claude Code / Codex lifecycle event names
/// (read stdin JSON, write response JSON). The remaining variants are
/// one-shot JSON-RPC client calls routed through the daemon hook socket.
#[derive(Subcommand, Debug)]
pub enum HookCommand {
    /// Lifecycle event: session started.
    SessionStart,

    /// Lifecycle event: user prompt submitted.
    UserPromptSubmit,

    /// Lifecycle event: tool use completed.
    PostToolUse,

    /// Lifecycle event: before tool execution (advisory).
    PreToolUse,

    /// Lifecycle event: agent stopped (end of episode — feeds prior mining).
    Stop,

    /// Trigger an index refresh via the daemon.
    Reindex {
        /// Restrict reindex to these files (absolute or repo-relative).
        #[arg(long, num_args = 1..)]
        files: Vec<String>,

        /// Target repo root (defaults to $PWD).
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Issue a search call through the daemon.
    Search {
        /// Search query.
        query: String,

        /// Search scope (docs, memory, code, symbols). Omit for docs+memory.
        #[arg(long)]
        scope: Option<String>,

        /// Maximum results.
        #[arg(long)]
        limit: Option<usize>,

        /// Target repo root (defaults to $PWD).
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Persist a memory entry via the daemon.
    MemoryWrite {
        /// Entry slug (e.g. "auth-oauth2-flow").
        #[arg(long)]
        id: String,

        /// Concise title.
        #[arg(long)]
        title: String,

        /// Entry type.
        #[arg(long = "entry-type", alias = "type", default_value = "topic", value_parser = entry_type_values())]
        entry_type: String,

        /// Entry body (markdown).
        #[arg(long)]
        content: String,

        /// Comma-separated tags.
        #[arg(long)]
        tags: Option<String>,

        /// TTL in seconds.
        #[arg(long)]
        ttl: Option<u64>,

        /// Target repo root (defaults to $PWD).
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Confirm a pending memory outcome via the daemon.
    MemoryConfirm {
        /// Entry slug.
        #[arg(long)]
        id: String,

        /// Outcome (confirmed, rejected, …).
        #[arg(long)]
        outcome: String,

        /// Target repo root (defaults to $PWD).
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Query daemon-side index status.
    Status {
        /// Target repo root (defaults to $PWD).
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

/// Session indexing subcommands.
#[derive(Subcommand, Debug)]
pub enum SessionCommand {
    /// Index Claude Code session JSONL files
    Index {
        /// Path to sessions base directory (default: ~/.claude/projects)
        #[arg(long)]
        sessions_path: Option<String>,

        /// Project root to match (default: current directory)
        #[arg(long)]
        project_root: Option<String>,
    },
}

/// Journal import subcommands.
#[derive(Subcommand, Debug)]
pub enum JournalCommand {
    /// Import a journal file to memory entries
    Import {
        /// Path to journal markdown file
        path: String,

        /// Dry run - show what would be imported without saving
        #[arg(short, long)]
        dry_run: bool,
    },

    /// Import all journal entries from a directory
    ImportAll {
        /// Path to journal directory (defaults to .claude/journal/)
        #[arg(long)]
        dir: Option<String>,

        /// Dry run - show what would be imported without saving
        #[arg(short = 'n', long)]
        dry_run: bool,

        /// Skip entries already imported (based on source_path)
        #[arg(long, default_value = "true")]
        skip_existing: bool,
    },
}

/// A/B Experiment subcommands.
#[derive(Subcommand, Debug)]
pub enum ExperimentCommand {
    /// Create a new A/B experiment
    Create {
        /// Experiment name (unique identifier)
        name: String,

        /// JSON config for variant A
        #[arg(long)]
        config_a: String,

        /// JSON config for variant B
        #[arg(long)]
        config_b: String,

        /// Description of the experiment
        #[arg(short, long)]
        description: Option<String>,

        /// Traffic split to variant A (0.0-1.0, default 0.5)
        #[arg(short, long, default_value = "0.5")]
        split: f64,

        /// Minimum samples per variant before significance calculation
        #[arg(short, long, default_value = "100")]
        min_samples: i64,
    },

    /// Show experiment status with metrics
    Status {
        /// Experiment name
        name: String,
    },

    /// End an experiment and record winner
    End {
        /// Experiment name
        name: String,

        /// Winning variant (A or B), auto-determined if not specified
        #[arg(short, long)]
        winner: Option<String>,
    },

    /// Cancel an experiment without a winner
    Cancel {
        /// Experiment name
        name: String,
    },

    /// List all experiments
    List {
        /// Show only running experiments
        #[arg(short, long)]
        running: bool,
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

    /// Rename a collection
    Rename {
        /// Current name
        old_name: String,

        /// New name
        new_name: String,
    },

    /// List collections with their path, pattern, and document count
    List,
}

/// Evaluation subcommands.
#[derive(Subcommand, Debug)]
pub enum EvalCommand {
    /// Recall@k / MRR over a fixture (deterministic, BM25-only, no API)
    Recall {
        /// Path to a JSON fixture (defaults to the bundled synthetic corpus)
        #[arg(short, long)]
        fixture: Option<std::path::PathBuf>,
        /// Cutoff rank k
        #[arg(short, long, default_value = "5")]
        k: usize,
    },

    /// Answer-support accuracy over a fixture (deterministic SubstringJudge)
    Judge {
        /// Path to a JSON fixture (defaults to the bundled synthetic corpus)
        #[arg(short, long)]
        fixture: Option<std::path::PathBuf>,
        /// Cutoff rank k
        #[arg(short, long, default_value = "5")]
        k: usize,
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
    #[command(alias = "write", alias = "create")]
    Add {
        /// Entry ID (slug, e.g., "auth-oauth2-flow")
        id: String,

        /// Concise title (max 50 chars)
        #[arg(short, long)]
        title: String,

        /// Entry type
        #[arg(short = 'T', long, alias = "type", default_value = "topic", value_parser = entry_type_values())]
        entry_type: String,

        /// Tags (comma-separated)
        #[arg(long)]
        tags: Option<String>,

        /// Content (if not provided, reads from stdin)
        #[arg(short, long, alias = "body")]
        content: Option<String>,

        /// Read content from file instead of --content or stdin.
        #[arg(short, long, conflicts_with = "content")]
        file: Option<std::path::PathBuf>,

        /// TTL in seconds. Entry expires after this duration.
        #[arg(long)]
        ttl: Option<u64>,

        /// Reminder due time in seconds from now. Use with --entry-type reminder.
        #[arg(long)]
        due_in: Option<u64>,

        /// Provenance/trust of this entry (default: user_statement on insert;
        /// preserved on re-write unless given). Drives the confidence authority
        /// multiplier, most to least authoritative: 1.0, 0.85, 0.70, 0.65.
        #[arg(long, value_parser = source_type_values())]
        source_type: Option<String>,
    },

    /// Show a memory entry
    #[command(alias = "get")]
    Show {
        /// Entry ID
        id: String,
    },

    /// Record a confirmation signal against an entry (raises/lowers confidence).
    /// Routed through the daemon like every other store mutation.
    Confirm {
        /// Entry ID (slug)
        id: String,

        /// confirmed (+1) or refuted (-1, floor 0)
        #[arg(long)]
        outcome: String,
    },

    /// Link a memory entry to another entry or document via a typed relation
    Link {
        /// Source entry ID (slug)
        id: String,

        /// Typed relation from the source entry to the target
        #[arg(value_parser = memory_relation_values())]
        relation: String,

        /// Target: a memory slug, or a doc relative path with --doc
        target: String,

        /// Treat target as a document path instead of a memory slug
        #[arg(long)]
        doc: bool,

        /// Record this agent as provenance on the source entry
        #[arg(long)]
        agent: Option<String>,
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
    #[command(alias = "delete")]
    Rm {
        /// Entry ID
        id: String,
    },

    /// Show revision history for a memory entry
    History {
        /// Entry ID
        id: String,
    },

    /// Export memory entries to a folder of markdown files
    Export {
        /// Output directory (default: .mdkb/memory/entries)
        #[arg(long)]
        dir: Option<std::path::PathBuf>,

        /// Include expired entries
        #[arg(long)]
        include_expired: bool,

        /// Overwrite existing files
        #[arg(long)]
        overwrite: bool,

        /// Show what would be exported without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Reconcile memory entries with their git-tracked markdown files
    ///
    /// Runs automatically as part of `mdkb update`; use this to pick up a
    /// `git pull` without reindexing documents.
    Sync,

    /// Import memory entries from a JSON file or markdown folder
    Import {
        /// Path to JSON file or markdown directory
        path: String,

        /// Show what would be imported without saving
        #[arg(long)]
        dry_run: bool,

        /// Skip entries that already exist (by ID)
        #[arg(long)]
        skip_duplicates: bool,
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

/// Knowledge-graph query subcommands.
#[derive(Subcommand, Debug)]
pub enum GraphCommand {
    /// Outgoing edges from an entity (what it points to)
    Links {
        /// Entity (document path, ID, or raw slug)
        entity: String,

        /// Filter by relation type
        #[arg(short, long)]
        relation: Option<String>,
    },

    /// Incoming edges to an entity (what points to it)
    Backlinks {
        /// Entity (document path, ID, or raw slug)
        entity: String,

        /// Filter by relation type
        #[arg(short, long)]
        relation: Option<String>,
    },

    /// Adjacent entities up to a traversal depth (undirected)
    Neighbors {
        /// Entity (document path, ID, or raw slug)
        entity: String,

        /// Filter by relation type
        #[arg(short, long)]
        relation: Option<String>,

        /// Maximum traversal depth
        #[arg(short, long, default_value = "1")]
        depth: u32,
    },

    /// Shortest path between two entities (undirected)
    Path {
        /// Start entity (document path, ID, or raw slug)
        a: String,

        /// Target entity (document path, ID, or raw slug)
        b: String,

        /// Maximum hops to search
        #[arg(long, default_value = "6")]
        max_hops: u32,
    },

    /// References pointing at no indexed document (full scan; explicit use only)
    Dangling,

    /// Entities ranked by degree centrality (full scan; explicit use only)
    Hubs {
        /// Filter to a single relation type
        #[arg(short, long)]
        relation: Option<String>,

        /// Maximum number of entities to return (0 = all)
        #[arg(short, long, default_value = "20")]
        limit: usize,
    },
}

/// Setup and configuration subcommands.
#[derive(Subcommand, Debug)]
pub enum SetupCommand {
    /// Register mdkb as an MCP server
    #[command(subcommand)]
    Mcp(SetupMcpCommand),

    /// Register mdkb lifecycle hooks
    #[command(subcommand)]
    Hooks(SetupHooksCommand),

    /// Remove mdkb registrations (MCP and/or hooks)
    #[command(subcommand)]
    Remove(SetupRemoveCommand),
}

/// MCP setup subcommands.
#[derive(Subcommand, Debug)]
pub enum SetupMcpCommand {
    /// Register mdkb with Claude Code
    Claude {
        /// Scope: local (project-specific, default), user (global), or project (deprecated alias for local)
        #[arg(short, long, default_value = "local")]
        scope: String,

        /// Skip confirmation prompt
        #[arg(short, long)]
        yes: bool,
    },

    /// Register mdkb with Codex CLI (writes ~/.codex/config.toml)
    Codex {
        /// Print the merged config.toml to stdout without writing
        #[arg(long)]
        dry_run: bool,
    },
}

/// Hooks setup subcommands.
#[derive(Subcommand, Debug)]
pub enum SetupHooksCommand {
    /// Register lifecycle hooks with Claude Code
    Claude {
        /// Scope: local writes .claude/settings.local.json; user writes ~/.claude/settings.json
        #[arg(short, long, default_value = "local")]
        scope: String,

        /// Comma-separated list of events to skip (session-start, user-prompt-submit, post-tool-use, pre-tool-use)
        #[arg(long, default_value = "")]
        disable: String,

        /// Print the merged settings to stdout without writing
        #[arg(long)]
        dry_run: bool,

        /// Claude Code profile directory (default: ~/.claude).
        /// Use for non-standard profiles like ~/.claude-private.
        #[arg(long)]
        profile_dir: Option<PathBuf>,
    },

    /// Register lifecycle hooks with Codex CLI (writes ~/.codex/hooks.json)
    Codex {
        /// Comma-separated list of events to skip (session-start, user-prompt-submit, post-tool-use)
        #[arg(long, default_value = "")]
        disable: String,

        /// Print the merged hooks.json to stdout without writing
        #[arg(long)]
        dry_run: bool,
    },
}

/// Removal subcommands.
#[derive(Subcommand, Debug)]
pub enum SetupRemoveCommand {
    /// Remove mdkb MCP server registration
    #[command(subcommand)]
    Mcp(RemoveMcpCommand),

    /// Remove mdkb lifecycle hooks
    #[command(subcommand)]
    Hooks(RemoveHooksCommand),

    /// Remove all Claude Code mdkb registrations (MCP + hooks)
    Claude {
        /// Scope: local (project-specific, default) or user (global)
        #[arg(short, long, default_value = "local")]
        scope: String,
    },
}

/// MCP removal subcommands.
#[derive(Subcommand, Debug)]
pub enum RemoveMcpCommand {
    /// Remove mdkb from Claude Code
    Claude {
        /// Scope: local (project-specific, default) or user (global)
        #[arg(short, long, default_value = "local")]
        scope: String,
    },

    /// Remove mdkb from Codex CLI (~/.codex/config.toml)
    Codex,
}

/// Hooks removal subcommands.
#[derive(Subcommand, Debug)]
pub enum RemoveHooksCommand {
    /// Remove mdkb hooks from Claude Code
    Claude {
        /// Scope: local or user
        #[arg(short, long, default_value = "local")]
        scope: String,

        /// Claude Code profile directory (default: ~/.claude)
        #[arg(long)]
        profile_dir: Option<PathBuf>,
    },

    /// Remove mdkb hooks from Codex CLI (~/.codex/hooks.json)
    Codex,
}

/// Code intelligence subcommands.
#[derive(Subcommand, Debug)]
pub enum CodeCommand {
    /// Initialize code index in .mdkb/code/
    Init,

    /// Build code index from source files
    Index {
        /// Paths to index (defaults to current directory)
        paths: Vec<String>,

        /// Force full reindex (discard existing index)
        #[arg(long)]
        force: bool,
    },

    /// Fuzzy symbol search
    Search {
        /// Search query
        query: String,

        /// Maximum results
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Filter by symbol kind (function, struct, method, etc.)
        #[arg(short, long)]
        kind: Option<String>,
    },

    /// Exact symbol lookup by name
    Find {
        /// Symbol name
        name: String,

        /// Filter by symbol kind
        #[arg(short, long)]
        kind: Option<String>,

        /// Filter by file path (substring match)
        #[arg(short, long)]
        file: Option<String>,

        /// Maximum number of results
        #[arg(short, long, default_value = "10")]
        limit: usize,
    },

    /// Show what functions a symbol calls
    Calls {
        /// Symbol name
        name: String,
    },

    /// Show what calls a given symbol
    Callers {
        /// Symbol name
        name: String,
    },

    /// Impact analysis: dependency graph from a symbol
    Impact {
        /// Symbol name
        name: String,

        /// Maximum traversal depth
        #[arg(short, long, default_value = "3")]
        depth: usize,
    },

    /// Show code index statistics
    Info,

    /// Parse a file and output symbols as JSONL
    Parse {
        /// File path to parse
        file: String,
    },
}

impl Cli {
    /// Parse CLI arguments.
    pub fn parse_args() -> Self {
        Self::parse()
    }
}
