//! Typed internal protocol for CLI mutations executed by the daemon.
//!
//! This is deliberately narrower than the Clap command tree: reads and local
//! commands never cross this boundary. The daemon executes a variant and
//! returns data; only the CLI adapter renders it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cli::journal::JournalImportResult;
use crate::core::Context;
use crate::core::indexing::{UpdateOutcome, UpdateRequest};
use crate::core::memory::{ConfirmResult, ImportResult};
use crate::core::memory_sync::MemorySyncSummary;
use crate::domain::UpdateResult;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum CliMutation {
    Update {
        request: UpdateRequest,
    },
    Embed {
        collection: Option<String>,
    },
    Compact {
        prune_sessions: bool,
        older_than: Option<String>,
        export: Option<PathBuf>,
    },
    CollectionAdd {
        name: String,
        path: String,
        pattern: String,
    },
    CollectionRemove {
        name: String,
    },
    CollectionRename {
        old_name: String,
        new_name: String,
    },
    MemoryAdd {
        id: String,
        title: String,
        entry_type: String,
        tags: Option<String>,
        content: String,
        source_path: Option<String>,
        ttl: Option<u64>,
        due_in: Option<u64>,
        source_type: Option<String>,
    },
    MemoryConfirm {
        id: String,
        outcome: String,
    },
    MemoryLink {
        id: String,
        relation: String,
        target: String,
        doc: bool,
        agent: Option<String>,
    },
    MemoryRemove {
        id: String,
    },
    MemorySync,
    MemoryImport {
        path: PathBuf,
        dry_run: bool,
        skip_duplicates: bool,
    },
    MemoryPrune {
        days: u32,
        dry_run: bool,
    },
    #[cfg(feature = "llm")]
    MemoryCondense {
        tag: Option<String>,
        dry_run: bool,
        min_entries: usize,
    },
    EvolveSupersedes {
        new: String,
        old: String,
        reason: Option<String>,
    },
    EvolveUpdates {
        new: String,
        old: String,
        scope: Option<String>,
        reason: Option<String>,
    },
    EvolveCorrects {
        new: String,
        old: String,
        reason: Option<String>,
    },
    EvolveRetracts {
        new: String,
        old: String,
        reason: Option<String>,
    },
    EvolveExtends {
        new: String,
        old: String,
        reason: Option<String>,
    },
    ExperimentCreate {
        name: String,
        config_a: String,
        config_b: String,
        description: Option<String>,
        split: f64,
        min_samples: i64,
    },
    ExperimentEnd {
        name: String,
        winner: Option<String>,
    },
    ExperimentCancel {
        name: String,
    },
    JournalImport {
        path: PathBuf,
        source_path: PathBuf,
        dry_run: bool,
    },
    JournalImportAll {
        dir: PathBuf,
        source_dir: PathBuf,
        dry_run: bool,
        skip_existing: bool,
    },
    CodeInit,
    CodeIndex {
        paths: Vec<String>,
        force: bool,
    },
    SessionIndex {
        sessions_path: PathBuf,
        project_root: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum CliMutationResult {
    Update {
        outcome: UpdateOutcome,
    },
    Embed {
        generated: usize,
        skipped: usize,
        errors: Vec<String>,
    },
    Compact {
        prune: Option<crate::core::ops::PruneSessionsSummary>,
        index_bytes: u64,
        code_bytes: Option<u64>,
    },
    CollectionAdded,
    CollectionRemoved {
        removed: bool,
    },
    CollectionRenamed,
    MemoryAdded,
    MemoryConfirmed {
        outcome: ConfirmResult,
    },
    MemoryLinked,
    MemoryRemoved {
        deleted: bool,
    },
    MemorySynced {
        summary: MemorySyncSummary,
    },
    MemoryImported {
        outcome: MemoryImportOutcome,
    },
    MemoryPruned {
        ids: Vec<String>,
    },
    #[cfg(feature = "llm")]
    MemoryCondensed {
        outcome: crate::core::memory::CondenseResult,
    },
    EvolutionCreated {
        id: i64,
    },
    ExperimentCreated {
        id: i64,
        name: String,
    },
    ExperimentEnded {
        winner: Option<String>,
    },
    ExperimentCancelled,
    JournalImported {
        outcome: JournalImportResult,
    },
    JournalsImported {
        outcomes: Vec<JournalImportResult>,
    },
    CodeInitialized,
    CodeIndexed {
        stats: crate::code::indexing::types::IndexStats,
    },
    SessionIndexed {
        outcome: UpdateResult,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryImportOutcome {
    RestoreDryRun,
    Restored,
    Bulk { outcome: ImportResult },
}

/// Execute a mutation that uses the main store context.
///
/// Code-index variants and compact are owned by the daemon adapter because it
/// must coordinate its persistent `IndexFacade`. Reaching this function with
/// one of those variants is a programming error, surfaced as an error rather
/// than silently doing a second open.
#[allow(clippy::enum_glob_use)]
pub fn execute_context_mutation(ctx: &Context, mutation: CliMutation) -> Result<CliMutationResult> {
    use CliMutation::*;
    use CliMutationResult as R;

    Ok(match mutation {
        Update { .. } | Compact { .. } | CodeInit | CodeIndex { .. } => {
            return Err(Error::other("mutation requires daemon-owned resources"));
        }
        Embed { collection } => {
            let r = crate::core::ops::handle_embed(ctx, collection.as_deref())?;
            CliMutationResult::Embed {
                generated: r.generated,
                skipped: r.skipped,
                errors: r.errors,
            }
        }
        CollectionAdd {
            name,
            path,
            pattern,
        } => {
            crate::core::graph::handle_collection_add(ctx, &name, &path, &pattern)?;
            R::CollectionAdded
        }
        CollectionRemove { name } => R::CollectionRemoved {
            removed: crate::core::graph::handle_collection_remove(ctx, &name)?,
        },
        CollectionRename { old_name, new_name } => {
            crate::core::graph::handle_collection_rename(ctx, &old_name, &new_name)?;
            R::CollectionRenamed
        }
        MemoryAdd {
            id,
            title,
            entry_type,
            tags,
            content,
            source_path,
            ttl,
            due_in,
            source_type,
        } => {
            crate::core::memory::handle_memory_add(
                ctx,
                &id,
                &title,
                &entry_type,
                tags.as_deref(),
                &content,
                source_path.as_deref(),
                ttl,
                due_in,
                source_type.as_deref(),
            )?;
            R::MemoryAdded
        }
        MemoryConfirm { id, outcome } => R::MemoryConfirmed {
            outcome: crate::core::memory::handle_memory_confirm(ctx, &id, &outcome)?,
        },
        MemoryLink {
            id,
            relation,
            target,
            doc,
            agent,
        } => {
            crate::core::memory::handle_memory_link(
                ctx,
                &id,
                &relation,
                &target,
                doc,
                agent.as_deref(),
            )?;
            R::MemoryLinked
        }
        MemoryRemove { id } => R::MemoryRemoved {
            deleted: crate::core::memory::handle_memory_rm(ctx, &id)?,
        },
        MemorySync => R::MemorySynced {
            summary: crate::core::memory_sync::sync_memory_files(ctx)?,
        },
        MemoryImport {
            path,
            dry_run,
            skip_duplicates,
        } => {
            let outcome = if path.is_dir() {
                MemoryImportOutcome::Bulk {
                    outcome: crate::core::memory::handle_memory_import_dir(
                        ctx,
                        &path,
                        dry_run,
                        skip_duplicates,
                    )?,
                }
            } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if dry_run {
                    MemoryImportOutcome::RestoreDryRun
                } else {
                    crate::core::memory::handle_memory_import_file(ctx, &path)?;
                    MemoryImportOutcome::Restored
                }
            } else {
                MemoryImportOutcome::Bulk {
                    outcome: crate::core::memory::handle_memory_import(
                        ctx,
                        &path.to_string_lossy(),
                        dry_run,
                        skip_duplicates,
                    )?,
                }
            };
            R::MemoryImported { outcome }
        }
        MemoryPrune { days, dry_run } => R::MemoryPruned {
            ids: crate::core::memory::handle_memory_prune(ctx, days, dry_run)?,
        },
        #[cfg(feature = "llm")]
        MemoryCondense {
            tag,
            dry_run,
            min_entries,
        } => R::MemoryCondensed {
            outcome: crate::core::memory::handle_memory_condense(
                ctx,
                tag.as_deref(),
                dry_run,
                min_entries,
            )?,
        },
        EvolveSupersedes { new, old, reason } => R::EvolutionCreated {
            id: crate::core::graph::handle_evolve_supersedes(ctx, &new, &old, reason.as_deref())?,
        },
        EvolveUpdates {
            new,
            old,
            scope,
            reason,
        } => R::EvolutionCreated {
            id: crate::core::graph::handle_evolve_updates(
                ctx,
                &new,
                &old,
                scope.as_deref(),
                reason.as_deref(),
            )?,
        },
        EvolveCorrects { new, old, reason } => R::EvolutionCreated {
            id: crate::core::graph::handle_evolve_corrects(ctx, &new, &old, reason.as_deref())?,
        },
        EvolveRetracts { new, old, reason } => R::EvolutionCreated {
            id: crate::core::graph::handle_evolve_retracts(ctx, &new, &old, reason.as_deref())?,
        },
        EvolveExtends { new, old, reason } => R::EvolutionCreated {
            id: crate::core::graph::handle_evolve_extends(ctx, &new, &old, reason.as_deref())?,
        },
        ExperimentCreate {
            name,
            config_a,
            config_b,
            description,
            split,
            min_samples,
        } => {
            let r = crate::core::ops::handle_experiment_create(
                ctx,
                &name,
                description.as_deref(),
                &config_a,
                &config_b,
                split,
                min_samples,
            )?;
            R::ExperimentCreated {
                id: r.id,
                name: r.name,
            }
        }
        ExperimentEnd { name, winner } => R::ExperimentEnded {
            winner: crate::core::ops::handle_experiment_end(ctx, &name, winner.as_deref())?,
        },
        ExperimentCancel { name } => {
            crate::core::ops::handle_experiment_cancel(ctx, &name)?;
            R::ExperimentCancelled
        }
        JournalImport {
            path,
            source_path,
            dry_run,
        } => R::JournalImported {
            outcome: crate::core::ops::handle_journal_import_from(
                ctx,
                &path,
                &source_path,
                dry_run,
            )?,
        },
        JournalImportAll {
            dir,
            source_dir,
            dry_run,
            skip_existing,
        } => R::JournalsImported {
            outcomes: crate::core::ops::handle_journal_import_all_from(
                ctx,
                &dir,
                &source_dir,
                dry_run,
                skip_existing,
            )?,
        },
        SessionIndex {
            sessions_path,
            project_root,
        } => R::SessionIndexed {
            outcome: crate::core::sessions::handle_session_index(
                ctx,
                &sessions_path,
                &project_root,
            )?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips_without_raw_cli_types() {
        let request = CliMutation::MemoryLink {
            id: "source".to_string(),
            relation: "supports".to_string(),
            target: "target".to_string(),
            doc: false,
            agent: Some("codex".to_string()),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["command"], "memory_link");
        assert!(matches!(
            serde_json::from_value::<CliMutation>(json).unwrap(),
            CliMutation::MemoryLink { id, agent: Some(agent), .. }
                if id == "source" && agent == "codex"
        ));

        let result = CliMutationResult::MemoryRemoved { deleted: false };
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["result"], "memory_removed");
        assert!(matches!(
            serde_json::from_value::<CliMutationResult>(json).unwrap(),
            CliMutationResult::MemoryRemoved { deleted: false }
        ));
    }

    #[test]
    fn context_executor_returns_data_instead_of_rendering() {
        let temp = tempfile::tempdir().unwrap();
        crate::core::ops::handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        let result = execute_context_mutation(
            &ctx,
            CliMutation::CollectionAdd {
                name: "docs".to_string(),
                path: "docs".to_string(),
                pattern: "**/*.md".to_string(),
            },
        )
        .unwrap();
        assert!(matches!(result, CliMutationResult::CollectionAdded));

        let result = execute_context_mutation(
            &ctx,
            CliMutation::CollectionRemove {
                name: "missing".to_string(),
            },
        )
        .unwrap();
        assert!(matches!(
            result,
            CliMutationResult::CollectionRemoved { removed: false }
        ));
    }

    #[test]
    fn routed_journal_import_preserves_the_callers_path_spelling() {
        let temp = tempfile::tempdir().unwrap();
        crate::core::ops::handle_init(temp.path()).unwrap();
        let journal = temp.path().join("journal.md");
        std::fs::write(&journal, "# Session\n\n## Summary\n\nUseful result").unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        let result = execute_context_mutation(
            &ctx,
            CliMutation::JournalImport {
                path: journal,
                source_path: PathBuf::from("journal.md"),
                dry_run: false,
            },
        )
        .unwrap();
        let CliMutationResult::JournalImported { outcome } = result else {
            panic!("wrong result");
        };
        assert_eq!(outcome.source_path, "journal.md");
        let entry = crate::store::memory::get_entry_without_tracking(&ctx.conn, "journal-insights")
            .unwrap()
            .unwrap();
        assert_eq!(entry.source_path.as_deref(), Some("journal.md"));
    }
}
