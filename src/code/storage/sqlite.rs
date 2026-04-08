//! SQLite-backed storage for the code intelligence index.
//!
//! Provides a single SQLite database that stores symbols, relationships,
//! and file metadata. FTS5 with trigram tokenizer enables substring
//! matching on symbol names ("Archive" finds "ArchiveAppService").

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolId, SymbolKind};

use super::schema;

/// SQLite-backed code intelligence index.
///
/// Wraps a `rusqlite::Connection` with the code index schema initialized.
/// Create with [`CodeDb::create`] or open an existing one with [`CodeDb::open`].
pub struct CodeDb {
    conn: Connection,
    path: PathBuf,
}

impl CodeDb {
    /// Create a new database at `path`. Fails if the file already exists.
    pub fn create(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!("database already exists: {}", path.display())),
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some(e.to_string()),
                )
            })?;
        }
        let conn = Connection::open(&path)?;
        schema::init_schema(&conn)?;
        Ok(Self { conn, path })
    }

    /// Open an existing database at `path`.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open(&path)?;
        schema::init_schema(&conn)?;
        Ok(Self { conn, path })
    }

    /// Open an existing database, or create one if none exists at `path`.
    pub fn open_or_create(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            Self::open(path)
        } else {
            Self::create(path)
        }
    }

    /// Get a reference to the underlying connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Get the database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // --- File operations ---

    /// Insert or replace a file record. Returns the SQLite rowid.
    pub fn insert_file(
        &self,
        path: &str,
        rel_path: &str,
        hash: &str,
        language: Option<&str>,
        mtime: Option<i64>,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO code_files (path, rel_path, hash, language, mtime, indexed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch()) \
             ON CONFLICT(path) DO UPDATE SET \
             rel_path=excluded.rel_path, hash=excluded.hash, \
             language=excluded.language, mtime=excluded.mtime, \
             indexed_at=unixepoch()",
            params![path, rel_path, hash, language, mtime],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Delete a file and all its symbols/relationships (via CASCADE).
    pub fn delete_by_file(&self, path: &str) -> rusqlite::Result<usize> {
        // Delete symbols explicitly first so the FTS5 delete trigger fires.
        // CASCADE deletes do NOT invoke row-level triggers in SQLite.
        self.conn.execute(
            "DELETE FROM code_symbols WHERE file_id = \
             (SELECT id FROM code_files WHERE path = ?1)",
            [path],
        )?;
        self.conn
            .execute("DELETE FROM code_files WHERE path = ?1", [path])
    }

    /// Get a map of (relative_path -> content_hash) for all indexed files.
    pub fn get_file_hashes(&self) -> rusqlite::Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT path, hash FROM code_files")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, hash) = row?;
            map.insert(path, hash);
        }
        Ok(map)
    }

    /// Retrieve stored mtimes keyed by relative path.
    pub fn get_file_mtimes(&self) -> rusqlite::Result<HashMap<String, u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, mtime FROM code_files WHERE mtime IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (path, mtime) = row?;
            map.insert(path, mtime);
        }
        Ok(map)
    }

    // --- Symbol operations ---

    /// Insert a symbol. Returns the SQLite rowid.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_symbol(
        &self,
        name: &str,
        kind: &str,
        file_id: i64,
        file_path: &str,
        line_start: u32,
        col_start: Option<u16>,
        line_end: Option<u32>,
        col_end: Option<u16>,
        visibility: u8,
        signature: Option<&str>,
        doc_comment: Option<&str>,
        module_path: Option<&str>,
        scope_context: Option<&str>,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT OR REPLACE INTO code_symbols \
             (name, kind, file_id, file_path, line_start, col_start, line_end, col_end, \
              visibility, signature, doc_comment, module_path, scope_context) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                name,
                kind,
                file_id,
                file_path,
                line_start,
                col_start.map(i64::from),
                line_end,
                col_end.map(i64::from),
                visibility,
                signature,
                doc_comment,
                module_path,
                scope_context,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    // --- Relationship operations ---

    /// Insert a relationship between symbols.
    pub fn insert_relationship(
        &self,
        from_symbol_id: Option<i64>,
        from_name: &str,
        to_name: &str,
        kind: &str,
        file_id: i64,
        to_line: Option<u32>,
        to_col: Option<u16>,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_line, to_col) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                from_symbol_id,
                from_name,
                to_name,
                kind,
                file_id,
                to_line,
                to_col.map(i64::from),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    // --- Count operations ---

    /// Number of indexed files.
    pub fn file_count(&self) -> rusqlite::Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM code_files", [], |r| r.get(0))
    }

    /// Number of indexed symbols.
    pub fn symbol_count(&self) -> rusqlite::Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM code_symbols", [], |r| r.get(0))
    }

    /// Number of indexed relationships.
    pub fn relationship_count(&self) -> rusqlite::Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM code_relationships", [], |r| r.get(0))
    }

    // --- Query operations ---

    /// Get a symbol by its SQLite rowid.
    pub fn get_symbol(&self, id: i64) -> rusqlite::Result<Option<Symbol>> {
        let mut stmt = self.conn.prepare_cached(SYMBOL_SELECT_BY_ID)?;
        let mut rows = stmt.query([id])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_symbol(row)?)),
            None => Ok(None),
        }
    }

    /// Find all symbols with an exact name match.
    pub fn find_symbols_by_name(&self, name: &str) -> rusqlite::Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare_cached(SYMBOL_SELECT_BY_NAME)?;
        let rows = stmt.query_map([name], |row| row_to_symbol(row))?;
        rows.collect()
    }

    /// Substring search on symbol names/signatures/doc_comments via FTS5 trigram.
    ///
    /// For queries shorter than 3 characters, falls back to LIKE.
    pub fn search_symbols(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<Symbol>> {
        if query.len() < 3 {
            // Trigram requires >= 3 chars; fall back to LIKE with escaped metacharacters
            let escaped = query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            let pattern = format!("%{escaped}%");
            let mut stmt = self.conn.prepare_cached(&format!(
                "{SYMBOL_COLUMNS} FROM code_symbols WHERE name LIKE ?1 ESCAPE '\\' LIMIT ?2"
            ))?;
            let rows = stmt.query_map(params![pattern, limit as i64], |row| row_to_symbol(row))?;
            return rows.collect();
        }

        // FTS5 trigram: escape embedded double-quotes to prevent query injection
        let escaped = query.replace('"', "\"\"");
        let fts_query = format!("\"{escaped}\"");
        let mut stmt = self.conn.prepare_cached(SYMBOL_SELECT_FTS)?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| row_to_symbol(row))?;
        rows.collect()
    }

    /// Get symbol IDs for all symbols belonging to a file (by relative path).
    pub fn get_symbol_ids_for_file(&self, rel_path: &str) -> rusqlite::Result<Vec<u32>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT s.id FROM code_symbols s \
             JOIN code_files f ON s.file_id = f.id \
             WHERE f.path = ?1",
        )?;
        let rows = stmt.query_map(params![rel_path], |row| {
            let id: i64 = row.get(0)?;
            Ok(id as u32)
        })?;
        rows.collect()
    }

    /// Get all symbols (for embedding generation).
    pub fn all_symbols(&self) -> rusqlite::Result<Vec<Symbol>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{SYMBOL_COLUMNS} FROM code_symbols"))?;
        let rows = stmt.query_map([], |row| row_to_symbol(row))?;
        rows.collect()
    }

    /// Batch lookup of symbols by their IDs.
    pub fn get_symbols_batch(&self, ids: &[i64]) -> rusqlite::Result<HashMap<i64, Symbol>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "{SYMBOL_COLUMNS} FROM code_symbols WHERE id IN ({})",
            placeholders.join(",")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let sql_params: Vec<&dyn rusqlite::types::ToSql> = ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(sql_params.as_slice(), |row| {
            let sym = row_to_symbol(row)?;
            let id: i64 = row.get(0)?;
            Ok((id, sym))
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let (id, sym) = row?;
            map.insert(id, sym);
        }
        Ok(map)
    }

    // --- Relationship queries ---

    /// Get symbols called by the given symbol.
    pub fn get_called_functions(&self, symbol_id: i64) -> rusqlite::Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "{SYMBOL_COLUMNS_PREFIXED} \
                 FROM code_relationships r \
                 JOIN code_symbols s ON s.name = r.to_name \
                 WHERE r.from_symbol_id = ?1 AND r.kind = 'Calls'"
        ))?;
        let rows = stmt.query_map([symbol_id], |row| row_to_symbol(row))?;
        rows.collect()
    }

    /// Get symbols that call the given symbol name.
    pub fn get_calling_functions(&self, symbol_name: &str) -> rusqlite::Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "{SYMBOL_COLUMNS_PREFIXED} \
                 FROM code_relationships r \
                 JOIN code_symbols s ON s.id = r.from_symbol_id \
                 WHERE r.to_name = ?1 AND r.kind = 'Calls'"
        ))?;
        let rows = stmt.query_map([symbol_name], |row| row_to_symbol(row))?;
        rows.collect()
    }

    /// Transitive impact radius: all symbols that directly or indirectly
    /// depend on the given symbol, up to `max_depth` hops.
    pub fn get_impact_radius(
        &self,
        symbol_name: &str,
        max_depth: u32,
    ) -> rusqlite::Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "WITH RECURSIVE impact(sym_id, depth) AS ( \
                     SELECT r.from_symbol_id, 0 FROM code_relationships r WHERE r.to_name = ?1 \
                     UNION \
                     SELECT r.from_symbol_id, i.depth + 1 \
                     FROM impact i \
                     JOIN code_symbols cs ON cs.id = i.sym_id \
                     JOIN code_relationships r ON r.to_name = cs.name \
                     WHERE i.depth < ?2 \
                 ) \
                 SELECT DISTINCT {SYMBOL_COLUMNS_BARE} \
                 FROM impact i \
                 JOIN code_symbols s ON s.id = i.sym_id"
        ))?;
        let rows = stmt.query_map(params![symbol_name, max_depth], |row| row_to_symbol(row))?;
        rows.collect()
    }

    /// Begin a transaction for batch operations.
    pub fn transaction(&mut self) -> rusqlite::Result<rusqlite::Transaction<'_>> {
        self.conn.transaction()
    }

    /// Delete all data (for full reindex).
    pub fn clear(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "DELETE FROM code_relationships; DELETE FROM code_symbols; DELETE FROM code_files;",
        )
    }
}

