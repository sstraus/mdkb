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
    MemoryCommand, SessionCommand,
};
use crate::core::cli_mutation::CliMutation;
use crate::core::indexing::UpdateRequest;
use crate::error::{Error, Result};

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
        Command::Update { .. } | Command::Embed { .. } | Command::Compact { .. } => {
            Routing::Mutation
        }
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
        Command::Metrics(_) => Routing::Read,

        // ── no store ────────────────────────────────────────────────────────
        Command::Init
        | Command::Serve { .. }
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

/// Convert a parsed mutating command into the complete internal wire request.
///
/// The match has no wildcard for mutating subcommands. Adding one therefore
/// requires an explicit protocol decision. `memory add` materializes stdin in
/// the caller before delivery so an admission refusal can still fall back
/// without trying to read an already-consumed stream.
pub fn mutation_request(
    command: &mut Command,
    invocation_dir: &std::path::Path,
    store_root: &std::path::Path,
) -> Result<Option<CliMutation>> {
    use CliMutation as M;
    let absolute = |path: &std::path::Path| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            invocation_dir.join(path)
        }
    };

    Ok(Some(match command {
        Command::Update { files, force } => M::Update {
            request: UpdateRequest {
                files: files.clone(),
                force: *force,
            },
        },
        Command::Embed { collection } => M::Embed {
            collection: collection.clone(),
        },
        Command::Compact {
            prune_sessions,
            older_than,
            export,
        } => M::Compact {
            prune_sessions: *prune_sessions,
            older_than: older_than.clone(),
            export: export.as_deref().map(absolute),
        },
        Command::Collection(c) => match c {
            CollectionCommand::Add {
                name,
                path,
                pattern,
            } => M::CollectionAdd {
                name: name.clone(),
                path: path.clone(),
                pattern: pattern.clone(),
            },
            CollectionCommand::Remove { name } => M::CollectionRemove { name: name.clone() },
            CollectionCommand::Rename { old_name, new_name } => M::CollectionRename {
                old_name: old_name.clone(),
                new_name: new_name.clone(),
            },
            CollectionCommand::List => return Ok(None),
        },
        Command::Memory(c) => match c {
            MemoryCommand::Add {
                id,
                title,
                entry_type,
                tags,
                content,
                file,
                ttl,
                due_in,
                source_type,
            } => {
                let (body, source_path) = if let Some(path) = file.as_ref() {
                    let abs = absolute(path);
                    let body = std::fs::read_to_string(&abs).map_err(|e| {
                        Error::other(format!("Failed to read file {}: {e}", path.display()))
                    })?;
                    (
                        body,
                        Some(
                            abs.canonicalize()
                                .unwrap_or(abs)
                                .to_string_lossy()
                                .to_string(),
                        ),
                    )
                } else if let Some(body) = content.clone() {
                    (body, None)
                } else {
                    use std::io::Read;
                    let mut body = String::new();
                    std::io::stdin()
                        .take(100_000)
                        .read_to_string(&mut body)
                        .map_err(Error::from)?;
                    *content = Some(body.clone());
                    (body, None)
                };
                M::MemoryAdd {
                    id: id.clone(),
                    title: title.clone(),
                    entry_type: entry_type.clone(),
                    tags: tags.clone(),
                    content: body,
                    source_path,
                    ttl: *ttl,
                    due_in: *due_in,
                    source_type: source_type.clone(),
                }
            }
            MemoryCommand::Confirm { id, outcome } => M::MemoryConfirm {
                id: id.clone(),
                outcome: outcome.clone(),
            },
            MemoryCommand::Link {
                id,
                relation,
                target,
                doc,
                agent,
            } => M::MemoryLink {
                id: id.clone(),
                relation: relation.clone(),
                target: target.clone(),
                doc: *doc,
                agent: agent.clone(),
            },
            MemoryCommand::Rm { id } => M::MemoryRemove { id: id.clone() },
            MemoryCommand::Sync => M::MemorySync,
            MemoryCommand::Import {
                path,
                dry_run,
                skip_duplicates,
            } => M::MemoryImport {
                path: absolute(std::path::Path::new(path)),
                dry_run: *dry_run,
                skip_duplicates: *skip_duplicates,
            },
            MemoryCommand::Prune { days, dry_run } => M::MemoryPrune {
                days: *days,
                dry_run: *dry_run,
            },
            #[cfg(feature = "llm")]
            MemoryCommand::Condense {
                tag,
                dry_run,
                interactive: _,
                min_entries,
            } => M::MemoryCondense {
                tag: tag.clone(),
                dry_run: *dry_run,
                min_entries: *min_entries,
            },
            MemoryCommand::Show { .. }
            | MemoryCommand::List { .. }
            | MemoryCommand::Search { .. }
            | MemoryCommand::Warmup { .. }
            | MemoryCommand::History { .. }
            | MemoryCommand::Export { .. } => return Ok(None),
        },
        Command::Evolve(c) => match c {
            EvolveCommand::Supersedes { new, old, reason } => M::EvolveSupersedes {
                new: new.clone(),
                old: old.clone(),
                reason: reason.clone(),
            },
            EvolveCommand::Updates {
                new,
                old,
                scope,
                reason,
            } => M::EvolveUpdates {
                new: new.clone(),
                old: old.clone(),
                scope: scope.clone(),
                reason: reason.clone(),
            },
            EvolveCommand::Corrects { new, old, reason } => M::EvolveCorrects {
                new: new.clone(),
                old: old.clone(),
                reason: reason.clone(),
            },
            EvolveCommand::Retracts { new, old, reason } => M::EvolveRetracts {
                new: new.clone(),
                old: old.clone(),
                reason: reason.clone(),
            },
            EvolveCommand::Extends { new, old, reason } => M::EvolveExtends {
                new: new.clone(),
                old: old.clone(),
                reason: reason.clone(),
            },
        },
        Command::Experiment(c) => match c {
            ExperimentCommand::Create {
                name,
                config_a,
                config_b,
                description,
                split,
                min_samples,
            } => M::ExperimentCreate {
                name: name.clone(),
                config_a: config_a.clone(),
                config_b: config_b.clone(),
                description: description.clone(),
                split: *split,
                min_samples: *min_samples,
            },
            ExperimentCommand::End { name, winner } => M::ExperimentEnd {
                name: name.clone(),
                winner: winner.clone(),
            },
            ExperimentCommand::Cancel { name } => M::ExperimentCancel { name: name.clone() },
            ExperimentCommand::Status { .. } | ExperimentCommand::List { .. } => return Ok(None),
        },
        Command::Journal(c) => match c {
            JournalCommand::Import { path, dry_run } => M::JournalImport {
                path: absolute(std::path::Path::new(path)),
                source_path: std::path::PathBuf::from(path.as_str()),
                dry_run: *dry_run,
            },
            JournalCommand::ImportAll {
                dir,
                dry_run,
                skip_existing,
            } => {
                let source_dir =
                    std::path::PathBuf::from(dir.as_deref().unwrap_or(".claude/journal"));
                M::JournalImportAll {
                    dir: absolute(&source_dir),
                    source_dir,
                    dry_run: *dry_run,
                    skip_existing: *skip_existing,
                }
            }
        },
        Command::Code(c) => match c {
            CodeCommand::Init => M::CodeInit,
            CodeCommand::Index { paths, force } => M::CodeIndex {
                paths: paths.clone(),
                force: *force,
            },
            CodeCommand::Search { .. }
            | CodeCommand::Find { .. }
            | CodeCommand::Calls { .. }
            | CodeCommand::Callers { .. }
            | CodeCommand::Impact { .. }
            | CodeCommand::Info
            | CodeCommand::Parse { .. } => return Ok(None),
        },
        Command::Session(SessionCommand::Index {
            sessions_path,
            project_root,
        }) => M::SessionIndex {
            sessions_path: match sessions_path {
                Some(path) => absolute(std::path::Path::new(path)),
                None => crate::daemon::config::home_dir()?.join(".claude/projects"),
            },
            project_root: project_root
                .clone()
                .unwrap_or_else(|| store_root.to_string_lossy().to_string()),
        },
        Command::Init
        | Command::Search { .. }
        | Command::Get { .. }
        | Command::Mget { .. }
        | Command::Serve { .. }
        | Command::Daemon(_)
        | Command::Mcp { .. }
        | Command::Stats { .. }
        | Command::Metrics(_)
        | Command::Eval(_)
        | Command::History { .. }
        | Command::Current { .. }
        | Command::SupersededBy { .. }
        | Command::Graph(_)
        | Command::Setup(_)
        | Command::Cheatsheet
        | Command::Surface
        | Command::Schema { .. }
        | Command::Hook(_) => return Ok(None),
    }))
}
