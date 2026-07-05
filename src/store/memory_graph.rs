//! Typed graph edges between memory entries and toward documents.
//!
//! An edge originates from an existing memory entry (`source_id`, FK with cascade
//! delete) and points at a free-text `target_ref` — another memory slug or a
//! document relative path. Targets are resolved at query time and are
//! dangling-tolerant, mirroring the document [`graph`](crate::store::graph) table.
//!
//! The `supersedes` relation is special: adding a `Memory`-kind `supersedes` edge
//! also sets the target's `superseded_by` scalar and `Superseded` status in the
//! same transaction, so the scalar and the edge can never diverge — this is the
//! single write path for that pairing.

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::store::graph::ref_forms;
use crate::store::memory::{self, MemoryEntry};

/// A typed relation from one memory entry to another entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryRelation {
    Supports,
    Contradicts,
    Supersedes,
    DerivedFrom,
    RelatesTo,
}

impl MemoryRelation {
    /// The closed set of valid relations, in declaration order.
    pub const ALL: [MemoryRelation; 5] = [
        Self::Supports,
        Self::Contradicts,
        Self::Supersedes,
        Self::DerivedFrom,
        Self::RelatesTo,
    ];

    /// Wire/storage form of the relation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supports => "supports",
            Self::Contradicts => "contradicts",
            Self::Supersedes => "supersedes",
            Self::DerivedFrom => "derived_from",
            Self::RelatesTo => "relates_to",
        }
    }

    /// The valid relations as a comma-separated list (for error messages).
    pub fn valid_set() -> String {
        Self::ALL
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl std::str::FromStr for MemoryRelation {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "supports" => Ok(Self::Supports),
            "contradicts" => Ok(Self::Contradicts),
            "supersedes" => Ok(Self::Supersedes),
            "derived_from" => Ok(Self::DerivedFrom),
            "relates_to" => Ok(Self::RelatesTo),
            _ => Err(format!(
                "invalid relation '{s}'; expected one of: {}",
                Self::valid_set()
            )),
        }
    }
}

/// What kind of entity a `target_ref` points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Memory,
    Doc,
}

impl TargetKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Doc => "doc",
        }
    }
}

impl std::str::FromStr for TargetKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "memory" => Ok(Self::Memory),
            "doc" => Ok(Self::Doc),
            _ => Err(format!(
                "invalid target_kind '{s}'; expected one of: memory, doc"
            )),
        }
    }
}

/// A typed memory edge: memory entry -> entity reference.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MemoryEdge {
    pub source_id: String,
    pub target_ref: String,
    pub target_kind: String,
    pub relation: String,
    pub created_at: i64,
}

const EDGE_COLUMNS: &str = "source_id, target_ref, target_kind, relation, created_at";

fn map_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEdge> {
    Ok(MemoryEdge {
        source_id: row.get(0)?,
        target_ref: row.get(1)?,
        target_kind: row.get(2)?,
        relation: row.get(3)?,
        created_at: row.get(4)?,
    })
}

/// Add a typed edge from `source` to `target`. Idempotent: re-adding the same
/// `(source, target, relation)` triple is a no-op (composite primary key).
///
/// For a `Memory`-kind `Supersedes` edge, the target's `superseded_by` scalar is
/// set to `source` and its status flipped to `superseded` in the same
/// transaction — keeping the scalar and the edge in lockstep.
pub fn add_edge(
    conn: &Connection,
    source: &str,
    target: &str,
    kind: TargetKind,
    rel: MemoryRelation,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    add_edge_in(&tx, source, target, kind, rel)?;
    tx.commit()?;
    Ok(())
}

/// Insert an edge using the caller's existing transaction/connection.
///
/// Unlike [`add_edge`], this does NOT open its own transaction — call it when the
/// edge write must be atomic with other statements (e.g. entry + edges in one
/// `memory_write`). The supersedes-scalar sync still applies.
pub fn add_edge_in(
    conn: &Connection,
    source: &str,
    target: &str,
    kind: TargetKind,
    rel: MemoryRelation,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_edges (source_id, target_ref, target_kind, relation, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![source, target, kind.as_str(), rel.as_str(), Utc::now().timestamp()],
    )?;

    if rel == MemoryRelation::Supersedes && kind == TargetKind::Memory {
        // `source supersedes target`: target is the older entry, now superseded by source.
        conn.execute(
            "UPDATE memory_entries
             SET superseded_by = ?1, status = 'superseded', updated_at = ?2
             WHERE id = ?3",
            params![source, Utc::now().timestamp(), target],
        )?;
    }

    Ok(())
}

