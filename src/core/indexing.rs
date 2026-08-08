//! Turning files on disk into indexed documents.
//!
//! The indexing pipeline, and the reporting that says what a run actually did.
//! Both the MCP layer and the daemon watcher drive this — a file change on disk
//! is not a command-line event — so it cannot live behind a CLI entry point.
//!
//! Split by responsibility rather than relocated wholesale:
//! * `handle_update` / `handle_update_force` — the run, start to finish;
//! * `update_all_collections` / `update_collection` — per-collection walking
//!   and document diffing;
//! * `report_collection_deltas` and the snapshot sidecar — what changed, and
//!   the one signal that survives a cascading unregister or a quarantine;
//! * `housekeeping`, `apply_conventions`, `bootstrap_code_index` — the setup a
//!   run does before it indexes anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::core::ops::handle_embed;

use rusqlite::Connection;

use crate::code::indexing::walker::{WalkOptions, walk_files};
use crate::config::Config;
use crate::core::Context;
use crate::core::memory_sync::sync_memory_files;
use crate::domain::frontmatter::{ParsedDocument, parse_frontmatter};
use crate::domain::{Collection, Document, UpdateResult};
use crate::error::{Error, Result};
use crate::store::evolution::RelationshipType;
use crate::store::{collections, documents, evolution, memory};
use std::collections::{HashMap, HashSet};

/// Run `body` inside a BEGIN IMMEDIATE / COMMIT transaction.
/// On error, ROLLBACK is attempted and logged if it also fails.
pub(crate) fn with_transaction<T>(
    conn: &Connection,
    body: impl FnOnce() -> Result<T>,
) -> Result<T> {
    documents::begin_transaction(conn)?;
    match body() {
        Ok(val) => {
            documents::commit_transaction(conn)?;
            Ok(val)
        }
        Err(e) => {
            if let Err(rb_err) = documents::rollback_transaction(conn) {
                tracing::error!(
                    rollback_error = %rb_err,
                    original_error = %e,
                    "ROLLBACK failed after transaction error"
                );
            }
            Err(e)
        }
    }
}
/// Create and populate the code index. Non-fatal: logs warnings on failure.
pub(crate) fn bootstrap_code_index(root: &Path) {
    let index_path = root.join(".mdkb/code.sqlite");
    match crate::code::indexing::IndexFacade::open_or_create(&index_path) {
        Ok(mut facade) => match facade.index_directory(root) {
            Ok(stats) => {
                eprintln!(
                    "Code index: {} files, {} symbols",
                    stats.files_indexed, stats.symbols_indexed,
                );
            }
            Err(e) => eprintln!("Warning: code indexing failed: {e}"),
        },
        Err(e) => eprintln!("Warning: could not create code index: {e}"),
    }
    crate::llm::release_cached_service();
}
/// What one `mdkb update` was asked to do.
///
/// A routed update is parsed in one process and executed in another, so the
/// arguments have to survive a socket. They did not: the daemon took the method
/// name and dropped the params, so `--force` was accepted and ignored and
/// `mdkb update one.md` reindexed the whole tree — both reporting success.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UpdateRequest {
    /// Reindex only these paths, relative to the store root. Empty means the
    /// whole tree, which is also the only run that indexes sessions — a
    /// targeted update is about named files, and sessions have no path the
    /// caller could have named.
    #[serde(default)]
    pub files: Vec<String>,
    /// Reindex documents whose mtime has not moved. Needed when what changed is
    /// the configuration rather than the files.
    #[serde(default)]
    pub force: bool,
}

/// What one `mdkb update` did, phase by phase.
///
/// The daemon returns this rather than pre-rendered text so the CLI can honour
/// `--format`: text formatted inside the daemon is text the caller cannot ask
/// for as JSON.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UpdateOutcome {
    /// The document phase, which is the only one that must succeed.
    pub docs: UpdateResult,
    /// The code phase. `None` when the code index was unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<crate::code::indexing::types::IndexStats>,
    /// Why the code phase failed, when it did.
    ///
    /// Carried rather than logged: the process that has to report it is not the
    /// one that ran it, and a warning printed into the daemon's log is a
    /// warning the user never sees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_error: Option<String>,
    /// The session phase, when it ran and did something.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<UpdateResult>,
}

