//! SQL schema for the code intelligence index.
//!
//! All code intelligence data (symbols, relationships, files) lives in
//! a single SQLite database (`code.sqlite`). FTS5 with trigram tokenizer
//! enables substring matching on symbol names.

/// SQL statements to initialize the code index schema.
///
/// Executed inside a transaction by [`init_schema`].
pub const CREATE_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS code_files (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    rel_path TEXT NOT NULL,
    hash TEXT NOT NULL,
    language TEXT,
    mtime INTEGER,
    indexed_at INTEGER NOT NULL DEFAULT (unixepoch()),
    token_estimate INTEGER
);

CREATE TABLE IF NOT EXISTS code_symbols (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    file_id INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE,
    file_path TEXT NOT NULL,
    line_start INTEGER NOT NULL,
    col_start INTEGER,
    line_end INTEGER,
    col_end INTEGER,
    visibility INTEGER NOT NULL DEFAULT 0,
    signature TEXT,
    doc_comment TEXT,
    module_path TEXT,
    scope_context TEXT,
    UNIQUE(name, file_id, line_start)
);

CREATE TABLE IF NOT EXISTS code_relationships (
    id INTEGER PRIMARY KEY,
    from_symbol_id INTEGER REFERENCES code_symbols(id) ON DELETE CASCADE,
    from_name TEXT NOT NULL,
    to_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    file_id INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE,
    to_line INTEGER,
    to_col INTEGER
);

CREATE INDEX IF NOT EXISTS idx_symbols_name ON code_symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON code_symbols(file_id);
CREATE INDEX IF NOT EXISTS idx_symbols_kind ON code_symbols(kind);
CREATE INDEX IF NOT EXISTS idx_rels_from ON code_relationships(from_symbol_id);
CREATE INDEX IF NOT EXISTS idx_rels_to_name ON code_relationships(to_name);
CREATE INDEX IF NOT EXISTS idx_rels_file ON code_relationships(file_id);
CREATE INDEX IF NOT EXISTS idx_files_hash ON code_files(hash);

CREATE VIRTUAL TABLE IF NOT EXISTS code_symbols_fts USING fts5(
    name, doc_comment, signature,
    content=code_symbols,
    content_rowid=id,
    tokenize='trigram case_sensitive 0'
);
"#;

/// Triggers to keep the FTS5 index in sync with `code_symbols`.
///
/// Separate from `CREATE_TABLES` because triggers cannot use `IF NOT EXISTS`.
/// Call [`init_schema`] which handles idempotency.
pub const CREATE_TRIGGERS: &str = r#"
CREATE TRIGGER code_symbols_fts_insert AFTER INSERT ON code_symbols BEGIN
    INSERT INTO code_symbols_fts(rowid, name, doc_comment, signature)
    VALUES (new.id, new.name, COALESCE(new.doc_comment, ''), COALESCE(new.signature, ''));
END;

CREATE TRIGGER code_symbols_fts_delete AFTER DELETE ON code_symbols BEGIN
    INSERT INTO code_symbols_fts(code_symbols_fts, rowid, name, doc_comment, signature)
    VALUES ('delete', old.id, COALESCE(old.name, ''), COALESCE(old.doc_comment, ''), COALESCE(old.signature, ''));
END;

CREATE TRIGGER code_symbols_fts_update AFTER UPDATE ON code_symbols BEGIN
    INSERT INTO code_symbols_fts(code_symbols_fts, rowid, name, doc_comment, signature)
    VALUES ('delete', old.id, COALESCE(old.name, ''), COALESCE(old.doc_comment, ''), COALESCE(old.signature, ''));
    INSERT INTO code_symbols_fts(rowid, name, doc_comment, signature)
    VALUES (new.id, new.name, COALESCE(new.doc_comment, ''), COALESCE(new.signature, ''));
END;
"#;

