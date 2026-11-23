//! Evolution tracking for document relationships.
//!
//! Implements RFC-style evolution tracking where documents can supersede,
//! update, correct, retract, or extend other documents.

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::store::documents;

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
    super::external::refuse_document(conn, source_doc_id)?;
    super::external::refuse_document(conn, target_doc_id)?;
    let now = Utc::now().timestamp();

    // Atomic: evolution record + status update must be consistent.
    // Use SAVEPOINT (not BEGIN) to support nesting inside existing transactions.
    documents::with_savepoint(conn, "add_evolution", || {
        refuse_cycle(conn, source_doc_id, target_doc_id)?;
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
            _ => {}
        }

        Ok(id)
    })
}

/// Refuse an edge `source -> target` that would close an evolution cycle.
///
/// Edges run newer -> older. The new edge closes a cycle when `target` already
/// reaches `source` through existing edges of any relation, or when the two are
/// the same document. A cycle marks every member superseded or retracted, so it
/// removes the current version from default search and recall.
fn refuse_cycle(conn: &Connection, source_doc_id: i64, target_doc_id: i64) -> Result<()> {
    let chain = if source_doc_id == target_doc_id {
        Some(vec![source_doc_id])
    } else {
        path_between(conn, target_doc_id, source_doc_id)?
    };
    let Some(mut chain) = chain else {
        return Ok(());
    };
    chain.push(target_doc_id);
    let names = chain
        .iter()
        .map(|id| doc_label(conn, *id))
        .collect::<Result<Vec<_>>>()?
        .join(" -> ");
    Err(crate::error::ErrorKind::EvolutionCycle { chain: names }.into())
}

/// Shortest newer -> older path from `from` to `to`, as document ids.
fn path_between(conn: &Connection, from: i64, to: i64) -> Result<Option<Vec<i64>>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE walk(id, path) AS (
             SELECT ?1, ',' || ?1 || ','
             UNION
             SELECT e.target_doc_id, walk.path || e.target_doc_id || ','
             FROM evolution e JOIN walk ON e.source_doc_id = walk.id
             WHERE instr(walk.path, ',' || e.target_doc_id || ',') = 0
         )
         SELECT path FROM walk WHERE id = ?2 ORDER BY length(path) LIMIT 1",
    )?;
    let path: Option<String> = stmt.query_row(params![from, to], |r| r.get(0)).optional()?;
    Ok(path.map(|p| p.split(',').filter_map(|id| id.parse().ok()).collect()))
}

fn doc_label(conn: &Connection, id: i64) -> Result<String> {
    Ok(documents::get_document(conn, id)?
        .map(|d| d.relative_path)
        .unwrap_or_else(|| format!("#{id}")))
}

/// Every evolution cycle in the store, one chain of document labels per cycle.
///
/// The write path refuses new cycles; this reports the ones written before it did.
pub fn find_cycles(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT source_doc_id, target_doc_id FROM evolution ORDER BY id")?;
    let edges = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut seen_members = std::collections::HashSet::new();
    let mut cycles = Vec::new();
    for (source, target) in edges {
        if seen_members.contains(&source) {
            continue;
        }
        let chain = if source == target {
            Some(vec![source])
        } else {
            path_between(conn, target, source)?
        };
        if let Some(mut chain) = chain {
            seen_members.extend(chain.iter().copied());
            chain.push(target);
            let labels = chain
                .iter()
                .map(|id| doc_label(conn, *id))
                .collect::<Result<Vec<_>>>()?;
            cycles.push(labels.join(" -> "));
        }
    }
    Ok(cycles)
}