impl UpdateRequest {
    /// Is this run scoped to named files?
    ///
    /// One question, one answer: the same predicate picks the document handler
    /// and decides that sessions are skipped, and the CLI and the daemon both
    /// ask it rather than each re-deriving "targeted" from `files`.
    pub fn is_targeted(&self) -> bool {
        !self.files.is_empty()
    }
}

/// Should this code-index result be reported at all?
///
/// A whole-tree update always reports its numbers — "0 files indexed" is real
/// information when you asked for everything. A targeted run that touched no
/// source file has nothing to say, and an empty code block after
/// `mdkb update notes.md` is noise. Shared so the routed and in-process runs
/// cannot disagree about when the block appears.
pub fn report_code_stats(
    targeted: bool,
    stats: crate::code::indexing::types::IndexStats,
) -> Option<crate::code::indexing::types::IndexStats> {
    if targeted && stats.files_indexed == 0 {
        None
    } else {
        Some(stats)
    }
}

/// The document phase of an update: the whole tree, or just the named files.
///
/// Both the in-process CLI and the daemon go through here, so a routed update
/// and a direct one cannot disagree about what `--force` and a file list mean.
pub fn update_documents(
    ctx: &Context,
    root: impl AsRef<Path>,
    request: &UpdateRequest,
) -> Result<UpdateResult> {
    if request.is_targeted() {
        handle_update_files_force(ctx, root, &request.files, request.force)
    } else {
        handle_update_force(ctx, root, request.force)
    }
}

