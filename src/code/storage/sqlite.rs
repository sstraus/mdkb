//! SQLite-backed storage for the code intelligence index.
//!
//! Provides a single SQLite database that stores symbols, relationships,
//! and file metadata. FTS5 with trigram tokenizer enables substring
//! matching on symbol names ("Archive" finds "ArchiveAppService").

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, params};

use crate::code::relationship::CallTarget;
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolId, SymbolKind};
use crate::store::heal::Soundness;

use super::schema;

/// How [`CodeDb::query_symbols`] matches the symbol name.
#[derive(Debug, Clone, Copy)]
pub enum NameMatch<'a> {
    /// Exact name equality — what `code find` looks up.
    Exact(&'a str),
    /// Substring match over name, signature and doc comment via the FTS5
    /// trigram index — what `search --scope symbols` looks up.
    Fuzzy(&'a str),
    /// No name predicate; only the kind and file filters narrow the set.
    Any,
}

/// What a call site said about its target, beyond the name.
///
/// One argument rather than four, because the four are one fact — "what does
/// the source say about where this call goes" — and they arrive together from
/// the parser, travel together through the pipeline and are read together by
/// the resolver. A relationship that is not a call carries
/// [`CallSite::default`], which says nothing and resolves by name alone.
#[derive(Debug, Default, Clone, Copy)]
pub struct CallSite<'a> {
    /// Everything written before the target's last separator, `None` for a
    /// bare name — see
    /// [`split_call_target`](crate::code::parsing::parser::split_call_target).
    /// It narrows the target and never widens it.
    pub qualifier: Option<&'a str>,
    /// The expression a method was called on, as written: `self.db`, `temp`,
    /// `build()`.
    pub receiver: Option<&'a str>,
    /// The name of the receiver's type, when the file the call is in says it.
    pub receiver_type: Option<&'a str>,
    /// The function whose return type is the receiver's type, when the file
    /// only names that. Resolved into `receiver_type` once every file is in
    /// the index — see
    /// [`resolve_types`](crate::code::indexing::receiver::resolve_types).
    pub receiver_call: Option<&'a str>,
}

/// SQLite-backed code intelligence index.
///
/// Write-capable constructors initialize the schema; [`CodeDb::open_read_only`]
/// trusts the existing schema and performs no mutation or repair.
pub struct CodeDb {
    conn: Connection,
    path: PathBuf,
    /// Shared advisory lock announcing this connection so nobody quarantines
    /// (renames) the database files underneath it. Never read — it only has to
    /// live as long as the connection does.
    _live_guard: Option<crate::store::mutation_lock::MutationGuard>,
}

impl CodeDb {
    /// Create a new database at `path`. Fails if the file already exists.
    pub fn create(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let _guard = acquire_code_lock(&path, "code-create")?;
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
        let live_guard = acquire_live_code_lock(&path)?;
        let conn = Connection::open(&path)?;
        schema::init_schema(&conn)?;
        Ok(Self {
            conn,
            path,
            _live_guard: Some(live_guard),
        })
    }

