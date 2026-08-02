//! CLI command handlers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::code::indexing::walker::{WalkOptions, walk_files};
use crate::config::Config;
use crate::domain::frontmatter::{ParsedDocument, parse_frontmatter};
use crate::domain::{Collection, Document, SearchQuery, SearchResult, UpdateResult};
use crate::error::{Error, ErrorKind, Result};
use crate::store::collections;
use crate::store::documents;
use crate::store::hybrid;
use crate::store::schema;
use crate::store::search;
use crate::store::stats;
use crate::store::vectors;
use globset::Glob;
use rusqlite::Connection;
use serde::Deserialize;
use walkdir::WalkDir;

/// Build/dependency directories pruned by the document walker by default.
/// Applied as glob excludes through the unified walker; callers can extend
/// this via a `.mdkbignore` (when `respect_gitignore=false`) or `.gitignore`.
const DOC_WALKER_DEFAULT_EXCLUDES: &[&str] = &[
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

/// Run `body` inside a BEGIN IMMEDIATE / COMMIT transaction.
/// On error, ROLLBACK is attempted and logged if it also fails.
fn with_transaction<T>(conn: &Connection, body: impl FnOnce() -> Result<T>) -> Result<T> {
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

/// Context for CLI operations.
pub struct Context {
    /// Database connection.
    pub conn: Connection,
    /// Config path.
    pub config_path: PathBuf,
    /// Database path.
    pub db_path: PathBuf,
    /// True when [`Context::open`] found the index structurally corrupt,
    /// quarantined it, and built this connection on a fresh empty database.
    /// The caller should trigger a reindex to repopulate it.
    pub rebuilt_from_corruption: bool,
    /// Always false for a successfully opened context.
    ///
    /// A corrupt in-use generation is now returned as
    /// [`ErrorKind::IndexCorruptInUse`] instead of exposing a connection to the
    /// malformed database. Retained so existing callers can inspect `Context`
    /// without an unrelated API break.
    pub corrupt_in_use: bool,
    /// Shared advisory lock announcing this connection for as long as the
    /// context lives, so no other process renames the database files underneath
    /// it. Never read — its whole job is to exist until drop.
    _live_guard: Option<crate::store::mutation_lock::MutationGuard>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("config_path", &self.config_path)
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

/// Run a mutation on a long-lived context slot, CLOSING it if the index turns
/// out to be corrupt.
///
/// A one-shot CLI process heals on its next run because it reopens; a daemon
/// does not — it holds the connection (and with it the live lock that stops
/// autoheal renaming the file) for days. Without this, a corrupt index means
/// every subsequent mutation fails the post-mutation probe, `Context::open`
/// reports `CorruptInUse` because the daemon is the very holder blocking it,
/// and the loop never ends: itview logged that failure every 30s for 13 days
/// while every memory write in that window was lost.
///
/// Dropping the context releases the connection and the live lock, so the next
/// `Context::open` quarantines the file, salvages memory out of it, and rebuilds.
/// Returns `None` when the slot was already empty (nothing to run).
pub fn run_mutation<T>(
    slot: &mut Option<Context>,
    what: &str,
    f: impl FnOnce(&mut Context) -> Result<T>,
) -> Option<Result<T>> {
    let result = f(slot.as_mut()?);

    if let Err(e) = &result {
        if e.is_index_corrupt() {
            tracing::error!(
                operation = what,
                error = %e,
                "index is corrupt — closing this connection so the next open can quarantine, salvage memory and rebuild"
            );
            *slot = None;
        }
    }
    Some(result)
}

impl Context {
    /// Apply the connection pragmas for the main index DB.
    ///
    /// The daemon and one-shot CLI processes open this same file as independent
    /// connections. `busy_timeout` makes ordinary write-lock contention wait
    /// briefly instead of failing immediately with `SQLITE_BUSY` (set first, so
    /// the migrations that run right after also benefit). WAL +
    /// `synchronous = NORMAL` match the code-index DB and the intent of the
    /// otherwise-unused `Store::setup_pragmas`.
    ///
    /// Deliberately no `mmap_size`: a 256 MiB persistent memory-map on the
    /// long-lived daemon connection, combined with page relocation/truncation
    /// from a concurrent connection's `VACUUM`/`incremental_vacuum`, tore
    /// `index.sqlite` pointer-map pages. `code.sqlite` never mapped and never
    /// corrupted; the marginal read speedup is not worth the data-loss risk.
    fn configure_connection(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            PRAGMA busy_timeout = 5000;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = memory;
            PRAGMA foreign_keys = ON;
            ",
        )?;
        Ok(())
    }

    /// Open or create context at the given root.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mdkb_dir = root.join(".mdkb");

        if !mdkb_dir.exists() {
            return Err(ErrorKind::DatabaseNotFound {
                path: mdkb_dir.join("index.sqlite"),
            }
            .into());
        }

        // Every cross-process identity below is derived from `db_path` as a
        // STRING: the `.mutation.lock` and `.live.lock` sidecars, and the
        // `-wal`/`-shm` files SQLite itself names. Two spellings of one file
        // therefore give two lock domains over a single inode — no shared WAL
        // index, no mutual exclusion, and the doubly-referenced pages that
        // follow. On a case-insensitive volume (APFS default) `Gits` and `GITS`
        // are exactly such a pair. Canonicalizing here makes the identity a
        // property of the store rather than of whatever spelling the caller
        // happened to hold.
        let mdkb_dir = mdkb_dir.canonicalize().map_err(|e| {
            Error::other(format!(
                "cannot canonicalize {}: {e} — refusing to open, because locks \
                 keyed on a non-canonical path silently corrupt the index",
                mdkb_dir.display()
            ))
        })?;
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");

        // Initialize sqlite-vec extension before opening connection
        vectors::init_sqlite_vec();

        // Opening is not read-only: every process runs migrations and creates
        // FTS/vector virtual tables if needed. Keep one project-wide lock from
        // the integrity probe through schema initialization and salvage; two
        // concurrent virtual-table constructors can otherwise fail with
        // `SQLITE_SCHEMA: vtable constructor failed`.
        let _open_guard = crate::store::mutation_lock::acquire(&db_path, "open-schema")?;

        // Autoheal: quarantine a structurally-corrupt index before we build on
        // it, so the `Connection::open` below lands on a clean file. Throttled,
        // so this is cheap on the hot open path.
        let quarantined = match crate::store::heal::ensure_sound_locked(&db_path)? {
            crate::store::heal::Heal::Sound => None,
            crate::store::heal::Heal::Quarantined { corrupt_path } => Some(corrupt_path),
            crate::store::heal::Heal::CorruptInUse => {
                // Do not turn a read command into another holder of a database
                // already known to be corrupt. `Context::open` initializes
                // schemas and ordinary reads update access statistics, so
                // continuing here both writes into the malformed generation
                // and extends the live-lock veto that prevents recovery.
                return Err(ErrorKind::IndexCorruptInUse {
                    path: db_path.clone(),
                }
                .into());
            }
        };

        // Announce this connection before opening it, so any concurrent heal
        // sees a live holder and leaves the files alone. Taken after the probe
        // above so it never vetoes our own quarantine.
        let live_guard = crate::store::mutation_lock::acquire_live_shared(&db_path)?;

        let conn = Connection::open(&db_path)?;
        Self::configure_connection(&conn)?;

        // Run schema migrations and vector table creation on every open.
        // This ensures tables added in newer versions (e.g. vec_memory)
        // exist even on databases created by older versions.
        schema::init_schema(&conn)?;
        vectors::init_vector_schema(&conn)?;
        // Stats tables (sessions/call_log/query_events) so hook-call telemetry
        // and query_events work on every transport, including the daemon-less
        // in-process CLI hook path (previously only the MCP server created them).
        crate::store::stats::init_stats_schema(&conn)?;

        // memory_entries/memory_edges live ONLY in this DB — salvage them out of
        // the quarantined file into the fresh schema before it takes over, and
        // record the loss so it surfaces loudly (stderr now, stats/warmup later).
        let rebuilt_from_corruption = if let Some(corrupt_path) = quarantined {
            let salvage = crate::store::heal::salvage_memory(&conn, &corrupt_path);
            crate::store::heal::write_report(&corrupt_path, salvage);
            tracing::warn!(
                corrupt = %corrupt_path.display(),
                salvaged_entries = salvage.entries,
                salvaged_edges = salvage.edges,
                "index.sqlite was structurally corrupt; quarantined and rebuilt empty — docs/code re-index from source"
            );
            eprintln!(
                "mdkb: index.sqlite was CORRUPT — quarantined to {}; salvaged {} memory entries + {} edges; run `mdkb update` to re-index docs",
                corrupt_path.display(),
                salvage.entries,
                salvage.edges
            );
            true
        } else {
            false
        };

        Ok(Self {
            conn,
            config_path,
            db_path,
            rebuilt_from_corruption,
            corrupt_in_use: false,
            _live_guard: Some(live_guard),
        })
    }

    /// Initialize a new mdkb directory.
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mdkb_dir = root.join(".mdkb");

        // Create directory if needed
        if !mdkb_dir.exists() {
            std::fs::create_dir_all(&mdkb_dir)?;
        }

        // Auto-init can be entered concurrently by several hook/MCP processes.
        // Derive every sidecar from the canonical store identity and serialize
        // config + virtual-table creation exactly like `open` does.
        let mdkb_dir = mdkb_dir.canonicalize().map_err(|e| {
            Error::other(format!(
                "cannot canonicalize {} during initialization: {e}",
                mdkb_dir.display()
            ))
        })?;
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");
        let _init_guard = crate::store::mutation_lock::acquire(&db_path, "init-schema")?;

        // Create memory directories
        let memory_dir = mdkb_dir.join("memory");
        std::fs::create_dir_all(memory_dir.join("entries"))?;
        std::fs::create_dir_all(memory_dir.join("archive"))?;

        // Create default config
        let config = Config::default();
        let config_str = toml::to_string_pretty(&config)?;
        std::fs::write(&config_path, config_str)?;

        // Initialize sqlite-vec extension
        vectors::init_sqlite_vec();

        // Create and initialize database
        let live_guard = crate::store::mutation_lock::acquire_live_shared(&db_path)?;
        let conn = Connection::open(&db_path)?;
        Self::configure_connection(&conn)?;
        schema::init_schema(&conn)?;
        vectors::init_vector_schema(&conn)?;

        Ok(Self {
            conn,
            config_path,
            db_path,
            rebuilt_from_corruption: false,
            corrupt_in_use: false,
            _live_guard: Some(live_guard),
        })
    }

    /// Get the project root directory (parent of `.mdkb/`).
    pub fn root(&self) -> &Path {
        self.db_path
            .parent()
            .and_then(|p| p.parent())
            .expect("db_path must be inside .mdkb/")
    }

    /// Get the memory directory path.
    pub fn memory_dir(&self) -> PathBuf {
        self.db_path.parent().unwrap().join("memory")
    }
}

/// Handle `mdkb init` command.
pub fn handle_init(root: impl AsRef<Path>) -> Result<()> {
    let root = root.as_ref();
    let mdkb_dir = root.join(".mdkb");

    if mdkb_dir.exists() {
        return Err(Error::other(format!(
            "mdkb already initialized at {}",
            mdkb_dir.display()
        )));
    }

    let ctx = Context::init(root)?;
    apply_conventions(&ctx, root)?;

    let config = Config::load_or_default(mdkb_dir.join("config.toml"));
    if config.code.enabled {
        bootstrap_code_index(root);
    }

    Ok(())
}

/// Create and populate the code index. Non-fatal: logs warnings on failure.
fn bootstrap_code_index(root: &Path) {
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

const MAX_COLLECTION_NAME_LEN: usize = 100;

fn validate_collection_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_COLLECTION_NAME_LEN {
        return Err(Error::other(format!(
            "Collection name must be 1-{MAX_COLLECTION_NAME_LEN} chars"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(Error::other(
            "Collection name must be lowercase alphanumeric with hyphens or underscores"
                .to_string(),
        ));
    }
    Ok(())
}

/// Validate that a collection path stays within the project root.
///
/// Uses `canonicalize` when the path exists on disk (resolves symlinks).
/// Falls back to lexical `..` rejection for paths that don't exist yet —
/// the robust canonicalize check in `handle_update_files` catches these
/// at index time regardless.
fn validate_collection_path(root: &Path, path: &str) -> Result<()> {
    let candidate = root.join(path);
    if let Ok(canonical) = candidate.canonicalize() {
        let canonical_root = root
            .canonicalize()
            .map_err(|e| Error::other(format!("Failed to canonicalize root: {e}")))?;
        if !canonical.starts_with(&canonical_root) {
            return Err(Error::other(format!(
                "Collection path '{}' escapes root directory (path traversal blocked)",
                path
            )));
        }
    } else if path.contains("..") {
        return Err(Error::other(format!(
            "Collection path '{}' contains path traversal pattern '..'",
            path
        )));
    }
    Ok(())
}

/// Handle `mdkb collection add` command.
pub fn handle_collection_add(ctx: &Context, name: &str, path: &str, pattern: &str) -> Result<()> {
    validate_collection_name(name)?;
    validate_collection_path(ctx.root(), path)?;

    let now = chrono::Utc::now().timestamp();
    let collection = Collection {
        name: name.to_string(),
        path: path.to_string(),
        pattern: pattern.to_string(),
        source: crate::domain::COLLECTION_SOURCE_MANUAL.to_string(),
        created_at: now,
        updated_at: now,
    };

    collections::add_collection(&ctx.conn, &collection)?;
    Ok(())
}

/// Handle `mdkb collection remove` command.
pub fn handle_collection_remove(ctx: &Context, name: &str) -> Result<bool> {
    collections::remove_collection(&ctx.conn, name)
}

/// A collection with its indexed-document count, for `collection list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CollectionInfo {
    pub name: String,
    pub path: String,
    pub pattern: String,
    pub doc_count: i64,
}

/// Handle `mdkb collection list` command.
pub fn handle_collection_list(ctx: &Context) -> Result<Vec<CollectionInfo>> {
    let colls = collections::list_collections(&ctx.conn)?;
    let mut out = Vec::with_capacity(colls.len());
    for c in colls {
        let doc_count = collections::get_collection_document_count(&ctx.conn, &c.name)?;
        out.push(CollectionInfo {
            name: c.name,
            path: c.path,
            pattern: c.pattern,
            doc_count,
        });
    }
    Ok(out)
}

/// Handle `mdkb collection rename` command.
pub fn handle_collection_rename(ctx: &Context, old_name: &str, new_name: &str) -> Result<()> {
    validate_collection_name(new_name)?;
    collections::rename_collection(&ctx.conn, old_name, new_name)
}

/// Handle `mdkb search` command.
pub fn handle_search(
    ctx: &Context,
    query_text: &str,
    limit: usize,
    collection: Option<&str>,
) -> Result<Vec<crate::domain::SearchResult>> {
    let query = SearchQuery {
        text: query_text.to_string(),
        limit,
        collection: collection.map(String::from),
        tags: vec![],
        include_superseded: false,
    };

    search::search(&ctx.conn, &query)
}

/// Handle `mdkb vsearch` command - vector semantic search.
///
/// Uses batch document retrieval to avoid N+1 queries.
/// Uses cached model to avoid 2-5 second load time per request.
pub fn handle_vsearch(
    ctx: &Context,
    query_text: &str,
    limit: usize,
    collection: Option<&str>,
) -> Result<Vec<SearchResult>> {
    // Use cached service to avoid reloading
    let service = crate::llm::get_cached_service()?;

    // Generate query embedding
    let query_embedding = service.embed_query(query_text)?;

    // Perform vector search - get more results to account for collection filtering
    let fetch_limit = if collection.is_some() {
        limit * 2
    } else {
        limit
    };
    let vector_results = vectors::vector_search(&ctx.conn, &query_embedding, fetch_limit)?;

    if vector_results.is_empty() {
        return Ok(Vec::new());
    }

    // Batch retrieve all documents in a single query (fixes N+1 query pattern)
    let doc_ids: Vec<i64> = vector_results.iter().map(|(id, _)| *id).collect();
    let docs = documents::get_documents_batch(&ctx.conn, &doc_ids)?;

    // Build a map for quick lookup
    let doc_map: std::collections::HashMap<i64, _> = docs.into_iter().map(|d| (d.id, d)).collect();

    // Convert to SearchResult format, preserving order from vector search
    let mut results = Vec::new();
    for (doc_id, distance) in vector_results {
        if let Some(doc) = doc_map.get(&doc_id) {
            // Filter by collection if specified; exclude claude_sessions from default search
            if let Some(coll) = collection {
                if doc.collection != coll {
                    continue;
                }
            } else if doc.collection == crate::domain::COLLECTION_CLAUDE_SESSIONS {
                continue;
            }

            results.push(SearchResult {
                id: doc.id,
                collection: doc.collection.clone(),
                path: doc.relative_path.clone(),
                title: doc.title.clone(),
                score: 1.0 - f64::from(distance), // Convert distance to similarity
                snippets: vec![],
                status: None,
                superseded_by: None,
                repo_root: None,
            });

            // Stop once we have enough results
            if results.len() >= limit {
                break;
            }
        }
    }

    Ok(results)
}

/// Handle `mdkb query` command - hybrid search with RRF fusion.
///
/// Uses batch document retrieval to avoid N+1 queries.
/// Uses cached model to avoid 2-5 second load time per request.
pub fn handle_hybrid_search(
    ctx: &Context,
    query_text: &str,
    limit: usize,
    collection: Option<&str>,
    include_superseded: bool,
) -> Result<Vec<SearchResult>> {
    let fts_query = search::escape_fts5_query(query_text);
    // Falls back to BM25-only if the embedding service is unavailable.
    let query_embedding =
        match crate::llm::get_cached_service().and_then(|s| s.embed_query(query_text)) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("Hybrid search falling back to BM25-only: {e}");
                None
            }
        };
    hybrid_search_fts(
        ctx,
        &fts_query,
        query_embedding.as_deref(),
        limit,
        collection,
        include_superseded,
    )
}