/// Handle `mdkb update` command - differential reindex.
///
/// Wraps all collection updates in a single transaction to ensure atomicity.
/// If any operation fails, the entire update is rolled back.
pub fn handle_update(ctx: &Context, root: impl AsRef<Path>) -> Result<UpdateResult> {
    handle_update_force(ctx, root, false)
}
/// Remove vestigial artifacts from earlier mdkb versions and warn on dead config
/// keys. Best-effort: a failed delete is logged, never fatal to `update`.
///
/// - `.mdkb/mdkb.sqlite` — a 0-byte orphan (never opened; deleted only when empty
///   so a future real DB at that path is never clobbered).
/// - `.mdkb/code-index/` — legacy tantivy index dir, superseded by `code.sqlite`.
/// - `.mdkb/reindex-queue.jsonl` — the file-based reindex queue; the daemon now
///   uses an in-process channel, so it has no writer.
/// - dead `[models]` embedding keys — the embedder is fixed to all-MiniLM-L6-v2.
fn housekeeping(root: &Path) {
    let mdkb_dir = root.join(".mdkb");

    // 0-byte orphan mdkb.sqlite (never delete a non-empty file at that path).
    let orphan = mdkb_dir.join("mdkb.sqlite");
    if orphan.metadata().map(|m| m.len() == 0).unwrap_or(false) {
        if let Err(e) = std::fs::remove_file(&orphan) {
            tracing::warn!("housekeeping: failed to remove orphan mdkb.sqlite: {e}");
        }
    }

    // Legacy tantivy code-index directory.
    let legacy_index = mdkb_dir.join("code-index");
    if legacy_index.is_dir() {
        if let Err(e) = std::fs::remove_dir_all(&legacy_index) {
            tracing::warn!("housekeeping: failed to remove legacy code-index dir: {e}");
        }
    }

    // Writer-less reindex queue.
    let queue = mdkb_dir.join("reindex-queue.jsonl");
    if queue.exists() {
        if let Err(e) = std::fs::remove_file(&queue) {
            tracing::warn!("housekeeping: failed to remove stale reindex-queue.jsonl: {e}");
        }
    }

    // Warn (do not silently accept) dead [models] embedding keys.
    let config_path = mdkb_dir.join("config.toml");
    if let Ok(raw) = std::fs::read_to_string(&config_path) {
        let dead = crate::config::detect_dead_model_keys(&raw);
        if !dead.is_empty() {
            eprintln!(
                "warning: [models] {} ignored: the embedder is fixed (all-MiniLM-L6-v2)",
                dead.join(", ")
            );
        }
    }
}
/// Like [`handle_update`], but `force` reindexes every file regardless of mtime.
pub fn handle_update_force(
    ctx: &Context,
    root: impl AsRef<Path>,
    force: bool,
) -> Result<UpdateResult> {
    let root = root.as_ref();
    let _mutation_guard = crate::store::mutation_lock::acquire(&ctx.db_path, "update")?;
    crate::store::heal::invalidate_marker(&ctx.db_path);

    // Remove vestigial artifacts / warn on dead config before indexing.
    housekeeping(root);

    // Detect and register convention-based collections before processing
    apply_conventions(ctx, root)?;

    let config = Config::load_or_default(&ctx.config_path);

    if config.code.enabled {
        let index_path = root.join(".mdkb/code.sqlite");
        if !index_path.exists() {
            bootstrap_code_index(root);
        }
    }

    // Read the per-collection document counts BEFORE indexing, so a collection
    // that has since been unregistered can still be named. `documents` keeps its
    // rows when a registration disappears, which is the only trace left of a
    // collection that autoheal (or anything else) dropped.
    let before = documents_per_collection(&ctx.conn).unwrap_or_default();

    let collections = collections::list_collections(&ctx.conn)?;
    let mut result = UpdateResult::default();

    with_transaction(&ctx.conn, || {
        update_all_collections(ctx, root, &config, &collections, force, &mut result)?;
        // Reclaim content rows stranded by prior updates/removes (and the
        // pre-fix backlog). No-op once the table is clean.
        documents::gc_orphaned_content(&ctx.conn)?;
        Ok(())
    })?;

    // Backfill embeddings for memory entries written without one (CLI cold-model
    // writes, or entries that predate embed-on-write). Runs after the index
    // commit so ONNX inference never holds the write lock. Failure is logged,
    // never fatal to `update` — the count surfaces in `mdkb stats`.
    match memory::backfill_memory_embeddings(&ctx.conn) {
        Ok(n) => result.memory_embeddings_backfilled = n,
        Err(e) => tracing::warn!("memory embedding backfill failed: {e}"),
    }

    // Reconcile the markdown projection with the DB (source of truth): backfill
    // files for DB-only entries, archive entries whose file was deleted.
    match sync_memory_files(ctx) {
        Ok(s) => {
            result.memory_files_projected = s.projected;
            result.memory_files_imported = s.imported;
            result.memory_files_adopted = s.adopted;
            result.memory_sync_conflicts = s.conflicts;
            result.memory_entries_revived = s.revived;
            result.memory_files_quarantined = s.quarantined;
            result.memory_entries_archived = s.archived;
            result.memory_entries_archive_skipped = s.archive_skipped;
            result.memory_gitignore_shadowed = s.gitignore_shadowed;
        }
        Err(e) => tracing::warn!("memory file sync failed: {e}"),
    }

    // Auto-embed changed documents (scoped: docs yes, claude_sessions no) so
    // hybrid search never silently degrades to BM25. Off via
    // `[search] auto_embed_docs = false`. Runs after commit; failure (e.g. cold
    // model) is logged, never fatal.
    if config.search.auto_embed_docs {
        match handle_embed(ctx, None) {
            Ok(r) => result.doc_embeddings_generated = r.generated,
            Err(e) => tracing::warn!("auto-embed on update failed: {e}"),
        }
    }

    report_collection_deltas(ctx, &before, &mut result);

    crate::store::heal::verify_and_mark(&ctx.conn, &ctx.db_path)?;

    Ok(result)
}
/// Sidecar listing the collections registered at the end of the last successful
/// update, keyed to document counts.
///
/// It lives OUTSIDE `index.sqlite` on purpose, and that is the whole point.
/// Neither surviving trace inside the database works:
/// * `documents.collection` has `ON DELETE CASCADE` onto `collections(name)`,
///   so unregistering a collection erases its documents in the same statement;
/// * an autoheal quarantine rebuilds the file empty, so both tables are gone
///   together.
///
/// A file the database cannot cascade over is therefore the only thing that can
/// answer "was this collection here last time?". It is advisory: a missing or
/// unreadable snapshot degrades to "no previous run known", never to an error.
fn collections_snapshot_path(ctx: &Context) -> PathBuf {
    ctx.db_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("collections.snapshot.json")
}
fn read_collections_snapshot(ctx: &Context) -> BTreeMap<String, usize> {
    std::fs::read_to_string(collections_snapshot_path(ctx))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn write_collections_snapshot(ctx: &Context, counts: &BTreeMap<String, usize>) {
    if let Ok(json) = serde_json::to_string_pretty(counts) {
        let _ = std::fs::write(collections_snapshot_path(ctx), json);
    }
}
/// Documents currently indexed, keyed by collection name.
fn documents_per_collection(conn: &Connection) -> Result<BTreeMap<String, usize>> {
    let mut stmt = conn.prepare(
        "SELECT collection, COUNT(*) FROM documents WHERE status != 'archived' GROUP BY collection",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}
/// Fill in the per-collection counts and the two loss signals.
///
/// Story 011-9a41: a collection of 2307 documents disappeared and `mdkb update`
/// printed "Docs: 3 indexed (3 new, 0 changed)" and exited 0 — output a healthy
/// store produces too. Recovery was `mdkb collection add map map && mdkb update`,
/// found by accident several runs later when a spot-check query failed.
///
/// A vanished collection is put in `errors` as well as its own field: a caller
/// that only checks `errors` (every hook and the MCP layer) would otherwise keep
/// treating the run as clean, which is the failure being fixed rather than a
/// second copy of it.
fn report_collection_deltas(
    ctx: &Context,
    before: &BTreeMap<String, usize>,
    result: &mut UpdateResult,
) {
    let registered = collections::list_collections(&ctx.conn).unwrap_or_default();
    let after = documents_per_collection(&ctx.conn).unwrap_or_default();

    result.collections = registered
        .iter()
        .map(|c| crate::domain::CollectionDelta {
            name: c.name.clone(),
            documents: after.get(&c.name).copied().unwrap_or(0),
            previous: before.get(&c.name).copied(),
        })
        .collect();

    // Held documents at the end of some previous run and is not registered now.
    // The in-DB counts from the start of THIS run are unioned with the sidecar
    // because each covers what the other cannot: the sidecar survives a cascade
    // and a quarantine, while the live counts cover a store whose snapshot was
    // never written (an upgrade, a hand-deleted sidecar).
    let mut previous = read_collections_snapshot(ctx);
    for (name, count) in before {
        let slot = previous.entry(name.clone()).or_insert(0);
        *slot = (*slot).max(*count);
    }

    let names: std::collections::HashSet<&str> =
        registered.iter().map(|c| c.name.as_str()).collect();
    for (name, count) in &previous {
        if *count > 0 && !names.contains(name.as_str()) {
            result.collections_vanished.push(name.clone());
            result.errors.push(format!(
                "collection `{name}` held {count} document(s) before this run and is no longer \
                 registered — nothing re-registers it; restore with `mdkb collection add {name} \
                 <path> && mdkb update`, and check `mdkb stats` for a quarantine that wiped it"
            ));
        }
    }

    result.no_collections_registered = registered.is_empty();
    if result.no_collections_registered {
        result.errors.push(
            "no document collection is registered — this run indexed nothing. Register one with \
             `mdkb collection add <name> <path>`."
                .to_string(),
        );
    }

    // Record what this run ended with, for the next run to compare against.
    // Vanished collections are deliberately NOT carried forward: once reported,
    // a deliberate `mdkb collection remove` must stop warning, or the message
    // becomes permanent and gets ignored — which is how the original loss went
    // unnoticed in the first place.
    write_collections_snapshot(ctx, &after);
}
/// Detect and register convention-based collections.
pub(crate) fn apply_conventions(ctx: &Context, root: &Path) -> Result<()> {
    let config = crate::config::Config::load_or_default(&ctx.config_path);
    if !config.conventions.enabled {
        return Ok(());
    }

    let existing = collections::list_collections(&ctx.conn)?;
    let proposals = crate::domain::conventions::detect_conventions(root, &existing);

    for proposal in &proposals {
        let coll = crate::domain::conventions::proposal_to_collection(proposal);
        collections::add_collection(&ctx.conn, &coll)?;
        tracing::info!("Auto-detected collection: {} ({})", coll.name, coll.path);
    }

    Ok(())
}
/// Update all collections within a transaction.
fn update_all_collections(
    ctx: &Context,
    root: &Path,
    config: &Config,
    collections: &[Collection],
    force: bool,
    result: &mut UpdateResult,
) -> Result<()> {
    for coll in collections {
        update_collection(ctx, root, config, coll, force, result)?;
    }
    Ok(())
}
/// Update a single collection by scanning for file changes.
fn update_collection(
    ctx: &Context,
    root: &Path,
    config: &Config,
    collection: &Collection,
    force: bool,
    result: &mut UpdateResult,
) -> Result<()> {
    let base_path = root.join(&collection.path);

    if !base_path.exists() {
        result.errors.push(format!(
            "Collection '{}' path does not exist: {}",
            collection.name,
            base_path.display()
        ));
        return Ok(());
    }

    // Validate path stays within root to prevent path traversal (fixes P2-SEC-001).
    // System-managed collections (e.g. claude_sessions) are allowed outside root.
    if collection.source != crate::domain::COLLECTION_SOURCE_SESSIONS {
        let canonical_root = root
            .canonicalize()
            .map_err(|e| Error::other(format!("Failed to canonicalize root path: {}", e)))?;
        let canonical_base = base_path.canonicalize().map_err(|e| {
            Error::other(format!(
                "Failed to canonicalize collection path '{}': {}",
                base_path.display(),
                e
            ))
        })?;

        if !canonical_base.starts_with(&canonical_root) {
            return Err(Error::other(format!(
                "Collection path '{}' escapes root directory (path traversal blocked)",
                collection.path
            )));
        }
    }

    // Build glob matcher (POSIX separator semantics — see compile_collection_matcher)
    let glob = compile_collection_matcher(&collection.pattern).map_err(|e| {
        Error::other(format!(
            "Invalid glob pattern '{}': {}",
            collection.pattern, e
        ))
    })?;

    // Get existing documents for this collection — prefetch to avoid N+1 queries
    let existing_docs = documents::list_documents(&ctx.conn, &collection.name)?;
    let mut existing_by_path: HashMap<String, Document> = existing_docs
        .into_iter()
        .map(|d| (d.relative_path.clone(), d))
        .collect();
    let mut existing_paths: HashSet<String> = existing_by_path.keys().cloned().collect();

    // Walk directory through the unified walker. When
    // `indexing.respect_gitignore` is false (the historical default), stories/,
    // plans/ and other gitignored collections keep being indexed and
    // `.mdkbignore` acts as the opt-in exclusion file.
    let ignore_patterns: Vec<String> = DOC_WALKER_DEFAULT_EXCLUDES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let discovered = walk_files(
        WalkOptions {
            root: &base_path,
            ignore_patterns: &ignore_patterns,
            respect_gitignore: config.indexing.respect_gitignore,
        },
        |path| {
            // Accept any file whose path relative to base_path matches the
            // collection's glob pattern.
            match path.strip_prefix(&base_path) {
                Ok(rel) => glob.is_match(rel),
                Err(_) => false,
            }
        },
    );

    for path in discovered {
        let relative = match path.strip_prefix(&base_path) {
            Ok(rel) => rel.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        // Remove from existing set (to track deletions)
        existing_paths.remove(&relative);

        // Check if document exists (from prefetched map)
        let existing_doc = existing_by_path.remove(&relative);

        index_single_file(
            SingleFileInput {
                ctx,
                collection_name: &collection.name,
                abs_path: &path,
                relative,
                existing_doc: existing_doc.as_ref(),
                display_name: &path.display().to_string(),
                graph_cfg: &config.graph,
                force,
            },
            result,
        );
    }

    // Remove documents for deleted files (remaining in prefetched map)
    for deleted_path in existing_paths {
        if let Some(doc) = existing_by_path.get(&deleted_path) {
            match documents::delete_document(&ctx.conn, doc.id) {
                Ok(true) => result.removed += 1,
                Ok(false) => {}
                Err(e) => {
                    result
                        .errors
                        .push(format!("Failed to remove {}: {}", deleted_path, e));
                }
            }
        }
    }

    Ok(())
}
/// Handle `mdkb update --files <paths>` — reindex only the specified files.
///
/// Resolves each path (absolute or relative to root) against registered collections.
/// Files that don't belong to any collection are silently skipped.
pub fn handle_update_files(
    ctx: &Context,
    root: impl AsRef<Path>,
    files: &[String],
) -> Result<UpdateResult> {
    handle_update_files_force(ctx, root, files, false)
}
/// Like [`handle_update_files`], but `force` reindexes the files regardless of mtime.
pub fn handle_update_files_force(
    ctx: &Context,
    root: impl AsRef<Path>,
    files: &[String],
    force: bool,
) -> Result<UpdateResult> {
    let root = root.as_ref();
    let collections = collections::list_collections(&ctx.conn)?;
    let mut result = UpdateResult::default();

    if collections.is_empty() || files.is_empty() {
        return Ok(result);
    }

    let _mutation_guard = crate::store::mutation_lock::acquire(&ctx.db_path, "update-files")?;
    crate::store::heal::invalidate_marker(&ctx.db_path);

    let canonical_root = root
        .canonicalize()
        .map_err(|e| Error::other(format!("Failed to canonicalize root: {}", e)))?;

    // Build glob matchers with pre-computed canonical bases.
    // Filter out sessions collections (they live outside root and should not
    // be reachable via user-supplied file paths).
    let matchers: Vec<(&Collection, globset::GlobMatcher, PathBuf)> = collections
        .iter()
        .filter(|c| c.source != crate::domain::COLLECTION_SOURCE_SESSIONS)
        .filter_map(|coll| match compile_collection_matcher(&coll.pattern) {
            Ok(matcher) => match root.join(&coll.path).canonicalize() {
                Ok(canonical_base) => Some((coll, matcher, canonical_base)),
                Err(e) => {
                    tracing::warn!(
                        collection = %coll.name,
                        path = %coll.path,
                        error = %e,
                        "Skipping collection: base path cannot be resolved"
                    );
                    result.errors.push(format!(
                        "Cannot resolve base path for collection '{}': {}",
                        coll.name, e
                    ));
                    None
                }
            },
            Err(e) => {
                tracing::warn!(
                    collection = %coll.name,
                    pattern = %coll.pattern,
                    error = %e,
                    "Skipping collection: invalid glob pattern"
                );
                result.errors.push(format!(
                    "Invalid glob pattern '{}' for collection '{}': {}",
                    coll.pattern, coll.name, e
                ));
                None
            }
        })
        .collect();

    let config = Config::load_or_default(&ctx.config_path);
    with_transaction(&ctx.conn, || {
        index_specified_files(
            ctx,
            &canonical_root,
            &matchers,
            files,
            &config.graph,
            force,
            &mut result,
        )
    })?;
    crate::store::heal::verify_and_mark(&ctx.conn, &ctx.db_path)?;
    Ok(result)
}
/// Build/dependency directories pruned by the document walker by default.
/// Applied as glob excludes through the unified walker; callers can extend
/// this via a `.mdkbignore` (when `respect_gitignore=false`) or `.gitignore`.
pub(crate) const DOC_WALKER_DEFAULT_EXCLUDES: &[&str] = &[
    "**/target/**",
    "**/node_modules/**",
    "**/.git/**",
    "**/vendor/**",
    "**/dist/**",
    "**/build/**",
    "**/__pycache__/**",
    "**/.tox/**",
    "**/.venv/**",
];
/// Compile a collection's glob pattern with POSIX-correct separator handling.
///
/// globset defaults to letting `*` cross `/`, so a non-recursive pattern like
/// `*.md` (the `_root` convention) would recursively swallow the whole tree and
/// duplicate every other collection's documents. `literal_separator(true)` stops
/// `*`/`?` from crossing `/` while `**` stays recursive — so `docs/**/*.md` is
/// unaffected and `*.md` matches only top-level files.
pub(crate) fn compile_collection_matcher(
    pattern: &str,
) -> std::result::Result<globset::GlobMatcher, globset::Error> {
    Ok(globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
}
/// Index a single file within a transaction. Shared by `update_collection` and
/// `handle_update_files` to avoid duplicating the mtime/read/index/evolution pipeline.
///
/// `existing_doc` is the previously indexed version (if any). Pass `None` for new files.
/// All errors are soft — pushed to `result.errors` and the caller continues.
pub(crate) struct SingleFileInput<'a> {
    pub(crate) ctx: &'a Context,
    pub(crate) collection_name: &'a str,
    pub(crate) abs_path: &'a Path,
    pub(crate) relative: String,
    pub(crate) existing_doc: Option<&'a Document>,
    pub(crate) display_name: &'a str,
    pub(crate) graph_cfg: &'a crate::config::GraphConfig,
    pub(crate) force: bool,
}
pub(crate) fn index_single_file(input: SingleFileInput<'_>, result: &mut UpdateResult) {
    let SingleFileInput {
        ctx,
        collection_name,
        abs_path,
        relative,
        existing_doc,
        display_name,
        graph_cfg,
        force,
    } = input;
    // Read file metadata for mtime
    let metadata = match std::fs::metadata(abs_path) {
        Ok(m) => m,
        Err(e) => {
            result.errors.push(format!(
                "Failed to read metadata for {}: {}",
                display_name, e
            ));
            return;
        }
    };

    let file_mtime = match metadata.modified() {
        Ok(t) => t
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or_else(|_| {
                tracing::warn!(file = %display_name, "mtime before epoch; forcing reindex");
                i64::MAX
            }),
        Err(e) => {
            tracing::warn!(
                file = %display_name,
                error = %e,
                "Cannot read mtime; forcing reindex"
            );
            i64::MAX
        }
    };

    // Check if document needs reindexing based on mtime (--force reprocesses all,
    // so config changes like graph relations reach already-indexed documents).
    let needs_index = match existing_doc {
        Some(_) if force => true,
        Some(doc) => file_mtime > doc.indexed_at,
        None => true,
    };

    if !needs_index {
        result.unchanged += 1;
        return;
    }

    // Read and index the file
    let content = match std::fs::read_to_string(abs_path) {
        Ok(c) => c,
        Err(e) => {
            result
                .errors
                .push(format!("Failed to read {}: {}", display_name, e));
            return;
        }
    };

    let parsed = parse_frontmatter(&content);
    let now = chrono::Utc::now().timestamp();

    let doc = Document {
        id: existing_doc.map(|d| d.id).unwrap_or(0),
        collection: collection_name.to_string(),
        relative_path: relative,
        hash: String::new(), // Will be computed by index_document
        title: parsed.title.clone(),
        metadata: parsed.frontmatter.clone(),
        file_modified_at: file_mtime,
        indexed_at: now,
        status: None,
    };

    match documents::index_document_in_tx(&ctx.conn, &doc, &content) {
        Ok(doc_id) => {
            if existing_doc.is_some() {
                result.updated += 1;
            } else {
                result.added += 1;
            }

            if has_evolution_refs(&parsed) {
                process_frontmatter_evolution(&ctx.conn, doc_id, collection_name, &parsed);
            }

            if graph_cfg.enabled {
                process_graph_edges(&ctx.conn, doc_id, &parsed, graph_cfg);
            }
        }
        Err(e) => {
            result
                .errors
                .push(format!("Failed to index {}: {}", display_name, e));
        }
    }
}
/// Process each user-supplied file path within a transaction.
pub(crate) fn index_specified_files(
    ctx: &Context,
    canonical_root: &Path,
    matchers: &[(&Collection, globset::GlobMatcher, PathBuf)],
    files: &[String],
    graph_cfg: &crate::config::GraphConfig,
    force: bool,
    result: &mut UpdateResult,
) -> Result<()> {
    for file_arg in files {
        let file_path = PathBuf::from(file_arg);

        // Resolve to absolute path
        let abs_path = if file_path.is_absolute() {
            file_path
        } else {
            canonical_root.join(&file_path)
        };

        // Canonicalize once per file for security check + collection matching
        let canonical_file = match abs_path.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                result
                    .errors
                    .push(format!("Cannot resolve '{}': {}", file_arg, e));
                continue;
            }
        };

        // Guard: file must resolve within root (path traversal protection, P2-SEC-001)
        if !canonical_file.starts_with(canonical_root) {
            result.errors.push(format!(
                "File '{}' escapes root directory (path traversal blocked)",
                file_arg
            ));
            continue;
        }

        // Find which collection this file belongs to
        let matched = matchers.iter().find_map(|(coll, matcher, canonical_base)| {
            let relative = canonical_file
                .strip_prefix(canonical_base)
                .ok()?
                .to_string_lossy()
                .to_string();

            if matcher.is_match(&relative) {
                Some((*coll, relative))
            } else {
                None
            }
        });

        let Some((collection, relative)) = matched else {
            continue;
        };

        // Look up existing document for mtime comparison
        let existing_doc = documents::get_document_by_path(&ctx.conn, &collection.name, &relative)?;

        index_single_file(
            SingleFileInput {
                ctx,
                collection_name: &collection.name,
                abs_path: &canonical_file,
                relative,
                existing_doc: existing_doc.as_ref(),
                display_name: file_arg,
                graph_cfg,
                force,
            },
            result,
        );
    }
    Ok(())
}
/// Process evolution references from frontmatter after document indexing.
///
/// This is called during the indexing phase to automatically create evolution
/// relationships based on frontmatter fields like `supersedes`, `updates`, etc.
/// Invalid references (pointing to non-existent documents) are logged as warnings
/// but don't fail the indexing operation.
fn process_frontmatter_evolution(
    conn: &Connection,
    source_doc_id: i64,
    collection: &str,
    parsed: &ParsedDocument,
) {
    // Helper to resolve path to doc ID
    let resolve_path = |path: &str| -> Option<i64> {
        // First try the path as-is in the same collection
        if let Ok(Some(doc)) = documents::get_document_by_path(conn, collection, path) {
            return Some(doc.id);
        }
        // Try without leading "./" or "/"
        let clean_path = path.trim_start_matches("./").trim_start_matches('/');
        if clean_path != path {
            if let Ok(Some(doc)) = documents::get_document_by_path(conn, collection, clean_path) {
                return Some(doc.id);
            }
        }
        None
    };

    // Process supersedes
    for evo_ref in &parsed.supersedes {
        if let Some(target_id) = resolve_path(&evo_ref.path) {
            if let Err(e) = evolution::add_evolution(
                conn,
                source_doc_id,
                target_id,
                RelationshipType::Supersedes,
                None,
                evo_ref.reason.as_deref(),
            ) {
                tracing::warn!("Failed to add supersedes evolution: {e}");
            }
        } else {
            tracing::warn!(
                "Evolution: supersedes reference '{}' not found, skipping",
                evo_ref.path
            );
        }
    }

    // Process updates
    for evo_ref in &parsed.updates {
        if let Some(target_id) = resolve_path(&evo_ref.path) {
            if let Err(e) = evolution::add_evolution(
                conn,
                source_doc_id,
                target_id,
                RelationshipType::Updates,
                evo_ref.scope.as_deref(),
                evo_ref.reason.as_deref(),
            ) {
                tracing::warn!("Failed to add updates evolution: {e}");
            }
        } else {
            tracing::warn!(
                "Evolution: updates reference '{}' not found, skipping",
                evo_ref.path
            );
        }
    }

    // Process corrects
    for evo_ref in &parsed.corrects {
        if let Some(target_id) = resolve_path(&evo_ref.path) {
            if let Err(e) = evolution::add_evolution(
                conn,
                source_doc_id,
                target_id,
                RelationshipType::Corrects,
                None,
                evo_ref.reason.as_deref(),
            ) {
                tracing::warn!("Failed to add corrects evolution: {e}");
            }
        } else {
            tracing::warn!(
                "Evolution: corrects reference '{}' not found, skipping",
                evo_ref.path
            );
        }
    }

    // Process extends
    for evo_ref in &parsed.extends {
        if let Some(target_id) = resolve_path(&evo_ref.path) {
            if let Err(e) = evolution::add_evolution(
                conn,
                source_doc_id,
                target_id,
                RelationshipType::Extends,
                None,
                evo_ref.reason.as_deref(),
            ) {
                tracing::warn!("Failed to add extends evolution: {e}");
            }
        } else {
            tracing::warn!(
                "Evolution: extends reference '{}' not found, skipping",
                evo_ref.path
            );
        }
    }
}
/// Check if a parsed document has any evolution references.
fn has_evolution_refs(parsed: &ParsedDocument) -> bool {
    !parsed.supersedes.is_empty()
        || !parsed.updates.is_empty()
        || !parsed.corrects.is_empty()
        || !parsed.extends.is_empty()
}
/// Populate knowledge-graph edges for a freshly indexed document.
///
/// Replaces the document's outgoing edges (idempotent re-index): edges from
/// allowlisted frontmatter keys (strong) and body wikilinks (soft). Target refs
/// are stored verbatim — dangling targets survive and resolve on later indexing.
pub(crate) fn process_graph_edges(
    conn: &Connection,
    doc_id: i64,
    parsed: &ParsedDocument,
    cfg: &crate::config::GraphConfig,
) {
    use crate::store::graph;

    if let Err(e) = graph::delete_edges_for_source(conn, doc_id) {
        tracing::warn!("Graph: failed to clear edges for doc {doc_id}: {e}");
        return;
    }

    for key in &cfg.frontmatter_relations {
        for target in
            crate::domain::frontmatter::extract_relation_refs(parsed.frontmatter.as_ref(), key)
        {
            if let Err(e) =
                graph::add_edge(conn, doc_id, &target, key, graph::KIND_FRONTMATTER, None)
            {
                tracing::warn!("Graph: failed to add frontmatter edge '{key}->{target}': {e}");
            }
        }
    }

    if cfg.include_wikilinks {
        for link in crate::domain::links::extract_wiki_links(&parsed.body) {
            if let Err(e) = graph::add_edge(
                conn,
                doc_id,
                &link.target,
                graph::RELATION_WIKILINK,
                graph::KIND_WIKILINK,
                None,
            ) {
                tracing::warn!(
                    "Graph: failed to add wikilink edge -> '{}': {e}",
                    link.target
                );
            }
        }
    }
}