/// Remove a specific typed edge. Idempotent: removing a missing edge returns 0.
pub fn remove_edge(
    conn: &Connection,
    source: &str,
    target: &str,
    rel: MemoryRelation,
) -> Result<usize> {
    let rows = conn.execute(
        "DELETE FROM memory_edges WHERE source_id = ?1 AND target_ref = ?2 AND relation = ?3",
        params![source, target, rel.as_str()],
    )?;
    Ok(rows)
}

/// Outgoing edges from a memory entry, optionally filtered by relation.
pub fn outgoing(
    conn: &Connection,
    source_id: &str,
    rel: Option<MemoryRelation>,
) -> Result<Vec<MemoryEdge>> {
    let mut sql = format!("SELECT {EDGE_COLUMNS} FROM memory_edges WHERE source_id = ?1");
    let mut binds: Vec<String> = vec![source_id.to_string()];
    if let Some(r) = rel {
        sql.push_str(" AND relation = ?2");
        binds.push(r.as_str().to_string());
    }
    sql.push_str(" ORDER BY created_at DESC, target_ref ASC");

    let params: Vec<&dyn rusqlite::ToSql> =
        binds.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), map_edge)?;
    let mut edges = Vec::new();
    for row in rows {
        edges.push(row?);
    }
    Ok(edges)
}

/// Incoming edges pointing at an entity (backlinks), optionally filtered by relation.
///
/// Matches `target_ref` against every equivalent textual form of `entity` (see
/// [`ref_forms`]) so a memory slug and a `path.md` doc reference resolve the same
/// way as the document graph.
pub fn incoming(
    conn: &Connection,
    entity: &str,
    rel: Option<MemoryRelation>,
) -> Result<Vec<MemoryEdge>> {
    let forms = ref_forms(entity);
    let form_count = forms.len();
    let placeholders = (1..=form_count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut sql =
        format!("SELECT {EDGE_COLUMNS} FROM memory_edges WHERE target_ref IN ({placeholders})");

    let mut binds: Vec<String> = forms;
    if let Some(r) = rel {
        sql.push_str(&format!(" AND relation = ?{}", form_count + 1));
        binds.push(r.as_str().to_string());
    }
    sql.push_str(" ORDER BY created_at DESC, source_id ASC");

    let params: Vec<&dyn rusqlite::ToSql> =
        binds.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), map_edge)?;
    let mut edges = Vec::new();
    for row in rows {
        edges.push(row?);
    }
    Ok(edges)
}

/// Resolve a memory `target_ref` to a live entry, or `None` when it does not
/// exist yet (dangling) or is no longer injectable (superseded / expired).
///
/// Used by recall expansion (Step 5) so only active neighbors surface; also
/// proves a dangling edge resolves once its target entry is created.
pub fn resolve_active(conn: &Connection, target_ref: &str) -> Result<Option<MemoryEntry>> {
    let Some(entry) = memory::get_entry(conn, target_ref)? else {
        return Ok(None);
    };
    if entry.status == memory::EntryStatus::Superseded {
        return Ok(None);
    }
    if let Some(exp) = entry.expires_at {
        if exp <= Utc::now().timestamp() {
            return Ok(None);
        }
    }
    Ok(Some(entry))
}

/// `(status, confirmations, corrections)` for a memory target, or `None` if it
/// does not exist. `corrections` is read directly from the column (it is not on
/// the `MemoryEntry` struct).
fn target_health(conn: &Connection, id: &str) -> Result<Option<(String, i64, i64)>> {
    let row = conn
        .query_row(
            "SELECT status, confirmations, corrections FROM memory_entries WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                ))
            },
        )
        .optional()?;
    Ok(row)
}

