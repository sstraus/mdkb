//! CLI layer - command parsing and execution with clap.

pub mod cheatsheet;
pub mod daemon;
pub mod doctor;
pub mod handlers;
// Portable: only the daemon socket transport inside is unix-gated. Hooks run
// their work in-process everywhere else (issue #7).
pub mod hook_client;
pub mod hook_logic;
pub mod host_agy;
pub mod journal;
#[cfg(unix)]
pub mod mcp_proxy;
pub mod memory;
pub mod priority;
pub mod repos;
pub mod retired;
pub mod setup;
pub mod setup_agy;
pub mod setup_ego;
pub mod setup_grok;
pub mod stats_render;
pub mod stats_render_report;
pub mod stats_report;
pub mod usage_errors;

use std::path::PathBuf;

use clap::builder::PossibleValuesParser;
use clap::{Args, Parser, Subcommand};

use crate::eval::recall::Mode as EvalMode;
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

fn parse_memory_trigger(
    raw: &str,
) -> std::result::Result<crate::store::priors::TriggerMatcher, String> {
    let matcher: crate::store::priors::TriggerMatcher =
        serde_json::from_str(raw).map_err(|error| format!("invalid trigger JSON: {error}"))?;
    crate::store::memory::validate_trigger_matchers(std::slice::from_ref(&matcher))
        .map_err(|error| error.to_string())?;
    Ok(matcher)
}

fn memory_relation_values() -> PossibleValuesParser {
    PossibleValuesParser::new(MemoryRelation::ALL.map(|r| r.as_str()))
}

/// mdkb - Local markdown knowledge base with semantic search.
#[derive(Parser, Debug)]
#[command(name = "mdkb")]
#[command(author, version, about, long_about = None)]
#[command(after_help = "START HERE
  mdkb cheatsheet            task -> command table, the MCP <-> CLI map, and how to start
  mdkb <command> --help      flags of one command
  mdkb schema <command>      the same as JSON
ONLINE REFERENCE (full URLs; this help needs no network)
  CLI commands: https://github.com/sstraus/mdkb/blob/main/README.md#cli-reference
  Configuration and ignore rules: https://github.com/sstraus/mdkb/blob/main/README.md#configuration
  All config keys, types, and validation: https://github.com/sstraus/mdkb/blob/main/src/config.rs
  Integration setup and hook settings: https://github.com/sstraus/mdkb/blob/main/docs/hooks.md
  Graph settings: https://github.com/sstraus/mdkb/blob/main/docs/graph.md
  Memory conventions: https://github.com/sstraus/mdkb/blob/main/docs/memory-guidelines.md")]
pub struct Cli {
    /// Verbose output (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Output format
    #[arg(long, global = true, default_value = "text")]
    pub format: OutputFormat,

    /// Repo to run in: a name, a path, a comma list of either, or "*" for every known repo.
    /// Only `search` and `memory search` fan out over several repos; every other command needs one.
    /// Omit for this directory's repo
    ///
    /// A `PathBuf` on purpose: `hook` subcommands declare their own `--root` as a path, and
    /// clap shares one value type per argument name across a global flag and its subcommands.
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,

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

/// Whatever was passed to a removed command. Accepted only so the command can
/// fail with its replacement instead of clap's generic "unrecognized
/// subcommand", which names nothing.
#[derive(Args, Debug)]
pub struct RetiredArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    pub args: Vec<String>,
}

pub use retired::consolidated_replacement;

/// The error a removed command fails with: what it was and the command that
/// replaces it. `run` must work as typed.
pub fn retired(old: &str, run: &str) -> crate::error::Error {
    crate::error::Error::usage(format!("`mdkb {old}` was removed"), run)
}

