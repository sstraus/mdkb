//! Database schema management and migrations.

use crate::error::Result;
use rusqlite::Connection;

/// Current schema version.
pub const SCHEMA_VERSION: i32 = 10;

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
    source TEXT DEFAULT 'manual',
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
    status TEXT DEFAULT 'current',        -- current, superseded, retracted
    status_reason TEXT,                   -- Reason for status change
    version TEXT,                         -- Optional version identifier
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
    last_accessed INTEGER,
    source_path TEXT,                 -- Original file path (for journal imports)
    confirmations INTEGER DEFAULT 0,  -- Positive confidence signals
    corrections INTEGER DEFAULT 0,    -- Negative confidence signals
    last_confirmed_at INTEGER,        -- Timestamp of last confirmation
    source_type TEXT DEFAULT 'user_statement',  -- official_docs, user_statement, inference
    expires_at INTEGER,                        -- Unix timestamp; NULL = permanent
    due_at INTEGER                             -- Unix timestamp; surfaces reminders at/after this time
);

CREATE INDEX IF NOT EXISTS idx_memory_type ON memory_entries(entry_type);
CREATE INDEX IF NOT EXISTS idx_memory_status ON memory_entries(status);
CREATE INDEX IF NOT EXISTS idx_memory_access ON memory_entries(access_count DESC);

-- FTS for memory content search (includes tags as space-separated text)
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    id,
    title,
    content,
    tags,
    tokenize = 'porter unicode61',
    content='',
    content_rowid='rowid'
);

-- Triggers to keep memory FTS in sync
-- Tags stored as JSON array, stripped to space-separated text for FTS
CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory_entries BEGIN
    INSERT INTO memory_fts(rowid, id, title, content, tags)
    VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
            REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
END;

CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
    VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
            REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
END;

CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
    VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
            REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
    INSERT INTO memory_fts(rowid, id, title, content, tags)
    VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
            REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
END;

-- Memory revision history (max 3 per entry, stores diffs)
CREATE TABLE IF NOT EXISTS memory_revisions (
    id INTEGER PRIMARY KEY,
    memory_id TEXT NOT NULL,
    diff TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_id ON memory_revisions(memory_id);

-- Evolution tracking for document relationships (RFC-style)
CREATE TABLE IF NOT EXISTS evolution (
    id INTEGER PRIMARY KEY,
    source_doc_id INTEGER NOT NULL,      -- The newer/superseding document
    target_doc_id INTEGER NOT NULL,      -- The older/superseded document
    relationship TEXT NOT NULL,          -- supersedes, updates, corrects, retracts, extends
    scope TEXT,                          -- NULL = full doc, or section path
    reason TEXT,                         -- Explanation for the evolution
    created_at INTEGER NOT NULL,
    FOREIGN KEY(source_doc_id) REFERENCES documents(id) ON DELETE CASCADE,
    FOREIGN KEY(target_doc_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_evolution_source ON evolution(source_doc_id);
CREATE INDEX IF NOT EXISTS idx_evolution_target ON evolution(target_doc_id);
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

    // Check for migrations
    let current = get_schema_version(conn)?;
    match current {
        None => {
            // Fresh database
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?)",
                [SCHEMA_VERSION],
            )?;
        }
        Some(v) if v < SCHEMA_VERSION => {
            // Run migrations
            migrate_schema(conn, v)?;
        }
        _ => {
            // Up to date
        }
    }

    Ok(())
}

/// Migrate schema from old version to current.
///
/// Wrapped in a transaction for atomicity — partial migration on crash
/// is rolled back automatically by SQLite.
fn migrate_schema(conn: &Connection, from_version: i32) -> Result<()> {
    conn.execute("BEGIN IMMEDIATE", [])?;
    let result = migrate_schema_inner(conn, from_version);
    match &result {
        Ok(()) => {
            conn.execute("COMMIT", [])?;
        }
        Err(_) => {
            let _ = conn.execute("ROLLBACK", []);
        }
    }
    result
}

