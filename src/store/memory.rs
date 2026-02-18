//! Memory entry storage operations.

use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// A memory entry for AI knowledge persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub title: String,
    pub content: String,
    pub entry_type: EntryType,
    pub tags: Vec<String>,
    pub status: EntryStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub superseded_by: Option<String>,
    pub access_count: u64,
    pub last_accessed: Option<i64>,
    #[serde(default)]
    pub source_path: Option<String>,
}

/// Type of memory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    Topic,
    Problem,
    Decision,
}

impl std::fmt::Display for EntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Topic => write!(f, "topic"),
            Self::Problem => write!(f, "problem"),
            Self::Decision => write!(f, "decision"),
        }
    }
}

impl std::str::FromStr for EntryType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "topic" => Ok(Self::Topic),
            "problem" => Ok(Self::Problem),
            "decision" => Ok(Self::Decision),
            _ => Err(format!("Invalid entry type: {s}")),
        }
    }
}

/// Status of a memory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EntryStatus {
    #[default]
    Active,
    Superseded,
    Archived,
}

impl std::fmt::Display for EntryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Active => write!(f, "active"),
            Self::Superseded => write!(f, "superseded"),
            Self::Archived => write!(f, "archived"),
        }
    }
}

impl std::str::FromStr for EntryStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "archived" => Ok(Self::Archived),
            _ => Err(format!("Invalid entry status: {s}")),
        }
    }
}

/// Add a new memory entry.
pub fn add_entry(conn: &Connection, entry: &MemoryEntry) -> Result<()> {
    let tags_json = serde_json::to_string(&entry.tags)?;

    conn.execute(
        "INSERT INTO memory_entries (id, title, content, entry_type, tags, status, created_at, updated_at, access_count, source_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            entry.id,
            entry.title,
            entry.content,
            entry.entry_type.to_string(),
            tags_json,
            entry.status.to_string(),
            entry.created_at,
            entry.updated_at,
            entry.access_count,
            entry.source_path,
        ],
    )?;

    Ok(())
}

/// Update an existing memory entry.
pub fn update_entry(conn: &Connection, entry: &MemoryEntry) -> Result<()> {
    let tags_json = serde_json::to_string(&entry.tags)?;
    let now = Utc::now().timestamp();

    conn.execute(
        "UPDATE memory_entries
         SET title = ?1, content = ?2, entry_type = ?3, tags = ?4, status = ?5, updated_at = ?6, superseded_by = ?7
         WHERE id = ?8",
        params![
            entry.title,
            entry.content,
            entry.entry_type.to_string(),
            tags_json,
            entry.status.to_string(),
            now,
            entry.superseded_by,
            entry.id,
        ],
    )?;

    Ok(())
}

/// Get a memory entry by ID and increment access count.
pub fn get_entry(conn: &Connection, id: &str) -> Result<Option<MemoryEntry>> {
    let now = Utc::now().timestamp();

    // Increment access count
    conn.execute(
        "UPDATE memory_entries SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
        params![now, id],
    )?;

    get_entry_without_tracking(conn, id)
}

/// Get a memory entry by ID without incrementing access count.
pub fn get_entry_without_tracking(conn: &Connection, id: &str) -> Result<Option<MemoryEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, content, entry_type, tags, status, created_at, updated_at, superseded_by, access_count, last_accessed, source_path
         FROM memory_entries WHERE id = ?1"
    )?;

    let entry = stmt.query_row(params![id], |row| {
        let tags_json: String = row.get(4)?;
        let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
        let entry_type_str: String = row.get(3)?;
        let status_str: String = row.get(5)?;

        Ok(MemoryEntry {
            id: row.get(0)?,
            title: row.get(1)?,
            content: row.get(2)?,
            entry_type: entry_type_str.parse().unwrap_or(EntryType::Topic),
            tags,
            status: status_str.parse().unwrap_or(EntryStatus::Active),
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
            superseded_by: row.get(8)?,
            access_count: row.get::<_, i64>(9)? as u64,
            last_accessed: row.get(10)?,
            source_path: row.get(11)?,
        })
    }).ok();

    Ok(entry)
}

/// Delete a memory entry.
pub fn delete_entry(conn: &Connection, id: &str) -> Result<bool> {
    let rows = conn.execute("DELETE FROM memory_entries WHERE id = ?1", params![id])?;
    Ok(rows > 0)
}