/// CLI subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialize .mdkb/ directory in current project
    Init,

    /// Manage collections
    #[command(subcommand)]
    Collection(CollectionCommand),

    /// Removed: use `mdkb code dup`.
    #[command(hide = true, disable_help_flag = true)]
    Dup(RetiredArgs),

    /// Removed: use `mdkb code coupling` (documents: `mdkb graph coupling`).
    #[command(hide = true, disable_help_flag = true)]
    Coupling(RetiredArgs),

    /// Search docs and memory (two groups); code has `code search` and `code grep`
    Search {
        /// Search words; unquoted words are joined into one query
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,

        /// Search documents only
        #[arg(long, conflicts_with_all = ["memory", "entry_type"])]
        docs: bool,

        /// Search memory entries only (`--entry-type` implies it)
        #[arg(long)]
        memory: bool,

        /// Maximum results per group
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Search one collection (documents only)
        #[arg(short, long, conflicts_with = "memory")]
        collection: Option<String>,

        /// Include superseded/retracted documents
        #[arg(long)]
        include_superseded: bool,

        /// Only memory entries of this type (implies --memory)
        #[arg(long = "entry-type", alias = "type", value_parser = entry_type_values())]
        entry_type: Option<String>,

        /// MCP scope name; still works so MCP and Wiz callers keep running
        #[arg(long, hide = true)]
        scope: Option<String>,
    },

    /// Retrieve a document by ID, path, memory slug, glob pattern, or comma-separated list
    Get {
        /// Document ID, relative path, memory slug, glob pattern (e.g., "docs/*.md"), or comma-separated list (e.g., "42,43,44")
        id: String,

        /// Line range (e.g., 10:50)
        #[arg(long)]
        lines: Option<String>,

        /// Keep only glob matches from this collection
        #[arg(short, long)]
        collection: Option<String>,
    },

    /// Reindex documents, source code and embeddings (everything by default)
    ///
    /// With no flag: reindex documents and source code, embed what is missing, and
    /// sync memory. One flag narrows the run: `--code` the source index only
    /// (`--code --force` rebuilds it from scratch), `--sessions` the Claude Code
    /// transcripts, `--reembed` every embedding for the selected model,
    /// `--collection NAME` the embeddings of one collection (the only way to
    /// embed `claude_sessions`, which the default run skips).
    Update {
        /// Only reindex specific files (absolute or relative paths)
        #[arg(long, num_args = 1.., conflicts_with_all = ["code", "sessions", "reembed", "collection"])]
        files: Vec<String>,

        /// Force document reindexing regardless of modification time. With
        /// `--code`: rebuild the source index from scratch
        #[arg(long, conflicts_with_all = ["sessions", "reembed", "collection"])]
        force: bool,

        /// Only the source code index
        #[arg(long, conflicts_with_all = ["sessions", "reembed", "collection"])]
        code: bool,

        /// Only Claude Code session transcripts
        #[arg(long, conflicts_with_all = ["reembed", "collection"])]
        sessions: bool,

        /// Rebuild documents, chunks, memories and semantic code embeddings for the selected model
        #[arg(long)]
        reembed: bool,

        /// Embed only this collection (add --reembed to rebuild it). Required to embed `claude_sessions`
        #[arg(long)]
        collection: Option<String>,

        /// With `--sessions`: sessions base directory (default: ~/.claude/projects)
        #[arg(long, requires = "sessions")]
        sessions_path: Option<String>,

        /// With `--sessions`: project root to match (default: this repo)
        #[arg(long, requires = "sessions")]
        project_root: Option<String>,
    },

    /// Hidden legacy alias of `mdkb update` (see `retired::legacy_alias_argv`).
    /// DEFERRED (2026-11-23) — remove in February 2027 (Boss).
    #[command(hide = true, disable_help_flag = true)]
    Embed(RetiredArgs),

    /// Start the MCP server (internal)
    #[command(hide = true)]
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

    /// Operate on every repository the daemon knows: `refresh`.
    #[command(subcommand)]
    Repos(ReposCommand),

    /// Connect Claude's stdio MCP transport to the daemon (internal)
    #[command(hide = true)]
    Mcp {
        /// Override the daemon socket path (default: ~/.mdkb/daemon.sock).
        /// Unix only, and only when the daemon proxy runs: passing it where
        /// MCP serves in-process is an error rather than a silent no-op.
        #[arg(long)]
        socket: Option<PathBuf>,
    },

    /// List problems with this installation and the fix for each (non-zero exit when one is an error)
    Doctor {
        /// Also run the slow checks (the live prior-distiller probe)
        #[arg(long)]
        full: bool,
    },

    /// One page: index health, memory, code, sessions, hooks, daemon, telemetry, recall
    ///
    /// Every line that is not healthy ends with the command that fixes it.
    /// `--format json` prints one object.
    Status {
        /// Disable ANSI color output
        #[arg(long)]
        no_color: bool,
    },

    /// Hidden legacy alias of `mdkb status` (see `retired::legacy_alias_argv`).
    /// DEFERRED (2026-11-23) — remove in February 2027 (Boss).
    #[command(hide = true, disable_help_flag = true)]
    Stats(RetiredArgs),

    /// Query metrics and search quality analysis
    #[command(subcommand)]
    Metrics(MetricsCommand),

    /// Evaluate memory retrieval quality against a fixture (internal)
    #[command(subcommand, hide = true)]
    Eval(EvalCommand),

    /// Manage memory entries for AI knowledge persistence
    #[command(subcommand)]
    Memory(MemoryCommand),

    /// Manage document evolution relationships
    #[command(subcommand)]
    Evolve(EvolveCommand),

    /// Query the knowledge graph (typed edges from frontmatter + wikilinks)
    Graph {
        #[command(subcommand)]
        cmd: GraphCommand,
    },

    /// Removed: use `mdkb memory import --journal PATH`.
    #[command(hide = true, disable_help_flag = true)]
    Journal(RetiredArgs),

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

    /// Prints the "MCP <-> CLI" section of `mdkb cheatsheet`
    #[command(hide = true)]
    Surface,

    /// Emit the CLI schema of one command as JSON
    Schema {
        /// Specific subcommand to describe (omit for the full schema)
        command: Option<String>,
    },

    /// Removed: use `mdkb update --sessions`.
    #[command(hide = true, disable_help_flag = true)]
    Session(RetiredArgs),

    /// Lifecycle hook dispatch and one-shot hook client (internal)
    #[command(subcommand, hide = true)]
    Hook(HookCommand),
}