/// Hybrid doc search over a pre-built FTS5 expression and a pre-computed query
/// embedding. Mirrors `memory::search_entries_hybrid_fts`: the caller owns both
/// escaping (so a full-sentence prompt can be OR-expanded) and embedding (so
/// inference never runs while a caller holds the context mutex).
///
/// `query_embedding: None` degrades to BM25-only.
pub fn hybrid_search_fts(
    ctx: &Context,
    fts_query: &str,
    query_embedding: Option<&[f32]>,
    limit: usize,
    collection: Option<&str>,
    include_superseded: bool,
) -> Result<Vec<SearchResult>> {
    // Get BM25 results
    let bm25_query = SearchQuery {
        text: String::new(), // ignored: `fts_query` is already escaped
        limit: limit * 2,    // Get more for fusion
        collection: collection.map(String::from),
        tags: vec![],
        include_superseded,
    };
    let bm25_results = search::search_fts(&ctx.conn, fts_query, &bm25_query)?;

    let vector_results = match query_embedding {
        Some(embedding) => vectors::chunk_vector_search(&ctx.conn, embedding, limit * 2)?,
        None => Vec::new(),
    };

    // Fuse results using RRF
    let config = hybrid::HybridConfig::default();
    let mut fused = hybrid::rrf_fusion(&bm25_results, &vector_results, &config);

    // Normalize scores
    hybrid::normalize_scores(&mut fused);

    if fused.is_empty() {
        return Ok(Vec::new());
    }

    // Batch retrieve all documents in a single query (includes status)
    let doc_ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
    let docs = documents::get_documents_batch(&ctx.conn, &doc_ids)?;

    // Build BM25 lookup for superseded_by metadata
    let bm25_map: HashMap<i64, &SearchResult> = bm25_results.iter().map(|r| (r.id, r)).collect();

    // Build a map for quick lookup
    let doc_map: std::collections::HashMap<i64, _> = docs.into_iter().map(|d| (d.id, d)).collect();

    // Convert to SearchResult format, preserving RRF order
    let mut results = Vec::new();
    for (doc_id, score) in fused {
        if let Some(doc) = doc_map.get(&doc_id) {
            // Filter by collection if specified; exclude claude_sessions from default search
            if let Some(coll) = collection {
                if doc.collection != coll {
                    continue;
                }
            } else if doc.collection == crate::domain::COLLECTION_CLAUDE_SESSIONS {
                continue;
            }

            // Filter superseded/retracted documents (vector search doesn't filter by status)
            if !include_superseded {
                if matches!(doc.status.as_deref(), Some("superseded" | "retracted")) {
                    continue;
                }
            }

            // Populate superseded_by from BM25 results if available
            let bm25 = bm25_map.get(&doc_id);
            results.push(SearchResult {
                id: doc.id,
                collection: doc.collection.clone(),
                path: doc.relative_path.clone(),
                title: doc.title.clone(),
                score,
                snippets: vec![],
                status: doc.status.clone(),
                superseded_by: bm25.and_then(|r| r.superseded_by.clone()),
                repo_root: None,
            });

            // Stop once we have enough results
            if results.len() >= limit {
                break;
            }
        }
    }

    Ok(results)
}

/// Result of a `get` command: document or memory entry.
#[derive(Debug)]
pub enum GetResult {
    /// A document with its content.
    Document(crate::domain::Document, String),
    /// A memory entry.
    Memory(crate::store::memory::MemoryEntry),
}

/// Handle `mdkb get` command.
///
/// Resolution order: numeric ID → file path → memory slug → fallback.
pub fn handle_get(ctx: &Context, id_or_path: &str, lines: Option<&str>) -> Result<GetResult> {
    // Try numeric ID first
    if let Ok(id) = id_or_path.parse::<i64>() {
        if let Some(doc) = documents::get_document(&ctx.conn, id)? {
            let content = get_document_content(ctx, &doc, lines)?;
            return Ok(GetResult::Document(doc, content));
        }
    }

    // Path resolution across collections. The path-like branch and the old
    // fallback both scanned every collection with the same call, so a not-found
    // path-like id (contains '/' or '.') scanned twice (BUG-E2). Run the scan
    // exactly once: for path-like ids here, for the rest in the fallback below.
    let path_like = id_or_path.contains('/') || id_or_path.contains('.');
    if path_like {
        let all_colls = collections::list_collections(&ctx.conn)?;
        for coll in &all_colls {
            if let Some(doc) = documents::get_document_by_path(&ctx.conn, &coll.name, id_or_path)? {
                let content = get_document_content(ctx, &doc, lines)?;
                return Ok(GetResult::Document(doc, content));
            }
        }
    }

    // Try memory slug
    if let Some(entry) = crate::store::memory::get_entry(&ctx.conn, id_or_path)? {
        return Ok(GetResult::Memory(entry));
    }

    // Fallback for non-path-like ids: try all collections without a path hint.
    if !path_like {
        let all_colls = collections::list_collections(&ctx.conn)?;
        for coll in &all_colls {
            if let Some(doc) = documents::get_document_by_path(&ctx.conn, &coll.name, id_or_path)? {
                let content = get_document_content(ctx, &doc, lines)?;
                return Ok(GetResult::Document(doc, content));
            }
        }
    }

    Err(Error::from(ErrorKind::DocumentNotFound {
        id: id_or_path.to_string(),
    }))
}

/// Get document content, applying optional line range.
fn get_document_content(
    ctx: &Context,
    doc: &crate::domain::Document,
    lines: Option<&str>,
) -> Result<String> {
    let content = documents::get_content(&ctx.conn, &doc.hash)?.ok_or_else(|| {
        Error::from(ErrorKind::DocumentNotFound {
            id: doc.id.to_string(),
        })
    })?;

    if let Some(range) = lines {
        apply_line_range(&content, range)
    } else {
        Ok(content)
    }
}

/// Handle `mdkb mget` command - batch retrieval by pattern.
///
/// Uses batch content retrieval to avoid N+1 queries.
pub fn handle_mget(
    ctx: &Context,
    pattern: &str,
    collection_filter: Option<&str>,
) -> Result<Vec<(Document, String)>> {
    let glob = Glob::new(pattern)
        .map_err(|e| Error::other(format!("Invalid glob pattern '{}': {}", pattern, e)))?
        .compile_matcher();

    // Get all documents, optionally filtered by collection
    let all_collections = collections::list_collections(&ctx.conn)?;
    let mut matching_docs = Vec::new();

    for coll in &all_collections {
        // Skip if collection filter doesn't match
        if let Some(filter) = collection_filter {
            if coll.name != filter {
                continue;
            }
        }

        let docs = documents::list_documents(&ctx.conn, &coll.name)?;

        for doc in docs {
            // Check if path matches pattern
            if glob.is_match(&doc.relative_path) {
                matching_docs.push(doc);
            }
        }
    }

    if matching_docs.is_empty() {
        return Ok(Vec::new());
    }

    // Batch retrieve all content in a single query (fixes N+1 query pattern)
    let hashes: Vec<&str> = matching_docs.iter().map(|d| d.hash.as_str()).collect();
    let content_map = documents::get_content_batch(&ctx.conn, &hashes)?;

    // Combine documents with their content
    let mut results = Vec::new();
    for doc in matching_docs {
        if let Some(content) = content_map.get(&doc.hash) {
            results.push((doc, content.clone()));
        }
    }

    Ok(results)
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
            result.memory_entries_archived = s.archived;
            result.memory_entries_archive_skipped = s.archive_skipped;
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

    crate::store::heal::verify_and_mark(&ctx.conn, &ctx.db_path)?;

    Ok(result)
}

/// Detect and register convention-based collections.
fn apply_conventions(ctx: &Context, root: &Path) -> Result<()> {
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

/// Compile a collection's glob pattern with POSIX-correct separator handling.
///
/// globset defaults to letting `*` cross `/`, so a non-recursive pattern like
/// `*.md` (the `_root` convention) would recursively swallow the whole tree and
/// duplicate every other collection's documents. `literal_separator(true)` stops
/// `*`/`?` from crossing `/` while `**` stays recursive — so `docs/**/*.md` is
/// unaffected and `*.md` matches only top-level files.
fn compile_collection_matcher(
    pattern: &str,
) -> std::result::Result<globset::GlobMatcher, globset::Error> {
    Ok(globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()?
        .compile_matcher())
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

/// Index a single file within a transaction. Shared by `update_collection` and
/// `handle_update_files` to avoid duplicating the mtime/read/index/evolution pipeline.
///
/// `existing_doc` is the previously indexed version (if any). Pass `None` for new files.
/// All errors are soft — pushed to `result.errors` and the caller continues.
struct SingleFileInput<'a> {
    ctx: &'a Context,
    collection_name: &'a str,
    abs_path: &'a Path,
    relative: String,
    existing_doc: Option<&'a Document>,
    display_name: &'a str,
    graph_cfg: &'a crate::config::GraphConfig,
    force: bool,
}

fn index_single_file(input: SingleFileInput<'_>, result: &mut UpdateResult) {
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

/// Process each user-supplied file path within a transaction.
fn index_specified_files(
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

/// A doc embedding taking longer than this is logged to `hook-slow.jsonl`.
const EMBED_SLOW_MS: u64 = 1000;

/// Single-chunk documents are embedded in groups of this size (PERF-G2) to bound
/// the peak result vector; `embed_documents` also batches to the model internally.
const SINGLE_DOC_EMBED_BATCH: usize = 256;

/// Whether a collection should be embedded given the `--collection` filter and
/// the `auto_embed_sessions` setting.
///
/// - `filter = Some(name)`: embed only that collection (the only way to embed
///   `claude_sessions`).
/// - `filter = None`: embed everything except `claude_sessions`, unless
///   `include_sessions` (from `[search] auto_embed_sessions`) is set.
fn should_embed_collection(coll_name: &str, filter: Option<&str>, include_sessions: bool) -> bool {
    match filter {
        Some(name) => coll_name == name,
        None => include_sessions || coll_name != crate::domain::COLLECTION_CLAUDE_SESSIONS,
    }
}

/// Append a slow-embedding record to `.mdkb/hook-slow.jsonl` (best-effort,
/// size-rotated via [`crate::mcp::dispatch::append_hook_log`]).
fn log_slow_embed(mdkb_dir: &Path, doc_path: &str, elapsed_ms: u64) {
    let ts = chrono::Utc::now().timestamp();
    let mut line = serde_json::json!({
        "ts": ts,
        "event": "embed",
        "doc": doc_path,
        "elapsed_ms": elapsed_ms,
    })
    .to_string();
    line.push('\n');
    crate::mcp::dispatch::append_hook_log(&mdkb_dir.join("hook-slow.jsonl"), &line);
}

/// Generate embeddings for documents that don't have them (the `has_embedding`
/// gate is the hash gate: content changes invalidate the old embedding, so only
/// new/changed docs are re-embedded; unchanged docs are skipped).
///
/// `collection = Some(name)` embeds only that collection — the explicit path for
/// large transcript collections. `None` embeds every collection except
/// `claude_sessions` (unless `[search] auto_embed_sessions`).
///
/// Uses batch content retrieval to avoid N+1 queries and the cached model to
/// avoid 2-5 second load time per request. Per-doc timing over 1s is logged to
/// `hook-slow.jsonl`.
pub fn handle_embed(ctx: &Context, collection: Option<&str>) -> Result<EmbedResult> {
    let mut result = EmbedResult::default();

    // Use cached service to avoid reloading
    let service = crate::llm::get_cached_service()?;

    // Load config once (chunking + session-embed policy)
    let config = crate::config::Config::load_or_default(&ctx.config_path);
    let chunking_config = config.chunking.clone();
    let include_sessions = config.search.auto_embed_sessions;
    let mdkb_dir = ctx.db_path.parent().map(|p| p.to_path_buf());

    // Get all documents
    let all_collections = collections::list_collections(&ctx.conn)?;

    // Which docs already have an embedding — fetched once as a set instead of one
    // `has_embedding` query per document (PERF-1: this path runs on every flush).
    let embedded_ids = vectors::embedded_document_ids(&ctx.conn)?;

    for coll in &all_collections {
        if !should_embed_collection(&coll.name, collection, include_sessions) {
            continue;
        }
        let docs = documents::list_documents(&ctx.conn, &coll.name)?;

        // Filter to docs that need embedding
        let mut docs_needing_embedding = Vec::new();
        for doc in docs {
            if embedded_ids.contains(&doc.id) {
                result.skipped += 1;
            } else {
                docs_needing_embedding.push(doc);
            }
        }

        if docs_needing_embedding.is_empty() {
            continue;
        }

        // Batch retrieve all content in a single query (fixes N+1 query pattern)
        let hashes: Vec<&str> = docs_needing_embedding
            .iter()
            .map(|d| d.hash.as_str())
            .collect();
        let content_map = documents::get_content_batch(&ctx.conn, &hashes)?;

        // Single-chunk docs (the common case for memory/small docs) are collected
        // and embedded in batches via embed_documents instead of one embed_query
        // call each (PERF-G2). Multi-chunk docs are embedded per-doc as before.
        let mut singles: Vec<(i64, &str)> = Vec::new();

        for doc in &docs_needing_embedding {
            // Get content from batch result
            let Some(content) = content_map.get(&doc.hash) else {
                result.errors.push(format!("No content for doc {}", doc.id));
                continue;
            };

            // Split into chunks using configured strategy
            let chunks = crate::store::chunks::split(content, &chunking_config);

            if chunks.len() <= 1 {
                // Small doc: defer to the batched pass below.
                singles.push((doc.id, content.as_str()));
                continue;
            }

            // Multi-chunk doc: embed each chunk (already a batched call).
            let embed_start = std::time::Instant::now();
            let texts: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
            match service.embed_documents(&texts) {
                Ok(embeddings) => {
                    vectors::store_chunk_embeddings(
                        &ctx.conn,
                        doc.id,
                        &chunks,
                        &embeddings,
                        crate::llm::embeddings::MODEL_NAME,
                    )?;
                    result.generated += chunks.len();
                }
                Err(e) => {
                    result.errors.push(format!(
                        "Failed to embed doc {} ({}, {} chunks): {}",
                        doc.id,
                        doc.relative_path,
                        chunks.len(),
                        e
                    ));
                }
            }

            let elapsed_ms = embed_start.elapsed().as_millis() as u64;
            if elapsed_ms > EMBED_SLOW_MS {
                if let Some(dir) = &mdkb_dir {
                    log_slow_embed(dir, &doc.relative_path, elapsed_ms);
                }
            }
        }

        // Batched embedding for all single-chunk docs in this collection. Grouped
        // to bound the peak result vector; embed_documents also batches to the
        // model internally. Results are distributed back to docs by index.
        for batch in singles.chunks(SINGLE_DOC_EMBED_BATCH) {
            let texts: Vec<&str> = batch.iter().map(|(_, text)| *text).collect();
            match service.embed_documents(&texts) {
                Ok(embeddings) => {
                    for ((doc_id, _), embedding) in batch.iter().zip(embeddings) {
                        vectors::store_embedding(
                            &ctx.conn,
                            *doc_id,
                            &embedding,
                            crate::llm::embeddings::MODEL_NAME,
                        )?;
                        result.generated += 1;
                    }
                }
                Err(e) => {
                    for (doc_id, _) in batch {
                        result
                            .errors
                            .push(format!("Failed to embed doc {doc_id}: {e}"));
                    }
                }
            }
        }
    }

    Ok(result)
}

/// Result of embedding generation.
#[derive(Debug, Clone, Default)]
pub struct EmbedResult {
    /// Number of embeddings generated.
    pub generated: usize,
    /// Number of documents skipped (already have embeddings).
    pub skipped: usize,
    /// Errors encountered.
    pub errors: Vec<String>,
}

/// Apply line range (e.g., "10:50") to content.
fn apply_line_range(content: &str, range: &str) -> Result<String> {
    let parts: Vec<&str> = range.split(':').collect();
    if parts.len() != 2 {
        return Err(Error::other(format!(
            "Invalid line range format: '{}', expected 'start:end'",
            range
        )));
    }

    let start: usize = parts[0]
        .parse()
        .map_err(|_| Error::other(format!("Invalid start line: '{}'", parts[0])))?;
    let end: usize = parts[1]
        .parse()
        .map_err(|_| Error::other(format!("Invalid end line: '{}'", parts[1])))?;

    if start == 0 {
        return Err(Error::other("Line numbers start at 1"));
    }
    if end < start {
        return Err(Error::other(format!(
            "End line ({}) must be >= start line ({})",
            end, start
        )));
    }

    let lines: Vec<&str> = content.lines().collect();
    let start_idx = start.saturating_sub(1);
    let end_idx = end.min(lines.len());

    if start_idx >= lines.len() {
        return Ok(String::new());
    }

    Ok(lines[start_idx..end_idx].join("\n"))
}

// ==================== Memory Handlers ====================

use crate::store::evolution::{self, Evolution, RelationshipType};
use crate::store::memory::{self, EntryStatus, EntryType, MemoryEntry};

/// Memory index.json structure.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryIndex {
    pub updated: String,
    pub entries: Vec<String>,
}

/// Generate and save memory index.json file.
pub fn generate_memory_index(ctx: &Context) -> Result<()> {
    let index = memory::get_warmup_index(&ctx.conn, 50)?;
    let now = chrono::Utc::now().to_rfc3339();

    let memory_index = MemoryIndex {
        updated: now,
        entries: index,
    };

    let index_path = ctx.memory_dir().join("index.json");
    let json = serde_json::to_string_pretty(&memory_index)?;
    std::fs::write(index_path, json)?;

    Ok(())
}

/// Load memory index from index.json (fast warmup).
pub fn load_memory_index(ctx: &Context) -> Result<Option<MemoryIndex>> {
    let index_path = ctx.memory_dir().join("index.json");
    if !index_path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(index_path)?;
    let index: MemoryIndex = serde_json::from_str(&content)?;
    Ok(Some(index))
}

/// Save a memory entry to disk as markdown (for backup).
///
/// Delegates to the shared YAML frontmatter serializer so export and per-add
/// backups stay byte-identical — otherwise a re-import would lose the fields
/// the hand-rolled version omitted (`source_type`, `created_at`, timestamps,
/// reminder `due_at`, etc.).
fn save_entry_to_disk(ctx: &Context, entry: &MemoryEntry) -> Result<()> {
    let entry_path = ctx
        .memory_dir()
        .join("entries")
        .join(format!("{}.md", entry.id));
    let content = crate::store::memory_file::to_markdown(entry);
    std::fs::write(entry_path, content)?;
    Ok(())
}

/// Outcome of [`sync_memory_files`].
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MemorySyncSummary {
    /// Entries that had no markdown file and were backfilled (DB → file).
    pub projected: usize,
    /// Entries whose (previously-projected) file was manually deleted and were
    /// therefore archived (file deletion → DB archive).
    pub archived: usize,
    /// Archival candidates NOT archived because too many projected files vanished
    /// at once (suspected bulk loss — git checkout/clean, backup restore — rather
    /// than deliberate per-entry deletion). See [`MEMORY_SYNC_BULK_ARCHIVE_CAP`].
    pub archive_skipped: usize,
}

/// Maximum entries `sync_memory_files` will archive in a single pass. Above this,
/// a vanished *set* of projected files is far more consistent with directory loss
/// (a `git checkout`/`stash`/`clean`, a partial restore, a mount blip) than with a
/// human deliberately deleting that many entries by hand — so we refuse to archive
/// and warn loudly instead of silently retiring a large slice of the memory corpus
/// (DATA-2). A human who really wants a bulk prune can re-run; nothing is lost by
/// waiting, everything is lost by archiving a corpus that's about to reappear.
const MEMORY_SYNC_BULK_ARCHIVE_CAP: usize = 10;

/// Reconcile the markdown projection with the DB, which is the source of truth.
///
/// Single documented direction: **DB → files**. Every active entry gets a
/// markdown file (backfilling the DB-only entries that MCP writes never
/// persisted), so a rebuild-from-files can never silently drop them. The one
/// file → DB signal honored is deletion: a *previously-projected* entry whose
/// file a human has since removed is archived (they asked for it gone). An entry
/// that was never projected is backfilled, never archived — that distinction is
/// what `projected_at` records.
pub fn sync_memory_files(ctx: &Context) -> Result<MemorySyncSummary> {
    let entries_dir = ctx.memory_dir().join("entries");
    std::fs::create_dir_all(&entries_dir)?;

    let now = chrono::Utc::now().timestamp();
    let mut summary = MemorySyncSummary::default();

    // Classify first, mutate second: the archival candidates (previously-projected
    // entries whose file is now gone) must be counted as a *set* before any are
    // archived, so the bulk circuit breaker below can veto them all at once.
    let mut to_project: Vec<String> = Vec::new();
    let mut to_mark: Vec<String> = Vec::new();
    let mut to_archive: Vec<String> = Vec::new();

    for (id, projected_at) in memory::list_active_projection_state(&ctx.conn)? {
        let file = entries_dir.join(format!("{id}.md"));
        match (file.exists(), projected_at) {
            // File exists but we hadn't recorded the projection (the 12 legacy
            // file-backed entries) — mark it, don't rewrite.
            (true, None) => to_mark.push(id),
            (true, Some(_)) => {} // already projected and present — nothing to do
            // Never projected (DB-only) → backfill the markdown file.
            (false, None) => to_project.push(id),
            // Was projected, file now gone → archival candidate (subject to the cap).
            (false, Some(_)) => to_archive.push(id),
        }
    }

    // DATA-2 guard: refuse to archive a large batch in one pass. A vanished set of
    // projected files is almost always directory loss, not intent — and archiving
    // the whole corpus silently (the count isn't even shown in the default update
    // output) is exactly the failure this rail prevents.
    if to_archive.len() > MEMORY_SYNC_BULK_ARCHIVE_CAP {
        tracing::warn!(
            "memory sync: {} projected entry files are missing at once (> cap {}) — \
             suspected bulk loss (git checkout/clean, backup restore), NOT archiving. \
             Restore .mdkb/memory/entries/ or re-run `mdkb update` once the files return.",
            to_archive.len(),
            MEMORY_SYNC_BULK_ARCHIVE_CAP,
        );
        summary.archive_skipped = to_archive.len();
        to_archive.clear();
    }

    for id in &to_mark {
        memory::set_projected_at(&ctx.conn, id, now)?;
    }
    for id in &to_project {
        if let Some(entry) = memory::get_entry_without_tracking(&ctx.conn, id)? {
            save_entry_to_disk(ctx, &entry)?;
            memory::set_projected_at(&ctx.conn, id, now)?;
            summary.projected += 1;
        }
    }
    for id in &to_archive {
        memory::set_status(&ctx.conn, id, EntryStatus::Archived)?;
        summary.archived += 1;
    }

    if summary.projected > 0 || summary.archived > 0 {
        generate_memory_index(ctx)?;
    }
    Ok(summary)
}

/// Move an entry to archive directory.
fn archive_entry_on_disk(ctx: &Context, id: &str) -> Result<()> {
    let entry_path = ctx.memory_dir().join("entries").join(format!("{}.md", id));
    let archive_path = ctx.memory_dir().join("archive").join(format!("{}.md", id));

    if entry_path.exists() {
        std::fs::rename(entry_path, archive_path)?;
    }
    Ok(())
}

/// Handle `mdkb memory add` command.
#[allow(clippy::too_many_arguments)]
pub fn handle_memory_add(
    ctx: &Context,
    id: &str,
    title: &str,
    entry_type: &str,
    tags: Option<&str>,
    content: &str,
    source_path: Option<&str>,
    ttl: Option<u64>,
    due_in: Option<u64>,
    source_type: Option<&str>,
) -> Result<()> {
    let entry_type: EntryType = entry_type
        .parse()
        .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))?;

    // Parse the explicit source_type (if any); default applied at insert only so
    // a defaulted re-write never silently downgrades an official_docs entry.
    let parsed_source_type: Option<memory::SourceType> = source_type
        .map(|s| s.parse())
        .transpose()
        .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))?;

    let tags: Vec<String> = tags
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    memory::validate_entry_input(id, title, &tags, content)?;

    if entry_type == EntryType::Prior && memory::is_mechanical_prior_noise(content) {
        return Err(ErrorKind::InvalidQuery(
            "Rejected mechanical tool-chain prior (no reusable lesson). Priors must carry a distilled, trigger-scoped lesson.".to_string(),
        )
        .into());
    }

    let now = chrono::Utc::now().timestamp();
    let expires_at = ttl.map(|s| now + s as i64);
    let due_at = due_in.map(|s| now + s as i64);

    let entry = MemoryEntry {
        id: id.to_string(),
        title: title.to_string(),
        content: content.to_string(),
        entry_type,
        tags,
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 0,
        last_accessed: None,
        source_path: source_path.map(String::from),
        confirmations: 0,
        last_confirmed_at: None,
        source_type: parsed_source_type.unwrap_or_default(),
        expires_at,
        due_at,
    };

    // Upsert: update in place when the id already exists, else insert. Mirrors
    // the MCP `memory_write` path so the CLI/bridge does not fail with a UNIQUE
    // constraint violation when re-writing an existing entry.
    let persisted = if let Some(mut existing) = memory::get_entry_without_tracking(&ctx.conn, id)? {
        if let Err(e) = memory::save_revision(
            &ctx.conn,
            id,
            &existing.content,
            content,
            existing.source_type,
        ) {
            tracing::warn!("Failed to save revision for {id}: {e}");
        }
        existing.title = entry.title;
        existing.content = entry.content;
        existing.entry_type = entry.entry_type;
        existing.tags = entry.tags;
        existing.expires_at = entry.expires_at;
        if due_in.is_some() {
            existing.due_at = entry.due_at;
        }
        // Only override provenance when the caller explicitly passed --source-type;
        // a defaulted re-write preserves the existing trust level.
        if let Some(st) = parsed_source_type {
            existing.source_type = st;
        }
        existing.updated_at = now;
        memory::update_entry(&ctx.conn, &existing)?;
        existing
    } else {
        memory::add_entry(&ctx.conn, &entry)?;
        entry
    };

    // Save to disk and regenerate index
    if let Err(e) = save_entry_to_disk(ctx, &persisted) {
        tracing::warn!("Failed to save entry to disk: {e}");
    }
    if let Err(e) = generate_memory_index(ctx) {
        tracing::warn!("Failed to regenerate memory index: {e}");
    }

    // Embed (or re-embed) so CLI/bridge writes are searchable by vector like the
    // MCP path. A cold model or embed failure leaves the entry pending — never
    // fails the write — and `mdkb update` backfills it (count in `mdkb stats`).
    // Gated by `[search] auto_embed_memory` (default on): off skips the ONNX call
    // entirely, leaving the entry pending for backfill (TEST-1 hermetic switch).
    if crate::config::Config::load_or_default(&ctx.config_path)
        .search
        .auto_embed_memory
    {
        if let Err(e) = memory::embed_entry(&ctx.conn, id, &persisted.title, &persisted.content) {
            tracing::warn!("Failed to store embedding for '{id}': {e}");
        }
    }

    Ok(())
}

