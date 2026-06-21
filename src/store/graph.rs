//! Knowledge-graph edges between documents and entity slugs.
//!
//! An edge originates from an indexed document (`source_doc_id`) and points at a
//! free-text `target_ref` (a slug or path). The target may resolve to another
//! document at query time, or stay dangling until its target is indexed —
//! dangling edges survive re-indexing so cross-document links work regardless of
//! indexing order.
//!
//! Edges are derived during indexing from two sources, distinguished by
//! `source_kind`: allowlisted frontmatter keys ("frontmatter", strong relations)
//! and body wikilinks ("wikilink", soft relations).

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::Utc;
use rusqlite::{Connection, params};

use crate::error::Result;

/// `source_kind` for an edge derived from a typed frontmatter relation.
pub const KIND_FRONTMATTER: &str = "frontmatter";
/// `source_kind` for an edge derived from a body wikilink.
pub const KIND_WIKILINK: &str = "wikilink";
/// `relation` assigned to wikilink edges (they carry no typed relation name).
pub const RELATION_WIKILINK: &str = "links_to";

/// A knowledge-graph edge: document -> entity reference.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Edge {
    pub id: i64,
    pub source_doc_id: i64,
    pub target_ref: String,
    pub relation: String,
    pub source_kind: String,
    pub scope: Option<String>,
    pub created_at: i64,
}

