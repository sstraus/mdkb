//! CLI command handlers.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::domain::frontmatter::parse_frontmatter;
use crate::domain::{Collection, Document, SearchQuery, UpdateResult};
#[cfg(feature = "llm")]
use crate::domain::SearchResult;
use crate::error::{Error, Result};
use crate::store::collections;
use crate::store::documents;
use crate::store::schema;
use crate::store::search;
use crate::store::vectors;
#[cfg(feature = "llm")]
use crate::store::hybrid;
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

impl Context {
    /// Open or create context at the given root.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mdkb_dir = root.join(".mdkb");
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");

        if !mdkb_dir.exists() {
            return Err(Error::DatabaseNotFound {
                path: db_path.clone(),
            });
        }

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
}

/// Handle `mdkb init` command.
pub fn handle_init(root: impl AsRef<Path>) -> Result<()> {
    let root = root.as_ref();
    let mdkb_dir = root.join(".mdkb");

    if mdkb_dir.exists() {
        return Err(Error::Other(format!(
            "mdkb already initialized at {}",
            mdkb_dir.display()
        )));
    }

    Context::init(root)?;
    Ok(())
}

/// Handle `mdkb collection add` command.
pub fn handle_collection_add(ctx: &Context, name: &str, path: &str, pattern: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let collection = Collection {
        name: name.to_string(),
        path: path.to_string(),
        pattern: pattern.to_string(),
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
    };

    search::search(&ctx.conn, &query)
}

/// Handle `mdkb vsearch` command - vector semantic search.
#[cfg(feature = "llm")]
pub fn handle_vsearch(
    ctx: &Context,
    query_text: &str,
    limit: usize,
    collection: Option<&str>,
) -> Result<Vec<SearchResult>> {
    use crate::llm::EmbeddingModel;

    // Load embedding model
    let model = EmbeddingModel::load(None, None)?;

    // Generate query embedding
    let query_embedding = model.embed_query(query_text)?;

    // Perform vector search
    let vector_results = vectors::vector_search(&ctx.conn, &query_embedding, limit)?;

    // Convert to SearchResult format
    let mut results = Vec::new();
    for (doc_id, distance) in vector_results {
        if let Some(doc) = documents::get_document(&ctx.conn, doc_id)? {
            // Filter by collection if specified
            if let Some(coll) = collection {
                if doc.collection != coll {
                    continue;
                }
            }

            results.push(SearchResult {
                id: doc.id,
                path: doc.relative_path.clone(),
                title: doc.title.clone(),
                score: 1.0 - distance as f64, // Convert distance to similarity
                snippets: vec![],
            });
        }
    }

    Ok(results)
}

