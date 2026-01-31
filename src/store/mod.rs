//! Storage layer - SQLite with FTS5 for full-text search.

use std::path::Path;

use crate::error::Result;

/// Main storage handle for mdkb database operations.
pub struct Store {
    conn: rusqlite::Connection,
}

impl Store {
    /// Open or create a database at the given path.
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
