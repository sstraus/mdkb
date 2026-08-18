//! Storage layer - SQLite with FTS5 for full-text search.

pub mod chunks;
pub mod collections;
pub mod documents;
pub mod evolution;
pub mod graph;
pub mod heal;
pub mod hybrid;
pub mod identity;
pub mod maintenance;
pub mod memory;
pub mod memory_file;
pub mod memory_graph;
pub mod mutation_lock;
pub mod priors;
pub mod schema;
pub mod search;
pub mod stats;
pub mod vectors;

use std::path::Path;

use crate::error::Result;

/// Main storage handle for mdkb database operations.
pub struct Store {
    conn: rusqlite::Connection,
    // Keep this after `conn`: Rust drops fields in declaration order, so the
    // SQLite handle closes before its live-lock announcement disappears.
    _live_guard: Option<mutation_lock::MutationGuard>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Open or create a database at the given path.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mdkb::store::Store;
    ///
    /// let store = Store::open(".mdkb/index.sqlite")?;
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if the database file can't be created or opened,
    /// or if SQLite initialization fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        // Ensure sqlite-vec is registered before opening any connection
        vectors::init_sqlite_vec();
        // `Store` is public and used as a low-level disk-backed opener. It must
        // participate in the same recovery protocol as `Context`; otherwise a
        // caller can keep an invisible connection alive while autoheal renames
        // and recreates the database at this path.
        let live_guard = mutation_lock::acquire_live_shared(path)?;
        let conn = rusqlite::Connection::open(path)?;
        let store = Self {
            conn,
            _live_guard: Some(live_guard),
        };
        store.setup_pragmas()?;
        Ok(store)
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self> {
        // Ensure sqlite-vec is registered before opening any connection
        vectors::init_sqlite_vec();
        let conn = rusqlite::Connection::open_in_memory()?;
        let store = Self {
            conn,
            _live_guard: None,
        };
        store.setup_pragmas()?;
        Ok(store)
    }

    /// Configure SQLite pragmas for performance and concurrency.
    ///
    /// Kept consistent with the production `Context::configure_connection`
    /// (busy_timeout for cross-process write-lock contention, WAL, NORMAL sync).
    /// No `mmap_size`: a large persistent memory-map on the long-lived daemon
    /// connection, combined with file truncation from another connection,
    /// corrupted `index.sqlite` — see `Context::configure_connection`.
    fn setup_pragmas(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA busy_timeout = 5000;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = memory;
            PRAGMA foreign_keys = ON;
            ",
        )?;
        Ok(())
    }

    /// Get a reference to the underlying connection.
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    /// Get a mutable reference to the underlying connection.
    pub fn conn_mut(&mut self) -> &mut rusqlite::Connection {
        &mut self.conn
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_open_in_memory() {
        let store = Store::open_in_memory().expect("failed to open in-memory db");
        let result: i32 = store
            .conn()
            .query_row("SELECT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(result, 1);
    }

    /// Unix-only: the live-connection probe rides on POSIX byte-range
    /// locks. On Windows the same probe errors with os error 33 (lock
    /// violation) while a connection is live — a real platform difference
    /// in the probe, not a test artifact; it needs its own fix.
    #[cfg(unix)]
    #[test]
    fn disk_store_vetoes_recovery_rename_for_its_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");

        let store = Store::open(&db).unwrap();
        assert!(
            mutation_lock::try_acquire_live_exclusive(&db)
                .unwrap()
                .is_none(),
            "a disk-backed Store must be visible to corruption recovery"
        );

        drop(store);
        assert!(
            mutation_lock::try_acquire_live_exclusive(&db)
                .unwrap()
                .is_some(),
            "dropping Store must release its live guard"
        );
    }
}
