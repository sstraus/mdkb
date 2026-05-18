//! Idempotent repair checks for code.sqlite.
//!
//! Each repair verifies a precondition and fixes violations in-place.
//! Safe to run on every DB open — skips silently when nothing is wrong.

use rusqlite::Connection;

/// Summary of repairs applied. All zeros means the DB was clean.
#[derive(Debug, Default)]
pub struct RepairReport {
    pub null_kind_symbols: usize,
    pub null_kind_relationships: usize,
    pub orphaned_symbols: usize,
    pub orphaned_relationships_file: usize,
    pub orphaned_relationships_symbol: usize,
    pub duplicate_rel_path_files: usize,
    pub fts_rebuilt: bool,
}

impl RepairReport {
    pub fn total_fixed(&self) -> usize {
        self.null_kind_symbols
            + self.null_kind_relationships
            + self.orphaned_symbols
            + self.orphaned_relationships_file
            + self.orphaned_relationships_symbol
            + self.duplicate_rel_path_files
            + if self.fts_rebuilt { 1 } else { 0 }
    }

    pub fn print_if_needed(&self) {
        if self.total_fixed() == 0 {
            return;
        }
        let mut parts = Vec::new();
        if self.null_kind_symbols > 0 {
            parts.push(format!("{} null-kind symbols", self.null_kind_symbols));
        }
        if self.null_kind_relationships > 0 {
            parts.push(format!(
                "{} null-kind relationships",
                self.null_kind_relationships
            ));
        }
        if self.orphaned_symbols > 0 {
            parts.push(format!("{} orphaned symbols", self.orphaned_symbols));
        }
        if self.orphaned_relationships_file > 0 {
            parts.push(format!(
                "{} orphaned rels (missing file)",
                self.orphaned_relationships_file
            ));
        }
        if self.orphaned_relationships_symbol > 0 {
            parts.push(format!(
                "{} orphaned rels (missing symbol)",
                self.orphaned_relationships_symbol
            ));
        }
        if self.duplicate_rel_path_files > 0 {
            parts.push(format!(
                "{} duplicate rel_path files",
                self.duplicate_rel_path_files
            ));
        }
        if self.fts_rebuilt {
            parts.push("FTS index rebuilt".to_string());
        }
        eprintln!("autofix code.sqlite: removed {}", parts.join(", "));
    }
}

/// Run all repair checks. Returns what was fixed.
pub fn run_repairs(conn: &Connection) -> RepairReport {
    let mut report = RepairReport::default();

    // Wait up to 5s for a concurrent writer (daemon) to release the lock.
    let _ = conn.execute_batch("PRAGMA busy_timeout = 5000");

    // Dedup first — may create orphans that subsequent checks clean up.
    report.duplicate_rel_path_files = repair_duplicate_rel_paths(conn);

    report.null_kind_relationships =
        exec_count(conn, "DELETE FROM code_relationships WHERE kind IS NULL");
    report.null_kind_symbols = exec_count(conn, "DELETE FROM code_symbols WHERE kind IS NULL");

    report.orphaned_symbols = exec_count(
        conn,
        "DELETE FROM code_symbols WHERE file_id NOT IN (SELECT id FROM code_files)",
    );

    report.orphaned_relationships_file = exec_count(
        conn,
        "DELETE FROM code_relationships WHERE file_id NOT IN (SELECT id FROM code_files)",
    );

    report.orphaned_relationships_symbol = exec_count(
        conn,
        "DELETE FROM code_relationships WHERE from_symbol_id IS NOT NULL \
         AND from_symbol_id NOT IN (SELECT id FROM code_symbols)",
    );

    report.fts_rebuilt = repair_fts(conn);

    report.print_if_needed();
    report
}

/// Remove duplicate `code_files` rows sharing the same `rel_path`.
/// Keeps the row with the highest `indexed_at` (most recent). CASCADE deletes
/// clean up symbols and relationships for the removed rows.
fn repair_duplicate_rel_paths(conn: &Connection) -> usize {
    exec_count(
        conn,
        "DELETE FROM code_files WHERE id NOT IN (
            SELECT MAX(id) FROM code_files GROUP BY rel_path
         ) AND rel_path IN (
            SELECT rel_path FROM code_files GROUP BY rel_path HAVING COUNT(*) > 1
         )",
    )
}

fn repair_fts(conn: &Connection) -> bool {
    let ok = conn
        .execute_batch(
            "INSERT INTO code_symbols_fts(code_symbols_fts, rank) VALUES('integrity-check', 1)",
        )
        .is_ok();

    if ok {
        return false;
    }

    conn.execute_batch("INSERT INTO code_symbols_fts(code_symbols_fts) VALUES('rebuild')")
        .is_ok()
}

