//! Database maintenance helpers.
//!
//! Three responsibilities with different lock profiles:
//!
//! - `ensure_incremental_autovacuum` — one-shot per database: switch to
//!   INCREMENTAL `auto_vacuum` and `VACUUM` once. The VACUUM both applies the
//!   mode change (SQLite ignores an `auto_vacuum` change until the next VACUUM)
//!   and reclaims free pages bloated under the historical `auto_vacuum = NONE`
//!   default. VACUUM holds an exclusive lock, so this is a startup-only call,
//!   gated by a `PRAGMA user_version` marker bit.
//! - `maybe_incremental_vacuum` — runtime reclaim: `PRAGMA incremental_vacuum`
//!   when the free-page ratio crosses a threshold. Cheap (moves/truncates free
//!   pages, no full rewrite), so it is safe to gate on the same drift counter as
//!   `run_optimize`. No-op unless the database is in INCREMENTAL mode.
//! - `run_optimize` — `PRAGMA optimize`, non-locking query-planner upkeep.

use rusqlite::Connection;

use crate::error::Result;

/// Bit in `PRAGMA user_version` recording the one-shot incremental conversion.
pub const AUTOVAC_DONE_FLAG: i64 = 1;

/// Free-page ratio (freelist / total) at or above which a runtime
/// `incremental_vacuum` is worth running.
pub const INCREMENTAL_VACUUM_THRESHOLD: f64 = 0.2;

/// Switch the database to INCREMENTAL `auto_vacuum` and reclaim accumulated free
/// pages, exactly once per database (marker in `PRAGMA user_version`).
///
/// Returns `Ok(true)` if it ran on this call, `Ok(false)` if the marker was
/// already set and the call was a no-op.
pub fn ensure_incremental_autovacuum(conn: &Connection) -> Result<bool> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version & AUTOVAC_DONE_FLAG != 0 {
        return Ok(false);
    }

    // The VACUUM is what actually applies `auto_vacuum = INCREMENTAL` to an
    // existing file; it also reclaims pages left behind by the old NONE default.
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")?;
    // user_version takes a literal, not a parameter.
    let next = version | AUTOVAC_DONE_FLAG;
    conn.execute_batch(&format!("PRAGMA user_version = {next};"))?;
    tracing::debug!("incremental auto_vacuum enabled, user_version={next}");
    Ok(true)
}

/// Current free-page ratio (`freelist_count / page_count`); 0.0 when empty.
pub fn free_page_ratio(conn: &Connection) -> Result<f64> {
    let (free, total): (i64, i64) = conn.query_row(
        "SELECT freelist_count, page_count FROM pragma_freelist_count(), pragma_page_count()",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(if total == 0 {
        0.0
    } else {
        free as f64 / total as f64
    })
}

/// Reclaim free pages via `PRAGMA incremental_vacuum` when the free-page ratio
/// is at or above `threshold`. Returns whether it ran.
///
/// No-op unless the database is in INCREMENTAL `auto_vacuum` mode (see
/// [`ensure_incremental_autovacuum`]).
pub fn maybe_incremental_vacuum(conn: &Connection, threshold: f64) -> Result<bool> {
    if free_page_ratio(conn)? < threshold {
        return Ok(false);
    }
    conn.execute_batch("PRAGMA incremental_vacuum;")?;
    tracing::debug!("incremental_vacuum reclaimed free pages");
    Ok(true)
}

/// Decide whether a drift-based optimize should run.
///
/// Returns true when `interval` is positive and `call_count` is a non-zero
/// multiple of `interval`. `interval = 0` disables runtime optimize entirely.
pub fn should_optimize(call_count: u64, interval: u64) -> bool {
    interval > 0 && call_count > 0 && call_count % interval == 0
}

/// Run `PRAGMA optimize` — non-locking, safe on the hot path.
pub fn run_optimize(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA optimize;")?;
    tracing::debug!("PRAGMA optimize executed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Open an in-memory DB already converted to INCREMENTAL mode with `rows`
    /// inserted then deleted, leaving free pages on the freelist.
    fn db_with_free_pages(rows: usize) -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        // auto_vacuum must be set before the first table is created.
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; PRAGMA page_size = 4096;")
            .unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, blob TEXT);")
            .unwrap();
        let payload = "x".repeat(2000);
        for i in 0..rows {
            conn.execute(
                "INSERT INTO t (id, blob) VALUES (?1, ?2)",
                (i as i64, &payload),
            )
            .unwrap();
        }
        conn.execute_batch("DELETE FROM t;").unwrap();
        conn
    }

    #[test]
    fn ensure_incremental_autovacuum_sets_mode_and_marker() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER);").unwrap();

        let ran = ensure_incremental_autovacuum(&conn).expect("first call");
        assert!(ran, "first call must run");

        let mode: i64 = conn
            .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, 2, "auto_vacuum must be INCREMENTAL (2)");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert!(version & AUTOVAC_DONE_FLAG != 0, "marker must be set");

        let ran_again = ensure_incremental_autovacuum(&conn).expect("second call");
        assert!(!ran_again, "second call must be a no-op");
    }

    #[test]
    fn maybe_incremental_vacuum_reclaims_above_threshold() {
        let conn = db_with_free_pages(500);
        let before = free_page_ratio(&conn).expect("ratio");
        assert!(
            before >= INCREMENTAL_VACUUM_THRESHOLD,
            "test setup must produce free pages above threshold, got {before}"
        );

        let ran = maybe_incremental_vacuum(&conn, INCREMENTAL_VACUUM_THRESHOLD).expect("vacuum");
        assert!(ran, "must reclaim when ratio above threshold");

        let after = free_page_ratio(&conn).expect("ratio after");
        assert!(
            after < before,
            "free-page ratio must drop after reclaim: {before} -> {after}"
        );
    }

    #[test]
    fn maybe_incremental_vacuum_skips_below_threshold() {
        let conn = db_with_free_pages(500);
        // A threshold above 1.0 can never be met.
        let ran = maybe_incremental_vacuum(&conn, 2.0).expect("vacuum");
        assert!(!ran, "must skip when ratio below threshold");
    }

    #[test]
    fn should_optimize_gates_on_interval() {
        assert!(!should_optimize(0, 200));
        assert!(!should_optimize(1, 200));
        assert!(should_optimize(200, 200));
        assert!(!should_optimize(201, 200));
        assert!(should_optimize(400, 200));
        assert!(!should_optimize(200, 0));
    }
}
