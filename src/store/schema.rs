//! Database schema management and migrations.

use crate::error::Result;
use rusqlite::Connection;

/// Current schema version.
pub const SCHEMA_VERSION: i32 = 1;

/// SQL for creating the database schema.
const SCHEMA_SQL: &str = r#"
-- Schema version for migrations
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY
);

-- Collections configuration
CREATE TABLE IF NOT EXISTS collections (
    name TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    pattern TEXT DEFAULT '**/*.md',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Content-addressable storage (deduplication)
CREATE TABLE IF NOT EXISTS content (
    hash TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Documents (file system mapping)
CREATE TABLE IF NOT EXISTS documents (
    id INTEGER PRIMARY KEY,
    collection TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    hash TEXT NOT NULL,
    title TEXT,
    metadata TEXT,
    file_modified_at INTEGER NOT NULL,
    indexed_at INTEGER NOT NULL,
    FOREIGN KEY(collection) REFERENCES collections(name) ON DELETE CASCADE,
    FOREIGN KEY(hash) REFERENCES content(hash),
    UNIQUE(collection, relative_path)
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_documents_collection ON documents(collection);
CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(hash);
CREATE INDEX IF NOT EXISTS idx_documents_path ON documents(relative_path);

-- Full-text search index with porter stemmer + column weighting
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    title,
    body,
    tokenize = 'porter unicode61',
    content='',
    content_rowid='id'
);

-- Trigger to keep FTS in sync on INSERT
CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN
    INSERT INTO documents_fts(rowid, title, body)
    SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
END;

-- Trigger to keep FTS in sync on DELETE
CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, title, body)
    VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
END;

-- Trigger to keep FTS in sync on UPDATE
CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, title, body)
    VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
    INSERT INTO documents_fts(rowid, title, body)
    SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
END;

-- Memory entries for AI knowledge persistence
CREATE TABLE IF NOT EXISTS memory_entries (
    id TEXT PRIMARY KEY,              -- slug: "auth-oauth2-flow"
    title TEXT NOT NULL,              -- Concise title (max 50 chars)
    content TEXT NOT NULL,            -- Full markdown content
    entry_type TEXT NOT NULL,         -- topic, problem, decision
    tags TEXT NOT NULL DEFAULT '[]',  -- JSON array: ["auth", "security"]
    status TEXT DEFAULT 'active',     -- active, superseded, archived
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    superseded_by TEXT,               -- ID of newer entry
    access_count INTEGER DEFAULT 0,   -- Track usage for ranking
    last_accessed INTEGER
);

CREATE INDEX IF NOT EXISTS idx_memory_type ON memory_entries(entry_type);
CREATE INDEX IF NOT EXISTS idx_memory_status ON memory_entries(status);
CREATE INDEX IF NOT EXISTS idx_memory_access ON memory_entries(access_count DESC);

-- FTS for memory content search
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    id,
    title,
    content,
    tokenize = 'porter unicode61',
    content='',
    content_rowid='rowid'
);

-- Triggers to keep memory FTS in sync
CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory_entries BEGIN
    INSERT INTO memory_fts(rowid, id, title, content)
    VALUES ((SELECT rowid FROM memory_entries WHERE id = NEW.id), NEW.id, NEW.title, NEW.content);
END;

CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, id, title, content)
    VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content);
END;

CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, id, title, content)
    VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content);
    INSERT INTO memory_fts(rowid, id, title, content)
    VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content);
END;
"#;

/// SQL for setting BM25 column weights (title 10x, body 1x).
const BM25_WEIGHTS_SQL: &str = r#"
INSERT OR REPLACE INTO documents_fts(documents_fts, rank) VALUES('rank', 'bm25(10.0, 1.0)');
"#;

/// Initialize the database schema.
pub fn init_schema(conn: &Connection) -> Result<()> {
    // Enable foreign keys
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    // Create schema
    conn.execute_batch(SCHEMA_SQL)?;

    // Set BM25 weights
    conn.execute_batch(BM25_WEIGHTS_SQL)?;

    // Set schema version if not set
    let current = get_schema_version(conn)?;
    if current.is_none() {
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?)",
            [SCHEMA_VERSION],
        )?;
    }

    Ok(())
}