    /// Open an existing database at `path`.
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let _guard = acquire_code_lock(&path, "code-integrity-check")?;
        // Probe before announcing this connection, so our own guard cannot veto
        // the quarantine we are about to perform.
        quarantine_if_corrupt(&path)?;
        let live_guard = acquire_live_code_lock(&path)?;
        let conn = Connection::open(&path)?;
        schema::init_schema(&conn)?;
        Ok(Self {
            conn,
            path,
            _live_guard: Some(live_guard),
        })
    }

    /// Open an existing code index for queries only.
    ///
    /// This deliberately skips the mutation/live locks, integrity quarantine,
    /// schema initialization, migrations, and repairs performed by [`Self::open`].
    /// `SQLITE_OPEN_READ_ONLY` is the enforcement boundary: a query path cannot
    /// accidentally become a second writer if code inside it changes later.
    /// SQLite also refuses a missing path under these flags, so a read never
    /// creates an empty index and hides the fact that indexing has not run.
    pub fn open_read_only(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self {
            conn,
            path,
            _live_guard: None,
        })
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
    ///
    /// Pre-deletes any row with the same `rel_path` but different `path` to
    /// handle the absolute→relative path migration (old rows had `path` as
    /// absolute, new code uses `rel_path` for both).
    pub fn insert_file(
        &self,
        path: &str,
        rel_path: &str,
        hash: &str,
        language: Option<&str>,
        mtime: Option<i64>,
        token_estimate: Option<i64>,
    ) -> rusqlite::Result<i64> {
        // Clean up legacy rows where path was absolute but rel_path matches.
        // FTS triggers require explicit symbol delete before file delete.
        self.conn.execute(
            "DELETE FROM code_symbols WHERE file_id IN \
             (SELECT id FROM code_files WHERE rel_path = ?1 AND path != ?2)",
            params![rel_path, path],
        )?;
        self.conn.execute(
            "DELETE FROM code_files WHERE rel_path = ?1 AND path != ?2",
            params![rel_path, path],
        )?;
        // RETURNING id yields the affected row's rowid for BOTH the INSERT and the
        // ON CONFLICT DO UPDATE path. `last_insert_rowid()` must NOT be used here:
        // on an UPDATE it is unchanged, so it returns a stale rowid from an earlier
        // insert on this connection, poisoning the caller's file_id map and raising
        // "FOREIGN KEY constraint failed" (or silently misattributing symbols).
        let id: i64 = self.conn.query_row(
            "INSERT INTO code_files (path, rel_path, hash, language, mtime, indexed_at, token_estimate) \
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch(), ?6) \
             ON CONFLICT(path) DO UPDATE SET \
             rel_path=excluded.rel_path, hash=excluded.hash, \
             language=excluded.language, mtime=excluded.mtime, \
             indexed_at=unixepoch(), token_estimate=excluded.token_estimate \
             RETURNING id",
            params![path, rel_path, hash, language, mtime, token_estimate],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Record whether tree-sitter recovered from a syntax error in this file.
    ///
    /// The file row is inserted before symbols and relationships, so this is a
    /// separate update rather than widening `insert_file`'s widely used API.
    pub fn set_file_has_error(&self, file_id: i64, has_error: bool) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE code_files SET has_error = ?2 WHERE id = ?1",
            params![file_id, has_error],
        )?;
        Ok(())
    }

    /// Record an import statement found in `file_id`.
    ///
    /// `OR IGNORE` against `idx_imports_unique`: a file re-indexed without its
    /// row being deleted first would otherwise accumulate a second copy of every
    /// import, and a file may legitimately name the same path twice.
    pub fn insert_import(
        &self,
        file_id: i64,
        path: &str,
        alias: Option<&str>,
        is_glob: bool,
        is_type_only: bool,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO code_imports (file_id, path, alias, is_glob, is_type_only) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![file_id, path, alias, is_glob, is_type_only],
        )?;
        Ok(())
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

    /// Record that a code index scan completed successfully.
    pub fn mark_index_scan_completed(&self) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO code_metadata (key, value) VALUES (?1, unixepoch()) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [schema::LAST_INDEX_SCAN_KEY],
        )?;
        Ok(())
    }

    /// Timestamp of the last completed code index scan.
    pub fn last_index_scan_at(&self) -> rusqlite::Result<Option<i64>> {
        schema::last_index_scan_at(&self.conn)
    }

    /// Look up file token estimates for a set of relative paths.
    ///
    /// Returns a map containing only paths with a known (non-NULL) estimate.
    /// Chunks into groups of 999 to stay within SQLite's variable limit.
    pub fn get_file_token_estimates(
        &self,
        rel_paths: &[String],
    ) -> rusqlite::Result<HashMap<String, u32>> {
        if rel_paths.is_empty() {
            return Ok(HashMap::new());
        }
        let mut map = HashMap::new();
        for chunk in rel_paths.chunks(999) {
            let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT rel_path, token_estimate FROM code_files \
                 WHERE token_estimate IS NOT NULL AND rel_path IN ({})",
                placeholders.join(",")
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let sql_params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .map(|p| p as &dyn rusqlite::types::ToSql)
                .collect();
            let rows = stmt.query_map(sql_params.as_slice(), |row| {
                let path: String = row.get(0)?;
                let est: i64 = row.get(1)?;
                Ok((path, est.max(0) as u32))
            })?;
            for row in rows {
                let (p, e) = row?;
                map.insert(p, e);
            }
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
        // `owner_name` is projected out of the scope JSON by SQLite rather than
        // passed in beside it. A member's owner is already in `scope_context`,
        // and the column exists only so the resolver can match it through an
        // index — `LIKE '%"class_name":"X"%'` cannot use one, and would turn
        // every edge resolution into a full scan of `code_symbols`. Projecting
        // it here is what keeps the two from ever disagreeing.
        self.conn.execute(
            "INSERT OR REPLACE INTO code_symbols \
             (name, kind, file_id, file_path, line_start, col_start, line_end, col_end, \
              visibility, signature, doc_comment, module_path, scope_context, owner_name) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
                     json_extract(?13, '$.ClassMember.class_name'))",
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
    ///
    /// `site` is what the call site said about its target beyond the name —
    /// empty ([`CallSite::default`]) for every relationship that is not a
    /// call.
    ///
    /// The row is eleven columns wide and the four that describe the target
    /// are already grouped; the rest name different things (who, what, where,
    /// which kind) and bundling them further would only hide them behind a
    /// struct nobody else builds.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_relationship(
        &self,
        from_symbol_id: Option<i64>,
        from_name: &str,
        to_name: &str,
        site: &CallSite<'_>,
        kind: &str,
        file_id: i64,
        to_position: (Option<u32>, Option<u16>),
    ) -> rusqlite::Result<i64> {
        let (to_line, to_col) = to_position;
        self.conn.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_line, to_col, to_qualifier, \
              to_receiver, to_receiver_type, to_receiver_call) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                from_symbol_id,
                from_name,
                to_name,
                kind,
                file_id,
                to_line,
                to_col.map(i64::from),
                site.qualifier,
                site.receiver,
                site.receiver_type,
                site.receiver_call,
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
        let rows = stmt.query_map([name], row_to_symbol)?;
        rows.collect()
    }

    /// Symbol lookup with every filter applied in SQL.
    ///
    /// Returns the capped rows plus the total number of matches before the cap.
    ///
    /// Doing the filtering in SQL is what makes both numbers honest. Fetching
    /// `limit` rows and then dropping the wrong kinds in Rust returns fewer
    /// symbols than exist, because the cap already excluded the rows the filter
    /// would have kept — and it leaves no way to know the real total.
    pub fn query_symbols(
        &self,
        name: NameMatch<'_>,
        kind: Option<&str>,
        file_pattern: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<(Vec<Symbol>, usize)> {
        let mut predicates: Vec<&str> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        match name {
            NameMatch::Exact(n) => {
                predicates.push("s.name = ?");
                args.push(Box::new(n.to_string()));
            }
            // Trigram needs >= 3 characters; below that, LIKE on the name.
            NameMatch::Fuzzy(q) if q.len() < 3 => {
                predicates.push("s.name LIKE ? ESCAPE '\\'");
                args.push(Box::new(like_pattern(q)));
            }
            NameMatch::Fuzzy(q) => {
                predicates.push(
                    "s.id IN (SELECT rowid FROM code_symbols_fts WHERE code_symbols_fts MATCH ?)",
                );
                // Quote the term so FTS5 treats it as a literal, not syntax.
                args.push(Box::new(format!("\"{}\"", q.replace('"', "\"\""))));
            }
            NameMatch::Any => {}
        }

        if let Some(kind) = kind {
            predicates.push("s.kind = ?");
            args.push(Box::new(kind.to_string()));
        }

        if let Some(file_pattern) = file_pattern {
            predicates.push("s.file_path LIKE ? ESCAPE '\\'");
            args.push(Box::new(like_pattern(file_pattern)));
        }

        let where_clause = if predicates.is_empty() {
            "1=1".to_string()
        } else {
            predicates.join(" AND ")
        };

        let filter_args: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| a.as_ref()).collect();

        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM code_symbols s WHERE {where_clause}"),
            filter_args.as_slice(),
            |row| row.get(0),
        )?;

        let limit = limit as i64;
        let mut row_args = filter_args;
        row_args.push(&limit);

        let mut stmt = self.conn.prepare_cached(&format!(
            "{SYMBOL_COLUMNS_PREFIXED} FROM code_symbols s WHERE {where_clause} \
             ORDER BY s.file_path, s.line_start LIMIT ?"
        ))?;
        let rows = stmt.query_map(row_args.as_slice(), row_to_symbol)?;
        Ok((rows.collect::<rusqlite::Result<Vec<_>>>()?, total as usize))
    }

    /// Substring search on symbol names/signatures/doc_comments via FTS5 trigram.
    ///
    /// For queries shorter than 3 characters, falls back to LIKE.
    pub fn search_symbols(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<Symbol>> {
        Ok(self
            .query_symbols(NameMatch::Fuzzy(query), None, None, limit)?
            .0)
    }

    /// Find all symbols in files matching a path substring.
    pub fn find_symbols_by_file(
        &self,
        file_pattern: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<Symbol>> {
        Ok(self
            .query_symbols(NameMatch::Any, None, Some(file_pattern), limit)?
            .0)
    }

    /// Get all symbols in a specific file, ordered by line_start (for outline view).
    pub fn symbols_in_file_ordered(&self, rel_path: &str) -> rusqlite::Result<Vec<Symbol>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "{SYMBOL_COLUMNS} FROM code_symbols WHERE file_path = ?1 ORDER BY line_start, col_start"
        ))?;
        let rows = stmt.query_map(params![rel_path], row_to_symbol)?;
        rows.collect()
    }

    /// Find the innermost symbol enclosing a given position.
    ///
    /// `line` is 0-based, because `line_start`/`line_end` hold tree-sitter rows
    /// verbatim (see `stage_index`, which passes `range.start_line` through). A
    /// caller holding a 1-based line — anything a human or an editor produced —
    /// must subtract one first, or this returns the symbol on the line below.
    pub fn symbol_at_position(
        &self,
        rel_path: &str,
        line: u32,
        _col: Option<u32>,
    ) -> rusqlite::Result<Option<Symbol>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "{SYMBOL_COLUMNS} FROM code_symbols \
             WHERE file_path = ?1 AND line_start <= ?2 AND (line_end IS NULL OR line_end >= ?2) \
             ORDER BY (COALESCE(line_end, line_start) - line_start) ASC \
             LIMIT 1"
        ))?;
        let mut rows = stmt.query_map(params![rel_path, line], row_to_symbol)?;
        match rows.next() {
            Some(Ok(sym)) => Ok(Some(sym)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
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
        let rows = stmt.query_map([], row_to_symbol)?;
        rows.collect()
    }

    /// Every live symbol id.
    ///
    /// One id column rather than [`Self::all_symbols`] because the caller that
    /// needs this — the vector-store sweep — only asks "does this id still
    /// exist", and building the full symbols would read every name, signature
    /// and doc comment to answer it.
    pub fn all_symbol_ids(&self) -> rusqlite::Result<HashSet<u32>> {
        let mut stmt = self.conn.prepare("SELECT id FROM code_symbols")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        rows.map(|id| id.map(|id| id as u32)).collect()
    }

    /// Get symbols whose `file_path` is in `paths`.
    ///
    /// Chunks into groups of 999 to stay within SQLite's variable limit.
    pub fn symbols_for_files(&self, paths: &[&str]) -> rusqlite::Result<Vec<Symbol>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        for chunk in paths.chunks(999) {
            let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "{SYMBOL_COLUMNS} FROM code_symbols WHERE file_path IN ({})",
                placeholders.join(",")
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let sql_params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .map(|p| p as &dyn rusqlite::types::ToSql)
                .collect();
            let rows = stmt.query_map(sql_params.as_slice(), row_to_symbol)?;
            for row in rows {
                result.push(row?);
            }
        }
        Ok(result)
    }

    /// Batch lookup of symbols by their IDs.
    ///
    /// Chunks into groups of 999 to stay within SQLite's variable limit.
    pub fn get_symbols_batch(&self, ids: &[i64]) -> rusqlite::Result<HashMap<i64, Symbol>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut map = HashMap::new();
        for chunk in ids.chunks(999) {
            let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "{SYMBOL_COLUMNS} FROM code_symbols WHERE id IN ({})",
                placeholders.join(",")
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let sql_params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .map(|id| id as &dyn rusqlite::types::ToSql)
                .collect();
            let rows = stmt.query_map(sql_params.as_slice(), |row| {
                let sym = row_to_symbol(row)?;
                let id: i64 = row.get(0)?;
                Ok((id, sym))
            })?;
            for row in rows {
                let (id, sym) = row?;
                map.insert(id, sym);
            }
        }
        Ok(map)
    }

    // --- Relationship queries ---

    /// Every `Calls` edge leaving `symbol_id`, classified.
    ///
    /// One entry per edge, in call-site order. Unlike
    /// This never silently drops an edge: a call the index cannot place comes
    /// back as `External` or `Unknown` rather than as nothing, which is the
    /// difference between "this calls nothing here" and "this calls nothing".
    pub fn get_call_targets(&self, symbol_id: i64) -> rusqlite::Result<Vec<CallTarget>> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT r.id, r.to_name, r.to_qualifier, r.to_receiver_type, \
                    s.id, {RESOLUTION_TIER} \
             FROM code_relationships r \
             JOIN code_symbols fs ON fs.id = r.from_symbol_id \
             LEFT JOIN code_symbols s ON s.name = r.to_name AND {callable} \
             WHERE r.kind = 'Calls' AND r.from_symbol_id = ?1 \
             ORDER BY r.id",
            callable = callable_kind_predicate("s.kind")
        ))?;
        let rows = stmt.query_map([symbol_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;

        // Grouped in Rust rather than in SQL: the classification needs the whole
        // candidate set of an edge at once, and the tier that wins is the
        // nearest any candidate reached.
        let mut edges: Vec<CandidateEdge> = Vec::new();
        for row in rows {
            let (rid, name, qualifier, receiver_type, sym_id, tier) = row?;
            if edges.last().map(|edge| edge.id) != Some(rid) {
                edges.push(CandidateEdge {
                    id: rid,
                    name,
                    qualifier,
                    receiver_type,
                    candidates: Vec::new(),
                });
            }
            if let Some(sym_id) = sym_id {
                edges
                    .last_mut()
                    .expect("just pushed")
                    .candidates
                    .push((tier, sym_id));
            }
        }

        Ok(edges
            .into_iter()
            .map(|edge| {
                let CandidateEdge {
                    name,
                    qualifier,
                    receiver_type,
                    candidates,
                    ..
                } = edge;
                let nearest = candidates.iter().map(|(tier, _)| *tier).min();
                let named_owner = qualifier.or(receiver_type);
                match (nearest, named_owner) {
                    (Some(TIER_EXTERNAL) | None, Some(qualifier)) => {
                        CallTarget::External { qualifier, name }
                    }
                    (Some(TIER_EXTERNAL) | None, None) => CallTarget::Unknown { name },
                    (Some(nearest), _) => CallTarget::Resolved {
                        tier: nearest,
                        targets: candidates
                            .iter()
                            .filter(|(tier, _)| *tier == nearest)
                            .filter_map(|(_, id)| SymbolId::new(*id as u32))
                            .collect(),
                    },
                }
            })
            .collect())
    }

    /// Get symbols whose calls resolve to the given symbol.
    ///
    /// Takes an id, not a name: two symbols can share a name, and which of them
    /// a call meant is the whole question the cascade answers.
    pub fn get_calling_functions(&self, symbol_id: i64) -> rusqlite::Result<Vec<Symbol>> {
        Ok(self
            .get_callers_by_tier(symbol_id)?
            .into_iter()
            .map(|(caller, _)| caller)
            .collect())
    }

    /// Callers of `symbol_id`, each with the nearest tier its call reached.
    ///
    /// [`TIER_UNPLACED`] means no rule placed the call: the caller wrote a bare
    /// name that this symbol happens to match, and so may every other symbol of
    /// that name. The count of those is the only thing worth saying on the
    /// callers side — an incoming edge is resolved by construction, so a
    /// `CallTarget` here would be `Resolved` every time and distinguish nothing.
    ///
    /// A caller reached by more than one edge keeps the nearest: one firmly
    /// placed call is enough to know it really calls this symbol.
    pub fn get_callers_by_tier(&self, symbol_id: i64) -> rusqlite::Result<Vec<(Symbol, i64)>> {
        self.callers_of_any(&[symbol_id])
    }

    /// Callers of any of `symbol_ids`, each once, at the nearest tier any of
    /// them was reached by.
    ///
    /// The whole set is asked for in one statement because a hop of the impact
    /// walk asks about every symbol it just reached, and one query per symbol
    /// runs the resolution cascade — a window function over every `Calls` edge
    /// of that name — once per symbol instead of once per hop. Measured on this
    /// repository (6310 symbols, 52 712 relationships), a depth-10 walk from a
    /// hot name issued ~2400 statements and took 1.5 s.
    ///
    /// Widening the name filter does not change any edge's tier: the candidate
    /// join is per edge, and `nearest` partitions by edge, so admitting more
    /// edges leaves each surviving row exactly as it was.
    fn callers_of_any(&self, symbol_ids: &[i64]) -> rusqlite::Result<Vec<(Symbol, i64)>> {
        let mut best: HashMap<i64, (Symbol, i64)> = HashMap::new();
        // Chunked into groups of 999 to stay within SQLite's variable limit.
        for chunk in symbol_ids.chunks(999) {
            // The same placeholders are read twice — once to narrow the edges
            // by name, once to keep the candidates that are the targets — so
            // each id is bound once.
            let placeholders = (1..=chunk.len())
                .map(|i| format!("?{i}"))
                .collect::<Vec<_>>()
                .join(",");
            let mut stmt = self.conn.prepare_cached(&format!(
                "SELECT {SYMBOL_COLUMNS_BARE}, MIN(c.tier) \
                     FROM ({}) c \
                     JOIN code_symbols s ON s.id = c.from_id \
                     WHERE c.tier = c.nearest AND c.tier <> {TIER_EXTERNAL} \
                       AND c.sym_id IN ({placeholders}) \
                     GROUP BY s.id",
                resolved_edges(&format!(
                    "r.to_name IN (SELECT name FROM code_symbols WHERE id IN ({placeholders}))"
                ))
            ))?;
            let sql_params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .map(|id| id as &dyn rusqlite::types::ToSql)
                .collect();
            let rows = stmt.query_map(sql_params.as_slice(), |row| {
                Ok((row_to_symbol(row)?, row.get::<_, i64>(14)?))
            })?;
            for row in rows {
                let (caller, tier) = row?;
                let caller_id = i64::from(caller.id.value());
                match best.get_mut(&caller_id) {
                    Some((_, nearest)) => *nearest = (*nearest).min(tier),
                    None => {
                        best.insert(caller_id, (caller, tier));
                    }
                }
            }
        }
        Ok(best.into_values().collect())
    }

    /// Transitive impact radius: all symbols that directly or indirectly
    /// depend on the given symbol, up to `max_depth` hops.
    ///
    /// Walked hop by hop through [`get_callers_by_tier`](Self::get_callers_by_tier)
    /// rather than in one recursive statement. The cascade needs a window
    /// function to pick the nearest scope per edge, and SQLite allows neither
    /// that nor a correlated subquery inside the recursive arm of a CTE.
    /// Iterating keeps one implementation of the rules instead of a second,
    /// divergent one; the cost is a query per newly reached symbol, bounded by
    /// `seen`.
    pub fn get_impact_radius(
        &self,
        symbol_id: i64,
        max_depth: u32,
    ) -> rusqlite::Result<Vec<Symbol>> {
        Ok(self
            .get_impact_by_tier(symbol_id, max_depth)?
            .reached
            .into_iter()
            .map(|(symbol, _)| symbol)
            .collect())
    }

    /// The impact radius: every symbol the walk reached, and what it refused to
    /// walk through.
    ///
    /// **Reported and expanded are two states, not one.** An arrival at
    /// [`TIER_UNPLACED`] is a bare name match — the caller wrote this name and
    /// no rule could say it meant this symbol. Reporting it is right: the
    /// reader can judge it. Walking through it is not: every symbol behind it
    /// would be reported as impacted on the strength of a name, and one hot
    /// name drags in a subtree that has nothing to do with the change.
    ///
    /// So an unplaced arrival is recorded and never expanded. A symbol first
    /// reached that way is still expanded later if a placed call reaches it —
    /// otherwise the rule would under-traverse, dropping a real path because an
    /// ambiguous one happened to arrive first. Each symbol is queued at most
    /// once, on its first expandable arrival, and carries the depth of the path
    /// that queued it: a late qualifying arrival gets no extra budget.
    ///
    /// The tier reported per symbol is the nearest arrival — the last edge,
    /// never the path. A symbol reached through a chain is not more or less
    /// certain than its last edge says, and no per-hop confidence is invented:
    /// the tiers are ordinal categories, not probabilities.
    pub fn get_impact_by_tier(
        &self,
        symbol_id: i64,
        max_depth: u32,
    ) -> rusqlite::Result<ImpactRadius> {
        /// One symbol the walk reached, and how.
        struct Visit {
            symbol: Symbol,
            /// The nearest tier any arrival reached it by.
            best_arrival_tier: i64,
            /// Whether it was ever queued for expansion. Once true it never
            /// queues again, so no symbol is walked twice however many
            /// arrivals reach it.
            queued_expandable: bool,
            /// Whether an unplaced arrival reached it while depth remained —
            /// the walk could have gone on and chose not to.
            stopped_with_budget: bool,
        }

        let mut visits: HashMap<i64, Visit> = HashMap::new();
        // The depth of a queued item is the hop that expands it. The start is
        // hop 0, whose arrivals are the direct callers; `max_depth` counts hops
        // beyond those.
        let mut queue: std::collections::VecDeque<(i64, u32)> =
            std::collections::VecDeque::from([(symbol_id, 0)]);

        while let Some(&(_, depth)) = queue.front() {
            // Every item queued at this depth is asked about in one query, not
            // one per symbol: the cascade needs a window function over every
            // `Calls` edge of the name, and that runs once per statement.
            let mut hop = Vec::new();
            while let Some(&(id, item_depth)) = queue.front() {
                if item_depth != depth {
                    break;
                }
                hop.push(id);
                queue.pop_front();
            }

            // Arrivals from this hop land one deeper, which has to be within
            // budget for any of them to be walked further.
            let room_to_expand = depth < max_depth;
            for (caller, tier) in self.callers_of_any(&hop)? {
                let caller_id = i64::from(caller.id.value());
                let expandable = tier != TIER_UNPLACED;
                let visit = visits.entry(caller_id).or_insert_with(|| Visit {
                    symbol: caller,
                    best_arrival_tier: tier,
                    queued_expandable: false,
                    stopped_with_budget: false,
                });
                visit.best_arrival_tier = visit.best_arrival_tier.min(tier);
                if !room_to_expand {
                    continue;
                }
                if expandable {
                    // The first expandable arrival is the only one that queues.
                    // A tier that merely improves changes the report, not the
                    // traversal.
                    if !visit.queued_expandable {
                        visit.queued_expandable = true;
                        queue.push_back((caller_id, depth + 1));
                    }
                } else {
                    visit.stopped_with_budget = true;
                }
            }
        }

        let stopped_arrivals = visits
            .values()
            .filter(|v| v.stopped_with_budget && !v.queued_expandable)
            .count();
        Ok(ImpactRadius {
            reached: visits
                .into_values()
                .map(|v| (v.symbol, v.best_arrival_tier))
                .collect(),
            stopped_arrivals,
        })
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

fn acquire_code_lock(
    path: &Path,
    operation: &str,
) -> rusqlite::Result<crate::store::mutation_lock::MutationGuard> {
    crate::store::mutation_lock::acquire(path, operation).map_err(|e| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some(e.to_string()),
        )
    })
}