/// True if the entry has an outgoing `derived_from` or `supports` edge to a
/// memory target that is superseded or net-refuted (corrections > confirmations).
///
/// A stale dependency means the entry's basis is no longer sound. This is a
/// read-only, query-time signal (surfaced as a `[STALE-DEP]` marker at injection)
/// — it never mutates stored confidence.
pub fn has_stale_dependency(conn: &Connection, id: &str) -> Result<bool> {
    for rel in [MemoryRelation::DerivedFrom, MemoryRelation::Supports] {
        for edge in outgoing(conn, id, Some(rel))? {
            if edge.target_kind != TargetKind::Memory.as_str() {
                continue;
            }
            if let Some((status, confirmations, corrections)) =
                target_health(conn, &edge.target_ref)?
            {
                if status == "superseded" || corrections > confirmations {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::memory::{EntryStatus, EntryType, SourceType};
    use crate::store::schema::init_schema;
    use std::str::FromStr;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn insert_memory(conn: &Connection, id: &str) {
        let entry = MemoryEntry {
            id: id.to_string(),
            title: format!("Title {id}"),
            content: "content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: 1,
            updated_at: 1,
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
        memory::add_entry(conn, &entry).unwrap();
    }

    #[test]
    fn test_relation_roundtrip_and_closed_set() {
        for r in MemoryRelation::ALL {
            assert_eq!(MemoryRelation::from_str(r.as_str()).unwrap(), r);
        }
        let err = MemoryRelation::from_str("mentions").unwrap_err();
        assert!(
            err.contains("supports"),
            "error must list the closed set: {err}"
        );
        assert!(err.contains("relates_to"));
    }

    #[test]
    fn test_add_and_get_outgoing() {
        let conn = setup_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        add_edge(
            &conn,
            "a",
            "b",
            TargetKind::Memory,
            MemoryRelation::Supports,
        )
        .unwrap();

        let out = outgoing(&conn, "a", None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_ref, "b");
        assert_eq!(out[0].relation, "supports");
        assert_eq!(out[0].target_kind, "memory");
    }

    #[test]
    fn test_add_edge_is_idempotent() {
        let conn = setup_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        add_edge(
            &conn,
            "a",
            "b",
            TargetKind::Memory,
            MemoryRelation::RelatesTo,
        )
        .unwrap();
        add_edge(
            &conn,
            "a",
            "b",
            TargetKind::Memory,
            MemoryRelation::RelatesTo,
        )
        .unwrap();

        assert_eq!(
            outgoing(&conn, "a", None).unwrap().len(),
            1,
            "duplicate edge must not double-insert"
        );
    }

    #[test]
    fn test_outgoing_relation_filter() {
        let conn = setup_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        insert_memory(&conn, "c");
        add_edge(
            &conn,
            "a",
            "b",
            TargetKind::Memory,
            MemoryRelation::Supports,
        )
        .unwrap();
        add_edge(
            &conn,
            "a",
            "c",
            TargetKind::Memory,
            MemoryRelation::Contradicts,
        )
        .unwrap();

        let supports = outgoing(&conn, "a", Some(MemoryRelation::Supports)).unwrap();
        assert_eq!(supports.len(), 1);
        assert_eq!(supports[0].target_ref, "b");
    }

    #[test]
    fn test_incoming_relation_filter_and_forms() {
        let conn = setup_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        add_edge(
            &conn,
            "a",
            "b",
            TargetKind::Memory,
            MemoryRelation::Supports,
        )
        .unwrap();

        let inc = incoming(&conn, "b", None).unwrap();
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].source_id, "a");

        // Relation filter that excludes it returns nothing.
        assert!(
            incoming(&conn, "b", Some(MemoryRelation::Contradicts))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_remove_edge_idempotent() {
        let conn = setup_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        add_edge(
            &conn,
            "a",
            "b",
            TargetKind::Memory,
            MemoryRelation::Supports,
        )
        .unwrap();

        assert_eq!(
            remove_edge(&conn, "a", "b", MemoryRelation::Supports).unwrap(),
            1
        );
        assert_eq!(
            remove_edge(&conn, "a", "b", MemoryRelation::Supports).unwrap(),
            0,
            "removing twice is a no-op"
        );
        assert!(outgoing(&conn, "a", None).unwrap().is_empty());
    }

    #[test]
    fn test_supersedes_edge_syncs_scalar_and_status_atomically() {
        let conn = setup_db();
        insert_memory(&conn, "new-entry");
        insert_memory(&conn, "old-entry");

        // new-entry supersedes old-entry.
        add_edge(
            &conn,
            "new-entry",
            "old-entry",
            TargetKind::Memory,
            MemoryRelation::Supersedes,
        )
        .unwrap();

        let old = memory::get_entry(&conn, "old-entry").unwrap().unwrap();
        assert_eq!(
            old.status,
            EntryStatus::Superseded,
            "target status must flip to superseded"
        );
        assert_eq!(
            old.superseded_by.as_deref(),
            Some("new-entry"),
            "scalar must point at superseding entry"
        );

        // The edge itself is present.
        let out = outgoing(&conn, "new-entry", Some(MemoryRelation::Supersedes)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_ref, "old-entry");
    }

    #[test]
    fn test_supersedes_doc_kind_does_not_touch_scalar() {
        // A supersedes edge to a DOC target must not mutate any memory entry.
        let conn = setup_db();
        insert_memory(&conn, "a");
        add_edge(
            &conn,
            "a",
            "docs/old.md",
            TargetKind::Doc,
            MemoryRelation::Supersedes,
        )
        .unwrap();

        let a = memory::get_entry(&conn, "a").unwrap().unwrap();
        assert_eq!(a.status, EntryStatus::Active);
        assert!(a.superseded_by.is_none());
    }

    #[test]
    fn test_dangling_target_resolves_once_target_exists() {
        let conn = setup_db();
        insert_memory(&conn, "a");
        // Edge to a target that does not exist yet.
        add_edge(
            &conn,
            "a",
            "future",
            TargetKind::Memory,
            MemoryRelation::RelatesTo,
        )
        .unwrap();

        // Edge is stored; outgoing returns it even while dangling.
        assert_eq!(outgoing(&conn, "a", None).unwrap().len(), 1);
        assert!(
            resolve_active(&conn, "future").unwrap().is_none(),
            "dangling target has no live entry yet"
        );

        // Once the target entry is created, it resolves.
        insert_memory(&conn, "future");
        assert!(
            resolve_active(&conn, "future").unwrap().is_some(),
            "target now resolves"
        );
    }

    #[test]
    fn test_resolve_active_excludes_superseded() {
        let conn = setup_db();
        insert_memory(&conn, "new-entry");
        insert_memory(&conn, "old-entry");
        add_edge(
            &conn,
            "new-entry",
            "old-entry",
            TargetKind::Memory,
            MemoryRelation::Supersedes,
        )
        .unwrap();

        // old-entry is now superseded → not injectable.
        assert!(resolve_active(&conn, "old-entry").unwrap().is_none());
        assert!(resolve_active(&conn, "new-entry").unwrap().is_some());
    }

    #[test]
    fn test_has_stale_dependency() {
        let conn = setup_db();
        insert_memory(&conn, "derived");
        insert_memory(&conn, "healthy-base");
        insert_memory(&conn, "old-base");

        // derived --derived_from--> healthy-base (active) → not stale.
        add_edge(
            &conn,
            "derived",
            "healthy-base",
            TargetKind::Memory,
            MemoryRelation::DerivedFrom,
        )
        .unwrap();
        assert!(!has_stale_dependency(&conn, "derived").unwrap());

        // Add a supports edge to a superseded base → now stale.
        conn.execute(
            "UPDATE memory_entries SET status = 'superseded' WHERE id = 'old-base'",
            [],
        )
        .unwrap();
        add_edge(
            &conn,
            "derived",
            "old-base",
            TargetKind::Memory,
            MemoryRelation::Supports,
        )
        .unwrap();
        assert!(has_stale_dependency(&conn, "derived").unwrap());
    }

    #[test]
    fn test_stale_dependency_on_net_refuted_target() {
        let conn = setup_db();
        insert_memory(&conn, "child");
        insert_memory(&conn, "refuted-base");
        // refuted-base has more corrections than confirmations → net-refuted.
        conn.execute(
            "UPDATE memory_entries SET confirmations = 1, corrections = 4 WHERE id = 'refuted-base'",
            [],
        )
        .unwrap();
        add_edge(
            &conn,
            "child",
            "refuted-base",
            TargetKind::Memory,
            MemoryRelation::DerivedFrom,
        )
        .unwrap();
        assert!(has_stale_dependency(&conn, "child").unwrap());
    }

    #[test]
    fn test_relates_to_edge_does_not_count_as_stale_dep() {
        let conn = setup_db();
        insert_memory(&conn, "note");
        insert_memory(&conn, "dead");
        conn.execute(
            "UPDATE memory_entries SET status = 'superseded' WHERE id = 'dead'",
            [],
        )
        .unwrap();
        // relates_to is a weak link, not a dependency → never triggers STALE-DEP.
        add_edge(
            &conn,
            "note",
            "dead",
            TargetKind::Memory,
            MemoryRelation::RelatesTo,
        )
        .unwrap();
        assert!(!has_stale_dependency(&conn, "note").unwrap());
    }

    #[test]
    fn test_edge_cascades_on_source_delete() {
        let conn = setup_db();
        insert_memory(&conn, "a");
        insert_memory(&conn, "b");
        add_edge(
            &conn,
            "a",
            "b",
            TargetKind::Memory,
            MemoryRelation::Supports,
        )
        .unwrap();

        conn.execute("DELETE FROM memory_entries WHERE id = 'a'", [])
            .unwrap();
        assert!(
            incoming(&conn, "b", None).unwrap().is_empty(),
            "edge must cascade-delete with its source"
        );
    }
}