/// Handle `mdkb memory show` command.
pub fn handle_memory_show(ctx: &Context, id: &str) -> Result<Option<MemoryEntry>> {
    memory::get_entry(&ctx.conn, id)
}

/// Outcome of a `mdkb memory confirm` command.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfirmResult {
    pub id: String,
    pub outcome: String,
    /// Confirmation count after applying the signal.
    pub confirmations: u32,
    pub message: String,
}

/// Handle `mdkb memory confirm <id> --outcome confirmed|refuted`.
///
/// Runs fully in-process against the local DB — no daemon required — so the
/// confirm loop is reachable on every transport (this is what the UPS recall
/// nudge points at). Confirmations live in the DB (source of truth for the
/// confidence signal); the markdown projection is refreshed on the next write.
pub fn handle_memory_confirm(ctx: &Context, id: &str, outcome: &str) -> Result<ConfirmResult> {
    let delta = memory::outcome_to_delta(outcome)?;
    let message = memory::confirm_entry(&ctx.conn, id, delta)?;
    // Re-read the persisted count so JSON callers get the exact new value.
    let confirmations = memory::get_entry_without_tracking(&ctx.conn, id)?
        .map(|e| e.confirmations)
        .unwrap_or(0);
    Ok(ConfirmResult {
        id: id.to_string(),
        outcome: outcome.to_string(),
        confirmations,
        message,
    })
}

/// Handle `mdkb memory link` command: add a typed graph edge from an existing
/// memory entry to another memory slug or a document path.
pub fn handle_memory_link(
    ctx: &Context,
    id: &str,
    relation: &str,
    target: &str,
    doc: bool,
    agent: Option<&str>,
) -> Result<()> {
    use crate::store::memory_graph::{self, MemoryRelation, TargetKind};
    use std::str::FromStr;

    let rel = MemoryRelation::from_str(relation)
        .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))?;

    if memory::get_entry(&ctx.conn, id)?.is_none() {
        return Err(ErrorKind::InvalidQuery(format!("Memory entry not found: {id}")).into());
    }

    let kind = if doc {
        TargetKind::Doc
    } else {
        TargetKind::Memory
    };
    memory_graph::add_edge(&ctx.conn, id, target, kind, rel)?;

    if let Some(agent) = agent {
        memory::set_provenance(&ctx.conn, id, None, Some(agent))?;
    }

    println!("Linked {id} --{relation}--> {target}");
    Ok(())
}

/// Handle `mdkb memory list` command.
pub fn handle_memory_list(
    ctx: &Context,
    limit: usize,
    status: Option<&str>,
) -> Result<Vec<MemoryEntry>> {
    let status_filter = status
        .map(|s| {
            s.parse::<EntryStatus>()
                .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))
        })
        .transpose()?;

    memory::list_entries(&ctx.conn, limit, status_filter)
}

/// Handle `mdkb memory search` command.
pub fn handle_memory_search(ctx: &Context, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
    memory::search_entries(&ctx.conn, query, limit)
}

/// Handle `mdkb memory warmup` command.
pub fn handle_memory_warmup(ctx: &Context, limit: usize) -> Result<Vec<String>> {
    memory::get_warmup_index(&ctx.conn, limit)
}

/// Handle `mdkb eval recall` — seed a fixture DB and score recall@k / MRR.
pub fn handle_eval_recall(
    fixture: Option<&std::path::Path>,
    k: usize,
) -> Result<crate::eval::recall::RecallReport> {
    let fx = match fixture {
        Some(p) => crate::eval::fixture::Fixture::load(p)?,
        None => crate::eval::fixture::Fixture::bundled()?,
    };
    let conn = fx.seed_db()?;
    crate::eval::recall::run_recall(&conn, &fx.recall_cases(), k)
}

/// Handle `mdkb eval judge` — seed a fixture DB and score answer support.
pub fn handle_eval_judge(
    fixture: Option<&std::path::Path>,
    k: usize,
) -> Result<crate::eval::judge::JudgeReport> {
    let fx = match fixture {
        Some(p) => crate::eval::fixture::Fixture::load(p)?,
        None => crate::eval::fixture::Fixture::bundled()?,
    };
    let conn = fx.seed_db()?;
    crate::eval::judge::run_judge(
        &conn,
        &fx.judge_cases(),
        k,
        &crate::eval::judge::SubstringJudge,
    )
}

/// Handle `mdkb memory rm` command.
pub fn handle_memory_rm(ctx: &Context, id: &str) -> Result<bool> {
    let deleted = memory::delete_entry(&ctx.conn, id)?;
    if deleted {
        // Archive from disk and regenerate index
        if let Err(e) = archive_entry_on_disk(ctx, id) {
            tracing::warn!("Failed to archive entry on disk: {e}");
        }
        if let Err(e) = generate_memory_index(ctx) {
            tracing::warn!("Failed to regenerate memory index: {e}");
        }
    }
    Ok(deleted)
}

/// Handle `mdkb memory prune` command.
/// Archives entries not accessed within the given number of days.
pub fn handle_memory_prune(ctx: &Context, days: u32, dry_run: bool) -> Result<Vec<String>> {
    let pruned = memory::prune_entries(&ctx.conn, days, dry_run)?;
    if !dry_run && !pruned.is_empty() {
        // Archive entries from disk and regenerate index
        for id in &pruned {
            if let Err(e) = archive_entry_on_disk(ctx, id) {
                tracing::warn!("Failed to archive entry {id} on disk: {e}");
            }
        }
        if let Err(e) = generate_memory_index(ctx) {
            tracing::warn!("Failed to regenerate memory index: {e}");
        }
    }
    Ok(pruned)
}

// ==================== Memory Import ====================

/// JSON format for memory import (matches wiz fallback format).
#[derive(Debug, Deserialize)]
struct ImportFile {
    entries: Vec<ImportEntry>,
}

/// A single entry in the import JSON file.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportEntry {
    id: String,
    title: String,
    content: String,
    #[serde(default = "default_import_entry_type")]
    entry_type: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_import_source_type")]
    source_type: String,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
}

fn default_import_entry_type() -> String {
    "topic".to_string()
}
fn default_import_source_type() -> String {
    "user_statement".to_string()
}

/// Result of a memory import operation.
#[derive(Debug)]
pub struct ImportResult {
    pub imported: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

/// Result of a memory export operation.
#[derive(Debug)]
pub struct ExportResult {
    pub exported: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

/// Reject entry IDs that would escape the export directory or hide dotfiles.
///
/// Accepts the same character class enforced by [`memory::validate_entry_input`]
/// (alphanumerics, `-`, `_`, `.`, `/`), then adds filename-level guards:
/// no empty string, no path separators, no `..`, no leading `.`.
fn sanitize_export_id(id: &str) -> std::result::Result<(), String> {
    if id.is_empty() {
        return Err("empty id".to_string());
    }
    if id.contains('/') || id.contains('\\') {
        return Err(format!("path separator in id: {id:?}"));
    }
    if id == "." || id == ".." || id.starts_with('.') {
        return Err(format!("dot-prefixed id: {id:?}"));
    }
    Ok(())
}

/// Export all memory entries to `<dir>/<id>.md` files with YAML frontmatter.
pub fn handle_memory_export(
    ctx: &Context,
    dir: &Path,
    include_expired: bool,
    overwrite: bool,
    dry_run: bool,
) -> Result<ExportResult> {
    let now = chrono::Utc::now().timestamp();
    let entries = memory::list_entries_all(&ctx.conn)?;

    let mut result = ExportResult {
        exported: 0,
        skipped: 0,
        errors: Vec::new(),
    };

    if !dry_run {
        std::fs::create_dir_all(dir).map_err(|e| ErrorKind::Io {
            path: dir.to_path_buf(),
            operation: format!("create_dir_all: {e}"),
        })?;
    }

    for entry in &entries {
        let is_expired = entry.expires_at.map(|t| t <= now).unwrap_or(false);
        if is_expired && !include_expired {
            result.skipped += 1;
            continue;
        }

        if let Err(msg) = sanitize_export_id(&entry.id) {
            result.errors.push(format!("{}: {msg}", entry.id));
            continue;
        }

        let dest = dir.join(format!("{}.md", entry.id));

        if !overwrite && dest.exists() {
            result.skipped += 1;
            continue;
        }

        if dry_run {
            result.exported += 1;
            continue;
        }

        let text = crate::store::memory_file::to_markdown(entry);
        match std::fs::write(&dest, text) {
            Ok(()) => result.exported += 1,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: write failed: {e}", dest.display()));
            }
        }
    }

    Ok(result)
}

/// Import memory entries from a directory of `.md` files with YAML frontmatter.
pub fn handle_memory_import_dir(
    ctx: &Context,
    dir: &Path,
    dry_run: bool,
    skip_duplicates: bool,
) -> Result<ImportResult> {
    let mut result = ImportResult {
        imported: 0,
        skipped: 0,
        errors: Vec::new(),
    };

    let read_dir = std::fs::read_dir(dir).map_err(|e| ErrorKind::Io {
        path: dir.to_path_buf(),
        operation: format!("read_dir: {e}"),
    })?;

    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry_res in read_dir {
        match entry_res {
            Ok(entry) => {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    paths.push(path);
                }
            }
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: read_dir entry error: {e}", dir.display()));
            }
        }
    }
    paths.sort();

    // Phase 1: read, parse, validate, and check duplicates — no DB writes.
    let mut entries_to_insert: Vec<MemoryEntry> = Vec::new();
    for path in &paths {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: read error: {e}", path.display()));
                continue;
            }
        };

        let mf = match crate::store::memory_file::from_markdown(&text) {
            Ok(mf) => mf,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: parse error: {e}", path.display()));
                continue;
            }
        };

        let id = mf.meta.id.clone();

        if let Err(e) =
            memory::validate_entry_input(&mf.meta.id, &mf.meta.title, &mf.meta.tags, &mf.content)
        {
            result.errors.push(format!("{id}: {e}"));
            continue;
        }

        if memory::get_entry_without_tracking(&ctx.conn, &id)?.is_some() {
            if skip_duplicates {
                result.skipped += 1;
            } else {
                result.errors.push(format!(
                    "{id}: already exists (use --skip-duplicates to ignore)"
                ));
            }
            continue;
        }

        if dry_run {
            result.imported += 1;
            continue;
        }

        // Derived counters (access_count etc.) are DB-owned; fresh entry starts at 0.
        entries_to_insert.push(mf.into_fresh_entry());
    }

    // Phase 2: insert all valid entries atomically.
    if !entries_to_insert.is_empty() {
        with_transaction(&ctx.conn, || {
            for entry in &entries_to_insert {
                memory::add_entry(&ctx.conn, entry)?;
            }
            Ok(())
        })?;

        // Gated by `[search] auto_embed_memory` (default on); loaded once, not per entry.
        let embed = crate::config::Config::load_or_default(&ctx.config_path)
            .search
            .auto_embed_memory;
        for entry in &entries_to_insert {
            if let Err(e) = save_entry_to_disk(ctx, entry) {
                tracing::warn!("Failed to save imported entry {} to disk: {e}", entry.id);
            }
            // Embed so imported entries are vector-searchable; a cold model
            // leaves them pending for `mdkb update` backfill (never fatal).
            if embed {
                if let Err(e) =
                    memory::embed_entry(&ctx.conn, &entry.id, &entry.title, &entry.content)
                {
                    tracing::warn!("Failed to embed imported entry {}: {e}", entry.id);
                }
            }
            result.imported += 1;
        }
    }

    if !dry_run && result.imported > 0 {
        if let Err(e) = generate_memory_index(ctx) {
            tracing::warn!("Failed to regenerate memory index: {e}");
        }
    }

    Ok(result)
}