/// Add an edge from `source_doc_id` to `target_ref`.
pub fn add_edge(
    conn: &Connection,
    source_doc_id: i64,
    target_ref: &str,
    relation: &str,
    source_kind: &str,
    scope: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO edges (source_doc_id, target_ref, relation, source_kind, scope, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            source_doc_id,
            target_ref,
            relation,
            source_kind,
            scope,
            Utc::now().timestamp(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Delete all edges originating from a document.
///
/// Called before re-inserting a document's edges so re-indexing is idempotent
/// (no duplicate rows).
pub fn delete_edges_for_source(conn: &Connection, source_doc_id: i64) -> Result<usize> {
    let rows = conn.execute(
        "DELETE FROM edges WHERE source_doc_id = ?1",
        params![source_doc_id],
    )?;
    Ok(rows)
}

fn map_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
    Ok(Edge {
        id: row.get(0)?,
        source_doc_id: row.get(1)?,
        target_ref: row.get(2)?,
        relation: row.get(3)?,
        source_kind: row.get(4)?,
        scope: row.get(5)?,
        created_at: row.get(6)?,
    })
}

const EDGE_COLUMNS: &str =
    "id, source_doc_id, target_ref, relation, source_kind, scope, created_at";

/// Outgoing edges from a document, optionally filtered by relation.
pub fn get_outgoing(
    conn: &Connection,
    source_doc_id: i64,
    relation: Option<&str>,
) -> Result<Vec<Edge>> {
    let mut edges = Vec::new();
    match relation {
        Some(rel) => {
            let sql = format!(
                "SELECT {EDGE_COLUMNS} FROM edges
                 WHERE source_doc_id = ?1 AND relation = ?2 ORDER BY created_at DESC, id DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![source_doc_id, rel], map_edge)?;
            for row in rows {
                edges.push(row?);
            }
        }
        None => {
            let sql = format!(
                "SELECT {EDGE_COLUMNS} FROM edges
                 WHERE source_doc_id = ?1 ORDER BY created_at DESC, id DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![source_doc_id], map_edge)?;
            for row in rows {
                edges.push(row?);
            }
        }
    }
    Ok(edges)
}

/// Incoming edges pointing at an entity (backlinks), optionally filtered by relation.
///
/// Matches `target_ref` against every equivalent textual form of `entity` (see
/// [`ref_forms`]) so a wikilink `[[projects/x]]` and a document path
/// `projects/x.md` resolve to the same node.
pub fn get_incoming(conn: &Connection, entity: &str, relation: Option<&str>) -> Result<Vec<Edge>> {
    let forms = ref_forms(entity);
    let form_count = forms.len();
    let placeholders = (1..=form_count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");

    let mut sql = format!("SELECT {EDGE_COLUMNS} FROM edges WHERE target_ref IN ({placeholders})");

    // Own all bind values so their lifetime outlives the query.
    let mut binds: Vec<String> = forms;
    if let Some(rel) = relation {
        sql.push_str(&format!(" AND relation = ?{}", form_count + 1));
        binds.push(rel.to_string());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");

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

/// The textual forms a reference may be written as: the raw string, with/without
/// a `.md` extension, and with a leading `./` or `/` trimmed.
pub fn ref_forms(reference: &str) -> Vec<String> {
    let trimmed = reference.trim_start_matches("./").trim_start_matches('/');
    let mut forms = Vec::new();
    let mut push = |s: String| {
        if !forms.contains(&s) {
            forms.push(s);
        }
    };
    push(reference.to_string());
    push(trimmed.to_string());
    if let Some(stripped) = trimmed.strip_suffix(".md") {
        push(stripped.to_string());
    } else {
        push(format!("{trimmed}.md"));
    }
    forms
}

/// Resolve a reference to a document id by matching `relative_path` against any
/// of its equivalent forms. Returns `None` for dangling references.
pub fn resolve_ref_to_doc(conn: &Connection, reference: &str) -> Result<Option<i64>> {
    for form in ref_forms(reference) {
        let id: Option<i64> = conn
            .query_row(
                "SELECT id FROM documents WHERE relative_path = ?1 LIMIT 1",
                params![form],
                |row| row.get(0),
            )
            .ok();
        if id.is_some() {
            return Ok(id);
        }
    }
    Ok(None)
}

fn doc_relative_path(conn: &Connection, doc_id: i64) -> Result<Option<String>> {
    let path: Option<String> = conn
        .query_row(
            "SELECT relative_path FROM documents WHERE id = ?1",
            params![doc_id],
            |row| row.get(0),
        )
        .ok();
    Ok(path)
}

/// The canonical node key for a reference: a resolved document's `relative_path`,
/// or the raw reference for a dangling target. Canonical keying lets traversal
/// treat `[[projects/x]]` and `projects/x.md` as the same node.
fn canonical_key(conn: &Connection, reference: &str) -> Result<String> {
    if let Some(doc_id) = resolve_ref_to_doc(conn, reference)? {
        if let Some(path) = doc_relative_path(conn, doc_id)? {
            return Ok(path);
        }
    }
    Ok(reference.to_string())
}

/// Undirected adjacency of a node (outgoing targets ∪ incoming sources), as
/// canonical node keys. A dangling node (not a document) has no adjacency.
fn adjacent_keys(conn: &Connection, node_key: &str, relation: Option<&str>) -> Result<Vec<String>> {
    let Some(doc_id) = resolve_ref_to_doc(conn, node_key)? else {
        return Ok(Vec::new());
    };

    let mut keys = Vec::new();
    for edge in get_outgoing(conn, doc_id, relation)? {
        keys.push(canonical_key(conn, &edge.target_ref)?);
    }
    for edge in get_incoming(conn, node_key, relation)? {
        if let Some(path) = doc_relative_path(conn, edge.source_doc_id)? {
            keys.push(path);
        }
    }
    Ok(keys)
}

/// A neighbor node reached during traversal, with its distance from the start.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Neighbor {
    pub entity: String,
    pub depth: u32,
}

/// Breadth-first neighbors of `start_doc_id` up to `depth` hops (undirected).
///
/// The start node itself is excluded. Dangling targets are included as leaves
/// but are not expanded further.
pub fn neighbors(
    conn: &Connection,
    start_doc_id: i64,
    relation: Option<&str>,
    depth: u32,
) -> Result<Vec<Neighbor>> {
    let Some(start_key) = doc_relative_path(conn, start_doc_id)? else {
        return Ok(Vec::new());
    };

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start_key.clone());
    let mut frontier = vec![start_key];
    let mut out = Vec::new();

    for hop in 1..=depth {
        let mut next = Vec::new();
        for node in &frontier {
            for key in adjacent_keys(conn, node, relation)? {
                if visited.insert(key.clone()) {
                    out.push(Neighbor {
                        entity: key.clone(),
                        depth: hop,
                    });
                    next.push(key);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(out)
}

/// Shortest undirected path from `start_doc_id` to `target` within `max_hops`.
///
/// Returns the sequence of node keys from start to target inclusive, or `None`
/// if unreachable. `target` is matched on its canonical key.
pub fn shortest_path(
    conn: &Connection,
    start_doc_id: i64,
    target: &str,
    max_hops: u32,
) -> Result<Option<Vec<String>>> {
    let Some(start_key) = doc_relative_path(conn, start_doc_id)? else {
        return Ok(None);
    };
    let target_key = canonical_key(conn, target)?;

    if start_key == target_key {
        return Ok(Some(vec![start_key]));
    }

    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(start_key.clone());
    let mut prev: HashMap<String, String> = HashMap::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((start_key.clone(), 0));

    while let Some((node, hops)) = queue.pop_front() {
        if hops >= max_hops {
            continue;
        }
        for key in adjacent_keys(conn, &node, None)? {
            if !visited.insert(key.clone()) {
                continue;
            }
            prev.insert(key.clone(), node.clone());
            if key == target_key {
                return Ok(Some(reconstruct_path(&prev, &start_key, &key)));
            }
            queue.push_back((key, hops + 1));
        }
    }
    Ok(None)
}

fn reconstruct_path(prev: &HashMap<String, String>, start: &str, end: &str) -> Vec<String> {
    let mut path = vec![end.to_string()];
    let mut cur = end.to_string();
    while cur != start {
        match prev.get(&cur) {
            Some(p) => {
                path.push(p.clone());
                cur = p.clone();
            }
            None => break,
        }
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::init_schema;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at)
             VALUES ('docs', './docs', '**/*.md', 1, 1)",
            [],
        )
        .unwrap();
        conn
    }

    /// Insert a document with the given relative path, return its id.
    fn insert_doc(conn: &Connection, path: &str) -> i64 {
        let hash = format!("h-{path}");
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?1, '# x', 1)",
            params![hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at)
             VALUES ('docs', ?1, ?2, 1, 1)",
            params![path, hash],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn test_add_and_get_outgoing() {
        let conn = setup_db();
        let doc = insert_doc(&conn, "a.md");
        let id = add_edge(&conn, doc, "alice", "owner", KIND_FRONTMATTER, None).unwrap();
        assert!(id > 0);

        let out = get_outgoing(&conn, doc, None).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target_ref, "alice");
        assert_eq!(out[0].relation, "owner");
        assert_eq!(out[0].source_kind, KIND_FRONTMATTER);
    }

    #[test]
    fn test_get_outgoing_relation_filter() {
        let conn = setup_db();
        let doc = insert_doc(&conn, "a.md");
        add_edge(&conn, doc, "alice", "owner", KIND_FRONTMATTER, None).unwrap();
        add_edge(&conn, doc, "growth", "themes", KIND_FRONTMATTER, None).unwrap();

        let owners = get_outgoing(&conn, doc, Some("owner")).unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].target_ref, "alice");
    }

    #[test]
    fn test_get_incoming_matches_equivalent_forms() {
        let conn = setup_db();
        let doc = insert_doc(&conn, "a.md");
        // Edge written as a wikilink without the .md extension.
        add_edge(
            &conn,
            doc,
            "projects/x",
            RELATION_WIKILINK,
            KIND_WIKILINK,
            None,
        )
        .unwrap();

        // Backlinks queried by the document path form must still match.
        let by_path = get_incoming(&conn, "projects/x.md", None).unwrap();
        assert_eq!(by_path.len(), 1);
        // And by the raw slug form.
        let by_slug = get_incoming(&conn, "projects/x", None).unwrap();
        assert_eq!(by_slug.len(), 1);
    }

    #[test]
    fn test_delete_edges_for_source_is_idempotent() {
        let conn = setup_db();
        let doc = insert_doc(&conn, "a.md");
        add_edge(&conn, doc, "alice", "owner", KIND_FRONTMATTER, None).unwrap();
        add_edge(&conn, doc, "bob", "owner", KIND_FRONTMATTER, None).unwrap();

        // Simulate re-index: purge then re-add the same edges.
        let removed = delete_edges_for_source(&conn, doc).unwrap();
        assert_eq!(removed, 2);
        add_edge(&conn, doc, "alice", "owner", KIND_FRONTMATTER, None).unwrap();
        add_edge(&conn, doc, "bob", "owner", KIND_FRONTMATTER, None).unwrap();

        let out = get_outgoing(&conn, doc, None).unwrap();
        assert_eq!(out.len(), 2, "re-index must not duplicate edges");
    }

    #[test]
    fn test_dangling_edge_survives_and_resolves_later() {
        let conn = setup_db();
        let project = insert_doc(&conn, "projects/x.md");
        // project references owner "alice" before alice.md exists.
        add_edge(&conn, project, "alice", "owner", KIND_FRONTMATTER, None).unwrap();

        // Dangling: backlinks of alice work, but it resolves to no doc yet.
        assert!(resolve_ref_to_doc(&conn, "alice").unwrap().is_none());
        assert_eq!(get_incoming(&conn, "alice", None).unwrap().len(), 1);

        // alice.md is indexed later — the same edge now resolves to a document.
        let alice = insert_doc(&conn, "alice.md");
        assert_eq!(resolve_ref_to_doc(&conn, "alice").unwrap(), Some(alice));
        assert_eq!(get_incoming(&conn, "alice", None).unwrap().len(), 1);
    }

    #[test]
    fn test_neighbors_depth_and_undirected() {
        let conn = setup_db();
        let a = insert_doc(&conn, "a.md");
        let _b = insert_doc(&conn, "b.md");
        let _c = insert_doc(&conn, "c.md");
        // a -> b (outgoing), c -> b (so b's neighbor c is reached via incoming).
        add_edge(&conn, a, "b", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();
        add_edge(&conn, _c, "b", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();

        let depth1 = neighbors(&conn, a, None, 1).unwrap();
        let d1: HashSet<_> = depth1.iter().map(|n| n.entity.as_str()).collect();
        assert!(d1.contains("b.md"), "b is a direct neighbor of a");
        assert!(!d1.contains("c.md"), "c is two hops away");

        let depth2 = neighbors(&conn, a, None, 2).unwrap();
        let d2: HashSet<_> = depth2.iter().map(|n| n.entity.as_str()).collect();
        assert!(
            d2.contains("c.md"),
            "c reachable from a at depth 2 (undirected)"
        );
    }

    #[test]
    fn test_neighbors_cycle_terminates() {
        let conn = setup_db();
        let a = insert_doc(&conn, "a.md");
        let b = insert_doc(&conn, "b.md");
        add_edge(&conn, a, "b", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();
        add_edge(&conn, b, "a", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();

        // Must not loop forever; b reached once, a is the start (excluded).
        let result = neighbors(&conn, a, None, 10).unwrap();
        let entities: HashSet<_> = result.iter().map(|n| n.entity.as_str()).collect();
        assert!(entities.contains("b.md"));
        assert!(
            !entities.contains("a.md"),
            "start node excluded from neighbors"
        );
    }

    #[test]
    fn test_shortest_path() {
        let conn = setup_db();
        let a = insert_doc(&conn, "a.md");
        let b = insert_doc(&conn, "b.md");
        let c = insert_doc(&conn, "c.md");
        add_edge(&conn, a, "b", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();
        add_edge(&conn, b, "c", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();
        let _ = c;

        let path = shortest_path(&conn, a, "c", 6).unwrap().unwrap();
        assert_eq!(path, vec!["a.md", "b.md", "c.md"]);
    }

    #[test]
    fn test_shortest_path_unreachable() {
        let conn = setup_db();
        let a = insert_doc(&conn, "a.md");
        let _isolated = insert_doc(&conn, "z.md");
        add_edge(&conn, a, "b", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();

        assert!(shortest_path(&conn, a, "z.md", 6).unwrap().is_none());
    }

    #[test]
    fn test_shortest_path_respects_max_hops() {
        let conn = setup_db();
        let a = insert_doc(&conn, "a.md");
        let b = insert_doc(&conn, "b.md");
        let c = insert_doc(&conn, "c.md");
        add_edge(&conn, a, "b", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();
        add_edge(&conn, b, "c", RELATION_WIKILINK, KIND_WIKILINK, None).unwrap();
        let _ = c;

        // c is 2 hops away; a max of 1 hop must not find it.
        assert!(shortest_path(&conn, a, "c", 1).unwrap().is_none());
    }
}
