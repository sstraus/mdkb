//! Evolution tracking for document relationships.
//!
//! Implements RFC-style evolution tracking where documents can supersede,
//! update, correct, retract, or extend other documents.

use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Relationship type between documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelationshipType {
    /// Complete replacement, old doc marked superseded
    Supersedes,
    /// Partial modification, old doc stays current
    Updates,
    /// Error fix, old doc stays current
    Corrects,
    /// Withdrawal, old doc marked retracted
    Retracts,
    /// Additive content, old doc stays current
    Extends,
}

impl std::fmt::Display for RelationshipType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Supersedes => write!(f, "supersedes"),
            Self::Updates => write!(f, "updates"),
            Self::Corrects => write!(f, "corrects"),
            Self::Retracts => write!(f, "retracts"),
            Self::Extends => write!(f, "extends"),
        }
    }
}

impl std::str::FromStr for RelationshipType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "supersedes" => Ok(Self::Supersedes),
            "updates" => Ok(Self::Updates),
            "corrects" => Ok(Self::Corrects),
            "retracts" => Ok(Self::Retracts),
            "extends" => Ok(Self::Extends),
            _ => Err(format!("Invalid relationship type: {s}")),
        }
    }
}

/// Document status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DocumentStatus {
    #[default]
    Current,
    Superseded,
    Retracted,
}

impl std::fmt::Display for DocumentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Current => write!(f, "current"),
            Self::Superseded => write!(f, "superseded"),
            Self::Retracted => write!(f, "retracted"),
        }
    }
}

impl std::str::FromStr for DocumentStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "current" => Ok(Self::Current),
            "superseded" => Ok(Self::Superseded),
            "retracted" => Ok(Self::Retracted),
            _ => Err(format!("Invalid document status: {s}")),
        }
    }
}

/// An evolution relationship between two documents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evolution {
    pub id: i64,
    pub source_doc_id: i64,
    pub target_doc_id: i64,
    pub relationship: RelationshipType,
    pub scope: Option<String>,
    pub reason: Option<String>,
    pub created_at: i64,
}

/// Add an evolution relationship between documents.
///
/// For supersedes/retracts relationships, this also updates the target document's status.
pub fn add_evolution(
    conn: &Connection,
    source_doc_id: i64,
    target_doc_id: i64,
    relationship: RelationshipType,
    scope: Option<&str>,
    reason: Option<&str>,
) -> Result<i64> {
    let now = Utc::now().timestamp();

    conn.execute(
        "INSERT INTO evolution (source_doc_id, target_doc_id, relationship, scope, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            source_doc_id,
            target_doc_id,
            relationship.to_string(),
            scope,
            reason,
            now,
        ],
    )?;

    let id = conn.last_insert_rowid();

    // Update target document status for supersedes/retracts
    match relationship {
        RelationshipType::Supersedes => {
            update_document_status(conn, target_doc_id, DocumentStatus::Superseded, reason)?;
        }
        RelationshipType::Retracts => {
            update_document_status(conn, target_doc_id, DocumentStatus::Retracted, reason)?;
        }
        _ => {
            // Updates, corrects, extends don't change the target status
        }
    }

    Ok(id)
}

/// Update a document's status.
pub fn update_document_status(
    conn: &Connection,
    doc_id: i64,
    status: DocumentStatus,
    reason: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE documents SET status = ?1, status_reason = ?2 WHERE id = ?3",
        params![status.to_string(), reason, doc_id],
    )?;
    Ok(())
}

/// Get the evolution chain for a document (what it supersedes/extends).
pub fn get_evolution_chain(conn: &Connection, doc_id: i64) -> Result<Vec<Evolution>> {
    let mut stmt = conn.prepare(
        "SELECT id, source_doc_id, target_doc_id, relationship, scope, reason, created_at
         FROM evolution
         WHERE source_doc_id = ?1
         ORDER BY created_at DESC"
    )?;

    let rows = stmt.query_map(params![doc_id], |row| {
        let rel_str: String = row.get(3)?;
        Ok(Evolution {
            id: row.get(0)?,
            source_doc_id: row.get(1)?,
            target_doc_id: row.get(2)?,
            relationship: rel_str.parse().unwrap_or(RelationshipType::Updates),
            scope: row.get(4)?,
            reason: row.get(5)?,
            created_at: row.get(6)?,
        })
    })?;

    let mut evolutions = Vec::new();
    for row in rows {
        evolutions.push(row?);
    }

    Ok(evolutions)
}

/// Get documents that supersede or update this document.
pub fn get_superseded_by(conn: &Connection, doc_id: i64) -> Result<Vec<Evolution>> {
    let mut stmt = conn.prepare(
        "SELECT id, source_doc_id, target_doc_id, relationship, scope, reason, created_at
         FROM evolution
         WHERE target_doc_id = ?1
         ORDER BY created_at DESC"
    )?;

    let rows = stmt.query_map(params![doc_id], |row| {
        let rel_str: String = row.get(3)?;
        Ok(Evolution {
            id: row.get(0)?,
            source_doc_id: row.get(1)?,
            target_doc_id: row.get(2)?,
            relationship: rel_str.parse().unwrap_or(RelationshipType::Updates),
            scope: row.get(4)?,
            reason: row.get(5)?,
            created_at: row.get(6)?,
        })
    })?;

    let mut evolutions = Vec::new();
    for row in rows {
        evolutions.push(row?);
    }

    Ok(evolutions)
}