/// Handle `mdkb memory import` command.
pub fn handle_memory_import(
    ctx: &Context,
    path: &str,
    dry_run: bool,
    skip_duplicates: bool,
) -> Result<ImportResult> {
    let file_content = std::fs::read_to_string(path).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: std::path::PathBuf::from(path),
            operation: format!("read: {e}"),
        })
    })?;

    let import_file: ImportFile = serde_json::from_str(&file_content).map_err(|e| {
        Error::from(ErrorKind::InvalidQuery(format!(
            "Failed to parse {path}: {e}"
        )))
    })?;

    let mut result = ImportResult {
        imported: 0,
        skipped: 0,
        errors: Vec::new(),
    };
    let now = chrono::Utc::now().timestamp();

    // Phase 1: parse, validate, and check duplicates — no DB writes.
    let mut entries_to_insert: Vec<MemoryEntry> = Vec::new();
    for raw in &import_file.entries {
        // Parse entry_type
        let entry_type: EntryType = match raw.entry_type.parse() {
            Ok(t) => t,
            Err(e) => {
                result.errors.push(format!("{}: {e}", raw.id));
                continue;
            }
        };

        // Parse source_type
        let source_type: memory::SourceType = match raw.source_type.parse() {
            Ok(t) => t,
            Err(e) => {
                result.errors.push(format!("{}: {e}", raw.id));
                continue;
            }
        };

        // Validate fields
        if let Err(e) = memory::validate_entry_input(&raw.id, &raw.title, &raw.tags, &raw.content) {
            result.errors.push(format!("{}: {e}", raw.id));
            continue;
        }

        // Check for duplicates
        if memory::get_entry_without_tracking(&ctx.conn, &raw.id)?.is_some() {
            if skip_duplicates {
                result.skipped += 1;
                continue;
            }
            result.errors.push(format!(
                "{}: already exists (use --skip-duplicates to ignore)",
                raw.id
            ));
            continue;
        }

        if dry_run {
            result.imported += 1;
            continue;
        }

        entries_to_insert.push(MemoryEntry {
            id: raw.id.clone(),
            title: raw.title.clone(),
            content: raw.content.clone(),
            entry_type,
            tags: raw.tags.clone(),
            status: EntryStatus::Active,
            created_at: raw.created_at.unwrap_or(now),
            updated_at: raw.updated_at.unwrap_or(now),
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type,
            expires_at: None,
            due_at: None,
        });
    }

    // Phase 2: insert all valid entries atomically.
    if !entries_to_insert.is_empty() {
        with_transaction(&ctx.conn, || {
            for entry in &entries_to_insert {
                memory::add_entry(&ctx.conn, entry)?;
            }
            Ok(())
        })?;

        // Gated by `[search] auto_embed_memory` (default on); loaded once, not per entry.
        let embed = crate::config::Config::load_or_default(&ctx.config_path)
            .search
            .auto_embed_memory;
        for entry in &entries_to_insert {
            if let Err(e) = save_entry_to_disk(ctx, entry) {
                tracing::warn!("Failed to save imported entry {} to disk: {e}", entry.id);
            }
            // Embed so imported entries are vector-searchable; a cold model
            // leaves them pending for `mdkb update` backfill (never fatal).
            if embed {
                if let Err(e) =
                    memory::embed_entry(&ctx.conn, &entry.id, &entry.title, &entry.content)
                {
                    tracing::warn!("Failed to embed imported entry {}: {e}", entry.id);
                }
            }
            result.imported += 1;
        }
    }

    if !dry_run && result.imported > 0 {
        if let Err(e) = generate_memory_index(ctx) {
            tracing::warn!("Failed to regenerate memory index: {e}");
        }
    }

    Ok(result)
}

// ==================== Memory Condense (LLM feature) ====================

/// Result of a condense operation.
#[cfg(feature = "llm")]
#[derive(Debug, Clone)]
pub struct CondenseResult {
    /// Groups of related entries found.
    pub groups: Vec<CondenseGroup>,
    /// Number of entries consolidated.
    pub consolidated_count: usize,
    /// Number of new merged entries created.
    pub merged_count: usize,
}

/// A group of related entries that can be condensed.
#[cfg(feature = "llm")]
#[derive(Debug, Clone)]
pub struct CondenseGroup {
    /// IDs of entries in this group.
    pub entry_ids: Vec<String>,
    /// Common tags shared by entries.
    pub common_tags: Vec<String>,
    /// Proposed merged ID (generated from tags).
    pub proposed_id: String,
    /// Proposed title (generated by LLM or from entries).
    pub proposed_title: Option<String>,
    /// Proposed content (generated by LLM).
    pub proposed_content: Option<String>,
}

/// Find groups of related memory entries based on overlapping tags.
#[cfg(feature = "llm")]
pub fn find_related_entries(
    ctx: &Context,
    tag_filter: Option<&str>,
    min_entries: usize,
) -> Result<Vec<CondenseGroup>> {
    // Get all active entries
    let entries = memory::list_entries(&ctx.conn, 1000, Some(memory::EntryStatus::Active))?;

    // Build tag -> entries index
    let mut tag_index: HashMap<String, Vec<&memory::MemoryEntry>> = HashMap::new();
    for entry in &entries {
        for tag in &entry.tags {
            // If filtering by tag, only include matching entries
            if let Some(filter) = tag_filter {
                if tag != filter {
                    continue;
                }
            }
            tag_index.entry(tag.clone()).or_default().push(entry);
        }
    }

    // Find groups with overlapping tags (use the largest tag groups first)
    let mut groups: Vec<CondenseGroup> = Vec::new();
    let mut processed_ids: HashSet<String> = HashSet::new();

    // Sort tags by entry count (descending)
    let mut sorted_tags: Vec<_> = tag_index.iter().collect();
    sorted_tags.sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));

    for (tag, tag_entries) in sorted_tags {
        // Skip if not enough entries
        if tag_entries.len() < min_entries {
            continue;
        }

        // Filter out already-processed entries
        let available: Vec<_> = tag_entries
            .iter()
            .filter(|e| !processed_ids.contains(&e.id))
            .copied()
            .collect();

        if available.len() < min_entries {
            continue;
        }

        // Find common tags among these entries
        let common_tags = find_common_tags(&available);

        // Generate proposed ID from common tags
        let proposed_id = if common_tags.len() > 1 {
            format!("{}-consolidated", common_tags.join("-"))
        } else {
            format!("{}-consolidated", tag)
        };

        let entry_ids: Vec<String> = available.iter().map(|e| e.id.clone()).collect();

        // Mark these entries as processed
        for id in &entry_ids {
            processed_ids.insert(id.clone());
        }

        groups.push(CondenseGroup {
            entry_ids,
            common_tags,
            proposed_id,
            proposed_title: None,
            proposed_content: None,
        });
    }

    Ok(groups)
}

/// Find common tags among a set of entries.
#[cfg(feature = "llm")]
fn find_common_tags(entries: &[&memory::MemoryEntry]) -> Vec<String> {
    if entries.is_empty() {
        return Vec::new();
    }

    let first_tags: HashSet<_> = entries[0].tags.iter().cloned().collect();
    let mut common: HashSet<_> = first_tags;

    for entry in entries.iter().skip(1) {
        let entry_tags: HashSet<_> = entry.tags.iter().cloned().collect();
        common = common.intersection(&entry_tags).cloned().collect();
    }

    let mut result: Vec<_> = common.into_iter().collect();
    result.sort();
    result
}

/// Generate consolidated content for memory entries.
/// Currently uses heuristic-based concatenation.
/// TODO: When llama-cpp-rs is integrated, use LLM for smarter consolidation.
#[cfg(feature = "llm")]
pub fn generate_consolidated_content(entries: &[memory::MemoryEntry]) -> Result<(String, String)> {
    // Generate title from common elements
    let title = if entries.len() == 1 {
        entries[0].title.clone()
    } else {
        // Find common words in titles
        let first_words: HashSet<_> = entries[0]
            .title
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect();
        let common_words: Vec<_> = first_words
            .iter()
            .filter(|w| {
                entries[1..]
                    .iter()
                    .all(|e| e.title.to_lowercase().contains(w.as_str()))
            })
            .take(3)
            .cloned()
            .collect();

        if common_words.is_empty() {
            format!("{} (consolidated)", entries[0].title)
        } else {
            format!("{} - Complete Guide", common_words.join(" ").to_uppercase())
        }
    };

    // Generate content (simple concatenation for now)
    let mut content = String::new();
    content.push_str(&format!("# {}\n\n", title));
    content.push_str("*This entry consolidates multiple related entries.*\n\n");

    for entry in entries {
        content.push_str(&format!("## From: {}\n\n", entry.title));
        // Skip the first line if it's a title
        let entry_content = entry
            .content
            .lines()
            .skip_while(|l| l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        content.push_str(&entry_content);
        content.push_str("\n\n");
    }

    Ok((title, content))
}

/// Handle `mdkb memory condense` command.
#[cfg(feature = "llm")]
pub fn handle_memory_condense(
    ctx: &Context,
    tag_filter: Option<&str>,
    dry_run: bool,
    min_entries: usize,
) -> Result<CondenseResult> {
    let mut result = CondenseResult {
        groups: Vec::new(),
        consolidated_count: 0,
        merged_count: 0,
    };

    // Find groups of related entries
    let groups = find_related_entries(ctx, tag_filter, min_entries)?;

    if groups.is_empty() {
        return Ok(result);
    }

    for mut group in groups {
        // Get full entries for this group
        let entries: Vec<memory::MemoryEntry> = group
            .entry_ids
            .iter()
            .filter_map(|id| {
                memory::get_entry_without_tracking(&ctx.conn, id)
                    .ok()
                    .flatten()
            })
            .collect();

        if entries.len() < min_entries {
            continue;
        }

        // Generate consolidated content
        let (title, content) = generate_consolidated_content(&entries)?;
        group.proposed_title = Some(title.clone());
        group.proposed_content = Some(content.clone());

        if !dry_run {
            let now = chrono::Utc::now().timestamp();

            // Create the merged entry
            let merged_entry = memory::MemoryEntry {
                id: group.proposed_id.clone(),
                title,
                content,
                entry_type: entries
                    .first()
                    .map(|e| e.entry_type)
                    .unwrap_or(memory::EntryType::Topic),
                tags: group.common_tags.clone(),
                status: memory::EntryStatus::Active,
                created_at: now,
                updated_at: now,
                superseded_by: None,
                access_count: 0,
                last_accessed: None,
                source_path: None,
                confirmations: 0,
                last_confirmed_at: None,
                source_type: memory::SourceType::AutoExtracted,
                expires_at: None,
                due_at: None,
            };

            // Use transaction to ensure atomicity - either all changes succeed or none
            with_transaction(&ctx.conn, || {
                memory::add_entry(&ctx.conn, &merged_entry)?;

                // Mark original entries as superseded
                for entry in &entries {
                    let mut updated = entry.clone();
                    updated.status = memory::EntryStatus::Superseded;
                    updated.superseded_by = Some(group.proposed_id.clone());
                    memory::update_entry(&ctx.conn, &updated)?;
                }
                Ok(())
            })?;
            result.merged_count += 1;
            result.consolidated_count += entries.len();
        }

        result.groups.push(group);
    }

    // Regenerate index if changes were made
    if !dry_run && result.merged_count > 0 {
        if let Err(e) = generate_memory_index(ctx) {
            tracing::warn!("Failed to regenerate memory index: {e}");
        }
    }

    Ok(result)
}

// ==================== Evolution Handlers ====================

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
fn process_graph_edges(
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

/// Resolve a document path or ID to a document ID.
fn resolve_document_id(ctx: &Context, path_or_id: &str) -> Result<i64> {
    // Try to parse as ID first
    if let Ok(id) = path_or_id.parse::<i64>() {
        // Verify it exists
        if documents::get_document(&ctx.conn, id)?.is_some() {
            return Ok(id);
        }
    }

    // Try as path - search across all collections
    let all_collections = collections::list_collections(&ctx.conn)?;
    for coll in &all_collections {
        if let Some(doc) = documents::get_document_by_path(&ctx.conn, &coll.name, path_or_id)? {
            return Ok(doc.id);
        }
    }

    Err(Error::from(ErrorKind::DocumentNotFound {
        id: path_or_id.to_string(),
    }))
}

/// Resolve a graph entity argument to a document id. Extends `resolve_document_id`
/// with the `.md`-form tolerance the rest of the graph layer uses, so that
/// `links`/`neighbors`/`path` accept a bare slug (`people/x`) exactly like
/// `backlinks` does — not only `people/x.md` or a numeric id.
fn resolve_graph_entity(ctx: &Context, entity: &str) -> Result<i64> {
    if let Ok(id) = resolve_document_id(ctx, entity) {
        return Ok(id);
    }
    // Tolerate collection-prefixed paths (`map/people/x.md` == `people/x.md`).
    if let Some(id) = crate::store::graph::resolve_entity_ref(&ctx.conn, entity)? {
        return Ok(id);
    }
    let tried = crate::store::graph::resolvable_forms(&ctx.conn, entity).join(", ");
    Err(Error::from(ErrorKind::DocumentNotFound {
        id: format!("{entity} (tried: {tried})"),
    }))
}

/// Handle `mdkb evolve supersedes` command.
pub fn handle_evolve_supersedes(
    ctx: &Context,
    new: &str,
    old: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Supersedes,
        None,
        reason,
    )
}

/// Handle `mdkb evolve updates` command.
pub fn handle_evolve_updates(
    ctx: &Context,
    new: &str,
    old: &str,
    scope: Option<&str>,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Updates,
        scope,
        reason,
    )
}

/// Handle `mdkb evolve corrects` command.
pub fn handle_evolve_corrects(
    ctx: &Context,
    new: &str,
    old: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Corrects,
        None,
        reason,
    )
}

/// Handle `mdkb evolve retracts` command.
pub fn handle_evolve_retracts(
    ctx: &Context,
    new: &str,
    old: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Retracts,
        None,
        reason,
    )
}

/// Handle `mdkb evolve extends` command.
pub fn handle_evolve_extends(
    ctx: &Context,
    new: &str,
    old: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Extends,
        None,
        reason,
    )
}

/// Evolution history entry for display.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvolutionHistoryEntry {
    pub doc_id: i64,
    pub path: String,
    pub title: Option<String>,
    pub relationship: String,
    pub scope: Option<String>,
    pub reason: Option<String>,
}

/// Handle `mdkb history` command - show evolution chain.
pub fn handle_history(ctx: &Context, path_or_id: &str) -> Result<Vec<EvolutionHistoryEntry>> {
    let doc_id = resolve_document_id(ctx, path_or_id)?;

    // Get what this document supersedes/extends
    let forward = evolution::get_evolution_chain(&ctx.conn, doc_id)?;

    // Get what supersedes/extends this document
    let backward = evolution::get_superseded_by(&ctx.conn, doc_id)?;

    let mut history = Vec::new();

    // Add backward relationships (what superseded this)
    for evo in backward {
        if let Some(doc) = documents::get_document(&ctx.conn, evo.source_doc_id)? {
            history.push(EvolutionHistoryEntry {
                doc_id: doc.id,
                path: doc.relative_path,
                title: doc.title,
                relationship: format!("{} (newer)", evo.relationship),
                scope: evo.scope,
                reason: evo.reason,
            });
        }
    }

    // Add forward relationships (what this supersedes)
    for evo in forward {
        if let Some(doc) = documents::get_document(&ctx.conn, evo.target_doc_id)? {
            history.push(EvolutionHistoryEntry {
                doc_id: doc.id,
                path: doc.relative_path,
                title: doc.title,
                relationship: format!("{} (older)", evo.relationship),
                scope: evo.scope,
                reason: evo.reason,
            });
        }
    }

    Ok(history)
}

/// Handle `mdkb current` command - find current version of superseded doc.
pub fn handle_current(ctx: &Context, path_or_id: &str) -> Result<Option<Document>> {
    let doc_id = resolve_document_id(ctx, path_or_id)?;

    // Follow the supersession chain until we find a current document
    let mut current_id = doc_id;
    let mut visited = std::collections::HashSet::new();

    loop {
        if visited.contains(&current_id) {
            // Cycle detected, stop
            break;
        }
        visited.insert(current_id);

        // Check if current document is superseded
        let superseded_by = evolution::get_superseded_by(&ctx.conn, current_id)?;

        // Find a supersedes relationship (not just updates/corrects)
        let supersession = superseded_by
            .iter()
            .find(|e| e.relationship == RelationshipType::Supersedes);

        if let Some(evo) = supersession {
            current_id = evo.source_doc_id;
        } else {
            // No supersession, this is the current version
            break;
        }
    }

    documents::get_document(&ctx.conn, current_id)
}

/// Handle `mdkb superseded-by` command - show what replaced this doc.
pub fn handle_superseded_by(ctx: &Context, path_or_id: &str) -> Result<Vec<Evolution>> {
    let doc_id = resolve_document_id(ctx, path_or_id)?;
    evolution::get_superseded_by(&ctx.conn, doc_id)
}

// ==================== Knowledge-Graph Handlers ====================

/// Outgoing edges from an entity (the entity must be an indexed document).
/// Edge sources are resolved to `relative_path` so output never leaks numeric ids.
pub fn handle_graph_links(
    ctx: &Context,
    entity: &str,
    relation: Option<&str>,
) -> Result<Vec<crate::store::graph::EdgeView>> {
    let doc_id = resolve_graph_entity(ctx, entity)?;
    let edges = crate::store::graph::get_outgoing(&ctx.conn, doc_id, relation)?;
    crate::store::graph::edge_views(&ctx.conn, &edges)
}

/// Incoming edges to an entity. Accepts a dangling slug — no document required.
/// Edge sources are resolved to `relative_path` so output never leaks numeric ids.
pub fn handle_graph_backlinks(
    ctx: &Context,
    entity: &str,
    relation: Option<&str>,
) -> Result<Vec<crate::store::graph::EdgeView>> {
    let edges = crate::store::graph::get_incoming(&ctx.conn, entity, relation)?;
    crate::store::graph::edge_views(&ctx.conn, &edges)
}

/// Adjacent entities up to `depth` hops (undirected); start must be a document.
pub fn handle_graph_neighbors(
    ctx: &Context,
    entity: &str,
    relation: Option<&str>,
    depth: u32,
) -> Result<Vec<crate::store::graph::Neighbor>> {
    let doc_id = resolve_graph_entity(ctx, entity)?;
    crate::store::graph::neighbors(&ctx.conn, doc_id, relation, depth)
}

/// Shortest undirected path from `a` (a document) to `b` (any entity).
pub fn handle_graph_path(
    ctx: &Context,
    a: &str,
    b: &str,
    max_hops: u32,
) -> Result<Option<Vec<String>>> {
    let start = resolve_graph_entity(ctx, a)?;
    let target = resolve_target_key(ctx, b)?;
    crate::store::graph::shortest_path(&ctx.conn, start, &target, max_hops)
}

/// Canonical key for a path target. When `b` names an existing document — by
/// numeric id or path, exactly as the start argument is resolved — use that
/// document's `relative_path` so traversal can match it. Otherwise keep `b`
/// verbatim, preserving the dangling-target semantics (an unreachable slug).
fn resolve_target_key(ctx: &Context, b: &str) -> Result<String> {
    if let Ok(id) = resolve_document_id(ctx, b) {
        if let Some(doc) = documents::get_document(&ctx.conn, id)? {
            return Ok(doc.relative_path);
        }
    }
    Ok(b.to_string())
}

/// References that resolve to no indexed document (full-table scan).
pub fn handle_graph_dangling(ctx: &Context) -> Result<Vec<crate::store::graph::DanglingRef>> {
    crate::store::graph::dangling(&ctx.conn)
}

/// Entities ranked by degree centrality (full-table scan).
pub fn handle_graph_hubs(
    ctx: &Context,
    relation: Option<&str>,
    limit: usize,
) -> Result<Vec<crate::store::graph::Hub>> {
    crate::store::graph::hubs(&ctx.conn, relation, limit)
}

