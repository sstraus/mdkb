//! The git commit a store's document index was last built at (story 220-711b).
//!
//! The HEAD commit date against the last index time only says "the index is
//! older than the last commit"; a checkout of an older branch looks fresh. The
//! commit itself says which tree the index describes.

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// Record the commit the index was built at, or clear the record when there is
/// none. A store indexed outside git, or in a repository without commits, must
/// not keep the commit of an earlier run: an old answer reads as a current one.
pub fn record(conn: &Connection, head: Option<&str>, now: i64) -> Result<()> {
    match head {
        Some(head) => {
            conn.execute(
                "INSERT INTO index_head (id, head, recorded_at) VALUES (1, ?1, ?2) \
                 ON CONFLICT(id) DO UPDATE SET head = excluded.head, \
                 recorded_at = excluded.recorded_at",
                params![head, now],
            )?;
        }
        None => {
            conn.execute("DELETE FROM index_head", [])?;
        }
    }
    Ok(())
}

/// The recorded commit, or `None` for a store not yet indexed since v34.
pub fn read(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT head FROM index_head WHERE id = 1", [], |row| {
            row.get(0)
        })
        .optional()?)
}
