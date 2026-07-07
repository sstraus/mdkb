//! Database maintenance helpers.
//!
//! `run_optimize` — `PRAGMA optimize`, non-locking query-planner upkeep.
//!
//! Note: automatic `auto_vacuum = INCREMENTAL` + `VACUUM`/`incremental_vacuum`
//! reclaim was removed. Converting to INCREMENTAL introduces pointer-map pages,
//! and relocating/truncating those pages from one connection while another held
//! the file in a large persistent mmap tore `index.sqlite`. Reclaim is not worth
//! that risk; the databases run with the historical `auto_vacuum = NONE`.

use rusqlite::Connection;

use crate::error::Result;

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

    #[test]
    fn should_optimize_gates_on_interval() {
        assert!(!should_optimize(0, 200));
        assert!(!should_optimize(1, 200));
        assert!(should_optimize(200, 200));
        assert!(!should_optimize(201, 200));
        assert!(should_optimize(400, 200));
        assert!(!should_optimize(200, 0));
    }

    #[test]
    fn run_optimize_succeeds_on_open_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER);").unwrap();
        run_optimize(&conn).expect("optimize is non-locking and always safe");
    }
}