// ==================== Metrics Handlers ====================

/// Handle `mdkb metrics show` command.
pub fn handle_metrics_show(ctx: &Context, period: u32) -> Result<stats::QueryMetricsSummary> {
    stats::init_stats_schema(&ctx.conn)?;
    stats::get_query_metrics(&ctx.conn, period)
}

/// Handle `mdkb metrics latency` command.
pub fn handle_metrics_latency(ctx: &Context) -> Result<Vec<stats::QueryLatencyStats>> {
    stats::init_stats_schema(&ctx.conn)?;
    stats::get_query_latency_stats(&ctx.conn)
}

/// Handle `mdkb metrics export` command.
pub fn handle_metrics_export(ctx: &Context, period: u32) -> Result<Vec<stats::QueryEvent>> {
    stats::init_stats_schema(&ctx.conn)?;
    stats::export_query_events(&ctx.conn, period)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup_temp_dir() -> TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    /// `.mutation.lock`, `.live.lock` and SQLite's own `-wal`/`-shm` are all
    /// named from `db_path` as a string. Two spellings of one store therefore
    /// mean two lock domains over a single inode: neither the open guard nor
    /// the live lock excludes the other writer, and the result is the
    /// doubly-referenced pages and freelist mismatch we kept recovering from.
    /// Opening through an alias must land on the identical path.
    #[test]
    fn open_canonicalizes_the_store_so_every_lock_shares_one_identity() {
        let temp = setup_temp_dir();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).expect("mkdir");
        handle_init(&real).expect("init");

        let direct = Context::open(&real).expect("open directly");

        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink");
        let aliased = Context::open(&alias).expect("open through the alias");

        assert_eq!(
            direct.db_path, aliased.db_path,
            "aliased spelling opened a second lock domain over the same inode"
        );
        assert_eq!(
            crate::store::mutation_lock::live_lock_path(&direct.db_path),
            crate::store::mutation_lock::live_lock_path(&aliased.db_path),
            "the live lock that guards against renames must be one file, not two"
        );
    }

    /// A missing store must fail loudly rather than open something. The
    /// canonicalization error branch itself is defensive only — the existence
    /// check above it means a `.mdkb` that resolves cannot then fail to
    /// canonicalize, so this covers the reachable half.
    #[test]
    fn open_refuses_a_store_that_is_not_there() {
        let temp = setup_temp_dir();
        let root = temp.path().join("gone");
        std::fs::create_dir_all(root.join(".mdkb")).expect("mkdir");
        std::fs::remove_dir(root.join(".mdkb")).expect("rmdir");
        let err = Context::open(&root).expect_err("must not open");
        let msg = err.to_string();
        assert!(
            msg.contains("index.sqlite") || msg.contains("canonicalize"),
            "error must name the store or the canonicalization failure: {msg}"
        );
    }

    // ==================== Memory confirm ====================

    fn confirm_test_ctx() -> (TempDir, Context) {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init");
        let ctx = Context::open(temp.path()).expect("open");
        handle_memory_add(
            &ctx, "c1", "Title", "topic", None, "content", None, None, None, None,
        )
        .expect("add");
        (temp, ctx)
    }

    #[test]
    fn handle_get_resolves_path_memory_and_errors_once_on_missing() {
        // BUG-E2 correctness: after gating the collection scan to run once, a
        // path-like doc still resolves, a memory slug still resolves, and a
        // not-found path-like id returns DocumentNotFound (no double scan / panic).
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let docs = temp.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("guide.md"), "# Guide\n\nbody text").unwrap();

        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        match handle_get(&ctx, "guide.md", None).expect("path-like doc resolves") {
            GetResult::Document(doc, _) => assert!(doc.relative_path.ends_with("guide.md")),
            GetResult::Memory(entry) => panic!("expected a document, got {entry:?}"),
        }

        assert!(
            handle_get(&ctx, "missing/thing.md", None).is_err(),
            "a not-found path-like id must return an error"
        );

        handle_memory_add(
            &ctx, "slug1", "T", "topic", None, "c", None, None, None, None,
        )
        .unwrap();
        match handle_get(&ctx, "slug1", None).expect("memory slug resolves") {
            GetResult::Memory(e) => assert_eq!(e.id, "slug1"),
            GetResult::Document(document, content) => {
                panic!("expected a memory entry, got {document:?} with {content:?}")
            }
        }
    }

    #[test]
    fn confirm_increments_and_reports_count() {
        let (_t, ctx) = confirm_test_ctx();
        let r = handle_memory_confirm(&ctx, "c1", "confirmed").expect("confirm");
        assert_eq!(r.confirmations, 1);
        assert!(r.message.contains("Confirmed"));
        let r2 = handle_memory_confirm(&ctx, "c1", "confirmed").expect("confirm2");
        assert_eq!(r2.confirmations, 2, "double confirm accumulates");
    }

    #[test]
    fn confirm_refuted_floors_at_zero() {
        let (_t, ctx) = confirm_test_ctx();
        let r = handle_memory_confirm(&ctx, "c1", "refuted").expect("refute");
        assert_eq!(r.confirmations, 0, "refute below zero floors at 0");
    }

    #[test]
    fn confirm_unknown_id_errors_cleanly() {
        let (_t, ctx) = confirm_test_ctx();
        let err = handle_memory_confirm(&ctx, "ghost", "confirmed").expect_err("unknown id");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn confirm_invalid_outcome_rejected() {
        let (_t, ctx) = confirm_test_ctx();
        let err = handle_memory_confirm(&ctx, "c1", "sideways").expect_err("bad outcome");
        assert!(err.to_string().contains("outcome"), "{err}");
    }

    // ==================== Embed scoping ====================

    #[test]
    fn embed_scope_default_excludes_sessions() {
        // filter=None, include_sessions=false → docs yes, claude_sessions no.
        assert!(should_embed_collection("docs", None, false));
        assert!(should_embed_collection("plans", None, false));
        assert!(!should_embed_collection("claude_sessions", None, false));
    }

    #[test]
    fn embed_scope_include_sessions_covers_all() {
        assert!(should_embed_collection("docs", None, true));
        assert!(should_embed_collection("claude_sessions", None, true));
    }

    #[test]
    fn log_slow_embed_writes_record_to_hook_slow() {
        let dir = setup_temp_dir();
        log_slow_embed(dir.path(), "docs/big.md", 1500);
        let raw = std::fs::read_to_string(dir.path().join("hook-slow.jsonl"))
            .expect("hook-slow.jsonl written");
        let v: serde_json::Value = serde_json::from_str(raw.trim()).expect("valid json line");
        assert_eq!(v["event"], "embed");
        assert_eq!(v["doc"], "docs/big.md");
        assert_eq!(v["elapsed_ms"], 1500);
        assert!(v["ts"].as_i64().is_some(), "ts recorded for stats cutoff");
    }

    #[test]
    fn embed_scope_explicit_filter_targets_one_collection() {
        // Explicit --collection embeds exactly that one, sessions included.
        assert!(should_embed_collection(
            "claude_sessions",
            Some("claude_sessions"),
            false
        ));
        assert!(!should_embed_collection(
            "docs",
            Some("claude_sessions"),
            false
        ));
        assert!(should_embed_collection("docs", Some("docs"), false));
        assert!(!should_embed_collection(
            "claude_sessions",
            Some("docs"),
            false
        ));
    }

    // ==================== Init Tests ====================

    #[test]
    fn test_handle_init_creates_mdkb_directory() {
        let temp = setup_temp_dir();

        handle_init(temp.path()).expect("init should succeed");

        assert!(temp.path().join(".mdkb").exists());
        assert!(temp.path().join(".mdkb/config.toml").exists());
        assert!(temp.path().join(".mdkb/index.sqlite").exists());
    }

    #[test]
    fn test_handle_init_creates_memory_directories() {
        let temp = setup_temp_dir();

        handle_init(temp.path()).expect("init should succeed");

        // Memory directories should be created
        assert!(temp.path().join(".mdkb/memory").exists());
        assert!(temp.path().join(".mdkb/memory/entries").exists());
        assert!(temp.path().join(".mdkb/memory/archive").exists());
    }

    #[test]
    fn test_memory_add_creates_index_json() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        // Add a memory entry
        handle_memory_add(
            &ctx,
            "test-entry",
            "Test Entry",
            "topic",
            Some("test,example"),
            "# Test content\n\nThis is test content.",
            None,
            None,
            None,
            None,
        )
        .expect("add memory should succeed");

        // index.json should be created
        let index_path = temp.path().join(".mdkb/memory/index.json");
        assert!(index_path.exists(), "index.json should be created");

        // Entry file should be created
        let entry_path = temp.path().join(".mdkb/memory/entries/test-entry.md");
        assert!(entry_path.exists(), "entry file should be created");

        // Load and verify index
        let index = load_memory_index(&ctx).expect("load index should succeed");
        assert!(index.is_some(), "index should load");
        let index = index.unwrap();
        assert_eq!(index.entries.len(), 1);
        assert!(index.entries[0].contains("test-entry"));
    }

    #[test]
    fn test_memory_add_rejects_mechanical_prior() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        let err = handle_memory_add(
            &ctx,
            "prior-deadbeefdeadbeef",
            "fix: fix|tools:Edit->Bash",
            "prior",
            Some("prior,fix"),
            "Pattern: fix|tools:Edit->Bash->Bash|files:none|error_in:none\nOutcome: fix",
            None,
            None,
            None,
            None,
        )
        .expect_err("mechanical tool-chain prior must be rejected");
        assert!(
            err.to_string().contains("mechanical tool-chain prior"),
            "{err}"
        );

        // A non-prior entry with the same text is NOT rejected (guard is prior-scoped).
        handle_memory_add(
            &ctx,
            "topic-tools",
            "Tooling",
            "topic",
            None,
            "Pattern: fix|tools:Edit->Bash|files:none|error_in:none",
            None,
            None,
            None,
            None,
        )
        .expect("non-prior entry must not be rejected");
    }

    #[test]
    fn test_memory_link_roundtrip() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        for id in ["a", "b"] {
            handle_memory_add(
                &ctx, id, "Title", "topic", None, "content", None, None, None, None,
            )
            .expect("add should succeed");
        }

        handle_memory_link(&ctx, "a", "supports", "b", false, None).expect("link should succeed");

        let out = crate::store::memory_graph::outgoing(&ctx.conn, "a", None)
            .expect("outgoing should succeed");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_ref, "b");
        assert_eq!(out[0].relation, "supports");
        assert_eq!(out[0].target_kind, "memory");
    }

    #[test]
    fn test_memory_link_invalid_relation_lists_closed_set() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        handle_memory_add(
            &ctx, "a", "Title", "topic", None, "content", None, None, None, None,
        )
        .expect("add should succeed");

        let err = handle_memory_link(&ctx, "a", "mentions", "b", false, None)
            .expect_err("invalid relation must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("supports"),
            "error must list the closed set: {msg}"
        );
        assert!(
            msg.contains("relates_to"),
            "error must list the closed set: {msg}"
        );
    }

    #[test]
    fn test_memory_link_missing_source_errors() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        let err = handle_memory_link(&ctx, "ghost", "supports", "b", false, None)
            .expect_err("missing source must be rejected");
        assert!(err.to_string().contains("ghost"), "{err}");
    }

    #[test]
    fn test_memory_link_records_agent_provenance() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        handle_memory_add(
            &ctx, "a", "Title", "topic", None, "content", None, None, None, None,
        )
        .expect("add should succeed");

        handle_memory_link(&ctx, "a", "relates_to", "b", false, Some("scout"))
            .expect("link with agent should succeed");

        let (_, agent) = memory::get_provenance(&ctx.conn, "a").expect("provenance read");
        assert_eq!(agent.as_deref(), Some("scout"));
    }

    #[test]
    fn test_memory_link_doc_target_kind() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        handle_memory_add(
            &ctx, "a", "Title", "topic", None, "content", None, None, None, None,
        )
        .expect("add should succeed");

        handle_memory_link(&ctx, "a", "derived_from", "docs/spec.md", true, None)
            .expect("doc link should succeed");

        let out = crate::store::memory_graph::outgoing(&ctx.conn, "a", None)
            .expect("outgoing should succeed");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_kind, "doc");
    }

    #[test]
    fn test_memory_add_existing_id_upserts() {
        // Writing the same id twice must update in place, not fail with a
        // UNIQUE constraint violation (the CLI/bridge memory-write path).
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        handle_memory_add(
            &ctx,
            "dup-id",
            "First",
            "topic",
            None,
            "original content",
            None,
            None,
            None,
            None,
        )
        .expect("first write should succeed");

        // Second write with the same id — must not error.
        handle_memory_add(
            &ctx,
            "dup-id",
            "Second",
            "decision",
            Some("a,b"),
            "updated content",
            None,
            None,
            None,
            None,
        )
        .expect("re-writing an existing id must upsert, not fail");

        // Still a single entry, with the updated fields.
        let entry = handle_memory_show(&ctx, "dup-id")
            .expect("show should succeed")
            .expect("entry should exist");
        assert_eq!(entry.title, "Second");
        assert_eq!(entry.content, "updated content");
        let count: i64 = ctx
            .conn
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "upsert must not create a duplicate row");
    }

    #[test]
    fn test_memory_rm_updates_index_json() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        // Add an entry
        handle_memory_add(
            &ctx,
            "to-delete",
            "To Delete",
            "topic",
            None,
            "Content",
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // Verify it exists
        let index = load_memory_index(&ctx).unwrap().unwrap();
        assert_eq!(index.entries.len(), 1);

        // Delete it
        handle_memory_rm(&ctx, "to-delete").expect("rm should succeed");

        // index.json should be updated (empty now)
        let index = load_memory_index(&ctx).unwrap().unwrap();
        assert_eq!(index.entries.len(), 0);

        // Entry should be in archive
        let archive_path = temp.path().join(".mdkb/memory/archive/to-delete.md");
        assert!(archive_path.exists(), "entry should be archived");
    }

    #[test]
    fn test_handle_init_fails_if_already_initialized() {
        let temp = setup_temp_dir();

        handle_init(temp.path()).expect("first init should succeed");
        let result = handle_init(temp.path());

        assert!(result.is_err());
    }

    #[test]
    fn test_context_open_fails_if_not_initialized() {
        let temp = setup_temp_dir();

        let result = Context::open(temp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_context_open_succeeds_after_init() {
        let temp = setup_temp_dir();

        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        assert!(ctx.db_path.exists());
    }

    #[test]
    fn test_context_open_creates_vec_memory_on_legacy_db() {
        use rusqlite::Connection;

        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();

        // Simulate a legacy DB by dropping vec_memory
        {
            let db_path = temp.path().join(".mdkb/index.sqlite");
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch("DROP TABLE IF EXISTS vec_memory")
                .unwrap();

            // Verify it's gone
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_memory')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(!exists, "vec_memory should be dropped");
        }

        // Re-open — should recreate vec_memory
        let ctx = Context::open(temp.path()).expect("open should succeed");
        let exists: bool = ctx
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='vec_memory')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "vec_memory should be recreated by Context::open");
    }

    // ==================== Collection Tests ====================

    #[test]
    fn test_handle_collection_add() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").expect("add should succeed");

        let collections = handle_collection_list(&ctx).unwrap();
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].name, "docs");
    }

    #[test]
    fn test_graph_edges_indexed_dangling_and_idempotent() {
        use crate::store::graph;

        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        std::fs::create_dir_all(temp.path().join("docs")).unwrap();
        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();

        let body = "---\nowner: alice\nthemes:\n  - growth\n---\nSee [[notes/related]].\n";
        std::fs::write(temp.path().join("docs/x.md"), body).unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        let doc = documents::get_document_by_path(&ctx.conn, "docs", "x.md")
            .unwrap()
            .expect("x.md should be indexed");

        // Frontmatter (strong) + wikilink (soft) edges, target refs stored verbatim.
        let out = graph::get_outgoing(&ctx.conn, doc.id, None).unwrap();
        assert_eq!(out.len(), 3, "owner + themes + wikilink");
        assert!(
            out.iter()
                .any(|e| e.relation == "owner" && e.target_ref == "alice")
        );
        assert!(
            out.iter()
                .any(|e| e.relation == "themes" && e.target_ref == "growth")
        );
        assert!(
            out.iter()
                .any(|e| e.source_kind == graph::KIND_WIKILINK && e.target_ref == "notes/related")
        );

        // 'alice' is dangling: no document yet, but the backlink exists.
        assert!(
            graph::resolve_ref_to_doc(&ctx.conn, "alice")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            graph::get_incoming(&ctx.conn, "alice", None).unwrap().len(),
            1
        );

        // Re-running the hook (simulating re-index) must not duplicate edges.
        let parsed = parse_frontmatter(body);
        process_graph_edges(
            &ctx.conn,
            doc.id,
            &parsed,
            &crate::config::GraphConfig::default(),
        );
        let after = graph::get_outgoing(&ctx.conn, doc.id, None).unwrap();
        assert_eq!(after.len(), 3, "re-index must be idempotent");

        // Index alice.md later: the previously dangling edge now resolves.
        std::fs::write(temp.path().join("docs/alice.md"), "# Alice\n").unwrap();
        handle_update(&ctx, temp.path()).unwrap();
        assert!(
            graph::resolve_ref_to_doc(&ctx.conn, "alice")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn test_update_force_reindexes_unchanged_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        std::fs::create_dir_all(temp.path().join("docs")).unwrap();
        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        std::fs::write(
            temp.path().join("docs/a.md"),
            "---\nowner: alice\n---\nbody\n",
        )
        .unwrap();

        let r1 = handle_update(&ctx, temp.path()).unwrap();
        assert_eq!(r1.added, 1);

        // Plain update: unchanged mtime → skipped, not reprocessed.
        let r2 = handle_update(&ctx, temp.path()).unwrap();
        assert_eq!(r2.unchanged, 1);
        assert_eq!(r2.updated, 0);

        // Forced update: reprocessed despite unchanged mtime (so config changes apply).
        let r3 = handle_update_force(&ctx, temp.path(), true).unwrap();
        assert_eq!(r3.updated, 1, "force must reprocess unchanged files");
        assert_eq!(r3.unchanged, 0);

        // Edges remain intact after the forced re-index (idempotent).
        let doc = documents::get_document_by_path(&ctx.conn, "docs", "a.md")
            .unwrap()
            .unwrap();
        assert_eq!(
            crate::store::graph::get_outgoing(&ctx.conn, doc.id, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_update_docs_indexed_counts_docs_on_doc_only_store() {
        // Regression for the "Files discovered: 0" lie: on a doc-only store the
        // update summary must report the real doc total, not the code delta.
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        std::fs::create_dir_all(temp.path().join("docs")).unwrap();
        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        for name in ["a.md", "b.md", "c.md"] {
            std::fs::write(temp.path().join("docs").join(name), "# t\nbody\n").unwrap();
        }

        let r1 = handle_update(&ctx, temp.path()).unwrap();
        assert_eq!(r1.added, 3);
        assert_eq!(r1.docs_indexed(), 3, "first run: 3 new docs indexed");

        // Re-run with no changes: delta is 0/0 but the honest total stays 3.
        let r2 = handle_update(&ctx, temp.path()).unwrap();
        assert_eq!(r2.added, 0);
        assert_eq!(r2.unchanged, 3);
        assert_eq!(
            r2.docs_indexed(),
            3,
            "unchanged re-run still reports 3 docs indexed, not 0"
        );
    }

    #[test]
    fn test_handle_collection_add_duplicate_is_idempotent() {
        // Re-adding the same collection name upserts in place (no error): this keeps
        // an explicit `collection add` from racing the served process's convention
        // auto-registration of the same name.
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        handle_collection_add(&ctx, "docs", "./other", "**/*.mdx")
            .expect("re-add must be idempotent, not error");

        let all = crate::store::collections::list_collections(&ctx.conn).unwrap();
        assert_eq!(all.len(), 1, "upsert must not create a duplicate row");
        let got = crate::store::collections::get_collection(&ctx.conn, "docs")
            .unwrap()
            .unwrap();
        assert_eq!(got.path, "./other", "path should update on re-add");
        assert_eq!(got.pattern, "**/*.mdx", "pattern should update on re-add");
    }

    #[test]
    fn test_handle_collection_add_rejects_invalid_name() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let too_long = "x".repeat(101);
        let cases = vec![
            ("My Docs", "spaces"),
            ("UPPER", "uppercase"),
            ("has/slash", "slash"),
            ("ctrl\x00char", "null byte"),
            ("", "empty"),
            (too_long.as_str(), "too long"),
        ];
        for (name, label) in cases {
            let result = handle_collection_add(&ctx, name, "./docs", "**/*.md");
            assert!(result.is_err(), "should reject {label}: {name:?}");
        }
    }

    #[test]
    fn test_handle_collection_add_accepts_valid_names() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        for name in &["docs", "my-docs", "my_docs", "docs123", "a"] {
            // Remove if exists, to allow re-adding
            let _ = handle_collection_remove(&ctx, name);
            handle_collection_add(&ctx, name, "./docs", "**/*.md")
                .unwrap_or_else(|e| panic!("should accept {name:?}: {e}"));
        }
    }

    #[test]
    fn test_handle_collection_rename_validates_new_name() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        let result = handle_collection_rename(&ctx, "docs", "Bad Name!");
        assert!(result.is_err(), "rename should reject invalid new name");
    }

    #[test]
    fn test_handle_collection_add_blocks_dotdot_traversal() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let result = handle_collection_add(&ctx, "evil", "../secret", "**/*");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("path traversal") || msg.contains("escapes root"));
    }

    #[test]
    fn test_handle_collection_add_blocks_absolute_path_outside_root() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let result = handle_collection_add(&ctx, "evil", "/etc", "**/*");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("escapes root"));
    }

    #[cfg(unix)]
    #[test]
    fn test_handle_collection_add_blocks_symlink_escape() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a symlink inside the project that points outside
        let link_path = temp.path().join("sneaky");
        std::os::unix::fs::symlink("/tmp", &link_path).unwrap();

        let result = handle_collection_add(&ctx, "evil", "sneaky", "**/*");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("escapes root"));
    }

    #[test]
    fn test_handle_collection_add_allows_valid_relative_path() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Non-existent but valid relative path — should succeed (best-effort)
        handle_collection_add(&ctx, "notes", "notes/drafts", "**/*.md")
            .expect("valid relative path should succeed");
    }

    #[test]
    fn test_handle_collection_add_allows_existing_subdir() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        std::fs::create_dir_all(temp.path().join("src/lib")).unwrap();
        handle_collection_add(&ctx, "src", "src/lib", "**/*.rs")
            .expect("existing subdir should succeed");
    }

    #[test]
    fn test_handle_collection_remove() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        let removed = handle_collection_remove(&ctx, "docs").unwrap();

        assert!(removed);
        let collections = handle_collection_list(&ctx).unwrap();
        assert!(collections.is_empty());
    }

    #[test]
    fn test_handle_collection_remove_nonexistent() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let removed = handle_collection_remove(&ctx, "nonexistent").unwrap();
        assert!(!removed);
    }

    #[test]
    fn test_handle_collection_list_empty() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let collections = handle_collection_list(&ctx).unwrap();
        assert!(collections.is_empty());
    }

    #[test]
    fn test_handle_collection_rename() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "old", "./path", "**/*.md").unwrap();
        handle_collection_rename(&ctx, "old", "new").expect("rename should succeed");

        let collections = handle_collection_list(&ctx).unwrap();
        assert_eq!(collections.len(), 1);
        assert_eq!(collections[0].name, "new");
    }

    // ==================== Search Tests ====================

    #[test]
    fn test_handle_search_empty_index() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let results = handle_search(&ctx, "test", 10, None).unwrap();
        assert!(results.is_empty());
    }

    // ==================== Line Range Tests ====================

    #[test]
    fn test_apply_line_range_basic() {
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";

        let result = apply_line_range(content, "2:4").unwrap();
        assert_eq!(result, "line 2\nline 3\nline 4");
    }

    #[test]
    fn test_apply_line_range_single_line() {
        let content = "line 1\nline 2\nline 3";

        let result = apply_line_range(content, "2:2").unwrap();
        assert_eq!(result, "line 2");
    }

    #[test]
    fn test_apply_line_range_beyond_end() {
        let content = "line 1\nline 2";

        let result = apply_line_range(content, "1:100").unwrap();
        assert_eq!(result, "line 1\nline 2");
    }

    #[test]
    fn test_apply_line_range_invalid_format() {
        let result = apply_line_range("content", "invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_line_range_zero_start() {
        let result = apply_line_range("content", "0:5");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_line_range_end_before_start() {
        let result = apply_line_range("content", "5:2");
        assert!(result.is_err());
    }

    // ==================== Update Tests ====================

    #[test]
    fn test_handle_update_empty_collections() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.removed, 0);
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn test_handle_update_indexes_new_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a docs directory with a markdown file
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Test\n\nContent").unwrap();

        // Add collection
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 1);
        assert_eq!(result.updated, 0);
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn test_handle_update_skips_unchanged_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a docs directory with a markdown file
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Test\n\nContent").unwrap();

        // Add collection and index
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Run update again - should skip unchanged
        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.unchanged, 1);
    }

    #[test]
    fn test_handle_update_reindexes_modified_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a docs directory with a markdown file
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Test\n\nOld Content").unwrap();

        // Add collection and index
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Wait for filesystem mtime granularity (most systems have 1-second precision)
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Modify the file to update mtime
        let new_content = "# Test\n\nNew Content - Modified";
        std::fs::write(&file_path, new_content).unwrap();

        // Run update again - should detect modification
        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 1);
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn test_handle_update_removes_deleted_files() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create a docs directory with a markdown file
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Test\n\nContent").unwrap();

        // Add collection and index
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Delete the file
        std::fs::remove_file(&file_path).unwrap();

        // Run update - should detect deletion
        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.removed, 1);
        assert_eq!(result.unchanged, 0);
    }

    #[test]
    fn test_handle_update_multiple_collections() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create two directories
        let docs_dir = temp.path().join("docs");
        let notes_dir = temp.path().join("notes");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::create_dir(&notes_dir).unwrap();
        std::fs::write(docs_dir.join("doc.md"), "# Doc").unwrap();
        std::fs::write(notes_dir.join("note.md"), "# Note").unwrap();

        // Add two collections
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_collection_add(&ctx, "notes", "notes", "**/*.md").unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 2);
    }

    #[test]
    fn test_handle_update_indexes_gitignored_collection() {
        let temp = setup_temp_dir();

        // Initialize git repo so .gitignore is respected by git-aware tools
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(temp.path())
            .output()
            .expect("git init");

        // Create stories/ dir with content, then gitignore it
        let stories_dir = temp.path().join("stories");
        std::fs::create_dir(&stories_dir).unwrap();
        std::fs::write(
            stories_dir.join("001-done.md"),
            "# Story 1\n\nCompleted work",
        )
        .unwrap();
        std::fs::write(stories_dir.join("002-done.md"), "# Story 2\n\nMore work").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "stories/\n").unwrap();

        // Init mdkb and manually register collection (as wiz would do)
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "stories", "stories", "**/*.md").unwrap();

        // Update should index both files despite gitignore
        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(
            result.added, 2,
            "gitignored directory should still be indexed by collection walker"
        );
    }

    #[test]
    fn test_handle_update_respects_gitignore_when_opted_in() {
        let temp = setup_temp_dir();

        // Use a non-conventional directory name so apply_conventions won't
        // auto-register it and clash with our explicit collection add.
        let dir = temp.path().join("knowledge");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep.md"), "# keep").unwrap();
        std::fs::write(dir.join("drop.md"), "# drop").unwrap();
        std::fs::write(temp.path().join(".gitignore"), "knowledge/drop.md\n").unwrap();

        handle_init(temp.path()).unwrap();

        // Opt into gitignore for document indexing
        let config_path = temp.path().join(".mdkb/config.toml");
        let toml = "[indexing]\nrespect_gitignore = true\n";
        std::fs::write(&config_path, toml).unwrap();

        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "knowledge", "knowledge", "**/*.md").unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(
            result.added, 1,
            "knowledge/drop.md should be excluded when respect_gitignore=true"
        );
    }

    #[test]
    fn test_handle_update_mdkbignore_excludes_collection_files() {
        let temp = setup_temp_dir();

        let dir = temp.path().join("knowledge");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("keep.md"), "# keep").unwrap();
        std::fs::write(dir.join("draft.md"), "# draft").unwrap();

        // .mdkbignore excludes draft.md; collection glob still matches *.md.
        // Default respect_gitignore=false makes the doc walker read .mdkbignore.
        std::fs::write(temp.path().join(".mdkbignore"), "knowledge/draft.md\n").unwrap();

        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "knowledge", "knowledge", "**/*.md").unwrap();

        let result = handle_update(&ctx, temp.path()).expect("update should succeed");
        assert_eq!(result.added, 1, ".mdkbignore entry should exclude draft.md");
    }

    // ==================== Update Files Tests ====================

    #[test]
    fn test_handle_update_files_indexes_specific_file() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create docs directory with two markdown files
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("a.md"), "# File A\n\nContent A").unwrap();
        std::fs::write(docs_dir.join("b.md"), "# File B\n\nContent B").unwrap();

        // Add collection
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Update only file A
        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[docs_dir.join("a.md").to_string_lossy().to_string()],
        )
        .expect("update_files should succeed");
        assert_eq!(result.added, 1, "should index only file A");
        assert_eq!(result.updated, 0);

        // File B should not be indexed
        let doc_b = documents::get_document_by_path(&ctx.conn, "docs", "b.md").unwrap();
        assert!(doc_b.is_none(), "file B should not be in index");
    }

    #[test]
    fn test_handle_update_files_updates_existing() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Old\n\nOld content").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Wait for mtime granularity
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Modify file
        std::fs::write(&file_path, "# New\n\nNew content").unwrap();

        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .expect("update_files should succeed");
        assert_eq!(result.updated, 1, "should update modified file");
        assert_eq!(result.added, 0);
    }

    #[test]
    fn test_handle_update_files_skips_file_outside_collections() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Test").unwrap();

        // Add collection for docs/
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Try to update a file outside any collection
        let other = temp.path().join("other.md");
        std::fs::write(&other, "# Other").unwrap();

        let result = handle_update_files(&ctx, temp.path(), &[other.to_string_lossy().to_string()])
            .expect("update_files should succeed");
        assert_eq!(
            result.added, 0,
            "file outside collections should be skipped"
        );
        assert_eq!(result.errors.len(), 0, "not an error, just skipped");
    }

    #[test]
    fn test_handle_update_files_with_relative_paths() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Test\n\nContent").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Use relative path
        let result = handle_update_files(&ctx, temp.path(), &["docs/readme.md".to_string()])
            .expect("update_files should succeed");
        assert_eq!(result.added, 1, "should index file via relative path");

        // Verify stored path is the canonical relative form
        let doc = documents::get_document_by_path(&ctx.conn, "docs", "readme.md").unwrap();
        assert!(
            doc.is_some(),
            "should be retrievable by canonical relative path"
        );
    }

    #[test]
    fn test_handle_update_files_file_not_found() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        let nonexistent = docs_dir.join("does_not_exist.md");
        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[nonexistent.to_string_lossy().to_string()],
        )
        .expect("should succeed overall");
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("Cannot resolve"));
        assert_eq!(result.added, 0);
    }

    #[test]
    fn test_handle_update_files_continues_after_bad_path() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("good.md"), "# Good\n\nContent").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // First path is bad, second is good — both should be processed
        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[
                "/tmp/nonexistent_xyz.md".to_string(),
                docs_dir.join("good.md").to_string_lossy().to_string(),
            ],
        )
        .expect("should succeed overall");
        assert_eq!(result.errors.len(), 1, "bad path should produce one error");
        assert_eq!(result.added, 1, "good path should still be indexed");
    }

    #[test]
    fn test_handle_update_files_skips_unchanged() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Test\n\nContent").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Index once
        let r1 = handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .unwrap();
        assert_eq!(r1.added, 1);

        // Call again immediately — mtime hasn't changed
        let r2 = handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .unwrap();
        assert_eq!(r2.unchanged, 1);
        assert_eq!(r2.updated, 0);
        assert_eq!(r2.added, 0);
    }

    #[test]
    fn test_handle_update_files_blocks_path_traversal() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Try to index a file outside the project root
        let result = handle_update_files(&ctx, temp.path(), &["/etc/hosts".to_string()])
            .expect("should succeed overall");
        assert_eq!(result.added, 0, "file outside root should not be indexed");
        assert!(
            result.errors.iter().any(|e| e.contains("path traversal")),
            "should report path traversal error"
        );
    }

    #[test]
    fn test_handle_update_files_updates_verifies_content() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        let file_path = docs_dir.join("readme.md");
        std::fs::write(&file_path, "# Old Title\n\nOld content").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .unwrap();

        std::thread::sleep(std::time::Duration::from_secs(1));
        std::fs::write(&file_path, "# New Title\n\nNew content").unwrap();

        let result = handle_update_files(
            &ctx,
            temp.path(),
            &[file_path.to_string_lossy().to_string()],
        )
        .unwrap();
        assert_eq!(result.updated, 1);

        // Verify stored content was actually updated
        let doc = documents::get_document_by_path(&ctx.conn, "docs", "readme.md")
            .unwrap()
            .expect("doc should exist");
        assert_eq!(doc.title, Some("New Title".to_string()));
    }

    // ==================== Mget Tests ====================

    #[test]
    fn test_handle_mget_empty_index() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let results = handle_mget(&ctx, "**/*.md", None).expect("mget should succeed");
        assert!(results.is_empty());
    }

    #[test]
    fn test_handle_mget_matches_pattern() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create and index docs
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# README").unwrap();
        std::fs::write(docs_dir.join("guide.md"), "# Guide").unwrap();
        std::fs::write(docs_dir.join("notes.txt"), "Notes").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Pattern matches only .md files
        let results = handle_mget(&ctx, "*.md", None).expect("mget should succeed");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_handle_mget_with_collection_filter() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create two collections
        let docs_dir = temp.path().join("docs");
        let notes_dir = temp.path().join("notes");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::create_dir(&notes_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Doc README").unwrap();
        std::fs::write(notes_dir.join("readme.md"), "# Note README").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_collection_add(&ctx, "notes", "notes", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Filter to only docs collection
        let results = handle_mget(&ctx, "*.md", Some("docs")).expect("mget should succeed");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.collection, "docs");
    }

    #[test]
    fn test_handle_mget_nested_pattern() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Create nested structure
        let docs_dir = temp.path().join("docs");
        let sub_dir = docs_dir.join("api");
        std::fs::create_dir_all(&sub_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# README").unwrap();
        std::fs::write(sub_dir.join("endpoints.md"), "# API").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Pattern matches nested files
        let results = handle_mget(&ctx, "api/*.md", None).expect("mget should succeed");
        assert_eq!(results.len(), 1);
        assert!(results[0].0.relative_path.contains("api"));
    }

    #[test]
    fn test_handle_mget_returns_content() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# Hello World\n\nContent here.").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        let results = handle_mget(&ctx, "*.md", None).expect("mget should succeed");
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("Hello World"));
    }

    #[test]
    fn root_collection_glob_is_non_recursive() {
        // The `_root` convention collection uses pattern "*.md" with the explicit
        // intent (conventions.rs) of matching ONLY root-level markdown. globset's
        // default lets `*` cross `/`, which made "*.md" swallow the whole repo and
        // duplicate every other collection's docs. Indexing must treat `*` as
        // non-separator-crossing.
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        std::fs::write(temp.path().join("README.md"), "# Root").unwrap();
        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();
        std::fs::write(docs_dir.join("guide.md"), "# Nested").unwrap();

        handle_collection_add(&ctx, "_root", ".", "*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        let paths: Vec<String> = documents::list_documents(&ctx.conn, "_root")
            .unwrap()
            .into_iter()
            .map(|d| d.relative_path)
            .collect();

        assert!(
            paths.iter().any(|p| p == "README.md"),
            "root-level README.md must be indexed in _root: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.contains('/')),
            "_root '*.md' must be non-recursive — nested files leaked: {paths:?}"
        );
    }

    #[test]
    fn recursive_glob_still_matches_nested() {
        // Regression guard: making `*` non-recursive must NOT break `**`, which is
        // the explicit recursive token used by docs/stories/plans/reviews.
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        let sub = docs_dir.join("api");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(docs_dir.join("readme.md"), "# R").unwrap();
        std::fs::write(sub.join("endpoints.md"), "# E").unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        let paths: Vec<String> = documents::list_documents(&ctx.conn, "docs")
            .unwrap()
            .into_iter()
            .map(|d| d.relative_path)
            .collect();

        assert!(
            paths.iter().any(|p| p == "readme.md"),
            "root-level doc must stay indexed: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("api")),
            "nested doc must stay indexed under '**': {paths:?}"
        );
    }

    // ==================== Evolution Frontmatter Tests ====================

    #[test]
    fn test_frontmatter_supersedes_creates_evolution() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        // Create old document first
        std::fs::write(
            docs_dir.join("api-v1.md"),
            "---\ntitle: API v1\n---\n\n# API v1\n\nOld API docs.",
        )
        .unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Create new document that supersedes the old one
        std::fs::write(
            docs_dir.join("api-v2.md"),
            "---\ntitle: API v2\nsupersedes:\n  - path: \"api-v1.md\"\n    reason: \"Complete redesign\"\n---\n\n# API v2\n\nNew API.",
        )
        .unwrap();

        // Re-index
        handle_update(&ctx, temp.path()).unwrap();

        // Get the new document and check its evolution chain
        let v2_doc = documents::get_document_by_path(&ctx.conn, "docs", "api-v2.md")
            .unwrap()
            .expect("v2 should exist");

        let chain = evolution::get_evolution_chain(&ctx.conn, v2_doc.id).unwrap();
        assert_eq!(chain.len(), 1, "should have one evolution relationship");
        assert_eq!(chain[0].relationship, RelationshipType::Supersedes);
        assert_eq!(chain[0].reason, Some("Complete redesign".to_string()));

        // Check the old document is marked as superseded
        let (status, _) = evolution::get_document_status(&ctx.conn, chain[0].target_doc_id)
            .unwrap()
            .unwrap();
        assert_eq!(status, evolution::DocumentStatus::Superseded);
    }

    #[test]
    fn test_frontmatter_updates_with_scope() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        // Create base document
        std::fs::write(
            docs_dir.join("security.md"),
            "---\ntitle: Security Guide\n---\n\n# Security\n\nSecurity info.",
        )
        .unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Create update document
        std::fs::write(
            docs_dir.join("security-jwt.md"),
            "---\ntitle: JWT Update\nupdates:\n  - path: \"security.md\"\n    scope: \"Token Handling\"\n    reason: \"JWT support\"\n---\n\nJWT info.",
        )
        .unwrap();

        handle_update(&ctx, temp.path()).unwrap();

        let jwt_doc = documents::get_document_by_path(&ctx.conn, "docs", "security-jwt.md")
            .unwrap()
            .expect("jwt doc should exist");

        let chain = evolution::get_evolution_chain(&ctx.conn, jwt_doc.id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].relationship, RelationshipType::Updates);
        assert_eq!(chain[0].scope, Some("Token Handling".to_string()));

        // Original document should still be current (updates don't supersede)
        let (status, _) = evolution::get_document_status(&ctx.conn, chain[0].target_doc_id)
            .unwrap()
            .unwrap();
        assert_eq!(status, evolution::DocumentStatus::Current);
    }

    #[test]
    fn test_frontmatter_evolution_invalid_path_warns() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        // Create document that references non-existent file
        std::fs::write(
            docs_dir.join("new.md"),
            "---\ntitle: New Doc\nsupersedes:\n  - \"nonexistent.md\"\n---\n\nContent.",
        )
        .unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();

        // Should not fail, just warn
        let result = handle_update(&ctx, temp.path());
        assert!(
            result.is_ok(),
            "update should succeed even with invalid reference"
        );

        // Document should be indexed
        let doc = documents::get_document_by_path(&ctx.conn, "docs", "new.md")
            .unwrap()
            .expect("doc should exist");

        // But no evolution chain
        let chain = evolution::get_evolution_chain(&ctx.conn, doc.id).unwrap();
        assert!(chain.is_empty(), "no relationships for invalid references");
    }

    #[test]
    fn test_frontmatter_simple_string_supersedes() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let docs_dir = temp.path().join("docs");
        std::fs::create_dir(&docs_dir).unwrap();

        // Create old document
        std::fs::write(
            docs_dir.join("old.md"),
            "---\ntitle: Old\n---\n\nOld content.",
        )
        .unwrap();

        handle_collection_add(&ctx, "docs", "docs", "**/*.md").unwrap();
        handle_update(&ctx, temp.path()).unwrap();

        // Create new document with simple string supersedes
        std::fs::write(
            docs_dir.join("new.md"),
            "---\ntitle: New\nsupersedes: \"old.md\"\n---\n\nNew content.",
        )
        .unwrap();

        handle_update(&ctx, temp.path()).unwrap();

        let new_doc = documents::get_document_by_path(&ctx.conn, "docs", "new.md")
            .unwrap()
            .expect("new doc should exist");

        let chain = evolution::get_evolution_chain(&ctx.conn, new_doc.id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].relationship, RelationshipType::Supersedes);
    }

    // ==================== Session Indexing Tests ====================

    /// Build a minimal valid JSONL session file with the given number of user turns.
    fn make_session_jsonl(session_id: &str, user_turns: usize) -> String {
        let mut lines = Vec::new();
        for i in 0..user_turns {
            lines.push(format!(
                r#"{{"type":"user","sessionId":"{}","message":{{"content":"User message {}"}}}}"#,
                session_id, i
            ));
            lines.push(format!(
                r#"{{"type":"assistant","sessionId":"{}","message":{{"content":"Assistant reply {}"}}}}"#,
                session_id, i
            ));
        }
        lines.join("\n")
    }

    #[test]
    fn test_handle_session_index_creates_collection_and_indexes() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Build fake session directory matching encode_project_path for temp path
        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();

        // Write a session file with enough turns
        std::fs::write(
            session_dir.join("abc-123.jsonl"),
            make_session_jsonl("abc-123", 4),
        )
        .unwrap();

        let result =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();

        assert!(result.added > 0);
        assert_eq!(result.errors.len(), 0);

        // Collection should exist
        let coll =
            collections::get_collection(&ctx.conn, crate::domain::COLLECTION_CLAUDE_SESSIONS)
                .unwrap();
        assert!(coll.is_some());

        // Document should be searchable
        let docs = documents::list_documents(&ctx.conn, crate::domain::COLLECTION_CLAUDE_SESSIONS)
            .unwrap();
        assert!(!docs.is_empty());
    }

    #[test]
    fn test_handle_session_index_skips_short_sessions() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();

        // Only 2 user turns — below min_turns=3
        std::fs::write(
            session_dir.join("short-session.jsonl"),
            make_session_jsonl("short-session", 2),
        )
        .unwrap();

        let result =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();

        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
    }

    #[test]
    fn test_handle_session_index_dedup_unchanged_content() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();

        std::fs::write(
            session_dir.join("dedup-test.jsonl"),
            make_session_jsonl("dedup-test", 4),
        )
        .unwrap();

        // First index
        let r1 =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();
        assert!(r1.added > 0);

        // Second index without modification — content hash matches, so skip.
        let r2 =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();
        assert_eq!(r2.added, 0);
        assert!(r2.unchanged > 0);
    }

    /// Regression for story 036: an append-only transcript must re-embed only the
    /// grown tail, not its whole body. Chunk keys are stable from turn 0, so after
    /// an append the earlier chunks are byte-identical and MUST be skipped by
    /// content hash — even though the file mtime bumped. The prior mtime-based
    /// dedup skipped nothing whenever the mtime changed, re-embedding the entire
    /// multi-MB file on every growth (the leak/CPU driver this story fixes).
    #[test]
    fn test_handle_session_index_append_skips_unchanged_chunks_despite_mtime_bump() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();
        let file = session_dir.join("grow.jsonl");

        // 8 user turns = 16 records → multiple stable chunks (size 10, stride 8):
        // chunk-000 = records[0..10], chunk-001 = records[8..16].
        std::fs::write(&file, make_session_jsonl("grow", 8)).unwrap();
        let r1 =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();
        assert!(r1.added > 0, "first index writes the initial chunks");

        // Append more turns. The content is deterministic per turn index, so the
        // first 16 records are byte-identical → chunk-000 (records[0..10]) is
        // unchanged; the tail grows and adds new chunks.
        std::fs::write(&file, make_session_jsonl("grow", 14)).unwrap();
        // Force mtime forward so the OLD mtime-dedup would reprocess EVERY chunk;
        // content-hash dedup must still skip the identical earlier chunk(s).
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_mins(2);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(bumped)
            .unwrap();

        let r2 =
            handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();

        assert!(
            r2.unchanged > 0,
            "byte-identical earlier chunks MUST be skipped by content hash despite \
             the bumped mtime (mtime dedup would give unchanged=0, re-embedding all)"
        );
        assert!(
            r2.added > 0,
            "the appended tail introduces at least one new chunk"
        );
    }

    /// Story 036 AC#3 — quantified before/after: on a large session, an append
    /// re-processes only the tail, not the whole body. `unchanged` (skipped, zero
    /// embedding cost) must dominate `added + updated` (re-embedded). Under the old
    /// mtime dedup, an mtime-bumping append gave unchanged=0 and reprocessed ALL
    /// chunks — the cost this story removes.
    #[test]
    fn test_handle_session_index_append_cost_is_delta_bounded() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        let encoded = crate::domain::sessions::encode_project_path(&temp.path().to_string_lossy());
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();
        let file = session_dir.join("big.jsonl");

        // 50 user turns = 100 records → ~12 chunks (size 10, stride 8).
        std::fs::write(&file, make_session_jsonl("big", 50)).unwrap();
        handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();

        // Append 2 turns; force mtime forward (worst case for the old dedup).
        std::fs::write(&file, make_session_jsonl("big", 52)).unwrap();
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_mins(2);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(bumped)
            .unwrap();

        let r = handle_session_index(&ctx, &sessions_base, &temp.path().to_string_lossy()).unwrap();
        let reprocessed = r.added + r.updated;
        assert!(
            r.unchanged >= 3 * reprocessed,
            "append must skip the bulk of chunks: unchanged={} should dominate \
             reprocessed(added+updated)={} (old mtime dedup: unchanged=0)",
            r.unchanged,
            reprocessed
        );
    }

    #[test]
    fn test_handle_session_index_no_session_dir() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let sessions_base = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions_base).unwrap();

        let result =
            handle_session_index(&ctx, &sessions_base, "/nonexistent/project/path").unwrap();

        assert_eq!(result.added, 0);
        assert_eq!(result.updated, 0);
        assert_eq!(result.unchanged, 0);
    }

    // ==================== Memory Import Tests ====================

    fn write_import_json(dir: &std::path::Path, content: &str) -> String {
        let path = dir.join("import.json");
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[test]
    fn test_memory_import_basic() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let json = r#"{"entries": [
            {"id": "test-import", "title": "Test Import", "content": "Some content", "entryType": "decision", "tags": ["db"], "sourceType": "auto_extracted"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.skipped, 0);
        assert!(result.errors.is_empty());

        let entry = handle_memory_show(&ctx, "test-import").unwrap().unwrap();
        assert_eq!(entry.title, "Test Import");
        assert_eq!(entry.source_type, memory::SourceType::AutoExtracted);
    }

    #[test]
    fn test_memory_import_dry_run() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let json = r#"{"entries": [
            {"id": "dry-run-entry", "title": "Dry Run", "content": "Content"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, true, false).unwrap();
        assert_eq!(result.imported, 1);

        // Entry should NOT exist in DB
        let entry = handle_memory_show(&ctx, "dry-run-entry").unwrap();
        assert!(entry.is_none(), "dry-run should not create entries");
    }

    #[test]
    fn test_memory_import_skip_duplicates() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Add an entry first
        handle_memory_add(
            &ctx, "existing", "Existing", "topic", None, "Content", None, None, None, None,
        )
        .unwrap();

        let json = r#"{"entries": [
            {"id": "existing", "title": "Duplicate", "content": "Content"},
            {"id": "new-one", "title": "New One", "content": "Content"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, true).unwrap();
        assert_eq!(result.imported, 1);
        assert_eq!(result.skipped, 1);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_memory_import_duplicate_warns() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_memory_add(
            &ctx, "existing", "Existing", "topic", None, "Content", None, None, None, None,
        )
        .unwrap();

        let json = r#"{"entries": [
            {"id": "existing", "title": "Duplicate", "content": "Content"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].contains("already exists"));
    }

    #[test]
    fn test_memory_import_empty_entries() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let json = r#"{"entries": []}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 0);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn test_memory_import_malformed_json() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let path = write_import_json(temp.path(), "not json at all");

        let result = handle_memory_import(&ctx, &path, false, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_memory_import_defaults() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Minimal JSON — only required fields
        let json = r#"{"entries": [
            {"id": "minimal", "title": "Minimal Entry", "content": "Content"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 1);

        let entry = handle_memory_show(&ctx, "minimal").unwrap().unwrap();
        assert_eq!(entry.source_type, memory::SourceType::UserStatement);
        assert_eq!(entry.entry_type, EntryType::Topic);
        assert!(entry.tags.is_empty());
    }

    #[test]
    fn test_memory_import_invalid_entry_type() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let json = r#"{"entries": [
            {"id": "bad-type", "title": "Bad Type", "content": "Content", "entryType": "invalid"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false).unwrap();
        assert_eq!(result.imported, 0);
        assert_eq!(result.errors.len(), 1);
    }

    /// Atomicity: when two entries share the same ID the second INSERT violates the UNIQUE
    /// constraint, causing the transaction to roll back so that *neither* entry lands in the DB.
    #[test]
    fn test_memory_import_atomic_rollback_on_duplicate_id() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        // Both entries share the same id.  Phase-1 sees neither in the DB (no pre-existing rows),
        // so both are queued for insert.  Phase-2 inserts "dup-id" successfully, then hits a
        // UNIQUE constraint on the second insert, triggering a rollback of the whole transaction.
        let json = r#"{"entries": [
            {"id": "dup-id", "title": "First",  "content": "Content A"},
            {"id": "dup-id", "title": "Second", "content": "Content B"}
        ]}"#;
        let path = write_import_json(temp.path(), json);

        let result = handle_memory_import(&ctx, &path, false, false);

        // The function must return Err (UNIQUE constraint propagated via `?`).
        assert!(
            result.is_err(),
            "expected Err from UNIQUE constraint violation"
        );

        // Neither entry must survive the rollback.
        let entry = handle_memory_show(&ctx, "dup-id").unwrap();
        assert!(
            entry.is_none(),
            "rollback must have removed the first insert"
        );
    }
}