/// Initialize the code index schema on an open connection.
///
/// Idempotent: safe to call on an already-initialized database.
/// Enables WAL mode and foreign keys as a side effect.
pub fn init_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(CREATE_TABLES)?;

    // Additive migration: ensure token_estimate column exists on pre-existing DBs.
    let has_token_estimate: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('code_files') WHERE name='token_estimate')",
        [],
        |row| row.get(0),
    )?;
    if !has_token_estimate {
        conn.execute_batch("ALTER TABLE code_files ADD COLUMN token_estimate INTEGER")?;
    }

    // Triggers don't support IF NOT EXISTS — check before creating.
    let trigger_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='code_symbols_fts_insert')",
        [],
        |row| row.get(0),
    )?;
    if !trigger_exists {
        conn.execute_batch(CREATE_TRIGGERS)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_schema_init_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Verify all three tables exist
        for table in &["code_files", "code_symbols", "code_relationships"] {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "table {table} should exist");
        }
    }

    #[test]
    fn test_schema_init_creates_fts_table() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='code_symbols_fts')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "FTS5 table should exist");
    }

    #[test]
    fn test_schema_init_creates_triggers() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        for trigger in &[
            "code_symbols_fts_insert",
            "code_symbols_fts_delete",
            "code_symbols_fts_update",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name=?1)",
                    [trigger],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "trigger {trigger} should exist");
        }
    }

    #[test]
    fn test_schema_init_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // Second call should not fail
        init_schema(&conn).unwrap();
    }

    #[test]
    fn test_schema_foreign_keys_enabled() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let fk_enabled: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk_enabled, 1);
    }

    #[test]
    fn test_schema_cascade_delete() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Insert a file
        conn.execute(
            "INSERT INTO code_files (path, rel_path, hash) VALUES (?1, ?2, ?3)",
            ["abs/path.rs", "path.rs", "abc123"],
        )
        .unwrap();
        let file_id: i64 = conn.last_insert_rowid();

        // Insert a symbol
        conn.execute(
            "INSERT INTO code_symbols (name, kind, file_id, file_path, line_start) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["my_fn", "Function", file_id, "path.rs", 10],
        )
        .unwrap();
        let sym_id: i64 = conn.last_insert_rowid();

        // Insert a relationship
        conn.execute(
            "INSERT INTO code_relationships (from_symbol_id, from_name, to_name, kind, file_id) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![sym_id, "my_fn", "other_fn", "Calls", file_id],
        )
        .unwrap();

        // Verify counts
        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sym_count, 1);

        let rel_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_relationships", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rel_count, 1);

        // Delete file — CASCADE should remove symbols and relationships
        conn.execute("DELETE FROM code_files WHERE id = ?1", [file_id])
            .unwrap();

        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sym_count, 0, "symbols should be cascade-deleted");

        let rel_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_relationships", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rel_count, 0, "relationships should be cascade-deleted");
    }

    #[test]
    fn test_schema_duplicate_symbol_replaced() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO code_files (path, rel_path, hash) VALUES ('a', 'a', 'h')",
            [],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT OR REPLACE INTO code_symbols (name, kind, file_id, file_path, line_start, doc_comment) VALUES ('foo', 'Function', ?1, 'a', 10, 'old')",
            [file_id],
        )
        .unwrap();

        // Same name + file + line should replace, not fail
        conn.execute(
            "INSERT OR REPLACE INTO code_symbols (name, kind, file_id, file_path, line_start, doc_comment) VALUES ('foo', 'Function', ?1, 'a', 10, 'new')",
            [file_id],
        )
        .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM code_symbols WHERE name = 'foo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "duplicate should be replaced, not added");

        let doc: String = conn
            .query_row(
                "SELECT doc_comment FROM code_symbols WHERE name = 'foo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(doc, "new", "last insert should win");
    }

    #[test]
    fn test_fts5_trigram_search() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO code_files (path, rel_path, hash) VALUES ('a', 'a', 'h')",
            [],
        )
        .unwrap();
        let file_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO code_symbols (name, kind, file_id, file_path, line_start) VALUES ('ArchiveAppService', 'Struct', ?1, 'a', 1)",
            [file_id],
        )
        .unwrap();

        // Trigram search for substring "Archive"
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM code_symbols_fts WHERE code_symbols_fts MATCH '\"Archive\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "trigram should match substring 'Archive'");

        // Trigram search for substring "Service"
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM code_symbols_fts WHERE code_symbols_fts MATCH '\"Service\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "trigram should match substring 'Service'");
    }
}
