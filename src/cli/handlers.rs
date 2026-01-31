//! CLI command handlers.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::domain::{Collection, SearchQuery};
use crate::error::{Error, Result};
use crate::store::collections;
use crate::store::documents;
use crate::store::schema;
use crate::store::search;
use rusqlite::Connection;

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

        // Create and initialize database
        let conn = Connection::open(&db_path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        schema::init_schema(&conn)?;

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
}