/// `mdkb daemon <cmd>` subcommands. See `cli::daemon` for behavior.
#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Removed: use `mdkb status`.
    #[command(hide = true, disable_help_flag = true)]
    Status(RetiredArgs),

    /// Send SIGTERM to the running daemon and wait for shutdown.
    Stop,

    /// Stop (if running) and re-spawn the daemon detached.
    Restart,
}

/// `mdkb repos <cmd>` subcommands. See `cli::repos` for behavior.
#[derive(Subcommand, Debug)]
pub enum ReposCommand {
    /// List the repository map, one row per repo, read live from the stores.
    ///
    /// Asks the running daemon for its repos; with no daemon, reads
    /// `repos.json`. Never opens a store for writing and never walks the disk.
    List,

    /// Migrate stores older than this binary to its schema, one at a time.
    ///
    /// Each store is copied to `index.sqlite.pre-migrate-v<N>-<time>` beside it
    /// and the copy is verified before anything is written. A store whose copy
    /// fails is left untouched. Exits non-zero when any store failed.
    Refresh {
        /// Which stores to refresh.
        #[arg(long, value_enum)]
        only: RefreshFilter,
    },

    /// Remove repositories that are gone from disk or hold no store from the
    /// daemon's registry.
    ///
    /// Asks the running daemon, which owns the in-memory map and `repos.json`.
    /// Only roots whose directory no longer exists (Gone) or whose `.mdkb` is
    /// missing, empty or holds only lock files (NoStore) are removed. With no PATH, every such
    /// root is removed and all other roots are skipped without a mention. With
    /// PATHs, a path that is healthy, cannot be read or is not in the registry
    /// is refused and reported, the removable paths of the same selection are
    /// still removed, and the exit status is non-zero.
    /// Nothing but the registry entry is removed: no repository, store or index
    /// is deleted, and hand-written scopes and `daemon.toml` stay. PATH must be
    /// spelled as `mdkb repos list` prints it. Fails without changing anything
    /// when no daemon runs or the daemon predates this command. On Unix,
    /// install this build and run `mdkb daemon restart`; other platforms have
    /// no daemon to prune. A timeout leaves the outcome unconfirmed, so check
    /// `mdkb repos list` before retrying.
    Prune {
        /// Roots to remove; default: every Gone or NoStore root.
        paths: Vec<PathBuf>,

        /// Report what would be removed and change nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Which stores `mdkb repos refresh` acts on.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshFilter {
    /// Every store whose schema is older than this binary's.
    Outdated,
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

    /// Lifecycle event from Antigravity CLI (`agy`), in agy's own hook format.
    Agy {
        /// The agy event this hook was registered for.
        #[arg(value_enum)]
        event: crate::cli::host_agy::AgyEvent,
    },

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

        /// confirmed, refuted, or misfired for an injected prior.
        #[arg(long)]
        outcome: String,

        /// Target repo root (defaults to $PWD).
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Removed: use `mdkb doctor` or `mdkb status`.
    #[command(hide = true, disable_help_flag = true)]
    Status(RetiredArgs),
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

    /// Change a collection's path or pattern in place, without dropping it
    Update {
        /// Collection name
        name: String,

        /// New glob pattern for files (unchanged when omitted). Documents that
        /// still match keep their index entry and embedding.
        #[arg(short, long)]
        pattern: Option<String>,

        /// New path to directory (unchanged when omitted). Document paths are
        /// stored against the old base, so the next `update` re-indexes and
        /// re-embeds the collection's contents.
        #[arg(long)]
        path: Option<String>,
    },

    /// Remove a collection
    #[command(alias = "remove", alias = "delete")]
    Rm {
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
    /// Recall@k / MRR over a fixture, through the production memory search
    Recall {
        /// Path to a JSON fixture (defaults to the bundled synthetic corpus)
        #[arg(short, long)]
        fixture: Option<std::path::PathBuf>,
        /// Cutoff rank k
        #[arg(short, long, default_value = "5")]
        k: usize,
        /// Retrieval mode; `all` runs bm25, embedding and hybrid in turn
        #[arg(long, default_value = "all", value_parser = eval_mode_values())]
        mode: String,
        /// Fetch the ONNX model when it is not cached (otherwise the embedding
        /// and hybrid modes are skipped with a reason)
        #[arg(long)]
        download: bool,
        /// Exit 1 when any mode that ran scores recall@k below this
        #[arg(long)]
        min_recall: Option<f64>,
        /// Exit 1 when any mode that ran scores precision below this. Guards
        /// the absolute relevance floor: lowering it raises recall, so a
        /// recall floor alone cannot catch its removal.
        #[arg(long)]
        min_precision: Option<f64>,
    },

    /// Answer-support accuracy over a fixture (deterministic SubstringJudge)
    Judge {
        /// Path to a JSON fixture (defaults to the bundled synthetic corpus)
        #[arg(short, long)]
        fixture: Option<std::path::PathBuf>,
        /// Cutoff rank k
        #[arg(short, long, default_value = "5")]
        k: usize,
        /// Retrieval mode; `all` runs bm25, embedding and hybrid in turn
        #[arg(long, default_value = "all", value_parser = eval_mode_values())]
        mode: String,
        /// Fetch the ONNX model when it is not cached (otherwise the embedding
        /// and hybrid modes are skipped with a reason)
        #[arg(long)]
        download: bool,
    },
}

fn eval_mode_values() -> PossibleValuesParser {
    let mut values = vec!["all"];
    values.extend(EvalMode::ALL.map(|m| m.as_str()));
    PossibleValuesParser::new(values)
}

/// Expand the `--mode` value: `all` is every mode in `EvalMode::ALL` order.
pub fn parse_eval_modes(mode: &str) -> Vec<EvalMode> {
    if mode == "all" {
        return EvalMode::ALL.to_vec();
    }
    // `eval_mode_values` already rejected anything else.
    mode.parse().into_iter().collect()
}

/// Metrics subcommands.
#[derive(Subcommand, Debug)]
pub enum MetricsCommand {
    /// Removed: use `mdkb status`.
    #[command(hide = true, disable_help_flag = true)]
    Status(RetiredArgs),

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

    /// Delete all stored developer query telemetry
    Purge {
        /// Confirm destructive deletion
        #[arg(long)]
        yes: bool,
    },
}

/// Memory management subcommands.
#[derive(Subcommand, Debug)]
pub enum MemoryCommand {
    /// Merge curator-reviewed prior clusters into the oldest cluster (internal)
    #[command(hide = true)]
    CuratePriors {
        /// At least two distinct active cluster IDs
        #[arg(required = true, num_args = 2..)]
        cluster_ids: Vec<String>,
    },

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

        /// JSON trigger matcher; repeat for OR alternatives. For example: {"tool":"Agent"}.
        #[arg(long, value_parser = parse_memory_trigger)]
        trigger: Vec<crate::store::priors::TriggerMatcher>,

        /// Provenance/trust of this entry (default: user_statement on insert;
        /// preserved on re-write unless given). Drives the confidence authority
        /// multiplier, most to least authoritative: 1.0, 0.85, 0.70, 0.65.
        #[arg(long, value_parser = source_type_values())]
        source_type: Option<String>,

        /// Typed edge RELATION:TARGET[:memory|doc]. Repeat for multiple edges.
        #[arg(long)]
        relates: Vec<crate::core::memory::WriteRelation>,

        /// Record this agent as entry provenance.
        #[arg(long)]
        agent: Option<String>,

        /// On near-duplicate conflict, write and attach a contradicts edge.
        #[arg(long, value_parser = ["contradicts"])]
        on_conflict: Option<String>,

        /// Validate and describe the write without persisting it.
        #[arg(long)]
        dry_run: bool,
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

        /// confirmed or refuted; misfired means a true injected prior fired out of context.
        #[arg(long)]
        outcome: String,

        /// Session that received an injected prior (printed by the recall hook).
        #[arg(long)]
        session: Option<String>,
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

        /// Filter by status: active (default), superseded, archived, or all
        #[arg(short, long)]
        status: Option<String>,
    },

    /// List cluster-less user priors with conservative migration proposals (internal)
    #[command(hide = true)]
    Propose,

    /// Search memory entries (same as `mdkb search QUERY --memory`)
    Search {
        /// Search words; unquoted words are joined into one query
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,

        /// Maximum results
        #[arg(short, long, default_value = "10")]
        limit: usize,

        /// Only entries of this type
        #[arg(long = "entry-type", alias = "type", value_parser = entry_type_values())]
        entry_type: Option<String>,
    },

    /// Get warmup index (compact list for AI session start)
    Warmup {
        /// Maximum entries
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },

    /// Delete a memory entry
    #[command(alias = "delete", alias = "remove")]
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

    /// Import memory entries from a JSON file or markdown folder, or from Claude Code journals
    Import {
        /// Path to JSON file or markdown directory
        #[arg(required_unless_present = "journal", conflicts_with = "journal")]
        path: Option<String>,

        /// Import Claude Code journal entries instead: a journal markdown file, or a directory
        /// of them (such as .claude/journal). Entries already imported are skipped
        #[arg(long, value_name = "PATH")]
        journal: Option<String>,

        /// Show what would be imported without saving
        #[arg(long)]
        dry_run: bool,

        /// Skip entries that already exist (by ID)
        #[arg(long)]
        skip_duplicates: bool,
    },

    /// Archive expired entries and aged lifecycle entries (reminder, prior, handoff)
    Prune {
        /// Age in days after which an unread reminder, prior or handoff is archived.
        /// Topics, problems and decisions are never archived for age: only their
        /// --ttl retires them (default: 90)
        #[arg(short, long, default_value = "90")]
        days: u32,

        /// List exactly what would be archived without archiving anything
        #[arg(long)]
        dry_run: bool,
    },

    /// List entries worth re-reading, selected from mechanical signals
    ///
    /// Reports dead code references, source changed since a measurement,
    /// near-duplicate and contradicting pairs, and expired or aged entries.
    /// It decides nothing: no confirmation, refutation or supersession is
    /// written. Thresholds live under `[memory.audit]` in `.mdkb/config.toml`.
    Audit {
        /// Select and report without recording that the entries were looked at
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
    /// Show the evolution history of a document
    History {
        /// Document path or ID
        path: String,
    },

    /// Find the current version of a superseded document
    Current {
        /// Document path or ID
        path: String,
    },

    /// Show what superseded this document
    SupersededBy {
        /// Document path or ID
        path: String,
    },

    /// Record that a document supersedes, updates, corrects, retracts or extends another
    ///
    /// Edges run newer -> older. `supersedes` and `retracts` hide the older document.
    Link {
        /// The newer document (path or ID)
        new: String,

        /// How NEW relates to OLD
        #[arg(value_parser = ["supersedes", "updates", "corrects", "retracts", "extends"])]
        relation: String,

        /// The older document (path or ID)
        old: String,

        /// Section the edge covers (`updates` only)
        #[arg(short, long)]
        scope: Option<String>,

        /// Why the edge exists
        #[arg(short, long)]
        reason: Option<String>,
    },

    /// Remove an evolution edge by the id `history` prints
    ///
    /// A document no other supersedes/retracts edge hides becomes current again.
    #[command(alias = "rm", alias = "remove", alias = "delete")]
    Unlink {
        /// Evolution relationship id
        id: i64,
    },

    /// Removed: use `mdkb evolve link NEW supersedes OLD`.
    #[command(hide = true, disable_help_flag = true)]
    Supersedes(RetiredArgs),

    /// Removed: use `mdkb evolve link NEW updates OLD`.
    #[command(hide = true, disable_help_flag = true)]
    Updates(RetiredArgs),

    /// Removed: use `mdkb evolve link NEW corrects OLD`.
    #[command(hide = true, disable_help_flag = true)]
    Corrects(RetiredArgs),

    /// Removed: use `mdkb evolve link NEW retracts OLD`.
    #[command(hide = true, disable_help_flag = true)]
    Retracts(RetiredArgs),

    /// Removed: use `mdkb evolve link NEW extends OLD`.
    #[command(hide = true, disable_help_flag = true)]
    Extends(RetiredArgs),
}

/// Knowledge-graph query subcommands.
#[derive(Subcommand, Debug)]
pub enum GraphCommand {
    /// Outgoing edges from an entity (what it points to)
    Links {
        /// Entity (document path, ID, or raw slug; a memory id with --scope memory)
        entity: String,

        /// Filter by relation type
        #[arg(short, long)]
        relation: Option<String>,

        /// Graph to read: doc (frontmatter and wikilinks) or memory (memory link edges)
        #[arg(long, default_value = "doc", value_parser = ["doc", "memory"])]
        scope: String,
    },

    /// Incoming edges to an entity (what points to it)
    Backlinks {
        /// Entity (document path, ID, or raw slug; a memory id or doc path with --scope memory)
        entity: String,

        /// Filter by relation type
        #[arg(short, long)]
        relation: Option<String>,

        /// Graph to read: doc (frontmatter and wikilinks) or memory (memory link edges)
        #[arg(long, default_value = "doc", value_parser = ["doc", "memory"])]
        scope: String,
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
    Dangling {
        /// Only edges whose source document is in this collection
        #[arg(short, long)]
        collection: Option<String>,
    },

    /// Frontmatter keys whose values name indexed documents (full scan; explicit use only)
    Relations {
        /// Write the detected keys into `graph.frontmatter_relations`.
        ///
        /// A no-op under `graph.relations = "auto"`, which already extracts
        /// them without touching the config.
        #[arg(long)]
        apply: bool,
    },

    /// Document <-> source pairs that change together, marking documents the source changed after
    Coupling {
        /// Fewest shared commits before a pair counts. Omit for the built-in floor
        #[arg(long = "min-cochanges")]
        min_cochanges: Option<usize>,

        /// How far back to read history, in git's own wording. Omit for the last year
        #[arg(long)]
        since: Option<String>,

        /// Which revision to walk. Omit to walk the current branch
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },

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
#[command(after_help = "CONFIGURATION GUIDE
  Repository settings: mdkb init creates .mdkb/config.toml with every default commented out.
  Edit only the settings you need; see mdkb --help for topic guides and key definitions.
  Integration commands: mdkb setup mcp --help; mdkb setup hooks --help.
  Preview changes where supported with --dry-run; verify with mdkb doctor --full.
  Online setup guide: https://github.com/sstraus/mdkb/blob/main/README.md#installation
  Hook options and troubleshooting: https://github.com/sstraus/mdkb/blob/main/docs/hooks.md")]
pub enum SetupCommand {
    /// Enable privacy-minimized local telemetry for mdkb development
    Developer {
        /// Delete query events older than this many days
        #[arg(long, default_value = "30", value_parser = clap::value_parser!(u32).range(1..=365))]
        retention_days: u32,

        /// Print the merged repository config without writing files
        #[arg(long)]
        dry_run: bool,
    },

    /// Register mdkb as an MCP server
    #[command(subcommand)]
    Mcp(SetupMcpCommand),

    /// Register mdkb lifecycle hooks
    #[command(subcommand)]
    Hooks(SetupHooksCommand),

    /// Remove mdkb registrations (MCP and/or hooks)
    #[command(subcommand)]
    Remove(SetupRemoveCommand),

    /// Removed: use `mdkb doctor --full`.
    #[command(hide = true, disable_help_flag = true)]
    Check(RetiredArgs),
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

    /// Register mdkb with the Grok CLI (writes [mcp_servers.mdkb] in ~/.grok/config.toml)
    Grok {
        /// Print the merged config.toml to stdout without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Register mdkb with Antigravity CLI (writes ~/.gemini/config/mcp_config.json)
    Agy {
        /// Print the merged mcp_config.json to stdout without writing
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

        /// Comma-separated list of events to skip (session-start, user-prompt-submit, post-tool-use, pre-tool-use, stop)
        #[arg(long, default_value = "")]
        disable: String,

        /// Print the merged settings to stdout without writing
        #[arg(long)]
        dry_run: bool,

        /// Claude Code profile directory for --scope user (default: $CLAUDE_CONFIG_DIR
        /// when set, else ~/.claude). Use for non-standard profiles like ~/.claude-private.
        #[arg(long)]
        profile_dir: Option<PathBuf>,

        /// Register supported events as native HTTP hooks at this base URL.
        /// SessionStart remains a command hook because Claude Code does not
        /// support HTTP handlers for that event.
        #[arg(long)]
        http_url: Option<String>,
    },

    /// Register generic lifecycle hooks with ego (writes $EGO_HOME/config.toml)
    Ego {
        /// Retained for compatibility; generic hook setup merges without replacing user hooks
        #[arg(long)]
        force: bool,

        /// Print the merged config.toml to stdout without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Register lifecycle hooks with Codex CLI (writes ~/.codex/hooks.json)
    Codex {
        /// Comma-separated list of events to skip (session-start, user-prompt-submit, post-tool-use, pre-tool-use, stop)
        #[arg(long, default_value = "")]
        disable: String,

        /// Print the merged hooks.json to stdout without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Register tool hooks with the Grok CLI (writes ~/.grok/hooks/mdkb.json).
    /// Grok drops SessionStart and UserPromptSubmit context, so only the tool
    /// and stop events are registered
    Grok {
        /// Register even when grok already runs the mdkb hooks of ~/.claude/settings.json
        #[arg(long)]
        force: bool,

        /// Print hooks/mdkb.json to stdout without writing
        #[arg(long)]
        dry_run: bool,
    },

    /// Register lifecycle hooks with Antigravity CLI (writes an mdkb group in
    /// ~/.gemini/config/hooks.json)
    Agy {
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

    /// Remove mdkb from the Grok CLI (~/.grok/config.toml)
    Grok,

    /// Remove mdkb from Antigravity CLI (~/.gemini/config/mcp_config.json)
    Agy,
}

/// Hooks removal subcommands.
#[derive(Subcommand, Debug)]
pub enum RemoveHooksCommand {
    /// Remove generic mdkb hooks and their execution allowances from ego
    Ego,

    /// Remove mdkb hooks from Claude Code
    Claude {
        /// Scope: local or user
        #[arg(short, long, default_value = "local")]
        scope: String,

        /// Claude Code profile directory for --scope user (default: $CLAUDE_CONFIG_DIR
        /// when set, else ~/.claude)
        #[arg(long)]
        profile_dir: Option<PathBuf>,
    },

    /// Remove mdkb hooks from Codex CLI (~/.codex/hooks.json)
    Codex,

    /// Remove mdkb hooks from the Grok CLI (~/.grok/hooks/mdkb.json)
    Grok,

    /// Remove the mdkb hook group from Antigravity CLI (~/.gemini/config/hooks.json)
    Agy,
}

/// Code intelligence subcommands.
#[derive(Subcommand, Debug)]
pub enum CodeCommand {
    /// Removed: use `mdkb update --code`.
    #[command(hide = true, disable_help_flag = true)]
    Init(RetiredArgs),

    /// Hidden legacy alias of `mdkb update --code` (see `retired::legacy_alias_argv`).
    /// DEFERRED (2026-11-23) — remove in February 2027 (Boss).
    #[command(hide = true, disable_help_flag = true)]
    Index(RetiredArgs),

    /// Find symbols by name (fuzzy, exact matches first); `--semantic` searches by meaning
    Search(CodeSearchArgs),

    /// Search live source lines; `--literals` searches indexed AST literal values
    Grep(CodeGrepArgs),

    /// Alias of `code search --exact`
    #[command(hide = true)]
    Find(CodeSearchArgs),

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

    /// Removed: use `mdkb status`.
    #[command(hide = true, disable_help_flag = true)]
    Info(RetiredArgs),

    /// Report duplicated code
    Dup {
        /// Also run the semantic pass: loads a model, costs minutes on a large repository. Off unless asked for
        #[arg(long)]
        semantic: bool,

        /// Cosine floor for the semantic pass, which it also enables. Omit to use code.duplication.similarity_threshold
        #[arg(long)]
        threshold: Option<f32>,

        /// Fewest AST nodes a body needs to be worth comparing. Omit to use code.duplication.min_nodes
        #[arg(long = "min-nodes")]
        min_nodes: Option<u32>,

        /// Report clusters with a member under this path prefix, still compared against the whole index. Omit to report every cluster
        #[arg(short, long)]
        file: Option<String>,

        /// Review mode: report only clusters touching what this ref changed, still scored against the whole index
        #[arg(long)]
        since: Option<String>,
    },

    /// Report files that change together but have no edge between them
    Coupling {
        /// Fewest shared commits before a pair counts. Omit for the built-in floor
        #[arg(long = "min-cochanges")]
        min_cochanges: Option<usize>,

        /// How far back to read history, in git's own wording. Omit for the last year
        #[arg(long)]
        since: Option<String>,

        /// Which revision to walk. Omit to walk the current branch
        #[arg(long = "ref")]
        git_ref: Option<String>,
    },

    /// Parse a file and output symbols as JSONL (internal)
    #[command(hide = true)]
    Parse {
        /// File path to parse
        file: String,
    },
}

/// Symbol kinds `-k` accepts, as `SymbolKind` prints them.
pub const SYMBOL_KINDS: [&str; 16] = [
    "function",
    "method",
    "struct",
    "enum",
    "trait",
    "interface",
    "class",
    "module",
    "variable",
    "constant",
    "field",
    "parameter",
    "type_alias",
    "macro",
    "actor",
    "signal",
];

fn symbol_kind_values() -> PossibleValuesParser {
    PossibleValuesParser::new(SYMBOL_KINDS)
}

/// Arguments of `code search`, which `code find` shares.
#[derive(Args, Debug, Clone)]
pub struct CodeSearchArgs {
    /// Symbol name or words; unquoted words are joined into one query
    #[arg(required = true, num_args = 1..)]
    pub name: Vec<String>,

    /// Filter by symbol kind
    #[arg(short, long, value_parser = symbol_kind_values())]
    pub kind: Option<String>,

    /// Repository-relative path prefix
    #[arg(long, conflicts_with = "semantic")]
    pub path: Option<String>,

    /// Maximum number of results
    #[arg(short, long, default_value = "10")]
    pub limit: usize,

    /// Only symbols named exactly NAME
    #[arg(long, conflicts_with = "semantic")]
    pub exact: bool,

    /// Rank by meaning instead of by name (needs code embeddings)
    #[arg(long)]
    pub semantic: bool,

    /// Removed: use --path PREFIX
    #[arg(short, long, hide = true)]
    pub file: Option<String>,
}

/// Arguments of `code grep`.
#[derive(Args, Debug, Clone)]
pub struct CodeGrepArgs {
    /// Pattern; unquoted words are joined into one. Omit when -e gives them all
    #[arg(num_args = 0.., required_unless_present = "patterns")]
    pub pattern: Vec<String>,

    /// Another pattern; every pattern is an OR alternative (repeat -e)
    #[arg(short = 'e', long = "pattern")]
    pub patterns: Vec<String>,

    /// Treat the patterns as regular expressions
    #[arg(long, conflicts_with = "literals")]
    pub regex: bool,

    /// Case-insensitive match (Unicode lowercase)
    #[arg(short = 'i', long)]
    pub ignore_case: bool,

    /// Repository-relative path prefix
    #[arg(long)]
    pub path: Option<String>,

    /// Maximum number of hits
    #[arg(short, long, default_value = "10")]
    pub limit: usize,

    /// Search indexed AST literal values (JSX text, i18n strings) instead of source lines
    #[arg(long)]
    pub literals: bool,
}

impl Cli {
    /// Parse CLI arguments. A usage error prints one line naming the command
    /// that works and exits 2 (see [`usage_errors`]).
    pub fn parse_args() -> Self {
        let argv: Vec<String> = std::env::args().collect();
        let cli =
            Self::try_parse_from(&argv).unwrap_or_else(|err| usage_errors::exit_with(err, &argv));
        // DEFERRED (2026-11-23) — remove in February 2027 (Boss): hidden legacy aliases.
        match retired::legacy_alias_argv(&cli.command, &argv) {
            Some(rewritten) => Self::try_parse_from(&rewritten)
                .unwrap_or_else(|err| usage_errors::exit_with(err, &rewritten)),
            None => cli,
        }
    }
}
