//! Database maintenance helpers.
//!
//! Two separate responsibilities with different lock profiles:
//!
//! - `run_startup_vacuum_if_needed` — VACUUM holds an exclusive lock and rewrites
//!   the database. Call once on the very first open of a database; the marker
//!   lives in `PRAGMA user_version` bit 0. Safe during startup before the MCP
//!   server accepts traffic.
//! - `run_optimize` — wraps `PRAGMA optimize`, which samples query plans and may
//!   run non-locking `ANALYZE` on a few tables. Safe on the hot path. Gate the
//!   call with [`should_optimize`] to avoid firing on every tool call.

use rusqlite::Connection;

use crate::error::Result;

/// Bit in `PRAGMA user_version` that records "startup VACUUM has run".
pub const VACUUM_DONE_FLAG: i64 = 1;

/// Run `VACUUM` iff the startup-vacuum marker has not been set on this database.
///
/// Returns `Ok(true)` if VACUUM was executed on this call, `Ok(false)` if the
/// marker was already set and the call was a no-op.
pub fn run_startup_vacuum_if_needed(conn: &Connection) -> Result<bool> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version & VACUUM_DONE_FLAG != 0 {
        return Ok(false);
    }

    conn.execute_batch("VACUUM;")?;
    // user_version takes a literal, not a parameter.
    let next = version | VACUUM_DONE_FLAG;
    conn.execute_batch(&format!("PRAGMA user_version = {next};"))?;
    tracing::debug!("startup VACUUM completed, user_version={next}");
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