/// Get the current schema version from the database.
pub fn get_schema_version(conn: &Connection) -> Result<Option<i32>> {
    // Check if table exists first
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);

    if !exists {
        return Ok(None);
    }

    let version: Option<i32> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .ok();

    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        Connection::open_in_memory().expect("failed to open in-memory db")
    }

    // ==================== Schema Version Tests ====================

    #[test]
    fn test_init_schema_creates_version_table() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // schema_version table should exist
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "schema_version table should exist");
    }

    #[test]
    fn test_init_schema_sets_version() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let version = get_schema_version(&conn)
            .expect("get_schema_version failed")
            .expect("version should be set");
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn test_init_schema_idempotent() {
        let conn = setup_db();
        init_schema(&conn).expect("first init_schema failed");
        init_schema(&conn).expect("second init_schema should succeed (idempotent)");

        let version = get_schema_version(&conn)
            .expect("get_schema_version failed")
            .expect("version should be set");
        assert_eq!(version, SCHEMA_VERSION);
    }

    // ==================== Collections Table Tests ====================

    #[test]
    fn test_collections_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='collections'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "collections table should exist");
    }

    #[test]
    fn test_collections_table_columns() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Insert a collection to verify columns
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        )
        .expect("insert into collections should work");

        let (name, path, pattern): (String, String, String) = conn
            .query_row(
                "SELECT name, path, pattern FROM collections WHERE name = ?",
                ["docs"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query collections should work");

        assert_eq!(name, "docs");
        assert_eq!(path, "./docs");
        assert_eq!(pattern, "**/*.md");
    }

    #[test]
    fn test_collections_name_is_primary_key() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        )
        .expect("first insert should work");

        // Second insert with same name should fail
        let result = conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./other", "**/*.md", "1706700000", "1706700000"],
        );
        assert!(result.is_err(), "duplicate collection name should fail");
    }

    // ==================== Content Table Tests ====================

    #[test]
    fn test_content_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "content table should exist");
    }

    #[test]
    fn test_content_deduplication_by_hash() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let hash = "abc123def456";
        let body = "# Hello World\n\nThis is content.";

        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            [hash, body, "1706700000"],
        )
        .expect("first insert should work");

        // Same hash should fail (content deduplication)
        let result = conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            [hash, "different body", "1706700000"],
        );
        assert!(
            result.is_err(),
            "duplicate hash should fail (deduplication)"
        );
    }

    // ==================== Documents Table Tests ====================

    #[test]
    fn test_documents_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='documents'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "documents table should exist");
    }

    #[test]
    fn test_documents_table_columns() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up collection and content first
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        )
        .expect("insert collection");

        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash123", "# Test\n\nBody content", "1706700000"],
        )
        .expect("insert content");

        // Insert document
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, metadata, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            ["docs", "readme.md", "hash123", "Test", r#"{"tags": ["test"]}"#, "1706700000", "1706700000"],
        )
        .expect("insert document should work");

        let (id, title, metadata): (i64, String, String) = conn
            .query_row(
                "SELECT id, title, metadata FROM documents WHERE relative_path = ?",
                ["readme.md"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query document");

        assert!(id > 0);
        assert_eq!(title, "Test");
        assert!(metadata.contains("tags"));
    }

    #[test]
    fn test_documents_unique_path_per_collection() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "body1", "1706700000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash2", "body2", "1706700000"],
        )
        .unwrap();

        // First document
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "readme.md", "hash1", "Test 1", "1706700000", "1706700000"],
        )
        .expect("first document");

        // Same path in same collection should fail
        let result = conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "readme.md", "hash2", "Test 2", "1706700000", "1706700000"],
        );
        assert!(
            result.is_err(),
            "duplicate path in same collection should fail"
        );
    }

    #[test]
    fn test_documents_foreign_key_to_collection() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "body", "1706700000"],
        )
        .unwrap();

        // Insert document with non-existent collection should fail
        let result = conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["nonexistent", "readme.md", "hash1", "Test", "1706700000", "1706700000"],
        );
        assert!(
            result.is_err(),
            "foreign key constraint should prevent invalid collection"
        );
    }

    // ==================== FTS5 Tests ====================

    #[test]
    fn test_fts5_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='documents_fts'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "documents_fts FTS5 table should exist");
    }

    #[test]
    fn test_fts5_uses_porter_stemmer() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Check FTS5 table SQL contains porter tokenizer
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='documents_fts'",
                [],
                |row| row.get(0),
            )
            .expect("should get FTS table SQL");

        assert!(
            sql.to_lowercase().contains("porter"),
            "FTS5 should use porter stemmer, got: {sql}"
        );
    }

    #[test]
    fn test_fts5_search_with_stemming() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up test data
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            [
                "hash1",
                "The runners are running quickly through the park",
                "1706700000",
            ],
        )
        .unwrap();

        // Insert document (should trigger FTS update via trigger)
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "test.md", "hash1", "Running Test", "1706700000", "1706700000"],
        ).unwrap();

        // Search for "run" should match "runners" and "running" due to stemming
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM documents_fts WHERE documents_fts MATCH 'run'",
                [],
                |row| row.get(0),
            )
            .expect("FTS search should work");

        assert_eq!(
            count, 1,
            "stemmed search for 'run' should find document with 'running'"
        );
    }

    #[test]
    fn test_fts5_bm25_ranking() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up test data
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();

        // Doc 1: "rust" in title only
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "This is about programming.", "1706700000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "doc1.md", "hash1", "Rust Guide", "1706700000", "1706700000"],
        ).unwrap();

        // Doc 2: "rust" in body only
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            [
                "hash2",
                "Rust is a systems programming language.",
                "1706700000",
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "doc2.md", "hash2", "Programming Languages", "1706700000", "1706700000"],
        ).unwrap();

        // Search with BM25 ranking - title match should rank higher (10x weight)
        let results: Vec<(i64, f64)> = conn
            .prepare("SELECT rowid, bm25(documents_fts) FROM documents_fts WHERE documents_fts MATCH 'rust' ORDER BY bm25(documents_fts)")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert_eq!(results.len(), 2, "should find 2 documents");
        // First result should have better (more negative) BM25 score
        // BM25 returns negative scores, more negative = better match
        assert!(
            results[0].1 < results[1].1,
            "title match (doc1) should rank higher than body match (doc2)"
        );
    }

    // ==================== Trigger Tests ====================

    #[test]
    fn test_fts_insert_trigger() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "Searchable body content here", "1706700000"],
        )
        .unwrap();

        // FTS should be empty before document insert
        let count_before: i32 = conn
            .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, 0);

        // Insert document
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "test.md", "hash1", "Searchable Title", "1706700000", "1706700000"],
        ).unwrap();

        // FTS should have 1 entry after document insert (trigger fired)
        let count_after: i32 = conn
            .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 1, "FTS insert trigger should add entry");

        // Verify content is searchable
        let found: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM documents_fts WHERE documents_fts MATCH 'searchable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, 1);
    }

    #[test]
    fn test_fts_delete_trigger() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up and insert
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "Body content", "1706700000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "test.md", "hash1", "Title", "1706700000", "1706700000"],
        ).unwrap();

        let count_before: i32 = conn
            .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, 1);

        // Delete document
        conn.execute("DELETE FROM documents WHERE relative_path = 'test.md'", [])
            .unwrap();

        // FTS should be empty after delete (trigger fired)
        let count_after: i32 = conn
            .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 0, "FTS delete trigger should remove entry");
    }

    // ==================== Index Tests ====================

    #[test]
    fn test_indexes_exist() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        // Should have at least these indexes
        assert!(
            indexes
                .iter()
                .any(|i| i.contains("documents") || i.contains("doc")),
            "should have document-related indexes"
        );
    }
}
