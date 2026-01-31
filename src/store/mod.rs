//! Storage layer - SQLite with FTS5 for full-text search.

pub mod collections;
pub mod documents;
pub mod hybrid;
pub mod schema;
pub mod search;
pub mod stats;
pub mod vectors;

use std::path::Path;

use crate::error::Result;

/// Main storage handle for mdkb database operations.
pub struct Store {
    conn: rusqlite::Connection,
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
        let conn = rusqlite::Connection::open(path)?;
        let store = Self { conn };
        store.setup_pragmas()?;
        Ok(store)
    }

    /// Open an in-memory database (for testing).
    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let store = Self { conn };
        store.setup_pragmas()?;
        Ok(store)
    }

    /// Configure SQLite pragmas for performance.
    fn setup_pragmas(&self) -> Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = memory;
            PRAGMA mmap_size = 268435456;
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
}