// ==================== Experiment Handlers ====================

/// Result of creating an experiment.
#[derive(Debug, Clone)]
pub struct ExperimentCreateResult {
    pub id: i64,
    pub name: String,
}

/// Maximum size for experiment JSON configs (10KB).
const MAX_CONFIG_SIZE: usize = 10_000;

/// Maximum length for experiment names.
const MAX_NAME_LENGTH: usize = 100;

/// Handle `mdkb experiment create`.
pub fn handle_experiment_create(
    ctx: &Context,
    name: &str,
    description: Option<&str>,
    config_a: &str,
    config_b: &str,
    split: f64,
    min_samples: i64,
) -> Result<ExperimentCreateResult> {
    // Validate name length and format
    if name.is_empty() || name.len() > MAX_NAME_LENGTH {
        return Err(Error::from(ErrorKind::Config(format!(
            "Experiment name must be 1-{} characters",
            MAX_NAME_LENGTH
        ))));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::from(ErrorKind::Config(
            "Experiment name must contain only alphanumeric characters, hyphens, and underscores"
                .to_string(),
        )));
    }

    // Validate config sizes before parsing
    if config_a.len() > MAX_CONFIG_SIZE {
        return Err(Error::from(ErrorKind::Config(format!(
            "config-a exceeds maximum size of {} bytes",
            MAX_CONFIG_SIZE
        ))));
    }
    if config_b.len() > MAX_CONFIG_SIZE {
        return Err(Error::from(ErrorKind::Config(format!(
            "config-b exceeds maximum size of {} bytes",
            MAX_CONFIG_SIZE
        ))));
    }

    // Validate JSON configs
    serde_json::from_str::<serde_json::Value>(config_a)
        .map_err(|e| Error::from(ErrorKind::Config(format!("Invalid config-a JSON: {e}"))))?;
    serde_json::from_str::<serde_json::Value>(config_b)
        .map_err(|e| Error::from(ErrorKind::Config(format!("Invalid config-b JSON: {e}"))))?;

    // Validate split
    if !(0.0..=1.0).contains(&split) {
        return Err(Error::from(ErrorKind::Config(
            "Traffic split must be between 0.0 and 1.0".to_string(),
        )));
    }

    // Validate min_samples
    if !(1..=10_000).contains(&min_samples) {
        return Err(Error::from(ErrorKind::Config(
            "min_samples must be between 1 and 10,000".to_string(),
        )));
    }

    // Initialize schema if needed
    stats::init_experiments_schema(&ctx.conn)?;

    let id = stats::create_experiment(
        &ctx.conn,
        name,
        description,
        config_a,
        config_b,
        split,
        min_samples,
    )?;

    Ok(ExperimentCreateResult {
        id,
        name: name.to_string(),
    })
}