fn migrate_schema_inner(conn: &Connection, from_version: i32) -> Result<()> {
    // Migration from v1 to v2: add status/version columns to documents and evolution table
    if from_version < 2 {
        // Add new columns to documents table if they don't exist
        // SQLite doesn't have IF NOT EXISTS for ALTER TABLE, so we check first
        let has_status: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_status {
            conn.execute_batch(
                r#"
                ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
                ALTER TABLE documents ADD COLUMN status_reason TEXT;
                ALTER TABLE documents ADD COLUMN version TEXT;
                "#,
            )?;
        }

        // Evolution table is created by SCHEMA_SQL with IF NOT EXISTS
    }

    // Migration from v2 to v3: add source_path to memory_entries
    if from_version < 3 {
        let has_source_path: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'source_path'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_source_path {
            conn.execute("ALTER TABLE memory_entries ADD COLUMN source_path TEXT", [])?;
        }
    }

    // Migration from v3 to v4: add source column to collections
    if from_version < 4 {
        let has_source: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('collections') WHERE name = 'source'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_source {
            conn.execute(
                "ALTER TABLE collections ADD COLUMN source TEXT DEFAULT 'manual'",
                [],
            )?;
        }
    }

    // Migration from v4 to v5: fix FTS INSERT trigger to use NEW.rowid directly
    if from_version < 5 {
        conn.execute_batch(
            r#"
            DROP TRIGGER IF EXISTS memory_ai;
            CREATE TRIGGER memory_ai AFTER INSERT ON memory_entries BEGIN
                INSERT INTO memory_fts(rowid, id, title, content)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content);
            END;
            "#,
        )?;
    }

    // Migration from v5 to v6: add tags column to memory FTS
    if from_version < 6 {
        conn.execute_batch(
            r#"
            DROP TABLE IF EXISTS memory_fts;
            CREATE VIRTUAL TABLE memory_fts USING fts5(
                id, title, content, tags,
                tokenize = 'porter unicode61',
                content='', content_rowid='rowid'
            );

            DROP TRIGGER IF EXISTS memory_ai;
            DROP TRIGGER IF EXISTS memory_ad;
            DROP TRIGGER IF EXISTS memory_au;

            CREATE TRIGGER memory_ai AFTER INSERT ON memory_entries BEGIN
                INSERT INTO memory_fts(rowid, id, title, content, tags)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
                        REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
            END;
            CREATE TRIGGER memory_ad AFTER DELETE ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
                        REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
            END;
            CREATE TRIGGER memory_au AFTER UPDATE ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
                        REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
                INSERT INTO memory_fts(rowid, id, title, content, tags)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
                        REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
            END;

            -- Repopulate FTS from existing entries
            INSERT INTO memory_fts(rowid, id, title, content, tags)
            SELECT rowid, id, title, content,
                   REPLACE(REPLACE(REPLACE(tags, '"', ''), '[', ''), ']', '')
            FROM memory_entries;
            "#,
        )?;
    }

    // Migration from v6 to v7: add confidence columns to memory_entries
    if from_version < 7 {
        let has_confirmations: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'confirmations'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_confirmations {
            conn.execute_batch(
                r#"
                ALTER TABLE memory_entries ADD COLUMN confirmations INTEGER DEFAULT 0;
                ALTER TABLE memory_entries ADD COLUMN corrections INTEGER DEFAULT 0;
                ALTER TABLE memory_entries ADD COLUMN last_confirmed_at INTEGER;
                ALTER TABLE memory_entries ADD COLUMN source_type TEXT DEFAULT 'user_statement';
                "#,
            )?;
        }
    }

    // Migration from v7 to v8: add memory_revisions table for change history
    if from_version < 8 {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memory_revisions (
                id INTEGER PRIMARY KEY,
                memory_id TEXT NOT NULL,
                diff TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_id
                ON memory_revisions(memory_id);
            "#,
        )?;
    }

    // Migration from v8 to v9: add expires_at column to memory_entries for TTL support
    if from_version < 9 {
        let has_expires_at: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'expires_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_expires_at {
            conn.execute_batch("ALTER TABLE memory_entries ADD COLUMN expires_at INTEGER;")?;
        }
    }

    // Migration from v9 to v10: add due_at column to memory_entries for reminder support
    if from_version < 10 {
        let has_due_at: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'due_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_due_at {
            conn.execute_batch("ALTER TABLE memory_entries ADD COLUMN due_at INTEGER;")?;
        }
    }

    // Update schema version
    conn.execute("UPDATE schema_version SET version = ?", [SCHEMA_VERSION])?;

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

    // ==================== Evolution Table Tests ====================

    #[test]
    fn test_evolution_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='evolution'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "evolution table should exist");
    }

    #[test]
    fn test_evolution_indexes_exist() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_evolution_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(
            indexes.iter().any(|i| i.contains("source")),
            "should have idx_evolution_source index"
        );
        assert!(
            indexes.iter().any(|i| i.contains("target")),
            "should have idx_evolution_target index"
        );
    }

    #[test]
    fn test_documents_has_status_columns() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Check status column exists
        let has_status: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_status, "documents table should have status column");

        // Check status_reason column exists
        let has_reason: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status_reason'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_reason,
            "documents table should have status_reason column"
        );

        // Check version column exists
        let has_version: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'version'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_version, "documents table should have version column");
    }

    #[test]
    fn test_documents_status_default() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up collection and content first
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash123", "# Test", "1706700000"],
        )
        .unwrap();

        // Insert document without specifying status
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "test.md", "hash123", "Test", "1706700000", "1706700000"],
        ).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM documents WHERE relative_path = 'test.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(status, "current", "default status should be 'current'");
    }

    #[test]
    fn test_schema_version_is_current() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let version = get_schema_version(&conn).unwrap().unwrap();
        assert_eq!(
            version, SCHEMA_VERSION,
            "schema version should match SCHEMA_VERSION"
        );
    }

    // ==================== Migration Tests ====================
    //
    // These tests create a genuine old schema from scratch (no init_schema),
    // then call migrate_schema to verify migrations work correctly.

    /// Create a v1 schema: no status/version columns on documents, no evolution table,
    /// no source_path on memory_entries, no source on collections.
    fn create_v1_schema(conn: &Connection) {
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
            INSERT INTO schema_version (version) VALUES (1);

            CREATE TABLE collections (
                name TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                pattern TEXT DEFAULT '**/*.md',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE content (
                hash TEXT PRIMARY KEY,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE documents (
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

            CREATE INDEX idx_documents_collection ON documents(collection);
            CREATE INDEX idx_documents_hash ON documents(hash);
            CREATE INDEX idx_documents_path ON documents(relative_path);

            CREATE VIRTUAL TABLE documents_fts USING fts5(
                title, body,
                tokenize = 'porter unicode61',
                content='', content_rowid='id'
            );

            CREATE TRIGGER documents_ai AFTER INSERT ON documents BEGIN
                INSERT INTO documents_fts(rowid, title, body)
                SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
            END;
            CREATE TRIGGER documents_ad AFTER DELETE ON documents BEGIN
                INSERT INTO documents_fts(documents_fts, rowid, title, body)
                VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
            END;
            CREATE TRIGGER documents_au AFTER UPDATE ON documents BEGIN
                INSERT INTO documents_fts(documents_fts, rowid, title, body)
                VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
                INSERT INTO documents_fts(rowid, title, body)
                SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
            END;

            CREATE TABLE memory_entries (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                entry_type TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '[]',
                status TEXT DEFAULT 'active',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                superseded_by TEXT,
                access_count INTEGER DEFAULT 0,
                last_accessed INTEGER
            );

            CREATE INDEX idx_memory_type ON memory_entries(entry_type);
            CREATE INDEX idx_memory_status ON memory_entries(status);
            CREATE INDEX idx_memory_access ON memory_entries(access_count DESC);

            CREATE VIRTUAL TABLE memory_fts USING fts5(
                id, title, content,
                tokenize = 'porter unicode61',
                content='', content_rowid='rowid'
            );

            CREATE TRIGGER memory_ai AFTER INSERT ON memory_entries BEGIN
                INSERT INTO memory_fts(rowid, id, title, content)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content);
            END;
            CREATE TRIGGER memory_ad AFTER DELETE ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content);
            END;
            CREATE TRIGGER memory_au AFTER UPDATE ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content);
                INSERT INTO memory_fts(rowid, id, title, content)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content);
            END;

            INSERT OR REPLACE INTO documents_fts(documents_fts, rank) VALUES('rank', 'bm25(10.0, 1.0)');
            "#,
        )
        .unwrap();
    }

    #[test]
    fn test_migrate_v1_to_current() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);

        // Verify starting at v1
        assert_eq!(get_schema_version(&conn).unwrap(), Some(1));

        // v1 should NOT have status column on documents
        let has_status: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_status, "v1 should not have status column");

        // Run migration
        migrate_schema(&conn, 1).unwrap();

        // Should be at current version
        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        // v2 migration: documents should now have status, status_reason, version
        let has_status: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_status, "should have status column after migration");

        // v3 migration: memory_entries should have source_path
        let has_source_path: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'source_path'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_source_path,
            "should have source_path column after migration"
        );

        // v4 migration: collections should have source
        let has_source: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('collections') WHERE name = 'source'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_source, "should have source column after migration");
    }

    #[test]
    fn test_migrate_v1_preserves_existing_data() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);

        // Insert data at v1
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1000", "1000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["h1", "body text", "1000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "readme.md", "h1", "Readme", "1000", "1000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            ["test-entry", "Test", "Content", "topic", "[\"tag1\"]", "1000", "1000"],
        ).unwrap();

        // Migrate
        migrate_schema(&conn, 1).unwrap();

        // Data should survive
        let title: String = conn
            .query_row(
                "SELECT title FROM documents WHERE relative_path = 'readme.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "Readme");

        let mem_title: String = conn
            .query_row(
                "SELECT title FROM memory_entries WHERE id = 'test-entry'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mem_title, "Test");

        // New columns should have defaults
        let status: String = conn
            .query_row(
                "SELECT status FROM documents WHERE relative_path = 'readme.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "current");

        let source: String = conn
            .query_row(
                "SELECT source FROM collections WHERE name = 'docs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, "manual");
    }

    #[test]
    fn test_migrate_v3_to_current() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);
        // Manually apply v2 and v3 migrations to get to v3
        conn.execute_batch(
            r#"
            ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
            ALTER TABLE documents ADD COLUMN status_reason TEXT;
            ALTER TABLE documents ADD COLUMN version TEXT;
            ALTER TABLE memory_entries ADD COLUMN source_path TEXT;
            UPDATE schema_version SET version = 3;
            "#,
        )
        .unwrap();

        // v3 should NOT have source on collections
        let has_source: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('collections') WHERE name = 'source'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_source, "v3 should not have source column");

        // Migrate from v3
        migrate_schema(&conn, 3).unwrap();

        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        let has_source: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('collections') WHERE name = 'source'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_source,
            "should have source column after v3→v4 migration"
        );
    }

    #[test]
    fn test_malformed_tags_json_returns_empty_vec() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);
        migrate_schema(&conn, 1).unwrap();

        // Insert a memory entry with malformed JSON in tags
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            ["bad-tags", "Bad Tags", "Content", "topic", "not valid json{{{", "1000", "1000"],
        ).unwrap();

        // Read it back via the memory module
        let entry = crate::store::memory::get_entry(&conn, "bad-tags")
            .expect("get_entry should not error")
            .expect("entry should exist");

        assert!(
            entry.tags.is_empty(),
            "malformed tags_json should result in empty tags vec, got: {:?}",
            entry.tags
        );
    }

    #[test]
    fn test_migrate_v8_to_v9_adds_expires_at() {
        let conn = Connection::open_in_memory().unwrap();
        // Create fresh schema at v8 by init + roll back version
        init_schema(&conn).unwrap();

        // Verify expires_at does NOT exist yet at v8
        // (It will exist because init_schema creates latest DDL. So we simulate
        // a v8 database by dropping the column — but SQLite doesn't support DROP COLUMN
        // before 3.35. Instead, we test the migration path directly.)

        // Create a v8-like database from v1 + migrations up to v7
        let conn2 = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn2);
        // Apply migrations up through v7 manually
        conn2
            .execute_batch(
                r#"
                ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
                ALTER TABLE documents ADD COLUMN status_reason TEXT;
                ALTER TABLE documents ADD COLUMN version TEXT;
                ALTER TABLE memory_entries ADD COLUMN source_path TEXT;
                ALTER TABLE collections ADD COLUMN source TEXT DEFAULT 'manual';
                CREATE TABLE IF NOT EXISTS document_evolution (
                    id INTEGER PRIMARY KEY,
                    document_path TEXT NOT NULL,
                    from_version TEXT,
                    to_version TEXT NOT NULL,
                    change_type TEXT NOT NULL,
                    diff TEXT,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_evolution_path ON document_evolution(document_path);
                ALTER TABLE memory_entries ADD COLUMN confirmations INTEGER DEFAULT 0;
                ALTER TABLE memory_entries ADD COLUMN corrections INTEGER DEFAULT 0;
                ALTER TABLE memory_entries ADD COLUMN last_confirmed_at INTEGER;
                ALTER TABLE memory_entries ADD COLUMN source_type TEXT DEFAULT 'user_statement';
                CREATE TABLE IF NOT EXISTS memory_revisions (
                    id INTEGER PRIMARY KEY,
                    memory_id TEXT NOT NULL,
                    diff TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    FOREIGN KEY (memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_id
                    ON memory_revisions(memory_id);
                UPDATE schema_version SET version = 8;
                "#,
            )
            .unwrap();

        assert_eq!(get_schema_version(&conn2).unwrap(), Some(8));

        // Verify expires_at does NOT exist at v8
        let has_expires_at: bool = conn2
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'expires_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            !has_expires_at,
            "v8 should not have expires_at column"
        );

        // Run migration
        migrate_schema(&conn2, 8).unwrap();

        assert_eq!(get_schema_version(&conn2).unwrap(), Some(SCHEMA_VERSION));

        // Verify expires_at exists after migration
        let has_expires_at: bool = conn2
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'expires_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_expires_at,
            "should have expires_at column after v8→v9 migration"
        );

        // Verify existing entries have NULL expires_at
        conn2
            .execute(
                "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
                 VALUES ('test', 'Test', 'Content', 'topic', '[]', 1000, 1000)",
                [],
            )
            .unwrap();

        let expires_at: Option<i64> = conn2
            .query_row(
                "SELECT expires_at FROM memory_entries WHERE id = 'test'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            expires_at.is_none(),
            "existing entries should have NULL expires_at"
        );
    }

    #[test]
    fn test_migrate_v9_to_v10_adds_due_at() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);
        // Apply migrations up through v9 manually to simulate a v9 database.
        conn.execute_batch(
            r#"
            ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
            ALTER TABLE documents ADD COLUMN status_reason TEXT;
            ALTER TABLE documents ADD COLUMN version TEXT;
            ALTER TABLE memory_entries ADD COLUMN source_path TEXT;
            ALTER TABLE collections ADD COLUMN source TEXT DEFAULT 'manual';
            ALTER TABLE memory_entries ADD COLUMN confirmations INTEGER DEFAULT 0;
            ALTER TABLE memory_entries ADD COLUMN corrections INTEGER DEFAULT 0;
            ALTER TABLE memory_entries ADD COLUMN last_confirmed_at INTEGER;
            ALTER TABLE memory_entries ADD COLUMN source_type TEXT DEFAULT 'user_statement';
            ALTER TABLE memory_entries ADD COLUMN expires_at INTEGER;
            CREATE TABLE IF NOT EXISTS memory_revisions (
                id INTEGER PRIMARY KEY,
                memory_id TEXT NOT NULL,
                diff TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_id
                ON memory_revisions(memory_id);
            UPDATE schema_version SET version = 9;
            "#,
        )
        .unwrap();

        assert_eq!(get_schema_version(&conn).unwrap(), Some(9));

        // Insert an entry at v9 to verify data preservation.
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES ('pre-migration', 'Pre', 'Content', 'topic', '[]', 1000, 1000)",
            [],
        )
        .unwrap();

        // Verify due_at does NOT exist at v9.
        let has_due_at: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'due_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_due_at, "v9 should not have due_at column");

        // Run migration v9 → current.
        migrate_schema(&conn, 9).unwrap();

        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        // Verify due_at exists after migration.
        let has_due_at: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'due_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_due_at,
            "should have due_at column after v9→v10 migration"
        );

        // Pre-existing entry should survive with NULL due_at.
        let due_at: Option<i64> = conn
            .query_row(
                "SELECT due_at FROM memory_entries WHERE id = 'pre-migration'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            due_at.is_none(),
            "existing entries should have NULL due_at after migration"
        );
    }
}
