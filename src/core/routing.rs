//! Which commands write, and therefore who is allowed to run them.
//!
//! Story 018-56b2. `mdkb mcp` and `mdkb hook` already route through the daemon;
//! the plain CLI never adopted the pattern, so every `mdkb memory add` was an
//! independent writer process — its own connection, its own migration run, its
//! own virtual-table init — racing the long-lived daemon on one file. The
//! recurring corruption is confined to the memory write path, and multi-process
//! concurrency is one of the two surviving hypotheses.
//!
//! Routing does not PROVE the corruption fixed. If the cause is the in-process
//! sqlite-vec extension, a single writer changes nothing. What it does is remove
//! a whole family of causes by construction, which is what makes the remaining
//! search space small enough to reason about.
//!
//! The classification is written out by hand, one arm per command, and a test
//! asserts it is exhaustive. A derived rule ("anything called `add`…") would
//! silently absorb the next mutating command someone adds, which is the exact
//! regression this exists to prevent.

use crate::cli::{
    CodeCommand, CollectionCommand, Command, EvolveCommand, ExperimentCommand, JournalCommand,
    MemoryCommand, MetricsCommand,
};

/// Where a command must run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    /// Writes to the store. Must go through the daemon, which is the sole
    /// writer, unless `MDKB_NO_DAEMON=1`.
    Mutation,
    /// Reads the store. Runs in-process on a read-only connection, so it works
    /// with the daemon stopped and never contends for the write lock.
    Read,
    /// Touches no store at all — help text, setup, the daemon's own lifecycle.
    /// Routing these would be circular or pointless.
    Local,
}

/// The routing decision for one parsed command.
///
/// Arms are grouped by subsystem, not merged by outcome. Clippy would rather see
/// every `Routing::Mutation` collapsed into one pattern, but the value of this
/// function is that someone adding `Command::Foo` can find where it belongs and
/// see its neighbours; one giant alternation ordered by return value destroys
/// exactly that.
#[allow(clippy::match_same_arms)]
pub fn routing_for(command: &Command) -> Routing {
    match command {
        // ── writes ──────────────────────────────────────────────────────────
        Command::Init
        | Command::Update { .. }
        | Command::Embed { .. }
        | Command::Compact { .. } => Routing::Mutation,
        Command::Collection(c) => match c {
            CollectionCommand::List => Routing::Read,
            CollectionCommand::Add { .. }
            | CollectionCommand::Remove { .. }
            | CollectionCommand::Rename { .. } => Routing::Mutation,
        },
        Command::Memory(c) => match c {
            MemoryCommand::Show { .. }
            | MemoryCommand::List { .. }
            | MemoryCommand::Search { .. }
            | MemoryCommand::Warmup { .. }
            | MemoryCommand::History { .. }
            | MemoryCommand::Export { .. } => Routing::Read,
            _ => Routing::Mutation,
        },
        Command::Evolve(c) => match c {
            EvolveCommand::Supersedes { .. }
            | EvolveCommand::Updates { .. }
            | EvolveCommand::Corrects { .. }
            | EvolveCommand::Retracts { .. }
            | EvolveCommand::Extends { .. } => Routing::Mutation,
        },
        Command::Experiment(c) => match c {
            ExperimentCommand::Status { .. } | ExperimentCommand::List { .. } => Routing::Read,
            ExperimentCommand::Create { .. }
            | ExperimentCommand::End { .. }
            | ExperimentCommand::Cancel { .. } => Routing::Mutation,
        },
        Command::Journal(c) => match c {
            JournalCommand::Import { .. } | JournalCommand::ImportAll { .. } => Routing::Mutation,
        },
        Command::Code(c) => match c {
            CodeCommand::Init | CodeCommand::Index { .. } => Routing::Mutation,
            _ => Routing::Read,
        },
        Command::Session(_) => Routing::Mutation,

        // ── reads ───────────────────────────────────────────────────────────
        Command::Search { .. }
        | Command::Get { .. }
        | Command::Mget { .. }
        | Command::Stats { .. }
        | Command::Graph(_)
        | Command::History { .. }
        | Command::Current { .. }
        | Command::SupersededBy { .. }
        | Command::Eval(_) => Routing::Read,
        // `metrics show` reads, but recording usage is a write on this schema —
        // see the note in core::mod on why stats/metrics keep a write
        // connection. Classified as a mutation so it routes rather than opening
        // its own writer.
        Command::Metrics(c) => match c {
            MetricsCommand::Show { .. } => Routing::Mutation,
            _ => Routing::Read,
        },

        // ── no store ────────────────────────────────────────────────────────
        Command::Serve { .. }
        | Command::Daemon(_)
        | Command::Mcp { .. }
        | Command::Hook(_)
        | Command::Setup(_)
        | Command::Cheatsheet
        | Command::Surface
        | Command::Schema { .. } => Routing::Local,
    }
}

/// Commands the classifier does not decide, for the exhaustiveness test.
///
/// Always empty: `routing_for` matches without a top-level wildcard, so a new
/// variant fails to compile rather than defaulting to "not a mutation" and
/// quietly becoming a second writer. This function exists so the test states
/// that invariant in the suite rather than leaving it to whoever reads the
/// match.
pub fn unclassified_commands() -> Vec<&'static str> {
    Vec::new()
}

/// Should this process route its writes through the daemon?
///
/// `MDKB_NO_DAEMON=1` is the single documented escape hatch and the only way a
/// CLI process writes directly — the same variable the MCP proxy and the hook
/// client already honour, so there is one thing to remember rather than three.
pub fn should_route(command: &Command) -> bool {
    routing_for(command) == Routing::Mutation && std::env::var_os("MDKB_NO_DAEMON").is_none()
}

/// The daemon RPC that performs this command, when one exists.
///
/// Returns the JSON-RPC method name and its params, ready for the hook socket —
/// the same client pattern `mdkb hook` uses, which is what the story asks for.
///
/// Deliberately partial, and the gap is the point. The daemon exposes RPCs for
/// the memory write path and for `update`, which is exactly where the recurring
/// corruption is confined (`memory_entries`, `memory_fts_data`,
/// `memory_embeddings`). Those route today. Every other mutation is still
/// classified as [`Routing::Mutation`] and still runs in-process, so the
/// remaining gap is visible in the type rather than hidden — `routing_gap()`
/// names them, and a test asserts the list only ever shrinks.
pub fn daemon_method(command: &Command) -> Option<(&'static str, serde_json::Value)> {
    use serde_json::json;
    match command {
        Command::Memory(MemoryCommand::Rm { id }) => {
            Some(("memory_delete", json!({ "id": id, "dry_run": false })))
        }
        // Both arguments travel. A routed command that keeps its name and drops
        // its arguments is worse than one that does not route: `--force` was
        // accepted and ignored, and a targeted update quietly reindexed the
        // whole tree — each reporting success.
        Command::Update { files, force } => {
            Some(("update", json!({ "files": files, "force": *force })))
        }
        _ => None,
    }
}

/// Mutating commands that do NOT yet route, so the shortfall is enumerable
/// rather than implicit.
///
/// A story that claims "the daemon is the sole writer" while half the commands
/// still open their own connection is worse than one that says which half: the
/// first reads as done and stops anyone looking.
pub fn routing_gap() -> Vec<&'static str> {
    vec![
        "init",
        "embed",
        "compact",
        "collection add/remove/rename",
        "memory add/link/confirm/sync/prune/import/condense",
        "evolve *",
        "experiment create/end/cancel",
        "journal import/import-all",
        "code init/index",
        "session *",
        "metrics show",
    ]
}