/// List memory entries ordered by access count (most used first).
pub fn list_entries(conn: &Connection, limit: usize, status_filter: Option<EntryStatus>) -> Result<Vec<MemoryEntry>> {
    let sql = if status_filter.is_some() {
        "SELECT id, title, content, entry_type, tags, status, created_at, updated_at, superseded_by, access_count, last_accessed, source_path
         FROM memory_entries WHERE status = ?1 ORDER BY access_count DESC LIMIT ?2"
    } else {
        "SELECT id, title, content, entry_type, tags, status, created_at, updated_at, superseded_by, access_count, last_accessed, source_path
         FROM memory_entries ORDER BY access_count DESC LIMIT ?1"
    };

    let mut stmt = conn.prepare(sql)?;

    let rows = if let Some(status) = status_filter {
        stmt.query_map(params![status.to_string(), limit as i64], row_to_entry)?
    } else {
        stmt.query_map(params![limit as i64], row_to_entry)?
    };

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }

    Ok(entries)
}

/// Search memory entries using full-text search.
pub fn search_entries(conn: &Connection, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
    let fts_query = crate::store::search::escape_fts5_query(query);
    let mut stmt = conn.prepare(
        "SELECT m.id, m.title, m.content, m.entry_type, m.tags, m.status, m.created_at, m.updated_at, m.superseded_by, m.access_count, m.last_accessed, m.source_path
         FROM memory_entries m
         JOIN memory_fts f ON m.rowid = f.rowid
         WHERE memory_fts MATCH ?1
         ORDER BY bm25(memory_fts)
         LIMIT ?2"
    )?;

    let rows = stmt.query_map(params![fts_query, limit as i64], row_to_entry)?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }

    Ok(entries)
}

/// Get warmup index - compact list of top entries by access count.
pub fn get_warmup_index(conn: &Connection, limit: usize) -> Result<Vec<String>> {
    let entries = list_entries(conn, limit, Some(EntryStatus::Active))?;

    let index: Vec<String> = entries
        .iter()
        .map(|e| {
            let tags = e.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ");
            format!("{}: {} {}", e.id, e.title, tags)
        })
        .collect();

    Ok(index)
}

/// Get total count of memory entries.
pub fn count_entries(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_entries",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

/// Get count of active memory entries.
pub fn count_active_entries(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_entries WHERE status = 'active'",
        [],
        |row| row.get(0),
    )?;
    Ok(count as usize)
}