// --- SQL fragments ---

/// Standard 14-column SELECT list for symbol queries (no table prefix).
const SYMBOL_COLUMNS: &str = "SELECT id, name, kind, file_id, file_path, line_start, col_start, \
     line_end, col_end, visibility, signature, doc_comment, module_path, scope_context";

/// Standard 14-column SELECT list with `s.` table prefix for JOINs.
const SYMBOL_COLUMNS_PREFIXED: &str = "SELECT s.id, s.name, s.kind, s.file_id, s.file_path, s.line_start, s.col_start, \
     s.line_end, s.col_end, s.visibility, s.signature, s.doc_comment, s.module_path, s.scope_context";

/// Bare column list with `s.` prefix (no SELECT keyword) for use in format strings with DISTINCT.
const SYMBOL_COLUMNS_BARE: &str = "s.id, s.name, s.kind, s.file_id, s.file_path, s.line_start, s.col_start, \
     s.line_end, s.col_end, s.visibility, s.signature, s.doc_comment, s.module_path, s.scope_context";

const SYMBOL_SELECT_BY_ID: &str = "SELECT id, name, kind, file_id, file_path, line_start, col_start, \
     line_end, col_end, visibility, signature, doc_comment, module_path, scope_context \
     FROM code_symbols WHERE id = ?1";