/// Update a document's status.
pub fn update_document_status(
    conn: &Connection,
    doc_id: i64,
    status: DocumentStatus,
    reason: Option<&str>,
) -> Result<()> {
    super::external::refuse_document(conn, doc_id)?;
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
         ORDER BY created_at DESC",
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
         ORDER BY created_at DESC",
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
pub fn get_document_status(
    conn: &Connection,
    doc_id: i64,
) -> Result<Option<(DocumentStatus, Option<String>)>> {
    // `.optional()?` so a genuine SQLite error surfaces as Err; only an absent
    // row becomes None. `.ok()` here collapsed real errors into "not found",
    // silently defaulting callers to DocumentStatus::Current (BUG-B1).
    let result = conn
        .query_row(
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
        )
        .optional()?;

    Ok(result)
}

/// Delete an evolution relationship.
///
/// The target becomes current again when no remaining supersedes or retracts
/// edge hides it; otherwise it takes the status of the newest remaining one.
pub fn delete_evolution(conn: &Connection, id: i64) -> Result<bool> {
    documents::with_savepoint(conn, "delete_evolution", || {
        let target: Option<i64> = conn
            .query_row(
                "SELECT target_doc_id FROM evolution WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(target) = target else {
            return Ok(false);
        };
        conn.execute("DELETE FROM evolution WHERE id = ?1", params![id])?;
        let remaining: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT relationship, reason FROM evolution
                 WHERE target_doc_id = ?1 AND relationship IN ('supersedes', 'retracts')
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                params![target],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (status, reason) = match remaining {
            Some((rel, reason)) if rel == "retracts" => (DocumentStatus::Retracted, reason),
            Some((_, reason)) => (DocumentStatus::Superseded, reason),
            None => (DocumentStatus::Current, None),
        };
        update_document_status(conn, target, status, reason.as_deref())?;
        Ok(true)
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
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash2", "# Doc 2", "1706700000"],
        )
        .unwrap();

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

    /// A third document, so a chain of two edges can be closed into a cycle.
    fn insert_third_doc(conn: &Connection) -> i64 {
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash3", "# Doc 3", "1706700000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "newest.md", "hash3", "Newest Document", "1706700000", "1706700000"],
        ).unwrap();
        conn.last_insert_rowid()
    }

    fn status_of(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT COALESCE(status, 'current') FROM documents WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn reversed_supersede_is_refused_and_leaves_the_newer_doc_current() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);
        add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Supersedes,
            None,
            None,
        )
        .unwrap();

        let err = add_evolution(
            &conn,
            old_id,
            new_id,
            RelationshipType::Supersedes,
            None,
            None,
        )
        .expect_err("closing a supersede cycle must fail");

        assert!(
            err.to_string().contains("cycle"),
            "error names the cycle: {err}"
        );
        assert!(
            err.to_string().contains("new.md"),
            "error names the chain: {err}"
        );
        assert_eq!(status_of(&conn, new_id), "current");
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM evolution", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "the refused edge is not written");
    }

    #[test]
    fn a_cycle_through_a_longer_chain_and_any_relation_is_refused() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);
        let newest = insert_third_doc(&conn);
        add_evolution(&conn, new_id, old_id, RelationshipType::Extends, None, None).unwrap();
        add_evolution(&conn, newest, new_id, RelationshipType::Updates, None, None).unwrap();

        let err = add_evolution(
            &conn,
            old_id,
            newest,
            RelationshipType::Retracts,
            None,
            None,
        )
        .expect_err("a three-node cycle must fail");
        assert!(err.to_string().contains("cycle"), "{err}");
        assert_eq!(status_of(&conn, newest), "current");
    }

    #[test]
    fn a_document_cannot_evolve_itself() {
        let conn = setup_db();
        let (old_id, _) = insert_test_docs(&conn);
        assert!(
            add_evolution(
                &conn,
                old_id,
                old_id,
                RelationshipType::Supersedes,
                None,
                None
            )
            .is_err()
        );
        assert_eq!(status_of(&conn, old_id), "current");
    }

    #[test]
    fn a_second_successor_of_the_same_doc_is_not_a_cycle() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);
        let newest = insert_third_doc(&conn);
        add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Supersedes,
            None,
            None,
        )
        .unwrap();
        add_evolution(
            &conn,
            newest,
            old_id,
            RelationshipType::Corrects,
            None,
            None,
        )
        .expect("two docs evolving one older doc form no cycle");
    }

    #[test]
    fn removing_the_only_supersede_edge_makes_the_old_doc_current_again() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);
        let newest = insert_third_doc(&conn);
        let first = add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Supersedes,
            None,
            None,
        )
        .unwrap();
        let second = add_evolution(
            &conn,
            newest,
            old_id,
            RelationshipType::Retracts,
            None,
            None,
        )
        .unwrap();

        assert!(delete_evolution(&conn, second).unwrap());
        assert_eq!(
            status_of(&conn, old_id),
            "superseded",
            "the remaining supersede still hides it"
        );
        assert!(delete_evolution(&conn, first).unwrap());
        assert_eq!(status_of(&conn, old_id), "current");
        assert!(
            !delete_evolution(&conn, first).unwrap(),
            "a missing id reports false"
        );
    }

    #[test]
    fn find_cycles_reports_a_cycle_written_by_an_older_binary() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);
        add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Supersedes,
            None,
            None,
        )
        .unwrap();
        assert!(find_cycles(&conn).unwrap().is_empty());
        conn.execute(
            "INSERT INTO evolution (source_doc_id, target_doc_id, relationship, created_at) VALUES (?1, ?2, 'supersedes', 0)",
            params![old_id, new_id],
        )
        .unwrap();
        let cycles = find_cycles(&conn).unwrap();
        assert_eq!(cycles.len(), 1, "{cycles:?}");
        assert!(
            cycles[0].contains("old.md") && cycles[0].contains("new.md"),
            "{cycles:?}"
        );
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
        )
        .unwrap();

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
        )
        .unwrap();

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
        )
        .unwrap();

        // Check target document status changed to retracted
        let (status, _) = get_document_status(&conn, old_id).unwrap().unwrap();
        assert_eq!(status, DocumentStatus::Retracted);
    }

    #[test]
    fn test_get_evolution_chain() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);

        add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Supersedes,
            None,
            None,
        )
        .unwrap();

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

        add_evolution(
            &conn,
            new_id,
            old_id,
            RelationshipType::Supersedes,
            None,
            None,
        )
        .unwrap();

        let superseded_by = get_superseded_by(&conn, old_id).unwrap();
        assert_eq!(superseded_by.len(), 1);
        assert_eq!(superseded_by[0].source_doc_id, new_id);
    }

    #[test]
    fn test_delete_evolution() {
        let conn = setup_db();
        let (old_id, new_id) = insert_test_docs(&conn);

        let evo_id =
            add_evolution(&conn, new_id, old_id, RelationshipType::Updates, None, None).unwrap();

        let deleted = delete_evolution(&conn, evo_id).unwrap();
        assert!(deleted);

        let chain = get_evolution_chain(&conn, new_id).unwrap();
        assert!(chain.is_empty());
    }

    #[test]
    fn test_relationship_type_parsing() {
        assert_eq!(
            "supersedes".parse::<RelationshipType>().unwrap(),
            RelationshipType::Supersedes
        );
        assert_eq!(
            "updates".parse::<RelationshipType>().unwrap(),
            RelationshipType::Updates
        );
        assert_eq!(
            "corrects".parse::<RelationshipType>().unwrap(),
            RelationshipType::Corrects
        );
        assert_eq!(
            "retracts".parse::<RelationshipType>().unwrap(),
            RelationshipType::Retracts
        );
        assert_eq!(
            "extends".parse::<RelationshipType>().unwrap(),
            RelationshipType::Extends
        );
        assert!("invalid".parse::<RelationshipType>().is_err());
    }

    #[test]
    fn get_document_status_missing_row_is_none() {
        let conn = setup_db();
        // A doc id that doesn't exist is genuinely not-found → Ok(None).
        assert!(get_document_status(&conn, 999_999).unwrap().is_none());
    }

    #[test]
    fn get_document_status_surfaces_db_error() {
        // BUG-B1: a real DB error must not masquerade as not-found (which would
        // default callers to DocumentStatus::Current). Dropping the table makes
        // the query fail; get_document_status must return Err, not Ok(None).
        let conn = setup_db();
        conn.execute_batch("DROP TABLE documents").unwrap();
        assert!(
            get_document_status(&conn, 1).is_err(),
            "a query error must surface as Err, not be swallowed to None"
        );
    }

    #[test]
    fn test_document_status_parsing() {
        assert_eq!(
            "current".parse::<DocumentStatus>().unwrap(),
            DocumentStatus::Current
        );
        assert_eq!(
            "superseded".parse::<DocumentStatus>().unwrap(),
            DocumentStatus::Superseded
        );
        assert_eq!(
            "retracted".parse::<DocumentStatus>().unwrap(),
            DocumentStatus::Retracted
        );
        assert!("invalid".parse::<DocumentStatus>().is_err());
    }
}