/// Handle `mdkb experiment status`.
pub fn handle_experiment_status(
    ctx: &Context,
    name: &str,
) -> Result<Option<stats::ExperimentStatusReport>> {
    stats::init_experiments_schema(&ctx.conn)?;
    stats::get_experiment_status(&ctx.conn, name)
}

/// Handle `mdkb experiment end`.
pub fn handle_experiment_end(
    ctx: &Context,
    name: &str,
    winner: Option<&str>,
) -> Result<Option<String>> {
    stats::init_experiments_schema(&ctx.conn)?;

    // If winner not specified, try to auto-determine from significance
    let actual_winner = if winner.is_some() {
        winner.map(|s| s.to_string())
    } else {
        // Get experiment status and check for significant winner
        match stats::get_experiment_status(&ctx.conn, name)? {
            Some(status) => {
                if let Some(sig) = status.significance {
                    sig.winner
                } else {
                    None
                }
            }
            None => {
                return Err(Error::from(ErrorKind::Config(format!(
                    "Experiment '{name}' not found"
                ))));
            }
        }
    };

    stats::end_experiment(&ctx.conn, name, actual_winner.as_deref())?;

    Ok(actual_winner)
}

/// Handle `mdkb experiment cancel`.
pub fn handle_experiment_cancel(ctx: &Context, name: &str) -> Result<()> {
    stats::init_experiments_schema(&ctx.conn)?;
    stats::cancel_experiment(&ctx.conn, name)
}

/// Handle `mdkb experiment list`.
pub fn handle_experiment_list(ctx: &Context, running_only: bool) -> Result<Vec<stats::Experiment>> {
    stats::init_experiments_schema(&ctx.conn)?;

    if running_only {
        stats::get_active_experiments(&ctx.conn)
    } else {
        stats::list_experiments(&ctx.conn)
    }
}

// ============================================================================
// Journal Import Handlers
// ============================================================================

use crate::cli::journal::{self, JournalImportResult};

/// Handle `mdkb journal import`.
pub fn handle_journal_import(
    ctx: &Context,
    path: &Path,
    dry_run: bool,
) -> Result<JournalImportResult> {
    // Read journal file
    let content = std::fs::read_to_string(path).map_err(|e| Error::from(ErrorKind::IoError(e)))?;

    // Parse journal
    let parsed = journal::parse_journal(&content);

    // Generate base ID from filename
    let base_id = journal::path_to_base_id(path);

    // Convert to memory entries
    let entries = journal::journal_to_memory_entries(&parsed, path, &base_id);

    let mut result = JournalImportResult {
        source_path: path.to_string_lossy().to_string(),
        ..Default::default()
    };

    if entries.is_empty() {
        result
            .skipped
            .push(("No content".to_string(), "entire file".to_string()));
        return Ok(result);
    }

    for entry in entries {
        if dry_run {
            result.created.push(entry.id);
        } else {
            // Check if entry with this ID already exists
            if memory::get_entry_without_tracking(&ctx.conn, &entry.id)?.is_some() {
                result
                    .skipped
                    .push(("Already exists".to_string(), entry.id));
                continue;
            }

            memory::add_entry(&ctx.conn, &entry)?;
            result.created.push(entry.id);
        }
    }

    Ok(result)
}