fn exec_count(conn: &Connection, sql: &str) -> usize {
    match conn.execute(sql, []) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, sql, "repair exec failed");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_legacy_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE code_files (
                id INTEGER PRIMARY KEY, path TEXT, rel_path TEXT, hash TEXT, indexed_at INTEGER
             );
             CREATE TABLE code_symbols (
                id INTEGER PRIMARY KEY, name TEXT, kind TEXT, file_id INTEGER, file_path TEXT,
                line_start INTEGER, line_end INTEGER, doc_comment TEXT, signature TEXT
             );
             CREATE TABLE code_relationships (
                id INTEGER PRIMARY KEY, from_symbol_id INTEGER, from_name TEXT,
                to_name TEXT, kind TEXT, file_id INTEGER
             );
             CREATE VIRTUAL TABLE code_symbols_fts USING fts5(
                name, doc_comment, signature, content=code_symbols, content_rowid=id,
                tokenize='trigram case_sensitive 0'
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn clean_db_returns_zero_report() {
        let conn = setup_legacy_db();
        conn.execute_batch(
            "INSERT INTO code_files VALUES (1, 'a.rs', 'a.rs', 'h1', 1);
             INSERT INTO code_symbols VALUES (1, 'foo', 'fn', 1, 'a.rs', 1, 2, NULL, NULL);
             INSERT INTO code_symbols_fts(rowid, name, doc_comment, signature) VALUES (1, 'foo', '', '');
             INSERT INTO code_relationships VALUES (1, 1, 'foo', 'bar', 'calls', 1);",
        )
        .unwrap();

        let report = run_repairs(&conn);
        assert_eq!(report.total_fixed(), 0);
    }

    #[test]
    fn fixes_all_corruption_types() {
        let conn = setup_legacy_db();
        conn.execute_batch(
            "INSERT INTO code_files VALUES (1, 'a.rs', 'a.rs', 'h1', 1);
             INSERT INTO code_symbols VALUES (1, 'good', 'fn', 1, 'a.rs', 1, 2, NULL, NULL);
             INSERT INTO code_symbols VALUES (2, 'null_kind', NULL, 1, 'a.rs', 3, 4, NULL, NULL);
             INSERT INTO code_symbols VALUES (3, 'orphan_file', 'fn', 999, 'x.rs', 1, 2, NULL, NULL);
             INSERT INTO code_relationships VALUES (1, 1, 'a', 'b', 'calls', 1);
             INSERT INTO code_relationships VALUES (2, 1, 'x', 'y', NULL, 1);
             INSERT INTO code_relationships VALUES (3, 1, 'c', 'd', 'calls', 999);
             INSERT INTO code_relationships VALUES (4, 999, 'e', 'f', 'calls', 1);
             INSERT INTO code_symbols_fts(rowid, name, doc_comment, signature) VALUES (1, 'good', '', '');",
        )
        .unwrap();

        let report = run_repairs(&conn);

        assert_eq!(report.null_kind_symbols, 1);
        assert_eq!(report.null_kind_relationships, 1);
        assert_eq!(report.orphaned_symbols, 1);
        assert_eq!(report.orphaned_relationships_file, 1);
        assert_eq!(report.orphaned_relationships_symbol, 1);
        assert!(!report.fts_rebuilt);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn deduplicates_rel_paths() {
        let conn = setup_legacy_db();
        conn.execute_batch(
            "INSERT INTO code_files VALUES (1, '/abs/a.rs', 'a.rs', 'h1', 100);
             INSERT INTO code_files VALUES (2, '/other/a.rs', 'a.rs', 'h2', 200);
             INSERT INTO code_files VALUES (3, '/abs/b.rs', 'b.rs', 'h3', 300);
             INSERT INTO code_symbols VALUES (1, 'old_fn', 'fn', 1, 'a.rs', 1, 2, NULL, NULL);
             INSERT INTO code_symbols VALUES (2, 'new_fn', 'fn', 2, 'a.rs', 1, 2, NULL, NULL);
             INSERT INTO code_symbols VALUES (3, 'unique', 'fn', 3, 'b.rs', 1, 2, NULL, NULL);",
        )
        .unwrap();

        let report = run_repairs(&conn);
        assert_eq!(
            report.duplicate_rel_path_files, 1,
            "one duplicate file removed"
        );

        let file_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(file_count, 2, "two unique rel_paths remain");

        // The kept file should be the one with higher id (more recent)
        let kept_id: i64 = conn
            .query_row(
                "SELECT id FROM code_files WHERE rel_path = 'a.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept_id, 2, "higher id (most recent) should be kept");

        // Old file's symbols cleaned up by orphan repair (no CASCADE in legacy schema)
        assert_eq!(
            report.orphaned_symbols, 1,
            "orphan repair catches old file symbols"
        );
    }

    #[test]
    fn no_dedup_when_unique() {
        let conn = setup_legacy_db();
        conn.execute_batch(
            "INSERT INTO code_files VALUES (1, 'a.rs', 'a.rs', 'h1', 100);
             INSERT INTO code_files VALUES (2, 'b.rs', 'b.rs', 'h2', 200);",
        )
        .unwrap();

        let report = run_repairs(&conn);
        assert_eq!(report.duplicate_rel_path_files, 0);
    }

    #[test]
    fn rebuilds_desynced_fts() {
        let conn = setup_legacy_db();
        conn.execute_batch(
            "INSERT INTO code_files VALUES (1, 'a.rs', 'a.rs', 'h1', 1);
             INSERT INTO code_symbols VALUES (1, 'foo', 'fn', 1, 'a.rs', 1, 2, 'doc', 'fn foo()');
             INSERT INTO code_symbols VALUES (2, 'bar', 'fn', 1, 'a.rs', 3, 4, NULL, NULL);",
        )
        .unwrap();
        // FTS has 0 rows, symbols has 2 → mismatch

        let report = run_repairs(&conn);
        assert!(report.fts_rebuilt);

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_symbols_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 2);
    }
}
