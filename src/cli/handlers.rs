//! CLI command handlers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::domain::frontmatter::{parse_frontmatter, ParsedDocument};
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
use walkdir::WalkDir;

/// Context for CLI operations.
pub struct Context {
    /// Database connection.
    pub conn: Connection,
    /// Config path.
    pub config_path: PathBuf,
    /// Database path.
    pub db_path: PathBuf,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("config_path", &self.config_path)
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

impl Context {
    /// Open or create context at the given root.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mdkb_dir = root.join(".mdkb");
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");

        if !mdkb_dir.exists() {
            return Err(ErrorKind::DatabaseNotFound {
                path: db_path.clone(),
            }
            .into());
        }

        // Initialize sqlite-vec extension before opening connection
        vectors::init_sqlite_vec();

        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        Ok(Self {
            conn,
            config_path,
            db_path,
        })
    }

    /// Initialize a new mdkb directory.
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mdkb_dir = root.join(".mdkb");
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");

        // Create directory if needed
        if !mdkb_dir.exists() {
            std::fs::create_dir_all(&mdkb_dir)?;
        }

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
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        schema::init_schema(&conn)?;
        vectors::init_vector_schema(&conn)?;

        Ok(Self {
            conn,
            config_path,
            db_path,
        })
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
    Ok(())
}