/// Prune entries not accessed in the given number of days.
/// Marks entries as archived rather than deleting them.
/// Returns the list of pruned entry IDs.
pub fn prune_entries(conn: &Connection, days: u32, dry_run: bool) -> Result<Vec<String>> {
    let now = Utc::now().timestamp();
    let cutoff = now - (days as i64 * 24 * 60 * 60);

    // Find entries to prune:
    // - status is 'active'
    // - last_accessed is NULL (never accessed) and created_at < cutoff, OR
    // - last_accessed is not NULL and last_accessed < cutoff
    let mut stmt = conn.prepare(
        r#"
        SELECT id FROM memory_entries
        WHERE status = 'active'
        AND (
            (last_accessed IS NULL AND created_at < ?1)
            OR (last_accessed IS NOT NULL AND last_accessed < ?1)
        )
        "#,
    )?;

    let ids: Vec<String> = stmt
        .query_map(params![cutoff], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    if !dry_run && !ids.is_empty() {
        // Mark entries as archived
        conn.execute(
            r#"
            UPDATE memory_entries
            SET status = 'archived', updated_at = ?1
            WHERE status = 'active'
            AND (
                (last_accessed IS NULL AND created_at < ?2)
                OR (last_accessed IS NOT NULL AND last_accessed < ?2)
            )
            "#,
            params![now, cutoff],
        )?;
    }

    Ok(ids)
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let tags_json: String = row.get(4)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
    let entry_type_str: String = row.get(3)?;
    let status_str: String = row.get(5)?;

    Ok(MemoryEntry {
        id: row.get(0)?,
        title: row.get(1)?,
        content: row.get(2)?,
        entry_type: entry_type_str.parse().unwrap_or(EntryType::Topic),
        tags,
        status: status_str.parse().unwrap_or(EntryStatus::Active),
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        superseded_by: row.get(8)?,
        access_count: row.get::<_, i64>(9)? as u64,
        last_accessed: row.get(10)?,
        source_path: row.get(11)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::init_schema;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn test_add_and_get_entry() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let entry = MemoryEntry {
            id: "auth-oauth2".to_string(),
            title: "OAuth2 PKCE implementation".to_string(),
            content: "# OAuth2 PKCE\n\nDetails here...".to_string(),
            entry_type: EntryType::Topic,
            tags: vec!["auth".to_string(), "security".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
        };

        add_entry(&conn, &entry).unwrap();

        let retrieved = get_entry(&conn, "auth-oauth2").unwrap().unwrap();
        assert_eq!(retrieved.id, "auth-oauth2");
        assert_eq!(retrieved.title, "OAuth2 PKCE implementation");
        assert_eq!(retrieved.entry_type, EntryType::Topic);
        assert_eq!(retrieved.tags, vec!["auth", "security"]);
        assert_eq!(retrieved.access_count, 1); // Incremented by get_entry
    }

    #[test]
    fn test_update_entry() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let mut entry = MemoryEntry {
            id: "bug-fix".to_string(),
            title: "Null pointer bug".to_string(),
            content: "Original content".to_string(),
            entry_type: EntryType::Problem,
            tags: vec!["bug".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
        };

        add_entry(&conn, &entry).unwrap();

        entry.content = "Updated content with solution".to_string();
        entry.tags.push("fixed".to_string());
        update_entry(&conn, &entry).unwrap();

        let retrieved = get_entry_without_tracking(&conn, "bug-fix").unwrap().unwrap();
        assert_eq!(retrieved.content, "Updated content with solution");
        assert!(retrieved.tags.contains(&"fixed".to_string()));
    }

    #[test]
    fn test_delete_entry() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let entry = MemoryEntry {
            id: "to-delete".to_string(),
            title: "Will be deleted".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Decision,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
        };

        add_entry(&conn, &entry).unwrap();
        assert!(get_entry_without_tracking(&conn, "to-delete").unwrap().is_some());

        let deleted = delete_entry(&conn, "to-delete").unwrap();
        assert!(deleted);
        assert!(get_entry_without_tracking(&conn, "to-delete").unwrap().is_none());
    }

    #[test]
    fn test_list_entries_ordered_by_access() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        // Add entries with different access counts
        for (id, count) in [("low", 1), ("high", 100), ("medium", 50)] {
            let entry = MemoryEntry {
                id: id.to_string(),
                title: format!("{id} access"),
                content: "Content".to_string(),
                entry_type: EntryType::Topic,
                tags: vec![],
                status: EntryStatus::Active,
                created_at: now,
                updated_at: now,
                superseded_by: None,
                access_count: count,
                last_accessed: None,
            source_path: None,
            };
            add_entry(&conn, &entry).unwrap();
        }

        let entries = list_entries(&conn, 10, None).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "high");
        assert_eq!(entries[1].id, "medium");
        assert_eq!(entries[2].id, "low");
    }

    #[test]
    fn test_search_entries() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let entry1 = MemoryEntry {
            id: "auth-jwt".to_string(),
            title: "JWT authentication".to_string(),
            content: "JWT tokens for authentication".to_string(),
            entry_type: EntryType::Topic,
            tags: vec!["auth".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
        };

        let entry2 = MemoryEntry {
            id: "db-postgres".to_string(),
            title: "PostgreSQL setup".to_string(),
            content: "Database configuration for postgres".to_string(),
            entry_type: EntryType::Topic,
            tags: vec!["database".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
        };

        add_entry(&conn, &entry1).unwrap();
        add_entry(&conn, &entry2).unwrap();

        let results = search_entries(&conn, "authentication", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "auth-jwt");
    }

    #[test]
    fn test_warmup_index() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let entry = MemoryEntry {
            id: "test-entry".to_string(),
            title: "Test entry title".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Problem,
            tags: vec!["bug".to_string(), "urgent".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 5,
            last_accessed: None,
            source_path: None,
        };

        add_entry(&conn, &entry).unwrap();

        let index = get_warmup_index(&conn, 50).unwrap();
        assert_eq!(index.len(), 1);
        assert!(index[0].contains("test-entry"));
        assert!(index[0].contains("Test entry title"));
        assert!(index[0].contains("#bug"));
        assert!(index[0].contains("#urgent"));
    }

    #[test]
    fn test_count_entries() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        assert_eq!(count_entries(&conn).unwrap(), 0);

        for i in 0..3 {
            let entry = MemoryEntry {
                id: format!("entry-{i}"),
                title: format!("Entry {i}"),
                content: "Content".to_string(),
                entry_type: EntryType::Topic,
                tags: vec![],
                status: if i == 2 { EntryStatus::Archived } else { EntryStatus::Active },
                created_at: now,
                updated_at: now,
                superseded_by: None,
                access_count: 0,
                last_accessed: None,
            source_path: None,
            };
            add_entry(&conn, &entry).unwrap();
        }

        assert_eq!(count_entries(&conn).unwrap(), 3);
        assert_eq!(count_active_entries(&conn).unwrap(), 2);
    }

    #[test]
    fn test_prune_entries_no_stale() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        // Entry accessed recently
        let entry = MemoryEntry {
            id: "recent".to_string(),
            title: "Recent entry".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 1,
            last_accessed: Some(now),
            source_path: None,
        };
        add_entry(&conn, &entry).unwrap();

        // Prune with 30 days - should find nothing
        let pruned = prune_entries(&conn, 30, false).unwrap();
        assert!(pruned.is_empty());

        // Entry should still be active
        let retrieved = get_entry_without_tracking(&conn, "recent").unwrap().unwrap();
        assert_eq!(retrieved.status, EntryStatus::Active);
    }

    #[test]
    fn test_prune_entries_stale_by_last_accessed() {
        let conn = setup_db();
        let now = Utc::now().timestamp();
        let old_time = now - (100 * 24 * 60 * 60); // 100 days ago

        // Entry last accessed 100 days ago
        let entry = MemoryEntry {
            id: "stale".to_string(),
            title: "Stale entry".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Problem,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: old_time,
            updated_at: old_time,
            superseded_by: None,
            access_count: 5,
            last_accessed: Some(old_time),
            source_path: None,
        };
        add_entry(&conn, &entry).unwrap();

        // Prune with 30 days - should find the entry
        let pruned = prune_entries(&conn, 30, false).unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0], "stale");

        // Entry should now be archived
        let retrieved = get_entry_without_tracking(&conn, "stale").unwrap().unwrap();
        assert_eq!(retrieved.status, EntryStatus::Archived);
    }

    #[test]
    fn test_prune_entries_stale_by_created_at() {
        let conn = setup_db();
        let now = Utc::now().timestamp();
        let old_time = now - (100 * 24 * 60 * 60); // 100 days ago

        // Entry created 100 days ago, never accessed
        let entry = MemoryEntry {
            id: "never-used".to_string(),
            title: "Never used entry".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Decision,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: old_time,
            updated_at: old_time,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
        };
        add_entry(&conn, &entry).unwrap();

        // Prune with 30 days - should find the entry
        let pruned = prune_entries(&conn, 30, false).unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0], "never-used");
    }

    #[test]
    fn test_prune_entries_dry_run() {
        let conn = setup_db();
        let now = Utc::now().timestamp();
        let old_time = now - (100 * 24 * 60 * 60); // 100 days ago

        let entry = MemoryEntry {
            id: "stale-dry".to_string(),
            title: "Stale entry dry run".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: old_time,
            updated_at: old_time,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
        };
        add_entry(&conn, &entry).unwrap();

        // Dry run - should report but not change
        let pruned = prune_entries(&conn, 30, true).unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0], "stale-dry");

        // Entry should still be active (dry run)
        let retrieved = get_entry_without_tracking(&conn, "stale-dry").unwrap().unwrap();
        assert_eq!(retrieved.status, EntryStatus::Active);
    }

    #[test]
    fn test_prune_entries_excludes_already_archived() {
        let conn = setup_db();
        let now = Utc::now().timestamp();
        let old_time = now - (100 * 24 * 60 * 60); // 100 days ago

        // Already archived entry
        let entry = MemoryEntry {
            id: "already-archived".to_string(),
            title: "Already archived".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Archived,
            created_at: old_time,
            updated_at: old_time,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
        };
        add_entry(&conn, &entry).unwrap();

        // Prune - should find nothing (already archived)
        let pruned = prune_entries(&conn, 30, false).unwrap();
        assert!(pruned.is_empty());
    }

    #[test]
    fn test_prune_excludes_from_warmup() {
        let conn = setup_db();
        let now = Utc::now().timestamp();
        let old_time = now - (100 * 24 * 60 * 60); // 100 days ago

        // Recent entry
        let recent = MemoryEntry {
            id: "recent".to_string(),
            title: "Recent".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 10,
            last_accessed: Some(now),
            source_path: None,
        };
        add_entry(&conn, &recent).unwrap();

        // Stale entry
        let stale = MemoryEntry {
            id: "stale".to_string(),
            title: "Stale".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: old_time,
            updated_at: old_time,
            superseded_by: None,
            access_count: 5,
            last_accessed: Some(old_time),
            source_path: None,
        };
        add_entry(&conn, &stale).unwrap();

        // Before prune: both in warmup
        let warmup = get_warmup_index(&conn, 50).unwrap();
        assert_eq!(warmup.len(), 2);

        // Prune
        prune_entries(&conn, 30, false).unwrap();

        // After prune: only recent in warmup
        let warmup = get_warmup_index(&conn, 50).unwrap();
        assert_eq!(warmup.len(), 1);
        assert!(warmup[0].contains("recent"));
    }
}