/// Announce a live connection to the code index for as long as the returned
/// guard lives, so [`quarantine_if_corrupt`] never renames the files under it.
fn acquire_live_code_lock(
    path: &Path,
) -> rusqlite::Result<crate::store::mutation_lock::MutationGuard> {
    crate::store::mutation_lock::acquire_live_shared(path).map_err(|e| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some(e.to_string()),
        )
    })
}

/// Move a structurally corrupt, rebuildable code index out of the active path.
///
/// Unlike `index.sqlite`, the code index contains no unique user data, so no
/// salvage is needed. The original database and any WAL sidecars are retained
/// under `.mdkb/quarantine/` for diagnosis; the caller then creates a fresh
/// schema at the original path and the normal rescan repopulates it.
fn quarantine_if_corrupt(path: &Path) -> rusqlite::Result<()> {
    if !path.exists() {
        return Ok(());
    }

    // The same verdict as `index.sqlite`: only a file SQLite itself calls torn
    // is moved. A lock or an I/O fault fails this open instead.
    let probe = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    probe.busy_timeout(crate::store::heal::PROBE_BUSY_TIMEOUT)?;
    let reason = match crate::store::heal::is_structurally_sound(&probe) {
        Soundness::Sound => return Ok(()),
        Soundness::Corrupt { reason } => reason,
        Soundness::Undetermined(e) => return Err(e),
    };
    drop(probe);

    // Never rename under an open connection: the path would be recycled onto a
    // second inode while the survivor keeps deriving `-wal`/`-shm` from the old
    // name, so its frames can land in the replacement database. Leave the
    // corrupt file for the next open that finds nobody attached.
    let live = crate::store::mutation_lock::try_acquire_live_exclusive(path).map_err(|e| {
        io_as_sqlite(
            "probe live code-index connections",
            std::io::Error::other(e),
        )
    })?;
    if live.is_none() {
        tracing::warn!(
            database = %path.display(),
            reason,
            "code index is corrupt but held open by another process; left in place — restart the daemon to rebuild it"
        );
        return Ok(());
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let quarantine_dir = parent.join("quarantine");
    fs::create_dir_all(&quarantine_dir).map_err(|e| io_as_sqlite("create quarantine", e))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let file_name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("code.sqlite"));
    let target_base = quarantine_dir.join(file_name);
    let mut target = append_suffix(&target_base, &format!(".corrupt-{timestamp}"));
    let mut collision = 0_u32;
    while target.exists() {
        collision += 1;
        target = append_suffix(&target_base, &format!(".corrupt-{timestamp}-{collision}"));
    }
    fs::rename(path, &target).map_err(|e| io_as_sqlite("quarantine code database", e))?;

    for suffix in ["-wal", "-shm"] {
        let source = append_suffix(path, suffix);
        if source.exists() {
            let sidecar_target = append_suffix(&target, suffix);
            fs::rename(&source, &sidecar_target)
                .map_err(|e| io_as_sqlite("quarantine code database sidecar", e))?;
        }
    }

    tracing::warn!(
        database = %path.display(),
        quarantined = %target.display(),
        reason,
        "code index was corrupt; quarantined for automatic rebuild"
    );
    Ok(())
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn io_as_sqlite(operation: &str, error: std::io::Error) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
        Some(format!("{operation}: {error}")),
    )
}

// --- SQL fragments ---

/// How near the scope a candidate was found in is to the call site: the cascade
/// of `plans/qualified-symbol-resolution.md` §3, lower being nearer.
///
/// `r` is the call, `fs` the symbol that made it, `s` a symbol whose bare name
/// matches the target.
///
/// Tiers 1 to 3 read the qualifier the call site wrote. They are what keeps a
/// qualifier narrowing and never widening: an edge that carries one resolves
/// through its owner (1) or its module (2), or through nothing at all (3) — it
/// never falls through to the unqualified rules and picks up a same-named local
/// symbol. 3041 edges in this repository would gain a target they never called
/// if it did.
///
/// The receiver arms read the type the call was made on and place the call the
/// same way, for the same reason: the type of `store` in `store.get(1)` says
/// where `get` lives as surely as a written `Store::get` does. They sit before
/// the unqualified rules because a type beats proximity — a same-file `get`
/// belonging to something else is a wrong target, and tiers 4 to 6 would
/// prefer it. `to_receiver_type` is a bare name and `owner_name` may be a path
/// (`impl crate::store::Store`), so the suffix arms run the opposite way round
/// from the qualifier's.
///
/// Two types in two files can carry the same name — a test helper called `Env`
/// in each integration test is the common case — and a name is all the owner
/// match has. So a matching owner in the calling file is tier 1 and one
/// elsewhere is tier 2, which is the ordinal the whole cascade is built on:
/// nearer wins. Without the split, 47 edges of this repository that proximity
/// alone had answered correctly gained the other file's same-named method as a
/// second target.
///
/// `to_receiver_call` is the leftover: a receiver whose type is whatever a
/// function returns, where the pass that resolves those names found nothing of
/// that name in the index — see
/// [`resolve_types`](crate::code::indexing::receiver::resolve_types). The
/// receiver is a value from outside the index, so the call on it is too. The
/// pass also leaves the name alone when several symbols answer to it and
/// disagree, which is why this asks whether the name is indexed rather than
/// trusting the empty type: disagreement is not absence.
///
/// Tier 7 is "no rule placed it": those candidates are kept rather than
/// dropped, because a call the index cannot place is still better answered with
/// every same-named symbol than with an empty list. Tier 3 is the opposite —
/// the index CAN place the call, outside itself — so `resolved_edges` drops it.
///
/// The suffix arms compare with `substr` rather than `LIKE` because both of
/// `LIKE`'s conveniences are wrong here. Its wildcards are live in the *value*
/// being matched, and `_` is an ordinary character in half the languages
/// indexed, so `my_store` read as a pattern also claims `myastore`. And writing
/// PHP's separator into a `LIKE` pattern needs a backslash, which the escape
/// clause that would fix the first problem then takes for itself. `substr(x, -n)`
/// asks the question directly — "do these characters end it" — and a name too
/// short to fill `n` simply fails to compare equal.
pub(crate) const RESOLUTION_TIER: &str = "CASE \
     WHEN r.to_qualifier IS NOT NULL THEN ( CASE \
         WHEN s.owner_name IS NOT NULL AND ( \
             r.to_qualifier = s.owner_name \
             OR substr(r.to_qualifier, -length(s.owner_name) - 2) = '::' || s.owner_name \
             OR substr(r.to_qualifier, -length(s.owner_name) - 1) = '.' || s.owner_name \
             OR substr(r.to_qualifier, -length(s.owner_name) - 1) = '\\' || s.owner_name) THEN 1 \
         WHEN s.module_path IS NOT NULL AND ( \
             s.module_path = r.to_qualifier \
             OR (( substr(s.module_path, -length(r.to_qualifier) - 2) = '::' || r.to_qualifier \
                   OR substr(s.module_path, -length(r.to_qualifier) - 1) = '.' || r.to_qualifier) \
                 AND EXISTS ( \
                     SELECT 1 FROM code_imports i WHERE i.file_id = r.file_id AND ( \
                         i.path = s.module_path \
                         OR i.path = s.module_path || '::' || s.name \
                         OR i.path = s.module_path || '.' || s.name)))) THEN 2 \
         ELSE 3 END ) \
     WHEN r.to_receiver_type IS NOT NULL THEN ( CASE \
         WHEN s.owner_name IS NOT NULL AND ( \
             s.owner_name = r.to_receiver_type \
             OR substr(s.owner_name, -length(r.to_receiver_type) - 2) = '::' || r.to_receiver_type \
             OR substr(s.owner_name, -length(r.to_receiver_type) - 1) = '.' || r.to_receiver_type \
             OR substr(s.owner_name, -length(r.to_receiver_type) - 1) = '\\' || r.to_receiver_type) \
             THEN ( CASE WHEN s.file_id = r.file_id THEN 1 ELSE 2 END ) \
         ELSE 3 END ) \
     WHEN r.to_receiver_call IS NOT NULL AND NOT EXISTS ( \
         SELECT 1 FROM code_symbols o WHERE o.name = r.to_receiver_call) THEN 3 \
     WHEN s.file_id = r.file_id THEN 4 \
     WHEN s.module_path IS NOT NULL AND EXISTS ( \
         SELECT 1 FROM code_imports i WHERE i.file_id = r.file_id AND ( \
             i.path = s.module_path \
             OR i.path = s.module_path || '::' || s.name \
             OR i.path = s.module_path || '.' || s.name)) THEN 5 \
     WHEN s.module_path IS NOT NULL AND s.module_path = fs.module_path THEN 6 \
     ELSE 7 END";