/// Handle `mdkb journal import-all`.
pub fn handle_journal_import_all(
    ctx: &Context,
    dir: &Path,
    dry_run: bool,
    skip_existing: bool,
) -> Result<Vec<JournalImportResult>> {
    let mut results = Vec::new();

    // Get list of already imported source paths if skip_existing
    let existing_sources: HashSet<String> = if skip_existing && !dry_run {
        memory::list_entries(&ctx.conn, 10000, None)?
            .into_iter()
            .filter_map(|e| e.source_path)
            .collect()
    } else {
        HashSet::new()
    };

    // Walk journal directory
    for entry in WalkDir::new(dir)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
    {
        let path = entry.path();
        let path_str = path.to_string_lossy().to_string();

        // Skip if already imported
        if skip_existing && existing_sources.contains(&path_str) {
            results.push(JournalImportResult {
                source_path: path_str.clone(),
                skipped: vec![("Already imported".to_string(), path_str)],
                ..Default::default()
            });
            continue;
        }

        match handle_journal_import(ctx, path, dry_run) {
            Ok(result) => results.push(result),
            Err(e) => {
                results.push(JournalImportResult {
                    source_path: path_str.clone(),
                    skipped: vec![(format!("Error: {}", e), path_str)],
                    ..Default::default()
                });
            }
        }
    }

    Ok(results)
}

// ---------------------------------------------------------------------------
// Session indexing handlers
// ---------------------------------------------------------------------------

/// Index Claude Code session files into the `claude_sessions` collection.
///
/// Walks `*.jsonl` files in the session directory (non-recursive),
/// parses them into documents with chunking, and indexes the results.
/// Uses mtime-based dedup to skip unchanged files.
pub fn handle_session_index(
    ctx: &Context,
    sessions_path: &Path,
    project_root: &str,
) -> Result<UpdateResult> {
    use crate::domain::sessions::{SessionParseConfig, find_session_dir, parse_session_file};

    let Some(session_dir) = find_session_dir(sessions_path, project_root) else {
        tracing::debug!("No session directory found for {}", project_root);
        return Ok(UpdateResult::default());
    };

    // Ensure claude_sessions collection exists
    let collection_name = crate::domain::COLLECTION_CLAUDE_SESSIONS;
    if collections::get_collection(&ctx.conn, collection_name)?.is_none() {
        let now = chrono::Utc::now().timestamp();
        let coll = Collection {
            name: collection_name.to_string(),
            path: session_dir.to_string_lossy().to_string(),
            pattern: "*.jsonl".to_string(),
            source: crate::domain::COLLECTION_SOURCE_SESSIONS.to_string(),
            created_at: now,
            updated_at: now,
        };
        collections::add_collection(&ctx.conn, &coll)?;
        tracing::info!(
            "Created claude_sessions collection at {}",
            session_dir.display()
        );
    }

    let config = SessionParseConfig::default();
    let mut result = UpdateResult::default();

    // Pre-load existing documents for mtime-based dedup
    let existing_docs: HashMap<String, Document> =
        documents::list_documents(&ctx.conn, collection_name)?
            .into_iter()
            .map(|d| (d.relative_path.clone(), d))
            .collect();

    // Walk .jsonl files (non-recursive)
    let entries: Vec<_> = std::fs::read_dir(&session_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            path.is_file() && path.extension().map(|ext| ext == "jsonl").unwrap_or(false)
        })
        .collect();

    documents::begin_transaction(&ctx.conn)?;

    // Relative paths still produced by live transcript files this pass. Any
    // previously-indexed session doc NOT in this set has lost its source and is
    // archived below.
    let mut present_paths: HashSet<String> = HashSet::new();

    for entry in &entries {
        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        let file_mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let session_docs = match parse_session_file(&path, &config) {
            Ok(docs) => docs,
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to parse {}: {}", file_name, e));
                continue;
            }
        };

        for sdoc in &session_docs {
            present_paths.insert(sdoc.relative_path.clone());
            let existing = existing_docs.get(&sdoc.relative_path);

            // Content-hash dedup (NOT file mtime). A transcript is append-only, so
            // its mtime bumps on every growth while all but the tail chunk keep
            // byte-identical content under a stable `{sid}-chunk-NNN` key. Skipping
            // on file mtime therefore re-embedded the ENTIRE multi-MB file on every
            // append; skipping on the per-chunk content hash re-embeds only the
            // new/changed tail. `documents.hash` is the same SHA-256 the document
            // layer stores, so this is exact, not heuristic.
            if let Some(existing_doc) = existing {
                if existing_doc.hash == documents::compute_hash(&sdoc.content) {
                    result.unchanged += 1;
                    continue;
                }
            }

            let now = chrono::Utc::now().timestamp();
            let doc = Document {
                id: 0,
                collection: collection_name.to_string(),
                relative_path: sdoc.relative_path.clone(),
                hash: String::new(), // computed by index_document_in_tx
                title: Some(sdoc.metadata.session_id.clone()),
                metadata: None,
                file_modified_at: file_mtime,
                indexed_at: now,
                status: None,
            };

            documents::index_document_in_tx(&ctx.conn, &doc, &sdoc.content)?;

            if existing.is_some() {
                result.updated += 1;
            } else {
                result.added += 1;
            }
        }
    }

    // Archive session docs whose source jsonl is gone (deleted/rotated). Kept
    // searchable via explicit --collection claude_sessions; never hard-deleted
    // here (see `mdkb compact --prune-sessions`).
    result.sessions_archived = documents::archive_missing_sessions(&ctx.conn, &present_paths)?;

    documents::commit_transaction(&ctx.conn)?;

    Ok(result)
}

/// Parse a retention duration like `90d`, `12h`, or `2w` into seconds.
pub fn parse_retention_secs(s: &str) -> Result<i64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_alphabetic()).ok_or_else(|| {
        Error::other(format!(
            "Invalid duration '{s}': expected <N><unit> like 90d"
        ))
    })?;
    let (num, unit) = s.split_at(split);
    let n: i64 = num
        .trim()
        .parse()
        .map_err(|_| Error::other(format!("Invalid duration number in '{s}'")))?;
    if n < 0 {
        return Err(Error::other(format!(
            "Duration must be non-negative: '{s}'"
        )));
    }
    let mult = match unit.trim() {
        "d" => 86_400,
        "h" => 3_600,
        "w" => 604_800,
        other => {
            return Err(Error::other(format!(
                "Invalid duration unit '{other}' in '{s}': use d, h, or w"
            )));
        }
    };
    // Checked: a fat-fingered oversized value (extra digits) must NOT wrap to a
    // small/negative number in release builds — that would corrupt the prune
    // cutoff and make a destructive `--prune-sessions` delete far more than
    // intended (SEC-1).
    n.checked_mul(mult).ok_or_else(|| {
        Error::other(format!(
            "Duration '{s}' is too large (overflows). Use a smaller value."
        ))
    })
}

/// Outcome of a `mdkb compact --prune-sessions` run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PruneSessionsSummary {
    pub pruned: usize,
    pub exported: usize,
    pub export_dir: Option<String>,
}

/// Hard-delete archived session documents whose `indexed_at <= cutoff_ts`.
///
/// When `export_dir` is set, each transcript is written as markdown BEFORE it
/// is deleted, so mdkb never silently drops the only remaining copy. Only
/// `archived` sessions (source jsonl already gone) are eligible — current
/// transcripts are untouched.
pub fn handle_prune_sessions(
    ctx: &Context,
    cutoff_ts: i64,
    export_dir: Option<&Path>,
) -> Result<PruneSessionsSummary> {
    let candidates = documents::list_prunable_sessions(&ctx.conn, cutoff_ts)?;
    let mut summary = PruneSessionsSummary {
        pruned: 0,
        exported: 0,
        export_dir: export_dir.map(|p| p.display().to_string()),
    };

    if let Some(dir) = export_dir {
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: dir.to_path_buf(),
                operation: format!("create export dir: {e}"),
            })
        })?;
    }

    let hashes: Vec<&str> = candidates.iter().map(|c| c.hash.as_str()).collect();
    let content_map = documents::get_content_batch(&ctx.conn, &hashes)?;

    for c in &candidates {
        // Export first — a delete before a successful write would lose the only
        // copy. If `--export` is requested but the content body is missing, SKIP
        // the delete (leave the candidate for a future run) rather than silently
        // destroying an unexported transcript (DATA-1).
        if let Some(dir) = export_dir {
            let Some(body) = content_map.get(&c.hash) else {
                tracing::warn!(
                    "prune-sessions: content missing for '{}' (id {}) — skipping delete \
                     to avoid destroying the only copy",
                    c.relative_path,
                    c.id
                );
                continue;
            };
            let safe: String = c
                .relative_path
                .chars()
                .map(|ch| {
                    if ch.is_alphanumeric() || ch == '-' || ch == '_' {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            // Collision-proof: the lossy sanitizer above collapses distinct paths
            // (`a/b.md`, `a_b.md`, `a:b.md`) to the same stem. Suffix with the unique
            // document id (plus a hash prefix for human legibility) so two candidates
            // can never overwrite each other's export — another silent-loss path.
            let hash8 = c.hash.get(..8).unwrap_or(c.hash.as_str());
            let fname = format!("{safe}-{}-{hash8}.md", c.id);
            let path = dir.join(&fname);
            let title = c.title.clone().unwrap_or_else(|| c.relative_path.clone());
            let md = format!("# {title}\n\n{body}\n");
            std::fs::write(&path, md).map_err(|e| {
                Error::from(ErrorKind::Io {
                    path: path.clone(),
                    operation: format!("write export: {e}"),
                })
            })?;
            summary.exported += 1;
        }
        if documents::delete_document(&ctx.conn, c.id)? {
            summary.pruned += 1;
        }
    }
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Code intelligence handlers
// ---------------------------------------------------------------------------

/// Result of `code info` command.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodeInfoResult {
    pub symbols: u64,
    pub files: u64,
    pub relationships: usize,
}

/// Handle `mdkb code init` - initialize code index directory (idempotent).
pub fn handle_code_init(root: &Path) -> Result<()> {
    let index_path = root.join(".mdkb/code.sqlite");
    if index_path.exists() {
        return Ok(());
    }
    crate::code::indexing::IndexFacade::create(&index_path)
        .map_err(|e| Error::other(format!("Failed to create code index: {}", e)))?;
    Ok(())
}

/// Handle `mdkb code index` - build code index from source files.
pub fn handle_code_index(
    root: &Path,
    paths: &[String],
) -> Result<crate::code::indexing::types::IndexStats> {
    let index_path = root.join(".mdkb/code.sqlite");
    let mut facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let result = if paths.is_empty() {
        // Whole-tree refresh: `update` (not `index_directory`) because only a
        // caller that walked everything may prune files deleted from disk, and
        // this is that caller. The per-path branch below is NOT — it indexes one
        // subdirectory at a time, where "indexed but absent" is the rest of the
        // project, not a deletion.
        facade
            .update(root)
            .map_err(|e| Error::other(format!("Indexing failed: {}", e)))
    } else {
        let root_canonical = root
            .canonicalize()
            .map_err(|e| Error::other(format!("Cannot resolve root: {e}")))?;
        let mut total = crate::code::indexing::types::IndexStats::default();
        for p in paths {
            let candidate = root.join(p);
            let canonical = candidate
                .canonicalize()
                .map_err(|e| Error::other(format!("Cannot resolve path '{}': {e}", p)))?;
            if !canonical.starts_with(&root_canonical) {
                return Err(Error::other(format!("Path '{}' escapes project root", p)));
            }
            let stats = facade
                .index_directory(&canonical)
                .map_err(|e| Error::other(format!("Indexing '{}' failed: {}", p, e)))?;
            total.files_discovered += stats.files_discovered;
            total.files_indexed += stats.files_indexed;
            total.files_removed += stats.files_removed;
            total.symbols_indexed += stats.symbols_indexed;
            total.relationships_collected += stats.relationships_collected;
        }
        Ok(total)
    };
    crate::llm::release_cached_service();
    result
}

/// Handle `mdkb code index --force` - full reindex discarding existing data.
pub fn handle_code_reindex(
    root: &Path,
    paths: &[String],
) -> Result<crate::code::indexing::types::IndexStats> {
    let index_path = root.join(".mdkb/code.sqlite");
    let mut facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let target = if paths.is_empty() {
        root.to_path_buf()
    } else {
        let root_canonical = root
            .canonicalize()
            .map_err(|e| Error::other(format!("Cannot resolve root: {e}")))?;
        // Validate all paths, use first (reindex clears DB so multiple passes don't combine)
        for p in paths {
            let candidate = root.join(p);
            let canonical = candidate
                .canonicalize()
                .map_err(|e| Error::other(format!("Cannot resolve path '{}': {e}", p)))?;
            if !canonical.starts_with(&root_canonical) {
                return Err(Error::other(format!("Path '{}' escapes project root", p)));
            }
        }
        // Use first path as reindex target (reindex is a full rebuild)
        root.join(&paths[0])
            .canonicalize()
            .map_err(|e| Error::other(format!("Cannot resolve path '{}': {e}", paths[0])))?
    };

    let result = facade
        .reindex(&target)
        .map_err(|e| Error::other(format!("Reindexing failed: {}", e)));
    crate::llm::release_cached_service();
    result
}

/// Handle `mdkb code search` - fuzzy symbol search.
pub fn handle_code_search(
    root: &Path,
    query: &str,
    limit: usize,
    kind_filter: Option<&str>,
) -> Result<Vec<crate::code::symbol::Symbol>> {
    let index_path = root.join(".mdkb/code.sqlite");
    let facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let mut results = facade.search_symbols(query, limit * 2);

    if let Some(kind_str) = kind_filter {
        let kind: crate::code::types::SymbolKind = kind_str
            .parse()
            .map_err(|_| Error::other(format!("Unknown symbol kind: {}", kind_str)))?;
        results.retain(|s| s.kind == kind);
    }

    results.truncate(limit);
    Ok(results)
}

/// Handle `mdkb code find` - exact symbol lookup.
pub fn handle_code_find(
    root: &Path,
    name: &str,
    kind_filter: Option<&str>,
    file_filter: Option<&str>,
) -> Result<Vec<crate::code::symbol::Symbol>> {
    let index_path = root.join(".mdkb/code.sqlite");
    let facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let mut results = facade.find_symbols_by_name(name);

    if let Some(kind_str) = kind_filter {
        let kind: crate::code::types::SymbolKind = kind_str
            .parse()
            .map_err(|_| Error::other(format!("Unknown symbol kind: {}", kind_str)))?;
        results.retain(|s| s.kind == kind);
    }

    if let Some(file_substr) = file_filter {
        results.retain(|s| s.file_path.contains(file_substr));
    }

    Ok(results)
}

/// Handle `mdkb code calls` - show what a symbol calls.
pub fn handle_code_calls(
    root: &Path,
    name: &str,
) -> Result<(
    crate::code::symbol::Symbol,
    Vec<crate::code::symbol::Symbol>,
)> {
    let index_path = root.join(".mdkb/code.sqlite");
    let facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let callees = facade.get_called_functions(symbol.id);
    Ok((symbol, callees))
}

/// Handle `mdkb code callers` - show what calls a symbol.
pub fn handle_code_callers(
    root: &Path,
    name: &str,
) -> Result<(
    crate::code::symbol::Symbol,
    Vec<crate::code::symbol::Symbol>,
)> {
    let index_path = root.join(".mdkb/code.sqlite");
    let facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let callers = facade.get_calling_functions(symbol.id);
    Ok((symbol, callers))
}

/// Handle `mdkb code impact` - impact analysis from a symbol.
pub fn handle_code_impact(
    root: &Path,
    name: &str,
    depth: usize,
) -> Result<(
    crate::code::symbol::Symbol,
    Vec<crate::code::symbol::Symbol>,
)> {
    let index_path = root.join(".mdkb/code.sqlite");
    let facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let impacted_ids = facade.get_impact_radius(symbol.id, depth);
    let impacted: Vec<_> = impacted_ids
        .iter()
        .filter_map(|&id| facade.get_symbol(id))
        .collect();

    Ok((symbol, impacted))
}

/// Handle `mdkb code info` - show index statistics.
pub fn handle_code_info(root: &Path) -> Result<CodeInfoResult> {
    let index_path = root.join(".mdkb/code.sqlite");
    let facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    Ok(CodeInfoResult {
        symbols: facade.symbol_count(),
        files: facade.file_count(),
        relationships: facade.relationship_count(),
    })
}

/// Handle `mdkb code parse` - parse a single file and return symbols.
pub fn handle_code_parse(file: &Path) -> Result<Vec<crate::code::symbol::Symbol>> {
    use crate::code::parsing::language::Language;
    use crate::code::parsing::parser::LanguageParser;
    use crate::code::types::{FileId, SymbolCounter};

    let language = Language::from_path(file).ok_or_else(|| {
        Error::other(format!("Unsupported language for file: {}", file.display()))
    })?;

    let code = std::fs::read_to_string(file)
        .map_err(|e| Error::other(format!("Failed to read '{}': {}", file.display(), e)))?;

    let file_id = FileId::new(1).expect("1 is valid");
    let mut counter = SymbolCounter::new();

    let mut parser: Box<dyn LanguageParser> = match language {
        Language::Rust => Box::new(
            crate::code::parsing::rust::RustParser::new()
                .map_err(|e| Error::other(format!("Failed to create Rust parser: {}", e)))?,
        ),
        Language::Go => Box::new(
            crate::code::parsing::go::GoParser::new()
                .map_err(|e| Error::other(format!("Failed to create Go parser: {}", e)))?,
        ),
        Language::TypeScript | Language::JavaScript => Box::new(
            crate::code::parsing::typescript::TypeScriptParser::new()
                .map_err(|e| Error::other(format!("Failed to create TypeScript parser: {}", e)))?,
        ),
        Language::Python => Box::new(
            crate::code::parsing::python::PythonParser::new()
                .map_err(|e| Error::other(format!("Failed to create Python parser: {}", e)))?,
        ),
        Language::Java => Box::new(
            crate::code::parsing::java::JavaParser::new()
                .map_err(|e| Error::other(format!("Failed to create Java parser: {}", e)))?,
        ),
        Language::C => Box::new(
            crate::code::parsing::c_lang::CParser::new()
                .map_err(|e| Error::other(format!("Failed to create C parser: {}", e)))?,
        ),
        Language::Cpp => Box::new(
            crate::code::parsing::cpp::CppParser::new()
                .map_err(|e| Error::other(format!("Failed to create C++ parser: {}", e)))?,
        ),
        Language::CSharp => Box::new(
            crate::code::parsing::csharp::CSharpParser::new()
                .map_err(|e| Error::other(format!("Failed to create C# parser: {}", e)))?,
        ),
        Language::Php => Box::new(
            crate::code::parsing::php::PhpParser::new()
                .map_err(|e| Error::other(format!("Failed to create PHP parser: {}", e)))?,
        ),
        Language::Swift => Box::new(
            crate::code::parsing::swift::SwiftParser::new()
                .map_err(|e| Error::other(format!("Failed to create Swift parser: {}", e)))?,
        ),
        Language::Lua => Box::new(
            crate::code::parsing::lua::LuaParser::new()
                .map_err(|e| Error::other(format!("Failed to create Lua parser: {}", e)))?,
        ),
        Language::Gdscript => Box::new(
            crate::code::parsing::gdscript::GdscriptParser::new()
                .map_err(|e| Error::other(format!("Failed to create GDScript parser: {}", e)))?,
        ),
        Language::Kotlin => Box::new(
            crate::code::parsing::kotlin::KotlinParser::new()
                .map_err(|e| Error::other(format!("Failed to create Kotlin parser: {}", e)))?,
        ),
    };

    let symbols = parser.parse(&code, file_id, &mut counter);
    Ok(symbols)
}
