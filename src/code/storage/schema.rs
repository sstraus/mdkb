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
    owner_name TEXT,
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
    to_col INTEGER,
    to_qualifier TEXT
);

CREATE TABLE IF NOT EXISTS code_imports (
    id INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    alias TEXT,
    is_glob INTEGER NOT NULL DEFAULT 0,
    is_type_only INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS code_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_symbols_name ON code_symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON code_symbols(file_id);
CREATE INDEX IF NOT EXISTS idx_symbols_kind ON code_symbols(kind);
CREATE INDEX IF NOT EXISTS idx_rels_from ON code_relationships(from_symbol_id);
CREATE INDEX IF NOT EXISTS idx_rels_to_name ON code_relationships(to_name);
CREATE INDEX IF NOT EXISTS idx_rels_file ON code_relationships(file_id);
CREATE INDEX IF NOT EXISTS idx_files_hash ON code_files(hash);
-- insert_file DELETEs the legacy absolute-path row by rel_path on every insert;
-- without this index each DELETE is a full table scan, making a full index O(n²).
CREATE INDEX IF NOT EXISTS idx_files_rel_path ON code_files(rel_path);
CREATE INDEX IF NOT EXISTS idx_imports_path ON code_imports(path);
-- `alias` is nullable and SQLite treats NULLs as distinct, so a plain UNIQUE
-- would let `use std::fmt;` be inserted twice. Fold NULL to '' in the key.
CREATE UNIQUE INDEX IF NOT EXISTS idx_imports_unique
    ON code_imports(file_id, path, IFNULL(alias, ''));

CREATE VIRTUAL TABLE IF NOT EXISTS code_symbols_fts USING fts5(
    name, doc_comment, signature,
    content=code_symbols,
    content_rowid=id,
    tokenize='trigram case_sensitive 0'
);
"#;

pub const LAST_INDEX_SCAN_KEY: &str = "last_index_scan_at";

pub const RESOLUTION_VERSION_KEY: &str = "resolution_version";

/// What this binary extracts from source, as a number an index can be compared
/// against.
///
/// Bump it whenever the parsers start recording something an older index does
/// not hold — version 1 is `module_path`, which the call resolver needs to tell
/// two same-named functions apart; version 2 is `owner_name` and
/// `to_qualifier`, which tell it that `std::fs::write` is not this crate's
/// `write`; version 3 is the qualifier itself reaching every language, which
/// eight of the thirteen parsers used to drop before it was ever stored;
/// version 4 is a round of parser fixes that each change what a stored row
/// says — six visibility levels instead of four, GDScript doc comments on
/// declarations that never carried one, Kotlin companion objects as symbols and
/// Kotlin extensions named after the type they extend; version 5 finishes that
/// round — PHP enum cases, promoted properties and namespaces, and the Actor and
/// Signal kinds, which a Swift actor and a GDScript signal used to be stored
/// under the wrong number for; version 6 is Go visibility, where every
/// unexported name used to be stored as private and every function-local one
/// answered by the case of its first letter; version 7 is Go embedded fields,
/// which produced no symbol at all, and qualified types, which were invisible
/// wherever the parser looked for a type.
/// Without the bump an index keeps the wider,
/// pre-contract answers for every file that is never edited again, which is
/// most of a codebase.
pub const RESOLUTION_VERSION: i64 = 7;

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

/// The indexes over the columns [`init_schema`] adds after the fact.
///
/// They cannot live in `CREATE_TABLES`: that batch runs before the columns
/// exist on a database written by an older binary, and `CREATE INDEX` on a
/// missing column is an error, not a no-op.
const CREATE_ADDED_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_symbols_addr ON code_symbols(name, owner_name, module_path);
CREATE INDEX IF NOT EXISTS idx_rels_to_qual ON code_relationships(to_name, to_qualifier);
"#;

/// Add `column` to `table` unless it is already there.
///
/// `ALTER TABLE ADD COLUMN` has no `IF NOT EXISTS`, so the probe is the guard.
fn add_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> rusqlite::Result<()> {
    let present: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        [table, column],
        |row| row.get(0),
    )?;
    if !present {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    }
    Ok(())
}

/// Initialize the code index schema on an open connection.
///
/// Idempotent: safe to call on an already-initialized database.
/// Enables WAL mode and foreign keys as a side effect.
pub fn init_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    conn.execute_batch(CREATE_TABLES)?;

    // Additive migrations: a column added to CREATE_TABLES only reaches a
    // database that does not exist yet, so every one of them also has to be
    // added here for the indexes already on disk.
    add_column(conn, "code_files", "token_estimate", "INTEGER")?;
    add_column(conn, "code_symbols", "owner_name", "TEXT")?;
    add_column(conn, "code_relationships", "to_qualifier", "TEXT")?;
    conn.execute_batch(CREATE_ADDED_INDEXES)?;

    // Triggers don't support IF NOT EXISTS — check before creating.
    let trigger_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='trigger' AND name='code_symbols_fts_insert')",
        [],
        |row| row.get(0),
    )?;
    if !trigger_exists {
        conn.execute_batch(CREATE_TRIGGERS)?;
    }

    super::repair::run_repairs(conn);
    migrate_resolution_version(conn)?;

    Ok(())
}