const SYMBOL_SELECT_BY_NAME: &str = "SELECT id, name, kind, file_id, file_path, line_start, col_start, \
     line_end, col_end, visibility, signature, doc_comment, module_path, scope_context \
     FROM code_symbols WHERE name = ?1";

const SYMBOL_SELECT_FTS: &str = "SELECT s.id, s.name, s.kind, s.file_id, s.file_path, s.line_start, s.col_start, \
     s.line_end, s.col_end, s.visibility, s.signature, s.doc_comment, s.module_path, s.scope_context \
     FROM code_symbols_fts f \
     JOIN code_symbols s ON s.id = f.rowid \
     WHERE code_symbols_fts MATCH ?1 \
     LIMIT ?2";

/// Convert a row from the standard 14-column symbol SELECT to a `Symbol`.
fn row_to_symbol(row: &rusqlite::Row<'_>) -> rusqlite::Result<Symbol> {
    let id: i64 = row.get(0)?;
    let name: String = row.get(1)?;
    let kind_str: String = row.get(2)?;
    let file_id: i64 = row.get(3)?;
    let file_path: String = row.get(4)?;
    let line_start: u32 = row.get(5)?;
    let col_start: Option<i64> = row.get(6)?;
    let line_end: Option<u32> = row.get(7)?;
    let col_end: Option<i64> = row.get(8)?;
    let visibility_val: i64 = row.get(9)?;
    let signature: Option<String> = row.get(10)?;
    let doc_comment: Option<String> = row.get(11)?;
    let module_path: Option<String> = row.get(12)?;
    let scope_context_str: Option<String> = row.get(13)?;

    let symbol_id = u32::try_from(id)
        .ok()
        .and_then(SymbolId::new)
        .ok_or_else(|| rusqlite::Error::IntegralValueOutOfRange(0, id))?;
    let kind = kind_str.parse::<SymbolKind>().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            format!("Unknown SymbolKind: {kind_str}").into(),
        )
    })?;
    let fid = u32::try_from(file_id)
        .ok()
        .and_then(FileId::new)
        .ok_or_else(|| rusqlite::Error::IntegralValueOutOfRange(3, file_id))?;
    let range = Range::new(
        line_start,
        col_start.map_or(0, |v| v as u16),
        line_end.unwrap_or(line_start),
        col_end.map_or(0, |v| v as u16),
    );
    let visibility = match visibility_val {
        0 => Visibility::Public,
        1 => Visibility::Crate,
        2 => Visibility::Module,
        _ => Visibility::Private,
    };
    let scope_context = scope_context_str.and_then(|s| {
        serde_json::from_str(&s)
            .map_err(|e| {
                tracing::warn!("Malformed scope_context JSON for symbol {id}: {e}");
                e
            })
            .ok()
    });

    let mut sym = Symbol::new(symbol_id, &*name, kind, fid, range)
        .with_file_path(file_path)
        .with_visibility(visibility);

    if let Some(sig) = signature {
        sym = sym.with_signature(sig);
    }
    if let Some(doc) = doc_comment {
        sym = sym.with_doc(doc);
    }
    if let Some(mp) = module_path {
        sym = sym.with_module_path(mp);
    }
    if let Some(sc) = scope_context {
        sym = sym.with_scope(sc);
    }

    Ok(sym)
}