/// Handle `mdkb collection add` command.
///
/// Validates that the collection path doesn't escape the root directory
/// to prevent path traversal attacks.
pub fn handle_collection_add(ctx: &Context, name: &str, path: &str, pattern: &str) -> Result<()> {
    // Validate path doesn't contain traversal patterns (fixes P2-SEC-001)
    if path.contains("..") {
        return Err(Error::other(format!(
            "Collection path '{}' contains path traversal pattern '..'",
            path
        )));
    }

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

/// Handle `mdkb collection list` command.
pub fn handle_collection_list(ctx: &Context) -> Result<Vec<Collection>> {
    collections::list_collections(&ctx.conn)
}

/// Handle `mdkb collection rename` command.
pub fn handle_collection_rename(ctx: &Context, old_name: &str, new_name: &str) -> Result<()> {
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
    // Use cached model to avoid reloading (fixes P1-PERF-003)
    let model = crate::llm::get_cached_model()?;

    // Generate query embedding
    let query_embedding = model.embed_query(query_text)?;

    // Perform vector search - get more results to account for collection filtering
    let fetch_limit = if collection.is_some() { limit * 2 } else { limit };
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
            // Filter by collection if specified
            if let Some(coll) = collection {
                if doc.collection != coll {
                    continue;
                }
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
    // Get BM25 results
    let bm25_query = SearchQuery {
        text: query_text.to_string(),
        limit: limit * 2, // Get more for fusion
        collection: collection.map(String::from),
        tags: vec![],
        include_superseded,
    };
    let bm25_results = search::search(&ctx.conn, &bm25_query)?;

    // Use cached model to avoid reloading (fixes P1-PERF-003)
    let model = crate::llm::get_cached_model()?;
    let query_embedding = model.embed_query(query_text)?;
    let vector_results = vectors::vector_search(&ctx.conn, &query_embedding, limit * 2)?;

    // Fuse results using RRF
    let config = hybrid::HybridConfig::default();
    let mut fused = hybrid::rrf_fusion(&bm25_results, &vector_results, &config);

    // Normalize scores
    hybrid::normalize_scores(&mut fused);

    if fused.is_empty() {
        return Ok(Vec::new());
    }

    // Batch retrieve all documents in a single query (fixes N+1 query pattern)
    let doc_ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
    let docs = documents::get_documents_batch(&ctx.conn, &doc_ids)?;

    // Build a map for quick lookup
    let doc_map: std::collections::HashMap<i64, _> = docs.into_iter().map(|d| (d.id, d)).collect();

    // Convert to SearchResult format, preserving RRF order
    let mut results = Vec::new();
    for (doc_id, score) in fused {
        if let Some(doc) = doc_map.get(&doc_id) {
            // Filter by collection if specified
            if let Some(coll) = collection {
                if doc.collection != coll {
                    continue;
                }
            }

            results.push(SearchResult {
                id: doc.id,
                collection: doc.collection.clone(),
                path: doc.relative_path.clone(),
                title: doc.title.clone(),
                score,
                snippets: vec![],
                status: None, // Will be populated from BM25 results if available
                superseded_by: None,
            });

            // Stop once we have enough results
            if results.len() >= limit {
                break;
            }
        }
    }

    Ok(results)
}

/// Handle `mdkb status` command.
pub fn handle_status(ctx: &Context) -> Result<crate::domain::IndexStatus> {
    search::get_status(&ctx.conn)
}

/// Handle `mdkb get` command.
pub fn handle_get(
    ctx: &Context,
    id_or_path: &str,
    lines: Option<&str>,
) -> Result<(crate::domain::Document, String)> {
    // Try to parse as ID first
    let doc = if let Ok(id) = id_or_path.parse::<i64>() {
        documents::get_document(&ctx.conn, id)?
    } else {
        // Try as path - search for it
        None
    };

    let doc = doc.ok_or_else(|| {
        Error::from(ErrorKind::DocumentNotFound {
            id: id_or_path.to_string(),
        })
    })?;

    // Get content
    let content = documents::get_content(&ctx.conn, &doc.hash)?.ok_or_else(|| {
        Error::from(ErrorKind::DocumentNotFound {
            id: id_or_path.to_string(),
        })
    })?;

    // Apply line range if specified
    let content = if let Some(range) = lines {
        apply_line_range(&content, range)?
    } else {
        content
    };

    Ok((doc, content))
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
    let root = root.as_ref();

    // Detect and register convention-based collections before processing
    apply_conventions(ctx, root)?;

    let collections = collections::list_collections(&ctx.conn)?;
    let mut result = UpdateResult::default();

    // Begin transaction for all updates
    documents::begin_transaction(&ctx.conn)?;

    match update_all_collections(ctx, root, &collections, &mut result) {
        Ok(()) => {
            documents::commit_transaction(&ctx.conn)?;
            Ok(result)
        }
        Err(e) => {
            // Rollback on error
            let _ = documents::rollback_transaction(&ctx.conn);
            Err(e)
        }
    }
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
    collections: &[Collection],
    result: &mut UpdateResult,
) -> Result<()> {
    for coll in collections {
        update_collection(ctx, root, coll, result)?;
    }
    Ok(())
}

/// Update a single collection by scanning for file changes.
fn update_collection(
    ctx: &Context,
    root: &Path,
    collection: &Collection,
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

    // Validate path stays within root to prevent path traversal (fixes P2-SEC-001)
    let canonical_root = root.canonicalize().map_err(|e| {
        Error::other(format!("Failed to canonicalize root path: {}", e))
    })?;
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

    // Build glob matcher
    let glob = Glob::new(&collection.pattern)
        .map_err(|e| {
            Error::other(format!(
                "Invalid glob pattern '{}': {}",
                collection.pattern, e
            ))
        })?
        .compile_matcher();

    // Get existing documents for this collection
    let existing_docs = documents::list_documents(&ctx.conn, &collection.name)?;
    let mut existing_paths: HashSet<String> = existing_docs
        .iter()
        .map(|d| d.relative_path.clone())
        .collect();

    // Walk directory and process files
    for entry in WalkDir::new(&base_path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let relative = match path.strip_prefix(&base_path) {
            Ok(rel) => rel.to_string_lossy().to_string(),
            Err(_) => continue,
        };

        // Check if file matches glob pattern
        if !glob.is_match(&relative) {
            continue;
        }

        // Remove from existing set (to track deletions)
        existing_paths.remove(&relative);

        // Get file modification time
        let metadata = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(e) => {
                result.errors.push(format!(
                    "Failed to read metadata for {}: {}",
                    path.display(),
                    e
                ));
                continue;
            }
        };

        let file_mtime = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Check if document exists and compare mtime
        let existing_doc = documents::get_document_by_path(&ctx.conn, &collection.name, &relative)?;

        let needs_index = match &existing_doc {
            Some(doc) => file_mtime > doc.indexed_at,
            None => true,
        };

        if !needs_index {
            result.unchanged += 1;
            continue;
        }

        // Read and index the file
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to read {}: {}", path.display(), e));
                continue;
            }
        };

        // Parse frontmatter for title and evolution
        let parsed = parse_frontmatter(&content);
        let now = chrono::Utc::now().timestamp();

        let doc = Document {
            id: existing_doc.as_ref().map(|d| d.id).unwrap_or(0),
            collection: collection.name.clone(),
            relative_path: relative,
            hash: String::new(), // Will be computed by index_document
            title: parsed.title.clone(),
            metadata: parsed.frontmatter.clone(),
            file_modified_at: file_mtime,
            indexed_at: now,
        };

        // Use in-transaction version since we're inside a transaction
        match documents::index_document_in_tx(&ctx.conn, &doc, &content) {
            Ok(doc_id) => {
                if existing_doc.is_some() {
                    result.updated += 1;
                } else {
                    result.added += 1;
                }

                // Process evolution references from frontmatter
                if has_evolution_refs(&parsed) {
                    process_frontmatter_evolution(
                        &ctx.conn,
                        doc_id,
                        &collection.name,
                        &parsed,
                    );
                }
            }
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to index {}: {}", path.display(), e));
            }
        }
    }

    // Remove documents for deleted files
    for deleted_path in existing_paths {
        if let Some(doc) =
            documents::get_document_by_path(&ctx.conn, &collection.name, &deleted_path)?
        {
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

/// Generate embeddings for all documents that don't have them.
/// This is a separate operation that can be run after update.
///
/// Uses batch content retrieval to avoid N+1 queries.
/// Uses cached model to avoid 2-5 second load time per request.
pub fn handle_embed(ctx: &Context) -> Result<EmbedResult> {
    let mut result = EmbedResult::default();

    // Use cached model to avoid reloading (fixes P1-PERF-003)
    let model = crate::llm::get_cached_model()?;

    // Get all documents
    let all_collections = collections::list_collections(&ctx.conn)?;

    for coll in &all_collections {
        let docs = documents::list_documents(&ctx.conn, &coll.name)?;

        // Filter to docs that need embedding
        let mut docs_needing_embedding = Vec::new();
        for doc in docs {
            if vectors::has_embedding(&ctx.conn, doc.id)? {
                result.skipped += 1;
            } else {
                docs_needing_embedding.push(doc);
            }
        }

        if docs_needing_embedding.is_empty() {
            continue;
        }

        // Batch retrieve all content in a single query (fixes N+1 query pattern)
        let hashes: Vec<&str> = docs_needing_embedding.iter().map(|d| d.hash.as_str()).collect();
        let content_map = documents::get_content_batch(&ctx.conn, &hashes)?;

        for doc in docs_needing_embedding {
            // Get content from batch result
            let Some(content) = content_map.get(&doc.hash) else {
                result.errors.push(format!("No content for doc {}", doc.id));
                continue;
            };

            // Generate embedding
            match model.embed(content) {
                Ok(embedding) => {
                    vectors::store_embedding(
                        &ctx.conn,
                        doc.id,
                        &embedding,
                        crate::llm::embeddings::DEFAULT_EMBEDDING_REPO,
                    )?;
                    result.generated += 1;
                }
                Err(e) => {
                    result.errors.push(format!(
                        "Failed to embed doc {} ({}): {}",
                        doc.id, doc.relative_path, e
                    ));
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

/// Stats result from handle_stats.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct StatsResult {
    /// Aggregate stats across all sessions.
    pub aggregate: AggregateStats,
    /// Recent sessions.
    pub sessions: Vec<SessionStats>,
}

/// Aggregate stats.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AggregateStats {
    pub total_sessions: i64,
    pub total_calls: i64,
    pub total_tokens: i64,
    pub total_truncations: i64,
    pub avg_tokens_per_call: f64,
}

/// Session stats.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionStats {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub total_calls: i64,
    pub total_tokens: i64,
    pub truncation_count: i64,
    pub tool_usage: Vec<ToolUsageStats>,
}

/// Tool usage within a session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolUsageStats {
    pub tool_name: String,
    pub call_count: i64,
    pub total_tokens: i64,
    pub total_results: i64,
}

/// Handle stats command.
pub fn handle_stats(ctx: &Context, sessions: usize, aggregate_only: bool) -> Result<StatsResult> {
    // Initialize schema if needed
    stats::init_stats_schema(&ctx.conn)?;

    let agg = stats::get_aggregate_stats(&ctx.conn)?;
    let aggregate = AggregateStats {
        total_sessions: agg.total_sessions,
        total_calls: agg.total_calls,
        total_tokens: agg.total_tokens,
        total_truncations: agg.total_truncations,
        avg_tokens_per_call: agg.avg_tokens_per_call,
    };

    let sessions_list = if aggregate_only {
        vec![]
    } else {
        let recent = stats::get_recent_sessions(&ctx.conn, sessions)?;
        recent
            .into_iter()
            .map(|s| {
                let tool_usage = stats::get_tool_usage(&ctx.conn, s.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|t| ToolUsageStats {
                        tool_name: t.tool_name,
                        call_count: t.call_count,
                        total_tokens: t.total_tokens,
                        total_results: t.total_results,
                    })
                    .collect();

                SessionStats {
                    id: s.id,
                    started_at: s.started_at,
                    ended_at: s.ended_at,
                    total_calls: s.total_calls,
                    total_tokens: s.total_tokens,
                    truncation_count: s.truncation_count,
                    tool_usage,
                }
            })
            .collect()
    };

    Ok(StatsResult {
        aggregate,
        sessions: sessions_list,
    })
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

use crate::store::memory::{self, EntryStatus, EntryType, MemoryEntry};
use crate::store::evolution::{self, Evolution, RelationshipType};

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
fn save_entry_to_disk(ctx: &Context, entry: &MemoryEntry) -> Result<()> {
    let entry_path = ctx.memory_dir().join("entries").join(format!("{}.md", entry.id));

    let mut content = String::new();
    content.push_str("---\n");
    content.push_str(&format!("id: {}\n", entry.id));
    content.push_str(&format!("title: {}\n", entry.title));
    content.push_str(&format!("type: {}\n", entry.entry_type));
    content.push_str(&format!("tags: [{}]\n", entry.tags.join(", ")));
    content.push_str(&format!("status: {}\n", entry.status));
    content.push_str("---\n\n");
    content.push_str(&entry.content);

    std::fs::write(entry_path, content)?;
    Ok(())
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

/// Maximum number of tags per memory entry.
const MAX_TAGS: usize = 20;

/// Maximum length of a single tag.
const MAX_TAG_LENGTH: usize = 50;

/// Maximum content size for memory entries (100KB).
const MAX_CONTENT_SIZE: usize = 100_000;

/// Handle `mdkb memory add` command.
pub fn handle_memory_add(
    ctx: &Context,
    id: &str,
    title: &str,
    entry_type: &str,
    tags: Option<&str>,
    content: &str,
) -> Result<()> {
    // Validate content size
    if content.len() > MAX_CONTENT_SIZE {
        return Err(Error::from(ErrorKind::Config(
            format!("Content exceeds maximum size of {} bytes", MAX_CONTENT_SIZE)
        )));
    }

    let entry_type: EntryType = entry_type
        .parse()
        .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))?;

    let tags: Vec<String> = tags
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    // Validate tag count
    if tags.len() > MAX_TAGS {
        return Err(Error::from(ErrorKind::Config(
            format!("Too many tags (max {})", MAX_TAGS)
        )));
    }

    // Validate individual tag lengths
    for tag in &tags {
        if tag.len() > MAX_TAG_LENGTH {
            return Err(Error::from(ErrorKind::Config(
                format!("Tag '{}' exceeds maximum length of {} characters",
                    tag.chars().take(20).collect::<String>(), MAX_TAG_LENGTH)
            )));
        }
    }

    let now = chrono::Utc::now().timestamp();

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
        source_path: None,
    };

    memory::add_entry(&ctx.conn, &entry)?;

    // Save to disk and regenerate index
    let _ = save_entry_to_disk(ctx, &entry);
    let _ = generate_memory_index(ctx);

    Ok(())
}

/// Handle `mdkb memory show` command.
pub fn handle_memory_show(ctx: &Context, id: &str) -> Result<Option<MemoryEntry>> {
    memory::get_entry(&ctx.conn, id)
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

/// Handle `mdkb memory rm` command.
pub fn handle_memory_rm(ctx: &Context, id: &str) -> Result<bool> {
    let deleted = memory::delete_entry(&ctx.conn, id)?;
    if deleted {
        // Archive from disk and regenerate index
        let _ = archive_entry_on_disk(ctx, id);
        let _ = generate_memory_index(ctx);
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
            let _ = archive_entry_on_disk(ctx, id);
        }
        let _ = generate_memory_index(ctx);
    }
    Ok(pruned)
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
    use std::collections::HashMap;

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
    sorted_tags.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    for (tag, tag_entries) in sorted_tags {
        // Skip if not enough entries
        if tag_entries.len() < min_entries {
            continue;
        }

        // Filter out already-processed entries
        let available: Vec<_> = tag_entries
            .iter()
            .filter(|e| !processed_ids.contains(&e.id))
            .cloned()
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
pub fn generate_consolidated_content(
    entries: &[memory::MemoryEntry],
) -> Result<(String, String)> {
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
        let entry_content = entry.content.lines().skip_while(|l| l.starts_with('#')).collect::<Vec<_>>().join("\n");
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
            .filter_map(|id| memory::get_entry_without_tracking(&ctx.conn, id).ok().flatten())
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
                entry_type: entries.first()
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
            };

            // Use transaction to ensure atomicity - either all changes succeed or none
            documents::begin_transaction(&ctx.conn)?;
            match (|| -> Result<()> {
                memory::add_entry(&ctx.conn, &merged_entry)?;

                // Mark original entries as superseded
                for entry in &entries {
                    let mut updated = entry.clone();
                    updated.status = memory::EntryStatus::Superseded;
                    updated.superseded_by = Some(group.proposed_id.clone());
                    memory::update_entry(&ctx.conn, &updated)?;
                }
                Ok(())
            })() {
                Ok(()) => {
                    documents::commit_transaction(&ctx.conn)?;
                    result.merged_count += 1;
                    result.consolidated_count += entries.len();
                }
                Err(e) => {
                    let _ = documents::rollback_transaction(&ctx.conn);
                    return Err(e);
                }
            }
        }

        result.groups.push(group);
    }

    // Regenerate index if changes were made
    if !dry_run && result.merged_count > 0 {
        let _ = generate_memory_index(ctx);
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
            let _ = evolution::add_evolution(
                conn,
                source_doc_id,
                target_id,
                RelationshipType::Supersedes,
                None,
                evo_ref.reason.as_deref(),
            );
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
            let _ = evolution::add_evolution(
                conn,
                source_doc_id,
                target_id,
                RelationshipType::Updates,
                evo_ref.scope.as_deref(),
                evo_ref.reason.as_deref(),
            );
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
            let _ = evolution::add_evolution(
                conn,
                source_doc_id,
                target_id,
                RelationshipType::Corrects,
                None,
                evo_ref.reason.as_deref(),
            );
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
            let _ = evolution::add_evolution(
                conn,
                source_doc_id,
                target_id,
                RelationshipType::Extends,
                None,
                evo_ref.reason.as_deref(),
            );
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
        let supersession = superseded_by.iter().find(|e| {
            e.relationship == RelationshipType::Supersedes
        });

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
        ).expect("add memory should succeed");

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
    fn test_memory_rm_updates_index_json() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).expect("init should succeed");
        let ctx = Context::open(temp.path()).expect("open should succeed");

        // Add an entry
        handle_memory_add(&ctx, "to-delete", "To Delete", "topic", None, "Content").unwrap();

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
    fn test_handle_collection_add_duplicate_fails() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md").unwrap();
        let result = handle_collection_add(&ctx, "docs", "./other", "**/*.md");

        assert!(result.is_err());
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

    // ==================== Status Tests ====================

    #[test]
    fn test_handle_status() {
        let temp = setup_temp_dir();
        handle_init(temp.path()).unwrap();
        let ctx = Context::open(temp.path()).unwrap();

        let status = handle_status(&ctx).expect("status should succeed");
        assert_eq!(status.collections, 0);
        assert_eq!(status.documents, 0);
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
        assert!(result.is_ok(), "update should succeed even with invalid reference");

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
        std::fs::write(docs_dir.join("old.md"), "---\ntitle: Old\n---\n\nOld content.").unwrap();

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
        return Err(Error::from(ErrorKind::Config(
            format!("Experiment name must be 1-{} characters", MAX_NAME_LENGTH)
        )));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return Err(Error::from(ErrorKind::Config(
            "Experiment name must contain only alphanumeric characters, hyphens, and underscores".to_string()
        )));
    }

    // Validate config sizes before parsing
    if config_a.len() > MAX_CONFIG_SIZE {
        return Err(Error::from(ErrorKind::Config(
            format!("config-a exceeds maximum size of {} bytes", MAX_CONFIG_SIZE)
        )));
    }
    if config_b.len() > MAX_CONFIG_SIZE {
        return Err(Error::from(ErrorKind::Config(
            format!("config-b exceeds maximum size of {} bytes", MAX_CONFIG_SIZE)
        )));
    }

    // Validate JSON configs
    serde_json::from_str::<serde_json::Value>(config_a)
        .map_err(|e| Error::from(ErrorKind::Config(format!("Invalid config-a JSON: {e}"))))?;
    serde_json::from_str::<serde_json::Value>(config_b)
        .map_err(|e| Error::from(ErrorKind::Config(format!("Invalid config-b JSON: {e}"))))?;

    // Validate split
    if !(0.0..=1.0).contains(&split) {
        return Err(Error::from(ErrorKind::Config(
            "Traffic split must be between 0.0 and 1.0".to_string()
        )));
    }

    // Validate min_samples
    if min_samples < 1 || min_samples > 10_000 {
        return Err(Error::from(ErrorKind::Config(
            "min_samples must be between 1 and 10,000".to_string()
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
            None => return Err(Error::from(ErrorKind::Config(format!("Experiment '{name}' not found")))),
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
    let content = std::fs::read_to_string(path)
        .map_err(|e| Error::from(ErrorKind::IoError(e)))?;

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
        result.skipped.push(("No content".to_string(), "entire file".to_string()));
        return Ok(result);
    }

    for entry in entries {
        if dry_run {
            result.created.push(entry.id);
        } else {
            // Check if entry with this ID already exists
            if memory::get_entry_without_tracking(&ctx.conn, &entry.id)?.is_some() {
                result.skipped.push(("Already exists".to_string(), entry.id));
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
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "md"))
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