/// Get document status.
pub fn get_document_status(conn: &Connection, doc_id: i64) -> Result<Option<(DocumentStatus, Option<String>)>> {
    let result = conn.query_row(
        "SELECT status, status_reason FROM documents WHERE id = ?1",
        params![doc_id],
        |row| {
            let status_str: Option<String> = row.get(0)?;
            let reason: Option<String> = row.get(1)?;
            let status = status_str
                .and_then(|s| s.parse().ok())
                .unwrap_or(DocumentStatus::Current);
            Ok((status, reason))
        },
    ).ok();

    Ok(result)
}

/// Delete an evolution relationship.
pub fn delete_evolution(conn: &Connection, id: i64) -> Result<bool> {
    let rows = conn.execute("DELETE FROM evolution WHERE id = ?1", params![id])?;
    Ok(rows > 0)
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

    fn insert_test_docs(conn: &Connection) -> (i64, i64) {
        // Create collection
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();

        // Create content
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "# Doc 1", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash2", "# Doc 2", "1706700000"],
        ).unwrap();

        // Create documents
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "old.md", "hash1", "Old Document", "1706700000", "1706700000"],
        ).unwrap();
        let old_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "new.md", "hash2", "New Document", "1706700000", "1706700000"],
        ).unwrap();
        let new_id = conn.last_insert_rowid();

        (old_id, new_id)
    }

    #[test]
    fn test_add_evolution_supersedes() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);

        let evo_id = add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Supersedes,
            None,
            Some("New implementation"),
        ).unwrap();

        assert!(evo_id > 0);

        // Check target document status changed
        let (status, reason) = get_document_status(&conn, old_id).unwrap().unwrap();
        assert_eq!(status, DocumentStatus::Superseded);
        assert_eq!(reason, Some("New implementation".to_string()));
    }

    #[test]
    fn test_add_evolution_updates() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);

        add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Updates,
            Some("section:api"),
            Some("API changes"),
        ).unwrap();

        // Target document should still be current
        let (status, _) = get_document_status(&conn, old_id).unwrap().unwrap();
        assert_eq!(status, DocumentStatus::Current);
    }

    #[test]
    fn test_add_evolution_retracts() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);

        add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Retracts,
            None,
            Some("Information was incorrect"),
        ).unwrap();

        // Check target document status changed to retracted
        let (status, _) = get_document_status(&conn, old_id).unwrap().unwrap();
        assert_eq!(status, DocumentStatus::Retracted);
    }

    #[test]
    fn test_get_evolution_chain() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);

        add_evolution(&conn, new_id, old_id, RelationshipType::Supersedes, None, None).unwrap();

        let chain = get_evolution_chain(&conn, new_id).unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].source_doc_id, new_id);
        assert_eq!(chain[0].target_doc_id, old_id);
        assert_eq!(chain[0].relationship, RelationshipType::Supersedes);
    }

    #[test]
    fn test_get_superseded_by() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);

        add_evolution(&conn, new_id, old_id, RelationshipType::Supersedes, None, None).unwrap();

        let superseded_by = get_superseded_by(&conn, old_id).unwrap();
        assert_eq!(superseded_by.len(), 1);
        assert_eq!(superseded_by[0].source_doc_id, new_id);
    }

    #[test]
    fn test_delete_evolution() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);

        let evo_id = add_evolution(&conn, new_id, old_id, RelationshipType::Updates, None, None).unwrap();

        let deleted = delete_evolution(&conn, evo_id).unwrap();
        assert!(deleted);

        let chain = get_evolution_chain(&conn, new_id).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn test_relationship_type_parsing() {
        assert_eq!("supersedes".parse::<RelationshipType>().unwrap(), RelationshipType::Supersedes);
        assert_eq!("updates".parse::<RelationshipType>().unwrap(), RelationshipType::Updates);
        assert_eq!("corrects".parse::<RelationshipType>().unwrap(), RelationshipType::Corrects);
        assert_eq!("retracts".parse::<RelationshipType>().unwrap(), RelationshipType::Retracts);
        assert_eq!("extends".parse::<RelationshipType>().unwrap(), RelationshipType::Extends);
        assert!("invalid".parse::<RelationshipType>().is_err());
    }

    #[test]
    fn test_document_status_parsing() {
        assert_eq!("current".parse::<DocumentStatus>().unwrap(), DocumentStatus::Current);
        assert_eq!("superseded".parse::<DocumentStatus>().unwrap(), DocumentStatus::Superseded);
        assert_eq!("retracted".parse::<DocumentStatus>().unwrap(), DocumentStatus::Retracted);
        assert!("invalid".parse::<DocumentStatus>().is_err());
    }
}