/// Handle `mdkb query` command - hybrid search with RRF fusion.
#[cfg(feature = "llm")]
pub fn handle_hybrid_search(
    ctx: &Context,
    query_text: &str,
    limit: usize,
    collection: Option<&str>,
) -> Result<Vec<SearchResult>> {
    use crate::llm::EmbeddingModel;

    // Get BM25 results
    let bm25_query = SearchQuery {
        text: query_text.to_string(),
        limit: limit * 2, // Get more for fusion
        collection: collection.map(String::from),
        tags: vec![],
    };
    let bm25_results = search::search(&ctx.conn, &bm25_query)?;

    // Load embedding model and get vector results
    let model = EmbeddingModel::load(None, None)?;
    let query_embedding = model.embed_query(query_text)?;
    let vector_results = vectors::vector_search(&ctx.conn, &query_embedding, limit * 2)?;

    // Fuse results using RRF
    let config = hybrid::HybridConfig::default();
    let mut fused = hybrid::rrf_fusion(&bm25_results, &vector_results, &config);

    // Normalize scores
    hybrid::normalize_scores(&mut fused);

    // Convert to SearchResult format
    let mut results = Vec::new();
    for (doc_id, score) in fused.into_iter().take(limit) {
        if let Some(doc) = documents::get_document(&ctx.conn, doc_id)? {
            // Filter by collection if specified
            if let Some(coll) = collection {
                if doc.collection != coll {
                    continue;
                }
            }

            results.push(SearchResult {
                id: doc.id,
                path: doc.relative_path.clone(),
                title: doc.title.clone(),
                score,
                snippets: vec![],
            });
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

    let doc = doc.ok_or_else(|| Error::DocumentNotFound {
        id: id_or_path.to_string(),
    })?;

    // Get content
    let content = documents::get_content(&ctx.conn, &doc.hash)?
        .ok_or_else(|| Error::DocumentNotFound {
            id: id_or_path.to_string(),
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
pub fn handle_mget(
    ctx: &Context,
    pattern: &str,
    collection_filter: Option<&str>,
) -> Result<Vec<(Document, String)>> {
    let glob = Glob::new(pattern)
        .map_err(|e| Error::Other(format!("Invalid glob pattern '{}': {}", pattern, e)))?
        .compile_matcher();

    // Get all documents, optionally filtered by collection
    let all_collections = collections::list_collections(&ctx.conn)?;
    let mut results = Vec::new();

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
            if !glob.is_match(&doc.relative_path) {
                continue;
            }

            // Get content
            if let Some(content) = documents::get_content(&ctx.conn, &doc.hash)? {
                results.push((doc, content));
            }
        }
    }

    Ok(results)
}

/// Handle `mdkb update` command - differential reindex.
pub fn handle_update(ctx: &Context, root: impl AsRef<Path>) -> Result<UpdateResult> {
    let root = root.as_ref();
    let collections = collections::list_collections(&ctx.conn)?;
    let mut result = UpdateResult::default();

    for coll in &collections {
        update_collection(ctx, root, coll, &mut result)?;
    }

    Ok(result)
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

    // Build glob matcher
    let glob = Glob::new(&collection.pattern)
        .map_err(|e| Error::Other(format!("Invalid glob pattern '{}': {}", collection.pattern, e)))?
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
                result.errors.push(format!("Failed to read metadata for {}: {}", path.display(), e));
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
                result.errors.push(format!("Failed to read {}: {}", path.display(), e));
                continue;
            }
        };

        // Parse frontmatter for title
        let parsed = parse_frontmatter(&content);
        let now = chrono::Utc::now().timestamp();

        let doc = Document {
            id: existing_doc.as_ref().map(|d| d.id).unwrap_or(0),
            collection: collection.name.clone(),
            relative_path: relative,
            hash: String::new(), // Will be computed by index_document
            title: parsed.title,
            metadata: parsed.frontmatter,
            file_modified_at: file_mtime,
            indexed_at: now,
        };

        match documents::index_document(&ctx.conn, &doc, &content) {
            Ok(_) => {
                if existing_doc.is_some() {
                    result.updated += 1;
                } else {
                    result.added += 1;
                }
            }
            Err(e) => {
                result.errors.push(format!("Failed to index {}: {}", path.display(), e));
            }
        }
    }

    // Remove documents for deleted files
    for deleted_path in existing_paths {
        if let Some(doc) = documents::get_document_by_path(&ctx.conn, &collection.name, &deleted_path)? {
            match documents::delete_document(&ctx.conn, doc.id) {
                Ok(true) => result.removed += 1,
                Ok(false) => {}
                Err(e) => {
                    result.errors.push(format!("Failed to remove {}: {}", deleted_path, e));
                }
            }
        }
    }

    Ok(())
}

/// Generate embeddings for all documents that don't have them.
/// This is a separate operation that can be run after update.
#[cfg(feature = "llm")]
pub fn handle_embed(ctx: &Context) -> Result<EmbedResult> {
    use crate::llm::EmbeddingModel;

    let mut result = EmbedResult::default();

    // Load embedding model
    let model = EmbeddingModel::load(None, None)?;

    // Get all documents
    let all_collections = collections::list_collections(&ctx.conn)?;

    for coll in &all_collections {
        let docs = documents::list_documents(&ctx.conn, &coll.name)?;

        for doc in docs {
            // Skip if already has embedding
            if vectors::has_embedding(&ctx.conn, doc.id)? {
                result.skipped += 1;
                continue;
            }

            // Get content
            let content = match documents::get_content(&ctx.conn, &doc.hash)? {
                Some(c) => c,
                None => {
                    result.errors.push(format!("No content for doc {}", doc.id));
                    continue;
                }
            };

            // Generate embedding
            match model.embed(&content) {
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
#[cfg(feature = "llm")]
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
        return Err(Error::Other(format!(
            "Invalid line range format: '{}', expected 'start:end'",
            range
        )));
    }

    let start: usize = parts[0]
        .parse()
        .map_err(|_| Error::Other(format!("Invalid start line: '{}'", parts[0])))?;
    let end: usize = parts[1]
        .parse()
        .map_err(|_| Error::Other(format!("Invalid end line: '{}'", parts[1])))?;

    if start == 0 {
        return Err(Error::Other("Line numbers start at 1".to_string()));
    }
    if end < start {
        return Err(Error::Other(format!(
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

        handle_collection_add(&ctx, "docs", "./docs", "**/*.md")
            .expect("add should succeed");

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
}
