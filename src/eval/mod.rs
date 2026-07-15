//! Offline evaluation harness for memory retrieval quality.
//!
//! `recall` computes deterministic recall@k / MRR without a model or network
//! (BM25-only). `judge` scores whether the retrieved context answers a
//! question; its default `SubstringJudge` is deterministic and API-free, with
//! an LLM-backed judge pluggable as an alternate `Judge` impl.

pub mod fixture;
pub mod judge;
pub mod recall;

#[cfg(test)]
pub(crate) mod testkit {
    use crate::store::memory::{add_entry, EntryStatus, EntryType, MemoryEntry, SourceType};
    use crate::store::schema::init_schema;
    use chrono::Utc;
    use rusqlite::Connection;

    /// In-memory DB with the full schema applied — the shared fixture for eval tests.
    pub fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    /// Insert a minimal active `Topic` entry.
    pub fn add(conn: &Connection, id: &str, title: &str, content: &str, tags: &[&str]) {
        let now = Utc::now().timestamp();
        let entry = MemoryEntry {
            id: id.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            entry_type: EntryType::Topic,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        add_entry(conn, &entry).unwrap();
    }
}