/// Mark every indexed file as changed when the index predates the parse
/// contract this binary implements.
///
/// Nothing is deleted: the symbols stay queryable, and both change detectors
/// are simply made to disagree with disk. `index_scope` compares `mtime` while
/// `update` and `reindex_files` compare `hash`, so touching one alone leaves
/// half the entry points ignoring the migration. The reparse that follows
/// carries the embeddings over ([`split_by_reuse`](crate::code::indexing)), so
/// what it costs is parsing, not the ONNX pass that made a full reindex
/// unaffordable.
///
/// `mtime` is stamped to the epoch rather than cleared. `get_file_mtimes` skips
/// NULLs, so clearing it empties the map and `index_scope` concludes the index
/// is brand new — which takes the branch that re-embeds every symbol from
/// scratch, the exact cost this migration exists to avoid. Measured: 51 s wall
/// and 484 s CPU with 0 vectors carried over.
fn migrate_resolution_version(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    use rusqlite::OptionalExtension;

    let stored: Option<i64> = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM code_metadata WHERE key=?1",
            [RESOLUTION_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    if stored.is_some_and(|version| version >= RESOLUTION_VERSION) {
        return Ok(());
    }

    conn.execute("UPDATE code_files SET hash = '', mtime = 0", [])?;
    conn.execute(
        "INSERT INTO code_metadata (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![RESOLUTION_VERSION_KEY, RESOLUTION_VERSION],
    )?;
    Ok(())
}

/// Timestamp for the last completed code index scan.
///
/// Older databases do not have `code_metadata`, so callers fall back to the
/// newest per-file `indexed_at` value until the database is opened and migrated.
pub fn last_index_scan_at(conn: &rusqlite::Connection) -> rusqlite::Result<Option<i64>> {
    use rusqlite::OptionalExtension;

    let has_metadata: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='code_metadata')",
        [],
        |row| row.get(0),
    )?;
    if has_metadata {
        let scan_at: Option<i64> = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM code_metadata WHERE key=?1",
                [LAST_INDEX_SCAN_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if scan_at.is_some() {
            return Ok(scan_at);
        }
    }

    conn.query_row("SELECT MAX(indexed_at) FROM code_files", [], |row| {
        row.get::<_, Option<i64>>(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_schema_init_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Verify all tables exist
        for table in &[
            "code_files",
            "code_symbols",
            "code_relationships",
            "code_imports",
            "code_metadata",
        ] {
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

    /// An index written before the parse contract existed holds symbols without
    /// a `module_path`, and a file that is never edited again would keep them
    /// forever: both change detectors would go on agreeing with disk. Opening it
    /// has to make them disagree — and it must not touch the symbols, which stay
    /// the only answer available until the reparse runs.
    #[test]
    fn an_index_older_than_the_parse_contract_is_marked_for_reparse() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "DELETE FROM code_metadata WHERE key=?1",
            [RESOLUTION_VERSION_KEY],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO code_files (path, rel_path, hash, mtime) VALUES ('lib.rs', 'lib.rs', 'h1', 42)",
            [],
        )
        .unwrap();

        init_schema(&conn).unwrap();

        let (hash, mtime): (String, Option<i64>) = conn
            .query_row("SELECT hash, mtime FROM code_files", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(hash, "", "`update` compares hashes and must see a mismatch");
        assert_eq!(
            mtime,
            Some(0),
            "`index_scope` compares mtimes and must too — but the row has to stay \
             visible to `get_file_mtimes`, which skips NULLs"
        );
        assert_eq!(stored_resolution_version(&conn), Some(RESOLUTION_VERSION));
    }

    /// The reparse is expensive enough that repeating it on every open would be
    /// worse than the staleness it fixes: the daemon opens the index on every
    /// watch event.
    #[test]
    fn an_index_at_the_current_contract_is_left_alone() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO code_files (path, rel_path, hash, mtime) VALUES ('lib.rs', 'lib.rs', 'h1', 42)",
            [],
        )
        .unwrap();

        init_schema(&conn).unwrap();

        let (hash, mtime): (String, Option<i64>) = conn
            .query_row("SELECT hash, mtime FROM code_files", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!((hash.as_str(), mtime), ("h1", Some(42)));
    }

    /// A newer index opened by an older binary must not be dragged backwards:
    /// its files already carry more than this binary knows how to ask for.
    #[test]
    fn an_index_ahead_of_this_binary_is_left_alone() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "UPDATE code_metadata SET value = ?2 WHERE key = ?1",
            rusqlite::params![RESOLUTION_VERSION_KEY, RESOLUTION_VERSION + 1],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO code_files (path, rel_path, hash, mtime) VALUES ('lib.rs', 'lib.rs', 'h1', 42)",
            [],
        )
        .unwrap();

        init_schema(&conn).unwrap();

        let hash: String = conn
            .query_row("SELECT hash FROM code_files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(hash, "h1");
        assert_eq!(
            stored_resolution_version(&conn),
            Some(RESOLUTION_VERSION + 1),
            "the newer version survives"
        );
    }

    fn stored_resolution_version(conn: &Connection) -> Option<i64> {
        use rusqlite::OptionalExtension;
        conn.query_row(
            "SELECT CAST(value AS INTEGER) FROM code_metadata WHERE key=?1",
            [RESOLUTION_VERSION_KEY],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
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
    fn rel_path_delete_uses_index_not_full_scan() {
        // PERF-D2: insert_file runs two DELETEs filtered on rel_path for every
        // file. Without an index on rel_path each is a full table scan, making a
        // full index O(n²). Assert the query planner uses idx_files_rel_path.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        let plan: Vec<String> = conn
            .prepare("EXPLAIN QUERY PLAN DELETE FROM code_files WHERE rel_path = ?1 AND path != ?2")
            .unwrap()
            .query_map(["x", "y"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let detail = plan.join(" | ");

        assert!(
            detail.contains("idx_files_rel_path"),
            "rel_path DELETE should use idx_files_rel_path, plan was: {detail}"
        );
        assert!(
            !detail.contains("SCAN code_files"),
            "rel_path DELETE should not full-scan code_files, plan was: {detail}"
        );
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