/// One `Calls` edge with every candidate the bare-name join found for it.
///
/// The receiver type is part of the classification evidence even though it is
/// not a candidate: a tier-3 call on a known type is external, while the same
/// tier reached only because a receiver-producing call is absent is unknown.
struct CandidateEdge {
    id: i64,
    name: String,
    qualifier: Option<String>,
    receiver_type: Option<String>,
    candidates: Vec<(i64, i64)>,
}

/// What an impact walk reached, and what it would not walk through.
///
/// [`ImpactRadius::stopped_arrivals`] is the honest half of the answer: a
/// radius that stopped at ambiguous arrivals is short by whatever those symbols
/// call, and a reader who is not told so reads the list as complete.
#[derive(Debug, Default)]
pub struct ImpactRadius {
    /// Every symbol reached, once, with the nearest tier an arrival reached it
    /// by — the last edge, not the confidence of the path.
    pub reached: Vec<(Symbol, i64)>,
    /// Symbols an unplaced arrival reached while depth remained, and that no
    /// placed call ever reached. The walk stopped at each of them.
    pub stopped_arrivals: usize,
}

/// The tier that says "the target is named, and it is not in this index".
///
/// Its candidates are not targets; they are same-named symbols the qualifier
/// ruled out.
pub const TIER_EXTERNAL: i64 = 3;

/// The tier that says "no rule placed this call".
///
/// Its candidates are every symbol of that name anywhere in the index, so it
/// is the widest answer the cascade gives: 3907 edges at 4.19 candidates each
/// on this repository, against 1.01 to 1.42 for the tiers a rule reached.
///
/// What remains here is what no rule can read. The receiver arms took 2383
/// edges out of it at 7.24 candidates each, and no rule over *names* would
/// have: the receivers left are `self.field` bindings, `for` and closure
/// bindings, and method chains whose producing method a name cannot identify —
/// each listed with its reason in
/// [`rust::receiver`](crate::code::parsing::rust::receiver).
pub const TIER_UNPLACED: i64 = 7;