impl fmt::Debug for CodeDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempfile::TempDir, CodeDb) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        let db = CodeDb::create(&path).unwrap();
        (dir, db)
    }

    /// Insert a test file and return its rowid.
    fn insert_test_file(db: &CodeDb) -> i64 {
        db.insert_file("test.rs", "test.rs", "hash123", Some("Rust"), None)
            .unwrap()
    }

    #[test]
    fn test_create_and_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");

        let db = CodeDb::create(&path).unwrap();
        assert!(path.exists());
        assert_eq!(db.path(), path);
        drop(db);

        // Re-open
        let db = CodeDb::open(&path).unwrap();
        assert_eq!(db.path(), path);
    }

    #[test]
    fn test_create_fails_if_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        CodeDb::create(&path).unwrap();
        assert!(CodeDb::create(&path).is_err());
    }

    #[test]
    fn test_open_or_create_creates_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        let db = CodeDb::open_or_create(&path).unwrap();
        assert!(path.exists());
        assert_eq!(db.file_count().unwrap(), 0);
    }

    #[test]
    fn test_open_or_create_opens_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        {
            let db = CodeDb::create(&path).unwrap();
            db.insert_file("a", "a", "h", None, None).unwrap();
        }
        let db = CodeDb::open_or_create(&path).unwrap();
        assert_eq!(db.file_count().unwrap(), 1);
    }

    #[test]
    fn test_insert_file() {
        let (_dir, db) = temp_db();
        let id = db
            .insert_file("main.rs", "main.rs", "abc", Some("Rust"), Some(12345))
            .unwrap();
        assert!(id > 0);
        assert_eq!(db.file_count().unwrap(), 1);
    }

    #[test]
    fn test_insert_file_replaces_on_conflict() {
        let (_dir, db) = temp_db();
        db.insert_file("main.rs", "main.rs", "hash1", None, None)
            .unwrap();
        db.insert_file("main.rs", "main.rs", "hash2", None, None)
            .unwrap();
        assert_eq!(db.file_count().unwrap(), 1);
        let hashes = db.get_file_hashes().unwrap();
        assert_eq!(hashes.get("main.rs").unwrap(), "hash2");
    }

    #[test]
    fn test_get_file_hashes() {
        let (_dir, db) = temp_db();
        db.insert_file("a", "a", "h1", None, None).unwrap();
        db.insert_file("b", "b", "h2", None, None).unwrap();

        let hashes = db.get_file_hashes().unwrap();
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes.get("a").unwrap(), "h1");
        assert_eq!(hashes.get("b").unwrap(), "h2");
    }

    #[test]
    fn test_delete_by_file() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "my_fn", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();
        assert_eq!(db.symbol_count().unwrap(), 1);

        let deleted = db.delete_by_file("test.rs").unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(db.file_count().unwrap(), 0);
        assert_eq!(db.symbol_count().unwrap(), 0);
    }

    #[test]
    fn test_insert_symbol() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let sym_id = db
            .insert_symbol(
                "process_data",
                "Function",
                file_id,
                "test.rs",
                15,
                Some(4),
                Some(30),
                Some(1),
                0,
                Some("fn process_data(input: &[u8]) -> Vec<u8>"),
                Some("Process raw data into output."),
                Some("crate::processor"),
                None,
            )
            .unwrap();
        assert!(sym_id > 0);
        assert_eq!(db.symbol_count().unwrap(), 1);
    }

    #[test]
    fn test_insert_relationship() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let sym_id = db
            .insert_symbol(
                "caller", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        let rel_id = db
            .insert_relationship(
                Some(sym_id),
                "caller",
                "callee",
                "Calls",
                file_id,
                Some(5),
                Some(10),
            )
            .unwrap();
        assert!(rel_id > 0);
        assert_eq!(db.relationship_count().unwrap(), 1);
    }

    #[test]
    fn test_get_symbol() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let sym_id = db
            .insert_symbol(
                "my_struct",
                "Struct",
                file_id,
                "test.rs",
                20,
                Some(0),
                Some(50),
                Some(1),
                0,
                Some("pub struct MyStruct"),
                Some("A test struct."),
                Some("crate::models"),
                None,
            )
            .unwrap();

        let sym = db.get_symbol(sym_id).unwrap().unwrap();
        assert_eq!(sym.as_name(), "my_struct");
        assert_eq!(sym.kind, SymbolKind::Struct);
        assert_eq!(sym.range.start_line, 20);
        assert_eq!(sym.range.end_line, 50);
        assert_eq!(sym.as_signature(), Some("pub struct MyStruct"));
        assert_eq!(sym.as_doc_comment(), Some("A test struct."));
        assert_eq!(sym.visibility, Visibility::Public);
    }

    #[test]
    fn test_get_symbol_not_found() {
        let (_dir, db) = temp_db();
        assert!(db.get_symbol(999).unwrap().is_none());
    }

    #[test]
    fn test_find_symbols_by_name() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "process", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();
        db.insert_symbol(
            "process", "Method", file_id, "test.rs", 20, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();
        db.insert_symbol(
            "other", "Function", file_id, "test.rs", 40, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();

        let results = db.find_symbols_by_name("process").unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_symbols_trigram() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "ArchiveAppService",
            "Struct",
            file_id,
            "test.rs",
            1,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        db.insert_symbol(
            "UserService",
            "Struct",
            file_id,
            "test.rs",
            20,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        db.insert_symbol(
            "LogHandler",
            "Function",
            file_id,
            "test.rs",
            40,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // Substring "Archive" should match only ArchiveAppService
        let results = db.search_symbols("Archive", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_name(), "ArchiveAppService");

        // Substring "Service" should match both *Service structs
        let results = db.search_symbols("Service", 10).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_symbols_short_query_fallback() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "go", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
        )
        .unwrap();
        db.insert_symbol(
            "gopher", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();

        // "go" is < 3 chars, should fall back to LIKE
        let results = db.search_symbols("go", 10).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_all_symbols() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "a", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
        )
        .unwrap();
        db.insert_symbol(
            "b", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None, None, None,
        )
        .unwrap();

        let all = db.all_symbols().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_get_symbols_batch() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let id1 = db
            .insert_symbol(
                "a", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
            )
            .unwrap();
        let id2 = db
            .insert_symbol(
                "b", "Struct", file_id, "test.rs", 10, None, None, None, 0, None, None, None, None,
            )
            .unwrap();
        let _id3 = db
            .insert_symbol(
                "c", "Enum", file_id, "test.rs", 20, None, None, None, 0, None, None, None, None,
            )
            .unwrap();

        let batch = db.get_symbols_batch(&[id1, id2]).unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[&id1].as_name(), "a");
        assert_eq!(batch[&id2].as_name(), "b");
    }

    #[test]
    fn test_get_symbols_batch_empty() {
        let (_dir, db) = temp_db();
        let batch = db.get_symbols_batch(&[]).unwrap();
        assert!(batch.is_empty());
    }

    #[test]
    fn test_get_called_functions() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let caller_id = db
            .insert_symbol(
                "caller", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        db.insert_symbol(
            "callee_a", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();
        db.insert_symbol(
            "callee_b", "Function", file_id, "test.rs", 20, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();
        db.insert_relationship(
            Some(caller_id),
            "caller",
            "callee_a",
            "Calls",
            file_id,
            None,
            None,
        )
        .unwrap();
        db.insert_relationship(
            Some(caller_id),
            "caller",
            "callee_b",
            "Calls",
            file_id,
            None,
            None,
        )
        .unwrap();

        let called = db.get_called_functions(caller_id).unwrap();
        assert_eq!(called.len(), 2);
        let names: Vec<&str> = called.iter().map(|s| s.as_name()).collect();
        assert!(names.contains(&"callee_a"));
        assert!(names.contains(&"callee_b"));
    }

    #[test]
    fn test_get_calling_functions() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let caller_id = db
            .insert_symbol(
                "caller", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        db.insert_symbol(
            "callee", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();
        db.insert_relationship(
            Some(caller_id),
            "caller",
            "callee",
            "Calls",
            file_id,
            None,
            None,
        )
        .unwrap();

        let callers = db.get_calling_functions("callee").unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].as_name(), "caller");
    }

    #[test]
    fn test_get_impact_radius() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);

        // Chain: c -> b -> a (c calls b, b calls a)
        let _a_id = db
            .insert_symbol(
                "a", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
            )
            .unwrap();
        let b_id = db
            .insert_symbol(
                "b", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        let c_id = db
            .insert_symbol(
                "c", "Function", file_id, "test.rs", 20, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();

        db.insert_relationship(Some(b_id), "b", "a", "Calls", file_id, None, None)
            .unwrap();
        db.insert_relationship(Some(c_id), "c", "b", "Calls", file_id, None, None)
            .unwrap();

        // Impact of "a" with depth 0: just "b" (direct caller)
        let impact = db.get_impact_radius("a", 0).unwrap();
        assert_eq!(impact.len(), 1);
        assert_eq!(impact[0].as_name(), "b");

        // Impact of "a" with depth 1: "b" and "c" (transitive)
        let impact = db.get_impact_radius("a", 1).unwrap();
        assert_eq!(impact.len(), 2);
        let names: Vec<&str> = impact.iter().map(|s| s.as_name()).collect();
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
    }

    #[test]
    fn test_clear() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "a", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
        )
        .unwrap();
        db.insert_relationship(None, "a", "b", "Calls", file_id, None, None)
            .unwrap();

        db.clear().unwrap();
        assert_eq!(db.file_count().unwrap(), 0);
        assert_eq!(db.symbol_count().unwrap(), 0);
        assert_eq!(db.relationship_count().unwrap(), 0);
    }

    #[test]
    fn test_cascade_delete_with_relationships() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let sym_id = db
            .insert_symbol(
                "fn1", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        db.insert_relationship(Some(sym_id), "fn1", "fn2", "Calls", file_id, None, None)
            .unwrap();

        assert_eq!(db.symbol_count().unwrap(), 1);
        assert_eq!(db.relationship_count().unwrap(), 1);

        db.delete_by_file("test.rs").unwrap();
        assert_eq!(db.symbol_count().unwrap(), 0);
        assert_eq!(db.relationship_count().unwrap(), 0);
    }

    #[test]
    fn test_counts() {
        let (_dir, db) = temp_db();
        assert_eq!(db.file_count().unwrap(), 0);
        assert_eq!(db.symbol_count().unwrap(), 0);
        assert_eq!(db.relationship_count().unwrap(), 0);

        let file_id = insert_test_file(&db);
        assert_eq!(db.file_count().unwrap(), 1);

        db.insert_symbol(
            "a", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
        )
        .unwrap();
        assert_eq!(db.symbol_count().unwrap(), 1);

        db.insert_relationship(None, "a", "b", "Calls", file_id, None, None)
            .unwrap();
        assert_eq!(db.relationship_count().unwrap(), 1);
    }

    #[test]
    fn test_get_file_mtimes() {
        let (_dir, db) = temp_db();
        db.insert_file("a", "a", "h1", None, Some(1000)).unwrap();
        db.insert_file("b", "b", "h2", None, Some(2000)).unwrap();
        db.insert_file("c", "c", "h3", None, None).unwrap();

        let mtimes = db.get_file_mtimes().unwrap();
        assert_eq!(mtimes.len(), 2); // "c" excluded (NULL mtime)
        assert_eq!(mtimes["a"], 1000);
        assert_eq!(mtimes["b"], 2000);
    }

    #[test]
    fn test_fts5_sync_after_symbol_delete() {
        // Verifies FTS5 index stays in sync when symbols are deleted
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "ArchiveService",
            "Struct",
            file_id,
            "test.rs",
            1,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // FTS5 should find it
        let results = db.search_symbols("Archive", 10).unwrap();
        assert_eq!(results.len(), 1);

        // Delete the file (which deletes symbols first, firing FTS5 triggers)
        db.delete_by_file("test.rs").unwrap();

        // FTS5 should no longer find it
        let results = db.search_symbols("Archive", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_symbols_escapes_quotes() {
        // FTS5 query with double-quotes should not cause SQL errors
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "MyService",
            "Struct",
            file_id,
            "test.rs",
            1,
            None,
            None,
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        // Query containing quotes should not panic or error
        let results = db.search_symbols(r#"My"Service"#, 10).unwrap();
        // May or may not match depending on escaping, but must not error
        assert!(results.len() <= 1);
    }

    #[test]
    fn test_search_symbols_escapes_like_wildcards() {
        // LIKE metacharacters should be treated as literals
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "go", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
        )
        .unwrap();
        db.insert_symbol(
            "gx", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None, None, None,
        )
        .unwrap();

        // "g%" would match both if unescaped; should match neither as literal "g%"
        let results = db.search_symbols("g%", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_row_to_symbol_unknown_kind_errors() {
        // Inserting a symbol with a kind that doesn't parse should cause an error on retrieval
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        // Insert directly with an invalid kind string
        db.conn
            .execute(
                "INSERT INTO code_symbols (name, kind, file_id, file_path, line_start, visibility) \
             VALUES ('bad', 'Unicorn', ?1, 'test.rs', 1, 0)",
                [file_id],
            )
            .unwrap();

        // get_symbol should return an error, not silently default
        let result = db.get_symbol(1);
        assert!(result.is_err() || result.unwrap().is_none());
    }

    #[test]
    fn test_insert_file_upsert_preserves_symbols() {
        // ON CONFLICT DO UPDATE should not cascade-delete symbols
        let (_dir, db) = temp_db();
        let file_id = db
            .insert_file("main.rs", "main.rs", "hash1", Some("Rust"), None)
            .unwrap();
        db.insert_symbol(
            "my_fn", "Function", file_id, "main.rs", 10, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();
        assert_eq!(db.symbol_count().unwrap(), 1);

        // Upsert the same file with a new hash
        db.insert_file("main.rs", "main.rs", "hash2", Some("Rust"), None)
            .unwrap();

        // Symbol should still exist (not cascade-deleted by INSERT OR REPLACE)
        assert_eq!(db.symbol_count().unwrap(), 1);
    }

    #[test]
    fn test_search_symbols_empty_db() {
        let (_dir, db) = temp_db();
        let results = db.search_symbols("anything", 10).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_get_impact_radius_no_callers() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "orphan", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();

        let impact = db.get_impact_radius("orphan", 5).unwrap();
        assert!(impact.is_empty());
    }

    #[test]
    fn test_get_impact_radius_cycle() {
        // a calls b, b calls a — should not infinite-loop
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let a_id = db
            .insert_symbol(
                "a", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
            )
            .unwrap();
        let b_id = db
            .insert_symbol(
                "b", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        db.insert_relationship(Some(a_id), "a", "b", "Calls", file_id, None, None)
            .unwrap();
        db.insert_relationship(Some(b_id), "b", "a", "Calls", file_id, None, None)
            .unwrap();

        // UNION in the CTE deduplicates, so this should terminate
        let impact = db.get_impact_radius("a", 10).unwrap();
        assert!(impact.len() <= 2);
    }

    #[test]
    fn test_find_symbols_by_name_not_found() {
        let (_dir, db) = temp_db();
        let results = db.find_symbols_by_name("nonexistent").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_delete_by_file_nonexistent() {
        let (_dir, db) = temp_db();
        // Should not error when deleting a file that doesn't exist
        let deleted = db.delete_by_file("nonexistent").unwrap();
        assert_eq!(deleted, 0);
    }
}