/// SQL predicate: the symbol whose `kind` is in `kind_column` is one a call
/// can target — see [`SymbolKind::is_callable`] for the list and the reasons.
///
/// The candidate join of every `Calls` cascade carries it, so a call named
/// like a field never has the field as a candidate. Built from the enum so
/// that a new kind is classified in one place; a fragment listing the names
/// would answer "not callable" for it by omission.
pub(crate) fn callable_kind_predicate(kind_column: &str) -> String {
    let kinds = (0..SymbolKind::COUNT as u8)
        .filter_map(SymbolKind::from_u8)
        .filter(|kind| kind.is_callable())
        .map(|kind| format!("'{kind}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!("{kind_column} IN ({kinds})")
}

/// Every `Calls` edge matching `filter`, paired with each candidate target and
/// the nearest tier any candidate of that same edge reached.
///
/// Callers keep the rows where `tier = nearest`: that is the first rule of the
/// cascade to yield anything, and no later rule runs once one has.
pub(crate) fn resolved_edges(filter: &str) -> String {
    format!(
        "SELECT r.from_symbol_id AS from_id, s.id AS sym_id, \
                {RESOLUTION_TIER} AS tier, \
                MIN({RESOLUTION_TIER}) OVER (PARTITION BY r.id) AS nearest \
         FROM code_relationships r \
         JOIN code_symbols fs ON fs.id = r.from_symbol_id \
         JOIN code_symbols s ON s.name = r.to_name AND {callable} \
         WHERE r.kind = 'Calls' AND {filter}",
        callable = callable_kind_predicate("s.kind")
    )
}

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

/// Ordered, because two callers take a prefix of the result and neither can
/// defend an arbitrary one: `code_index_hits` shows the first `limit` and
/// `find_symbol_by_name` takes `.next()`. Without ORDER BY, which definition a
/// hook quotes depends on SQLite's row order.
const SYMBOL_SELECT_BY_NAME: &str = "SELECT id, name, kind, file_id, file_path, line_start, col_start, \
     line_end, col_end, visibility, signature, doc_comment, module_path, scope_context \
     FROM code_symbols WHERE name = ?1 ORDER BY file_path, line_start";

/// Wrap a substring in a LIKE pattern, escaping the LIKE metacharacters so a
/// `%` or `_` in user input matches literally.
fn like_pattern(substring: &str) -> String {
    let escaped = substring
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Convert a row from the standard 14-column symbol SELECT to a `Symbol`.
/// Decode the `visibility` column.
///
/// The numbers are the enum's own discriminants, which are pinned in its
/// declaration precisely because they are stored here. One definition, shared
/// with the duplication pass: a second copy would keep answering `Private` for
/// `Package` the day a level is added.
///
/// An unknown number reads as `Private` — the narrowest answer, so an index
/// written by a newer version under-reports reach instead of over-reporting it.
pub(crate) fn visibility_from_i64(value: i64) -> Visibility {
    match value {
        0 => Visibility::Public,
        1 => Visibility::Crate,
        2 => Visibility::Module,
        4 => Visibility::Package,
        5 => Visibility::Restricted,
        _ => Visibility::Private,
    }
}

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
    let visibility = visibility_from_i64(visibility_val);
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
        db.insert_file("test.rs", "test.rs", "hash123", Some("Rust"), None, None)
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
    fn read_only_open_cannot_write_or_create_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        CodeDb::create(&path).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
        }
        let wal = append_suffix(&path, "-wal");
        let shm = append_suffix(&path, "-shm");
        assert!(!wal.exists() && !shm.exists());

        let db = CodeDb::open_read_only(&path).unwrap();
        assert!(db.conn().execute("DELETE FROM code_files", []).is_err());
        drop(db);
        assert!(!wal.exists() && !shm.exists());
    }

    #[test]
    fn read_only_open_does_not_create_a_missing_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        assert!(CodeDb::open_read_only(&path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn read_only_open_does_not_quarantine_or_repair_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        std::fs::write(&path, b"not a sqlite database").unwrap();

        let db = CodeDb::open_read_only(&path).unwrap();
        assert!(db.file_count().is_err());
        drop(db);

        assert!(path.exists(), "a reader must not move the active database");
        assert!(
            !dir.path().join("quarantine").exists(),
            "a reader must not create quarantine state"
        );
    }

    #[test]
    fn corrupt_code_db_is_left_in_place_while_a_connection_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        std::fs::write(&path, b"not a sqlite database at all").unwrap();
        let quarantine_dir = dir.path().join("quarantine");

        // A live holder: renaming now would recycle the path onto a second
        // inode while that connection still derives `-wal`/`-shm` from it.
        let live = crate::store::mutation_lock::acquire_live_shared(&path).unwrap();
        quarantine_if_corrupt(&path).unwrap();
        assert!(path.exists(), "corrupt file stays where the holder sees it");
        assert!(
            !quarantine_dir.exists(),
            "nothing may be moved while a connection is open"
        );

        drop(live);
        quarantine_if_corrupt(&path).unwrap();
        assert!(!path.exists(), "with no holder left it is quarantined");
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
            db.insert_file("a", "a", "h", None, None, None).unwrap();
        }
        let db = CodeDb::open_or_create(&path).unwrap();
        assert_eq!(db.file_count().unwrap(), 1);
    }

    #[test]
    fn test_open_quarantines_structurally_corrupt_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        {
            let db = CodeDb::create(&path).unwrap();
            db.insert_file("a.rs", "a.rs", "hash", Some("Rust"), None, None)
                .unwrap();
        }
        std::fs::write(&path, b"not a sqlite database").unwrap();

        let db = CodeDb::open(&path).unwrap();

        assert_eq!(db.file_count().unwrap(), 0);
        let quarantined = std::fs::read_dir(dir.path().join("quarantine"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(quarantined.len(), 1);
        assert!(quarantined[0].starts_with("code.sqlite.corrupt-"));
    }

    #[test]
    fn test_insert_file() {
        let (_dir, db) = temp_db();
        let id = db
            .insert_file(
                "main.rs",
                "main.rs",
                "abc",
                Some("Rust"),
                Some(12345),
                Some(42),
            )
            .unwrap();
        assert!(id > 0);
        assert_eq!(db.file_count().unwrap(), 1);
    }

    #[test]
    fn test_insert_file_replaces_on_conflict() {
        let (_dir, db) = temp_db();
        db.insert_file("main.rs", "main.rs", "hash1", None, None, None)
            .unwrap();
        db.insert_file("main.rs", "main.rs", "hash2", None, None, None)
            .unwrap();
        assert_eq!(db.file_count().unwrap(), 1);
        let hashes = db.get_file_hashes().unwrap();
        assert_eq!(hashes.get("main.rs").unwrap(), "hash2");
    }

    /// Root cause of the incremental-reindex FK violation: `insert_file` must
    /// return the *existing* row's rowid when the `ON CONFLICT(path) DO UPDATE`
    /// upsert fires. `last_insert_rowid()` does NOT change on an UPDATE, so after
    /// any other insert on the same connection it returns a stale, wrong rowid.
    /// That poisoned file_id then flows into `code_symbols.file_id` /
    /// `code_relationships.file_id` (both FK → `code_files(id)`) and raises
    /// "FOREIGN KEY constraint failed".
    #[test]
    fn insert_file_upsert_returns_existing_rowid_not_last_insert() {
        let (_dir, db) = temp_db();
        let id_a = db
            .insert_file("a.rs", "a.rs", "h1", None, None, None)
            .unwrap();
        // A second insert advances last_insert_rowid past `id_a`.
        let id_b = db
            .insert_file("b.rs", "b.rs", "h2", None, None, None)
            .unwrap();
        assert_ne!(id_a, id_b, "distinct files get distinct rowids");

        // Upsert `a.rs` again — an UPDATE, not an INSERT. Must still return id_a.
        let id_a2 = db
            .insert_file("a.rs", "a.rs", "h1-updated", None, None, None)
            .unwrap();
        assert_eq!(
            id_a2, id_a,
            "upsert of an existing file must return its real rowid, not last_insert_rowid ({id_b})"
        );
    }

    #[test]
    fn test_insert_file_deduplicates_legacy_absolute_path() {
        let (_dir, db) = temp_db();
        // Simulate legacy row with absolute path
        db.insert_file("/abs/path/main.rs", "main.rs", "old_hash", None, None, None)
            .unwrap();
        db.insert_symbol(
            "old_fn", "Function", 1, "main.rs", 1, None, None, None, 0, None, None, None, None,
        )
        .unwrap();
        assert_eq!(db.file_count().unwrap(), 1);
        assert_eq!(db.symbol_count().unwrap(), 1);

        // New code inserts with rel_path for both path and rel_path
        db.insert_file("main.rs", "main.rs", "new_hash", None, None, None)
            .unwrap();

        // Should have exactly 1 file (legacy row cleaned up)
        assert_eq!(db.file_count().unwrap(), 1);
        let hashes = db.get_file_hashes().unwrap();
        assert_eq!(hashes.get("main.rs").unwrap(), "new_hash");

        // Old symbols should be gone (pre-delete cleaned them)
        let remaining: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM code_symbols WHERE name = 'old_fn'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "legacy symbols should be cleaned up");
    }

    #[test]
    fn test_get_file_hashes() {
        let (_dir, db) = temp_db();
        db.insert_file("a", "a", "h1", None, None, None).unwrap();
        db.insert_file("b", "b", "h2", None, None, None).unwrap();

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
                &CallSite::default(),
                "Calls",
                file_id,
                (Some(5), Some(10)),
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
    fn find_symbols_by_name_is_ordered_by_file_then_line() {
        // Two callers take a PREFIX of this result: the PreToolUse code-index
        // block shows the first few, and `find_symbol_by_name` takes the first
        // one. Without a deterministic order, which definition a hook quotes
        // depends on SQLite's row order — which is how a hook came to quote the
        // wrong line numbers.
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        // Inserted out of order on purpose.
        for (path, line) in [("z.rs", 5), ("a.rs", 90), ("a.rs", 12)] {
            db.insert_symbol(
                "handler", "Function", file_id, path, line, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        }

        let got: Vec<(String, u32)> = db
            .find_symbols_by_name("handler")
            .unwrap()
            .into_iter()
            .map(|s| (s.file_path.to_string(), s.range.start_line))
            .collect();

        assert_eq!(
            got,
            vec![
                ("a.rs".to_string(), 12),
                ("a.rs".to_string(), 90),
                ("z.rs".to_string(), 5),
            ]
        );
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

    /// The tier travels with the target. `helper` in the calling file is
    /// placed by proximity (tier 4) and `Store::open` by its qualifier
    /// (tier 1); a list of symbols alone would put the two on an equal footing,
    /// and the reader could not tell a certainty from a guess.
    #[test]
    fn a_resolved_target_carries_the_tier_it_was_placed_at() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let helper = function_in(&db, "helper", (here_file, "here.rs"), "crate::here", 10);
        let (store_file, _) = file_with_function(&db, "unrelated", "store.rs", "crate::store");
        let open = method_in(
            &db,
            "open",
            "Store",
            (store_file, "store.rs"),
            "crate::store",
            5,
        );
        for (name, qualifier) in [("helper", None), ("open", Some("Store"))] {
            db.insert_relationship(
                Some(caller),
                "caller",
                name,
                &CallSite {
                    qualifier,
                    ..CallSite::default()
                },
                "Calls",
                here_file,
                (None, None),
            )
            .unwrap();
        }

        assert_eq!(
            db.get_call_targets(caller).unwrap(),
            vec![
                CallTarget::Resolved {
                    tier: 4,
                    targets: vec![SymbolId::new(helper as u32).unwrap()],
                },
                CallTarget::Resolved {
                    tier: 1,
                    targets: vec![SymbolId::new(open as u32).unwrap()],
                },
            ]
        );
    }

    /// Insert a function symbol, returning its id.
    fn function_in(
        db: &CodeDb,
        name: &str,
        (file_id, file_path): (i64, &str),
        module: &str,
        line: u32,
    ) -> i64 {
        db.insert_symbol(
            name,
            "Function",
            file_id,
            file_path,
            line,
            None,
            None,
            None,
            0,
            None,
            None,
            Some(module),
            None,
        )
        .unwrap()
    }

    /// A fresh file with one function in it: `(file_id, symbol_id)`.
    fn file_with_function(db: &CodeDb, name: &str, file: &str, module: &str) -> (i64, i64) {
        let file_id = db
            .insert_file(file, file, "hash", Some("Rust"), None, None)
            .unwrap();
        (file_id, function_in(db, name, (file_id, file), module, 1))
    }

    /// `caller` in `crate::here`, and a `helper` in each of two other modules —
    /// one the calling file will import, one it will not. Every test below adds
    /// the third `helper` that the cascade is supposed to prefer.
    fn a_caller_and_two_distant_helpers(db: &CodeDb) -> (i64, i64, i64, i64) {
        let (here_file, caller) = file_with_function(db, "caller", "here.rs", "crate::here");
        let (_, imported) = file_with_function(db, "helper", "imported.rs", "crate::imported");
        let (_, elsewhere) = file_with_function(db, "helper", "other.rs", "crate::other");
        db.insert_relationship(
            Some(caller),
            "caller",
            "helper",
            &CallSite::default(),
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();
        (here_file, caller, imported, elsewhere)
    }

    /// Every indexed symbol the caller's calls resolved to, each once, in id
    /// order.
    fn called_ids(db: &CodeDb, caller: i64) -> Vec<i64> {
        let mut ids: Vec<i64> = db
            .get_call_targets(caller)
            .unwrap()
            .into_iter()
            .flat_map(|target| match target {
                CallTarget::Resolved { targets, .. } => targets,
                CallTarget::External { .. } | CallTarget::Unknown { .. } => Vec::new(),
            })
            .map(|id| i64::from(id.value()))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Three calls, three fates. An answer that reported only the first would
    /// say "calls one function" for a symbol that makes three calls, and an
    /// answer that reported none of them would say "calls nothing".
    #[test]
    fn every_call_is_reported_as_resolved_external_or_unknown() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let local = function_in(&db, "helper", (here_file, "here.rs"), "crate::here", 10);
        for (name, qualifier) in [
            ("helper", None),
            ("write", Some("std::fs")),
            ("unwrap", None),
        ] {
            db.insert_relationship(
                Some(caller),
                "caller",
                name,
                &CallSite {
                    qualifier,
                    ..CallSite::default()
                },
                "Calls",
                here_file,
                (None, None),
            )
            .unwrap();
        }

        assert_eq!(
            db.get_call_targets(caller).unwrap(),
            vec![
                CallTarget::Resolved {
                    tier: 4,
                    targets: vec![SymbolId::new(local as u32).unwrap()],
                },
                CallTarget::External {
                    qualifier: "std::fs".to_string(),
                    name: "write".to_string(),
                },
                CallTarget::Unknown {
                    name: "unwrap".to_string(),
                },
            ]
        );
    }

    /// A qualifier that named something real, whose member is not there, is
    /// still external — the target is named, just not indexed. Falling back to
    /// the same-named local symbol is the one answer that must not happen.
    #[test]
    fn a_qualified_call_with_no_matching_member_is_external_not_local() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        function_in(&db, "open", (here_file, "here.rs"), "crate::here", 10);
        db.insert_relationship(
            Some(caller),
            "caller",
            "open",
            &CallSite {
                qualifier: Some("Store"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            db.get_call_targets(caller).unwrap(),
            vec![CallTarget::External {
                qualifier: "Store".to_string(),
                name: "open".to_string(),
            }]
        );
    }

    /// A member of `class`, addressed by `owner_name` through its scope JSON.
    fn method_in(
        db: &CodeDb,
        name: &str,
        class: &str,
        (file_id, file_path): (i64, &str),
        module: &str,
        line: u32,
    ) -> i64 {
        db.insert_symbol(
            name,
            "Method",
            file_id,
            file_path,
            line,
            None,
            None,
            None,
            0,
            None,
            None,
            Some(module),
            Some(&format!(r#"{{"ClassMember":{{"class_name":"{class}"}}}}"#)),
        )
        .unwrap()
    }

    /// `caller` calls `Store::open` while its own file also defines a free
    /// `open`. The unqualified rules would answer with the local one — the call
    /// site said which `open` it meant, and rule 1 is what listens.
    #[test]
    fn a_qualified_call_resolves_to_the_member_of_the_type_it_named() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let local = function_in(&db, "open", (here_file, "here.rs"), "crate::here", 10);
        let (store_file, _) = file_with_function(&db, "unrelated", "store.rs", "crate::store");
        let member = method_in(
            &db,
            "open",
            "Store",
            (store_file, "store.rs"),
            "crate::store",
            5,
        );
        db.insert_relationship(
            Some(caller),
            "caller",
            "open",
            &CallSite {
                qualifier: Some("Store"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![member],
            "expected Store::open, not the local open {local}"
        );
    }

    /// A method `kind()` on a receiver the index cannot type, in a file that
    /// also declares a field `kind`. Proximity would hand the call to the field
    /// — 6831 kept candidates of this repository were fields, aliases, modules
    /// and constants, none of which a call can reach. Only symbols a call can
    /// target are candidates at all, so the function elsewhere wins.
    #[test]
    fn a_call_named_like_a_field_resolves_to_the_function_not_the_field() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let field = db
            .insert_symbol(
                "kind",
                "Field",
                here_file,
                "here.rs",
                5,
                None,
                None,
                None,
                0,
                None,
                None,
                Some("crate::here"),
                None,
            )
            .unwrap();
        let (_, function) = file_with_function(&db, "kind", "other.rs", "crate::other");
        db.insert_relationship(
            Some(caller),
            "caller",
            "kind",
            &CallSite::default(),
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![function],
            "expected the function, not the same-file field {field}"
        );
    }

    /// When the only symbol of that name is a field, the call has no target in
    /// this index at all. An empty answer is right; the field is not.
    #[test]
    fn a_call_whose_only_namesake_is_a_field_has_no_target() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        for (name, kind) in [
            ("get", "Field"),
            ("Err", "TypeAlias"),
            ("config", "Module"),
            ("MAX", "Constant"),
        ] {
            db.insert_symbol(
                name,
                kind,
                here_file,
                "here.rs",
                5,
                None,
                None,
                None,
                0,
                None,
                None,
                Some("crate::here"),
                None,
            )
            .unwrap();
            db.insert_relationship(
                Some(caller),
                "caller",
                name,
                &CallSite::default(),
                "Calls",
                here_file,
                (None, None),
            )
            .unwrap();
        }

        assert_eq!(called_ids(&db, caller), Vec::<i64>::new());
    }

    /// `SymbolId(7)` is a call, and its target is the struct: a tuple struct's
    /// constructor is the struct itself. The kind filter must let it through.
    #[test]
    fn a_tuple_struct_constructor_call_resolves_to_the_struct() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let strukt = db
            .insert_symbol(
                "SymbolId",
                "Struct",
                here_file,
                "here.rs",
                5,
                None,
                None,
                None,
                0,
                None,
                None,
                Some("crate::here"),
                None,
            )
            .unwrap();
        db.insert_relationship(
            Some(caller),
            "caller",
            "SymbolId",
            &CallSite::default(),
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(called_ids(&db, caller), vec![strukt]);
    }

    /// `std::fs::write` names a target this index does not contain. Answering it
    /// with this crate's own `write` is the single worst thing the resolver can
    /// do — it invents a call that was never made, and 3041 edges of this
    /// repository have exactly that shape.
    #[test]
    fn a_call_qualified_by_something_unindexed_is_not_given_a_local_target() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        function_in(&db, "write", (here_file, "here.rs"), "crate::here", 10);
        db.insert_relationship(
            Some(caller),
            "caller",
            "write",
            &CallSite {
                qualifier: Some("std::fs"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            Vec::<i64>::new(),
            "a qualifier narrows the candidates and must never widen them"
        );
    }

    /// PHP writes its separator with a backslash, so `\App\Util::run` qualifies
    /// `run` by `Util` exactly the way `App\Util::run` does in a namespace. The
    /// backslash arm of the cascade is the one arm no test reached, which is how
    /// it once shipped compiled down to a bare `'%'` that matched anything.
    #[test]
    fn a_backslash_qualifier_reaches_the_owner_it_names() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.php", "App\\Here");
        let local = function_in(&db, "run", (here_file, "here.php"), "App\\Here", 10);
        let (util_file, _) = file_with_function(&db, "unrelated", "util.php", "App\\Util");
        let member = method_in(&db, "run", "Util", (util_file, "util.php"), "App\\Util", 5);
        db.insert_relationship(
            Some(caller),
            "caller",
            "run",
            &CallSite {
                qualifier: Some("\\App\\Util"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![member],
            "expected Util::run, not the local run {local}"
        );
    }

    /// A qualifier that merely *ends with* an owner's name did not name that
    /// owner. `FooStore` is its own type; answering `FooStore::open` with
    /// `Store::open` invents a call, which is the one thing the cascade exists
    /// to prevent. Each arm must require its separator, not just the suffix.
    #[test]
    fn a_qualifier_that_only_ends_with_an_owner_name_is_not_that_owner() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let (store_file, _) = file_with_function(&db, "unrelated", "store.rs", "crate::store");
        method_in(
            &db,
            "open",
            "Store",
            (store_file, "store.rs"),
            "crate::store",
            5,
        );
        db.insert_relationship(
            Some(caller),
            "caller",
            "open",
            &CallSite {
                qualifier: Some("FooStore"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            Vec::<i64>::new(),
            "FooStore is not Store, and a suffix is not a qualifier"
        );
    }

    /// Snake_case is the majority convention in half the languages indexed here,
    /// so an owner name carrying `_` is the common case, not the exotic one. Read
    /// as a pattern rather than as text, `my_store` also matches `myastore` —
    /// a different type, one character away.
    #[test]
    fn an_underscore_in_an_owner_name_is_a_character_not_a_wildcard() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let (store_file, _) = file_with_function(&db, "unrelated", "store.rs", "crate::store");
        method_in(
            &db,
            "open",
            "my_store",
            (store_file, "store.rs"),
            "crate::store",
            5,
        );
        db.insert_relationship(
            Some(caller),
            "caller",
            "open",
            &CallSite {
                qualifier: Some("crate::myastore"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            Vec::<i64>::new(),
            "myastore is not my_store: the underscore is text, not a wildcard"
        );
    }

    /// `store.get(1)` never writes `Store`, and the type of `store` says the
    /// same thing. The local `get` is the trap: proximity is tier 4 and would
    /// answer with a function belonging to nothing, which is a call that was
    /// never made.
    #[test]
    fn a_call_on_a_typed_receiver_resolves_to_that_types_member() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let local = function_in(&db, "get", (here_file, "here.rs"), "crate::here", 10);
        let (store_file, _) = file_with_function(&db, "unrelated", "store.rs", "crate::store");
        let member = method_in(
            &db,
            "get",
            "Store",
            (store_file, "store.rs"),
            "crate::store",
            5,
        );
        db.insert_relationship(
            Some(caller),
            "caller",
            "get",
            &CallSite {
                receiver: Some("store"),
                receiver_type: Some("Store"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![member],
            "expected Store::get, not the neighbouring free get {local}"
        );
    }

    /// Every integration test file here defines its own `Env` helper, so the
    /// receiver's type name matches two owners and the type alone cannot say
    /// which. The one in the calling file can: proximity answered these
    /// correctly before the receiver arms existed, and an arm that read the
    /// type and forgot where it was would hand 47 edges of this repository a
    /// second target in a file they never reach into.
    #[test]
    fn a_receiver_type_two_files_share_resolves_to_the_one_at_hand() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let near = method_in(&db, "add", "Env", (here_file, "here.rs"), "crate::here", 30);
        let (other_file, _) = file_with_function(&db, "unrelated", "other.rs", "crate::other");
        let far = method_in(
            &db,
            "add",
            "Env",
            (other_file, "other.rs"),
            "crate::other",
            5,
        );
        db.insert_relationship(
            Some(caller),
            "caller",
            "add",
            &CallSite {
                receiver: Some("env"),
                receiver_type: Some("Env"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![near],
            "expected this file's Env::add, not also the other file's {far}"
        );
    }

    /// `impl crate::store::Store` records the path it was written as, while the
    /// receiver's type is inferred as a bare name. The suffix arms therefore run
    /// the opposite way round from the qualifier's, and a cascade that only
    /// compared the two for equality would miss every impl written out in full.
    #[test]
    fn a_receiver_type_reaches_an_owner_written_as_a_path() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let (store_file, _) = file_with_function(&db, "unrelated", "store.rs", "crate::store");
        let member = method_in(
            &db,
            "get",
            "crate::store::Store",
            (store_file, "store.rs"),
            "crate::store",
            5,
        );
        db.insert_relationship(
            Some(caller),
            "caller",
            "get",
            &CallSite {
                receiver: Some("store"),
                receiver_type: Some("Store"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(called_ids(&db, caller), vec![member]);
    }

    /// `FooStore` is its own type. A receiver arm that took a suffix for a type
    /// would answer `foo_store.get()` with `Store::get`, which is the invented
    /// call the whole cascade exists to prevent.
    #[test]
    fn a_receiver_type_that_only_ends_with_an_owner_name_is_not_that_owner() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let (store_file, _) = file_with_function(&db, "unrelated", "store.rs", "crate::store");
        method_in(
            &db,
            "get",
            "Store",
            (store_file, "store.rs"),
            "crate::store",
            5,
        );
        db.insert_relationship(
            Some(caller),
            "caller",
            "get",
            &CallSite {
                receiver: Some("foo"),
                receiver_type: Some("FooStore"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            Vec::<i64>::new(),
            "FooStore is not Store, and a suffix is not a type"
        );
    }

    /// `path.join(x)` on a `PathBuf` is a call this index does not contain. A
    /// known type whose member is absent is the same situation as a qualifier
    /// that named something unindexed: the target is named, just not here. The
    /// local `join` must not be offered in its place.
    #[test]
    fn a_call_on_a_type_with_no_such_member_is_external_not_local() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        function_in(&db, "join", (here_file, "here.rs"), "crate::here", 10);
        db.insert_relationship(
            Some(caller),
            "caller",
            "join",
            &CallSite {
                receiver: Some("path"),
                receiver_type: Some("PathBuf"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            db.get_call_targets(caller).unwrap(),
            vec![CallTarget::External {
                qualifier: "PathBuf".to_string(),
                name: "join".to_string(),
            }],
            "the type the call was made on names the external target"
        );
    }

    /// A known receiver type names an external target even when there is no
    /// same-named symbol for the candidate join to return. Classification must
    /// retain the receiver evidence independently of candidate rows.
    #[test]
    fn a_call_on_a_type_with_no_namesake_is_external() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        db.insert_relationship(
            Some(caller),
            "caller",
            "join",
            &CallSite {
                receiver: Some("path"),
                receiver_type: Some("PathBuf"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            db.get_call_targets(caller).unwrap(),
            vec![CallTarget::External {
                qualifier: "PathBuf".to_string(),
                name: "join".to_string(),
            }]
        );
    }

    /// `tempdir().path()` — the receiver's type is whatever `tempdir` returns,
    /// but nothing in this index declares `tempdir`. Without that type the call
    /// has no nameable owner, even though same-named local candidates exist.
    #[test]
    fn a_receiver_produced_by_an_unindexed_function_is_unknown() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        function_in(&db, "path", (here_file, "here.rs"), "crate::here", 10);
        db.insert_relationship(
            Some(caller),
            "caller",
            "path",
            &CallSite {
                receiver: Some("tempdir()"),
                receiver_call: Some("tempdir"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            db.get_call_targets(caller).unwrap(),
            vec![CallTarget::Unknown {
                name: "path".to_string(),
            }],
            "no type to name and no candidate: the local `path` is not offered"
        );
    }

    /// With neither a receiver type nor a same-named target, the missing
    /// producer still cannot name an external owner. It remains unknown rather
    /// than fabricating a qualifier from the receiver expression.
    #[test]
    fn an_unindexed_receiver_call_with_no_namesake_is_unknown() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        db.insert_relationship(
            Some(caller),
            "caller",
            "path",
            &CallSite {
                receiver: Some("tempdir()"),
                receiver_call: Some("tempdir"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            db.get_call_targets(caller).unwrap(),
            vec![CallTarget::Unknown {
                name: "path".to_string(),
            }]
        );
    }

    /// The resolution pass leaves a name alone for two different reasons, and
    /// only one of them means "external". Two indexed functions named `store`
    /// that return different types say nothing about which one ran, so the call
    /// falls back to the name rules rather than being declared to leave the
    /// index on the strength of a coin toss.
    #[test]
    fn a_receiver_call_the_index_does_hold_is_not_declared_external() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let local = function_in(&db, "path", (here_file, "here.rs"), "crate::here", 10);
        function_in(&db, "store", (here_file, "here.rs"), "crate::here", 20);
        db.insert_relationship(
            Some(caller),
            "caller",
            "path",
            &CallSite {
                receiver: Some("store()"),
                receiver_call: Some("store"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![local],
            "an unresolved type is not an absent one: disagreement is not absence"
        );
    }

    /// The qualifier names a module rather than a type. It is the tail of an
    /// indexed `module_path` that the calling file imports, so rule 2 places the
    /// call there instead of on the same-named function next to the caller.
    #[test]
    fn a_qualified_call_resolves_through_the_module_the_file_imported() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let local = function_in(&db, "helper", (here_file, "here.rs"), "crate::here", 10);
        let (_, imported) = file_with_function(&db, "helper", "imported.rs", "crate::imported");
        db.insert_import(here_file, "crate::imported", None, false, false)
            .unwrap();
        db.insert_relationship(
            Some(caller),
            "caller",
            "helper",
            &CallSite {
                qualifier: Some("imported"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![imported],
            "expected the imported module's helper, not the local one {local}"
        );
    }

    /// A caller is reported only for the call that actually resolved to it: the
    /// external classification has to hold from both ends, or `callers` answers
    /// with calls that never reached the symbol.
    #[test]
    fn callers_exclude_a_call_whose_qualifier_ruled_this_symbol_out() {
        let (_dir, db) = temp_db();
        let (here_file, caller) = file_with_function(&db, "caller", "here.rs", "crate::here");
        let local = function_in(&db, "write", (here_file, "here.rs"), "crate::here", 10);
        db.insert_relationship(
            Some(caller),
            "caller",
            "write",
            &CallSite {
                qualifier: Some("std::fs"),
                ..CallSite::default()
            },
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert!(
            db.get_calling_functions(local).unwrap().is_empty(),
            "std::fs::write is not a call into this crate's write"
        );
    }

    /// Two files define `helper`, neither in the calling file and neither
    /// imported, so no rule places the call: both are reported as callers and
    /// both arrivals are tier 7. Without the count the list reads as two callers
    /// known to call this `helper`, when at most one of them does.
    #[test]
    fn a_caller_that_no_rule_placed_is_counted_as_such() {
        let (_dir, db) = temp_db();
        let (_, caller, imported, _elsewhere) = a_caller_and_two_distant_helpers(&db);

        let arrivals = db.get_callers_by_tier(imported).unwrap();
        assert_eq!(arrivals.len(), 1, "the caller is reported");
        assert_eq!(
            arrivals[0].1, TIER_UNPLACED,
            "no rule placed it: {:?}",
            arrivals[0].1
        );
        assert_eq!(
            i64::from(arrivals[0].0.id.value()),
            caller,
            "the caller is the one that wrote the name"
        );
    }

    /// A call the calling file itself defines is placed by rule 4, so nothing on
    /// that list is reported as a bare-name arrival.
    #[test]
    fn a_caller_a_rule_placed_is_not_counted_as_unplaced() {
        let (_dir, db) = temp_db();
        let (here_file, caller, _, _) = a_caller_and_two_distant_helpers(&db);
        let local = function_in(&db, "helper", (here_file, "here.rs"), "crate::here", 10);

        let arrivals = db.get_callers_by_tier(local).unwrap();
        assert_eq!(arrivals.len(), 1);
        assert_eq!(i64::from(arrivals[0].0.id.value()), caller);
        assert_ne!(
            arrivals[0].1, TIER_UNPLACED,
            "rule 4 placed this call in the calling file"
        );
    }

    /// Three files define `helper` and `caller` calls it. Bare-name matching
    /// answers "all three", which is two wrong answers and no way to tell which.
    /// Rule 4 keeps the one declared in the calling file.
    #[test]
    fn a_call_resolves_to_the_definition_in_its_own_file() {
        let (_dir, db) = temp_db();
        let (here_file, caller, imported, elsewhere) = a_caller_and_two_distant_helpers(&db);
        let local = function_in(&db, "helper", (here_file, "here.rs"), "crate::here", 10);

        assert_eq!(
            called_ids(&db, caller),
            vec![local],
            "expected only the local helper, not {imported} or {elsewhere}"
        );
    }

    /// Nothing local to prefer, so rule 5 takes over: the file imports one of the
    /// two modules, and only that one can be what the call meant.
    #[test]
    fn a_call_with_no_local_definition_resolves_through_an_import() {
        let (_dir, db) = temp_db();
        let (here_file, caller, imported, elsewhere) = a_caller_and_two_distant_helpers(&db);
        db.insert_import(here_file, "crate::imported::helper", None, false, false)
            .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![imported],
            "expected the imported helper, not {elsewhere}"
        );
    }

    /// A glob import names the module rather than the item, and has to narrow
    /// just the same.
    #[test]
    fn a_glob_import_narrows_a_call_to_that_module() {
        let (_dir, db) = temp_db();
        let (here_file, caller, imported, elsewhere) = a_caller_and_two_distant_helpers(&db);
        db.insert_import(here_file, "crate::imported", None, true, false)
            .unwrap();

        assert_eq!(
            called_ids(&db, caller),
            vec![imported],
            "expected the imported helper, not {elsewhere}"
        );
    }

    /// Neither local nor imported, so rule 6: a file of the same module is nearer
    /// than a file of an unrelated one. `src/a.rs` and `src/a/mod.rs` are one
    /// module, and so are two classes of one Java package.
    #[test]
    fn a_call_with_no_import_resolves_within_its_own_module() {
        let (_dir, db) = temp_db();
        let (_, caller, _imported, elsewhere) = a_caller_and_two_distant_helpers(&db);
        let (_, sibling) = file_with_function(&db, "helper", "sibling.rs", "crate::here");

        assert_eq!(
            called_ids(&db, caller),
            vec![sibling],
            "expected the helper of the calling module, not {elsewhere}"
        );
    }

    /// No rule fires. The candidates stay as they are rather than collapsing to
    /// nothing: a call whose target the index cannot place is still better
    /// answered with every same-named symbol than with an empty list.
    #[test]
    fn a_call_that_matches_no_rule_keeps_every_candidate() {
        let (_dir, db) = temp_db();
        let (_, caller, imported, elsewhere) = a_caller_and_two_distant_helpers(&db);

        let mut got = called_ids(&db, caller);
        got.sort_unstable();
        let mut want = vec![imported, elsewhere];
        want.sort_unstable();
        assert_eq!(got, want);
    }

    /// An index written before symbols had an address answers as it always did.
    /// The cascade must degrade to bare-name matching on `module_path IS NULL`,
    /// not resolve to nothing: until the reparse runs, every row looks like this.
    #[test]
    fn an_index_without_addresses_answers_as_it_did_before() {
        let (_dir, db) = temp_db();
        let here_file = db
            .insert_file("here.rs", "here.rs", "hash", Some("Rust"), None, None)
            .unwrap();
        let other_file = db
            .insert_file("other.rs", "other.rs", "hash", Some("Rust"), None, None)
            .unwrap();
        let caller = db
            .insert_symbol(
                "caller", "Function", here_file, "here.rs", 1, None, None, None, 0, None, None,
                None, None,
            )
            .unwrap();
        // The far one only: nothing in the calling file to prefer.
        let far = db
            .insert_symbol(
                "helper", "Function", other_file, "other.rs", 1, None, None, None, 0, None, None,
                None, None,
            )
            .unwrap();
        db.insert_relationship(
            Some(caller),
            "caller",
            "helper",
            &CallSite::default(),
            "Calls",
            here_file,
            (None, None),
        )
        .unwrap();

        assert_eq!(called_ids(&db, caller), vec![far]);
        assert_eq!(
            db.get_calling_functions(far)
                .unwrap()
                .iter()
                .map(|s| s.as_name().to_string())
                .collect::<Vec<_>>(),
            vec!["caller"]
        );
    }

    /// The reverse direction has to agree with the forward one. `caller` calls
    /// the `helper` in its own file, so the two distant ones have no callers at
    /// all — reporting one for them is how a rename breaks the wrong code.
    #[test]
    fn callers_are_reported_only_for_the_definition_the_call_resolved_to() {
        let (_dir, db) = temp_db();
        let (here_file, _caller, imported, elsewhere) = a_caller_and_two_distant_helpers(&db);
        let local = function_in(&db, "helper", (here_file, "here.rs"), "crate::here", 10);

        assert_eq!(
            db.get_calling_functions(local)
                .unwrap()
                .iter()
                .map(|s| s.as_name().to_string())
                .collect::<Vec<_>>(),
            vec!["caller"]
        );
        for (label, id) in [("imported", imported), ("unrelated", elsewhere)] {
            assert!(
                db.get_calling_functions(id).unwrap().is_empty(),
                "the {label} helper is called by nobody"
            );
        }
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
        let callee_id = db
            .insert_symbol(
                "callee", "Function", file_id, "test.rs", 10, None, None, None, 0, None, None,
                None, None,
            )
            .unwrap();
        db.insert_relationship(
            Some(caller_id),
            "caller",
            "callee",
            &CallSite::default(),
            "Calls",
            file_id,
            (None, None),
        )
        .unwrap();

        let callers = db.get_calling_functions(callee_id).unwrap();
        assert_eq!(callers.len(), 1);
        assert_eq!(callers[0].as_name(), "caller");
    }

    #[test]
    fn test_get_impact_radius() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);

        // Chain: c -> b -> a (c calls b, b calls a)
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
        let c_id = db
            .insert_symbol(
                "c", "Function", file_id, "test.rs", 20, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();

        db.insert_relationship(
            Some(b_id),
            "b",
            "a",
            &CallSite::default(),
            "Calls",
            file_id,
            (None, None),
        )
        .unwrap();
        db.insert_relationship(
            Some(c_id),
            "c",
            "b",
            &CallSite::default(),
            "Calls",
            file_id,
            (None, None),
        )
        .unwrap();

        // Impact of "a" with depth 0: just "b" (direct caller)
        let impact = db.get_impact_radius(a_id, 0).unwrap();
        assert_eq!(impact.len(), 1);
        assert_eq!(impact[0].as_name(), "b");

        // Impact of "a" with depth 1: "b" and "c" (transitive)
        let impact = db.get_impact_radius(a_id, 1).unwrap();
        assert_eq!(impact.len(), 2);
        let names: Vec<&str> = impact.iter().map(|s| s.as_name()).collect();
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
    }

    /// A hop reaching several symbols at once must report each caller exactly
    /// once, at the nearest tier any of them reached it by. Walking the hop one
    /// target at a time and walking it in one query have to agree on that, so
    /// this pins it independently of how the hop is issued.
    #[test]
    fn a_caller_reached_twice_in_one_hop_keeps_its_nearest_tier() {
        let (_dir, db) = temp_db();
        let near_file = insert_test_file(&db);
        let far_file = db
            .insert_file("far.rs", "far.rs", "hash456", Some("Rust"), None, None)
            .unwrap();

        // `root` is called by both `near` and `far`; each of those is called by
        // the same `shared`, from `near`'s file. So `shared` is reached twice
        // in the second hop: tier 4 (same file as `near`) and tier 7 (nothing
        // places the call to `far`).
        let root = db
            .insert_symbol(
                "root", "Function", near_file, "test.rs", 1, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        let near = db
            .insert_symbol(
                "near", "Function", near_file, "test.rs", 10, None, None, None, 0, None, None,
                None, None,
            )
            .unwrap();
        let far = db
            .insert_symbol(
                "far", "Function", far_file, "far.rs", 10, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();
        let shared = db
            .insert_symbol(
                "shared", "Function", near_file, "test.rs", 20, None, None, None, 0, None, None,
                None, None,
            )
            .unwrap();

        db.insert_relationship(
            Some(near),
            "near",
            "root",
            &CallSite::default(),
            "Calls",
            near_file,
            (None, None),
        )
        .unwrap();
        db.insert_relationship(
            Some(far),
            "far",
            "root",
            &CallSite::default(),
            "Calls",
            far_file,
            (None, None),
        )
        .unwrap();
        db.insert_relationship(
            Some(shared),
            "shared",
            "near",
            &CallSite::default(),
            "Calls",
            near_file,
            (None, None),
        )
        .unwrap();
        db.insert_relationship(
            Some(shared),
            "shared",
            "far",
            &CallSite::default(),
            "Calls",
            near_file,
            (None, None),
        )
        .unwrap();

        let by_tier = db.get_impact_by_tier(root, 1).unwrap().reached;
        let tier_of = |name: &str| {
            by_tier
                .iter()
                .filter(|(s, _)| s.as_name() == name)
                .map(|(_, tier)| *tier)
                .collect::<Vec<_>>()
        };

        assert_eq!(tier_of("near"), vec![4], "near is in root's file");
        assert_eq!(tier_of("far"), vec![TIER_UNPLACED], "far is not placed");
        // One entry, and the nearer of the two tiers it was reached by.
        assert_eq!(tier_of("shared"), vec![4], "nearest tier wins, once");
        assert_eq!(by_tier.len(), 3, "no symbol is reported twice");
    }

    /// Insert a callable symbol, returning its id.
    fn impact_symbol(db: &CodeDb, name: &str, (file, path): (i64, &str), module: &str) -> i64 {
        db.insert_symbol(
            name,
            "Function",
            file,
            path,
            10,
            None,
            None,
            None,
            0,
            None,
            None,
            Some(module),
            None,
        )
        .unwrap()
    }

    /// Insert a `Calls` edge attributed to `file`, which is what the cascade
    /// reads: a target in that same file is tier 4, and a target sharing the
    /// caller's module path is tier 6.
    fn impact_call(db: &CodeDb, from: i64, from_name: &str, to: &str, file: i64) {
        db.insert_relationship(
            Some(from),
            from_name,
            to,
            &CallSite::default(),
            "Calls",
            file,
            (None, None),
        )
        .unwrap();
    }

    /// The walk reports an unplaced arrival and refuses to walk through it —
    /// without dropping a symbol a placed call also reaches.
    ///
    /// An arrival at [`TIER_UNPLACED`] means the caller wrote this name and no
    /// rule could say it meant this symbol. Expanding it reports that symbol's
    /// whole subtree as impacted on the strength of a name, which for a hot
    /// name is most of the index. The opposite mistake is as bad: refusing to
    /// expand a symbol *ever* because an ambiguous arrival reached it first
    /// drops real paths, so `X` below — unplaced from `root`, placed from `P` —
    /// must still be walked.
    #[test]
    fn an_unplaced_arrival_is_reported_but_never_walked_through() {
        let (_dir, db) = temp_db();
        let root_file = (insert_test_file(&db), "test.rs");
        let u_file = (
            db.insert_file("u.rs", "u.rs", "hash-u", Some("Rust"), None, None)
                .unwrap(),
            "u.rs",
        );
        let x_file = (
            db.insert_file("x.rs", "x.rs", "hash-x", Some("Rust"), None, None)
                .unwrap(),
            "x.rs",
        );

        let root = impact_symbol(&db, "root", root_file, "crate::root");
        let placed = impact_symbol(&db, "P", root_file, "crate::api");
        let ambiguous = impact_symbol(&db, "U", u_file, "crate::u");
        let behind = impact_symbol(&db, "E", u_file, "crate::u");
        let both_ways = impact_symbol(&db, "X", x_file, "crate::api");
        let deep = impact_symbol(&db, "D", x_file, "crate::x");

        impact_call(&db, placed, "P", "root", root_file.0); // tier 4: same file
        impact_call(&db, ambiguous, "U", "root", u_file.0); // tier 7: unplaced
        impact_call(&db, behind, "E", "U", u_file.0); // placed, but only behind U
        impact_call(&db, both_ways, "X", "root", x_file.0); // tier 7: the same symbol
        impact_call(&db, both_ways, "X", "P", x_file.0); // tier 6: shared module path
        impact_call(&db, deep, "D", "X", x_file.0); // tier 4: same file

        let radius = db.get_impact_by_tier(root, 2).unwrap();
        let mut names: Vec<&str> = radius.reached.iter().map(|(s, _)| s.as_name()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["D", "P", "U", "X"],
            "E is behind an unplaced arrival and must not be reported; every other \
             symbol appears exactly once"
        );

        let tier_of = |name: &str| {
            radius
                .reached
                .iter()
                .find(|(s, _)| s.as_name() == name)
                .map(|(_, tier)| *tier)
                .expect("reported")
        };
        assert_eq!(tier_of("U"), TIER_UNPLACED, "U arrived on a name alone");
        assert_eq!(
            tier_of("X"),
            6,
            "X is stored at the tier that placed it, not at the unplaced arrival \
             it also had"
        );
        assert_eq!(
            radius.stopped_arrivals, 1,
            "only U stopped the walk — X was reached again by a placed call, so \
             nothing was left unexplored behind it"
        );
    }

    /// A symbol that qualifies late is expanded from the path that qualified
    /// it, not from the one that first named it.
    ///
    /// `S` arrives unplaced among the direct callers and again, placed, one hop
    /// further out. It must be walked — otherwise the rule under-traverses —
    /// but with the budget of the second path. Crediting it the depth of its
    /// first arrival would buy it a hop it never earned, and `W`, three placed
    /// hops from `root` with `max_depth` 2, would appear.
    #[test]
    fn a_late_qualifying_arrival_gets_no_extra_traversal_budget() {
        let (_dir, db) = temp_db();
        let root_file = (insert_test_file(&db), "test.rs");
        let s_file = (
            db.insert_file("s.rs", "s.rs", "hash-s", Some("Rust"), None, None)
                .unwrap(),
            "s.rs",
        );

        let root = impact_symbol(&db, "root", root_file, "crate::root");
        // `A` shares `S`'s module path, which is what places the call from `S`.
        let alias = impact_symbol(&db, "A", root_file, "crate::s");
        let late = impact_symbol(&db, "S", s_file, "crate::s");
        let above_late = impact_symbol(&db, "T", s_file, "crate::s");
        let beyond = impact_symbol(&db, "W", s_file, "crate::s");

        impact_call(&db, alias, "A", "root", root_file.0); // tier 4
        impact_call(&db, late, "S", "root", s_file.0); // tier 7: unplaced
        impact_call(&db, late, "S", "A", s_file.0); // tier 6: placed, one hop out
        impact_call(&db, above_late, "T", "S", s_file.0); // tier 4
        impact_call(&db, beyond, "W", "T", s_file.0); // tier 4

        let radius = db.get_impact_by_tier(root, 2).unwrap();
        let mut names: Vec<&str> = radius.reached.iter().map(|(s, _)| s.as_name()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["A", "S", "T"],
            "S is walked (no under-traversal) and T is reached, but W is a hop \
             past the budget of the path that qualified S"
        );

        let s_tier = radius
            .reached
            .iter()
            .find(|(sym, _)| sym.as_name() == "S")
            .map(|(_, tier)| *tier)
            .expect("S is reported");
        assert_eq!(
            s_tier, 6,
            "the nearest arrival wins — and improving the tier of a symbol \
             already queued changes the report, never the traversal"
        );
        assert_eq!(
            radius.stopped_arrivals, 0,
            "nothing was left unexplored: the one unplaced arrival was later \
             reached by a placed call"
        );
    }

    #[test]
    fn test_clear() {
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        db.insert_symbol(
            "a", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None, None,
        )
        .unwrap();
        db.insert_relationship(
            None,
            "a",
            "b",
            &CallSite::default(),
            "Calls",
            file_id,
            (None, None),
        )
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
        db.insert_relationship(
            Some(sym_id),
            "fn1",
            "fn2",
            &CallSite::default(),
            "Calls",
            file_id,
            (None, None),
        )
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

        db.insert_relationship(
            None,
            "a",
            "b",
            &CallSite::default(),
            "Calls",
            file_id,
            (None, None),
        )
        .unwrap();
        assert_eq!(db.relationship_count().unwrap(), 1);
    }

    #[test]
    fn test_get_file_mtimes() {
        let (_dir, db) = temp_db();
        db.insert_file("a", "a", "h1", None, Some(1000), None)
            .unwrap();
        db.insert_file("b", "b", "h2", None, Some(2000), None)
            .unwrap();
        db.insert_file("c", "c", "h3", None, None, None).unwrap();

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
    fn test_find_symbols_by_file_escapes_like_metacharacters() {
        let (_dir, db) = temp_db();
        // Register two files: one with metacharacters in path, one normal
        let file_id_meta = db
            .insert_file(
                "src/100%_done/lib.rs",
                "src/100%_done/lib.rs",
                "h1",
                Some("Rust"),
                None,
                None,
            )
            .unwrap();
        let file_id_other = db
            .insert_file(
                "src/other.rs",
                "src/other.rs",
                "h2",
                Some("Rust"),
                None,
                None,
            )
            .unwrap();
        db.insert_symbol(
            "meta_fn",
            "Function",
            file_id_meta,
            "src/100%_done/lib.rs",
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
            "other_fn",
            "Function",
            file_id_other,
            "src/other.rs",
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

        // Literal "%" must not act as a wildcard — should match only the file whose
        // path actually contains "%", not both files.
        let results = db.find_symbols_by_file("100%_done", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_name(), "meta_fn");

        // A bare "%" alone would match everything if unescaped; escaped it matches nothing.
        let results = db.find_symbols_by_file("%", 10).unwrap();
        assert_eq!(results.len(), 1); // only the file whose path contains a literal "%"

        // A bare "_" would match any single char if unescaped.
        let results = db.find_symbols_by_file("_done", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].as_name(), "meta_fn");
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
            .insert_file("main.rs", "main.rs", "hash1", Some("Rust"), None, None)
            .unwrap();
        db.insert_symbol(
            "my_fn", "Function", file_id, "main.rs", 10, None, None, None, 0, None, None, None,
            None,
        )
        .unwrap();
        assert_eq!(db.symbol_count().unwrap(), 1);

        // Upsert the same file with a new hash
        db.insert_file("main.rs", "main.rs", "hash2", Some("Rust"), None, None)
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
        let orphan_id = db
            .insert_symbol(
                "orphan", "Function", file_id, "test.rs", 1, None, None, None, 0, None, None, None,
                None,
            )
            .unwrap();

        let impact = db.get_impact_radius(orphan_id, 5).unwrap();
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
        db.insert_relationship(
            Some(a_id),
            "a",
            "b",
            &CallSite::default(),
            "Calls",
            file_id,
            (None, None),
        )
        .unwrap();
        db.insert_relationship(
            Some(b_id),
            "b",
            "a",
            &CallSite::default(),
            "Calls",
            file_id,
            (None, None),
        )
        .unwrap();

        // The walk stops at symbols already seen, so this terminates.
        let impact = db.get_impact_radius(a_id, 10).unwrap();
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

    #[test]
    fn test_get_symbols_batch_chunked_across_999_limit() {
        // Insert 1001 symbols and fetch them all — verifies chunking across the
        // SQLite 999-variable limit does not error and returns all rows.
        let (_dir, db) = temp_db();
        let file_id = insert_test_file(&db);
        let mut inserted_ids: Vec<i64> = Vec::new();
        for i in 0..1001_u32 {
            let id = db
                .insert_symbol(
                    &format!("sym_{i}"),
                    "Function",
                    file_id,
                    "test.rs",
                    i,
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
            inserted_ids.push(id);
        }
        let batch = db.get_symbols_batch(&inserted_ids).unwrap();
        assert_eq!(batch.len(), 1001, "all 1001 symbols should be returned");
    }

    #[test]
    fn test_get_file_token_estimates_chunked_across_999_limit() {
        // Insert 1001 files with token estimates; fetch all — verifies chunking.
        let (_dir, db) = temp_db();
        let mut rel_paths: Vec<String> = Vec::new();
        for i in 0..1001_u32 {
            let rel = format!("file_{i}.rs");
            db.insert_file(&rel, &rel, "hash", Some("Rust"), None, Some(i64::from(i)))
                .unwrap();
            rel_paths.push(rel);
        }
        let map = db.get_file_token_estimates(&rel_paths).unwrap();
        assert_eq!(
            map.len(),
            1001,
            "all 1001 token estimates should be returned"
        );
    }

    #[test]
    fn test_get_file_token_estimates_skips_null_estimates() {
        let (_dir, db) = temp_db();
        db.insert_file("a.rs", "a.rs", "h1", Some("Rust"), None, Some(42))
            .unwrap();
        db.insert_file("b.rs", "b.rs", "h2", Some("Rust"), None, None)
            .unwrap();
        let paths = vec!["a.rs".to_string(), "b.rs".to_string()];
        let map = db.get_file_token_estimates(&paths).unwrap();
        assert_eq!(map.len(), 1, "only file with non-NULL estimate returned");
        assert_eq!(map["a.rs"], 42);
    }

    #[test]
    fn test_symbols_in_file_ordered() {
        let (_dir, db) = temp_db();
        let fid = insert_test_file(&db);
        db.insert_symbol(
            "beta",
            "Function",
            fid,
            "test.rs",
            20,
            None,
            Some(30),
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        db.insert_symbol(
            "alpha",
            "Function",
            fid,
            "test.rs",
            5,
            None,
            Some(15),
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        db.insert_symbol(
            "gamma",
            "Struct",
            fid,
            "test.rs",
            35,
            None,
            Some(50),
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let syms = db.symbols_in_file_ordered("test.rs").unwrap();
        assert_eq!(syms.len(), 3);
        assert_eq!(syms[0].name.as_ref(), "alpha");
        assert_eq!(syms[1].name.as_ref(), "beta");
        assert_eq!(syms[2].name.as_ref(), "gamma");
    }

    #[test]
    fn test_symbols_in_file_ordered_empty() {
        let (_dir, db) = temp_db();
        let syms = db.symbols_in_file_ordered("nonexistent.rs").unwrap();
        assert!(syms.is_empty());
    }

    #[test]
    fn test_symbol_at_position_innermost() {
        let (_dir, db) = temp_db();
        let fid = insert_test_file(&db);
        db.insert_symbol(
            "outer",
            "Function",
            fid,
            "test.rs",
            1,
            None,
            Some(50),
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        db.insert_symbol(
            "inner",
            "Function",
            fid,
            "test.rs",
            10,
            None,
            Some(20),
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let sym = db.symbol_at_position("test.rs", 15, None).unwrap();
        assert!(sym.is_some());
        assert_eq!(sym.unwrap().name.as_ref(), "inner");
    }

    #[test]
    fn test_symbol_at_position_no_match() {
        let (_dir, db) = temp_db();
        let fid = insert_test_file(&db);
        db.insert_symbol(
            "func",
            "Function",
            fid,
            "test.rs",
            10,
            None,
            Some(20),
            None,
            0,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let sym = db.symbol_at_position("test.rs", 5, None).unwrap();
        assert!(sym.is_none());
    }
}
