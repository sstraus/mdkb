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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

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

/// An edge rendered for display: the numeric `source_doc_id` is resolved to the
/// source document's `relative_path` so output never leaks opaque ids.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EdgeView {
    pub source: String,
    pub target_ref: String,
    pub relation: String,
    pub source_kind: String,
    pub scope: Option<String>,
}

/// Resolve every edge's `source_doc_id` to its document `relative_path` in a
/// single batched query, returning display-ready [`EdgeView`]s. A source that no
/// longer resolves (should not happen — edges are dropped with their document)
/// falls back to `#<id>` rather than a bare number.
pub fn edge_views(conn: &Connection, edges: &[Edge]) -> Result<Vec<EdgeView>> {
    let mut ids: Vec<i64> = edges.iter().map(|e| e.source_doc_id).collect();
    ids.sort_unstable();
    ids.dedup();

    let mut paths: HashMap<i64, String> = HashMap::new();
    if !ids.is_empty() {
        let placeholders = (1..=ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT id, relative_path FROM documents WHERE id IN ({placeholders})");
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, path) = row?;
            paths.insert(id, path);
        }
    }

    Ok(edges
        .iter()
        .map(|e| EdgeView {
            source: paths
                .get(&e.source_doc_id)
                .cloned()
                .unwrap_or_else(|| format!("#{}", e.source_doc_id)),
            target_ref: e.target_ref.clone(),
            relation: e.relation.clone(),
            source_kind: e.source_kind.clone(),
            scope: e.scope.clone(),
        })
        .collect())
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

/// Directory prefixes a collection-qualified reference may carry: the basename of
/// each collection's configured path (collection path `./map` → prefix `map`).
/// Stripping these lets `graph links map/people/x.md` resolve like `people/x.md`.
fn collection_prefixes(conn: &Connection) -> Vec<String> {
    let mut prefixes = Vec::new();
    let Ok(mut stmt) = conn.prepare("SELECT path FROM collections") else {
        return prefixes;
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        return prefixes;
    };
    for path in rows.flatten() {
        let base = path
            .trim_start_matches("./")
            .trim_start_matches('/')
            .trim_end_matches('/');
        let name = base.rsplit('/').next().unwrap_or(base);
        if !name.is_empty() && !prefixes.iter().any(|p| p == name) {
            prefixes.push(name.to_string());
        }
    }
    prefixes
}

/// Every textual form tried when resolving an entity reference, including
/// collection-prefix stripped variants. Exposed so NotFound errors can enumerate
/// what was attempted.
pub fn resolvable_forms(conn: &Connection, reference: &str) -> Vec<String> {
    let mut forms = ref_forms(reference);
    for prefix in collection_prefixes(conn) {
        if let Some(rest) = reference.strip_prefix(&format!("{prefix}/")) {
            for f in ref_forms(rest) {
                if !forms.contains(&f) {
                    forms.push(f);
                }
            }
        }
    }
    forms
}

/// Resolve an entity reference to a document id, tolerating collection-prefixed
/// paths (`map/people/x.md` == `people/x.md`). For hot traversal paths use
/// [`resolve_ref_to_doc`] instead (it skips the collection lookup).
pub fn resolve_entity_ref(conn: &Connection, reference: &str) -> Result<Option<i64>> {
    for form in resolvable_forms(conn, reference) {
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

/// Resolve `reference` to the canonical `relative_path` of an indexed document,
/// or `None` when it does not resolve to a real document (dangling reference or
/// an entity tag like `themes`/`owner`). One resolution pass — callers that need
/// "is this a real doc, and what's its path?" should use this instead of pairing
/// [`resolve_ref_to_doc`] with [`canonical_key`] (which would resolve twice).
pub fn resolve_to_path(conn: &Connection, reference: &str) -> Result<Option<String>> {
    match resolve_ref_to_doc(conn, reference)? {
        Some(doc_id) => doc_relative_path(conn, doc_id),
        None => Ok(None),
    }
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

/// Undirected adjacency of a node (outgoing targets ∪ incoming sources) as
/// `(canonical key, relation)` pairs. Carrying the relation lets traversal report
/// *why* nodes connect without a second query. A dangling node has no adjacency.
fn adjacent_pairs(
    conn: &Connection,
    node_key: &str,
    relation: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let Some(doc_id) = resolve_ref_to_doc(conn, node_key)? else {
        return Ok(Vec::new());
    };

    let mut pairs = Vec::new();
    for edge in get_outgoing(conn, doc_id, relation)? {
        pairs.push((canonical_key(conn, &edge.target_ref)?, edge.relation));
    }
    for edge in get_incoming(conn, node_key, relation)? {
        if let Some(path) = doc_relative_path(conn, edge.source_doc_id)? {
            pairs.push((path, edge.relation));
        }
    }
    Ok(pairs)
}

/// A neighbor node reached during traversal, with its distance from the start and
/// the relation label(s) it was reached through (`via`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Neighbor {
    pub entity: String,
    pub depth: u32,
    pub via: Vec<String>,
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
        // Discover this hop's new keys, accumulating every relation that reaches
        // each one (a key may be linked through several relations at the same hop).
        let mut order: Vec<String> = Vec::new();
        let mut via: HashMap<String, Vec<String>> = HashMap::new();
        for node in &frontier {
            for (key, rel) in adjacent_pairs(conn, node, relation)? {
                if visited.contains(&key) {
                    continue;
                }
                let rels = via.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    Vec::new()
                });
                if !rels.contains(&rel) {
                    rels.push(rel);
                }
            }
        }
        if order.is_empty() {
            break;
        }
        let mut next = Vec::new();
        for key in order {
            visited.insert(key.clone());
            let mut rels = via.remove(&key).unwrap_or_default();
            rels.sort();
            out.push(Neighbor {
                entity: key.clone(),
                depth: hop,
                via: rels,
            });
            next.push(key);
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
        for (key, _rel) in adjacent_pairs(conn, &node, None)? {
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

/// An edge whose `target_ref` resolves to no indexed document.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DanglingRef {
    pub target_ref: String,
    pub relation: String,
    pub source: String,
}

/// Edges pointing at a reference that resolves to no indexed document.
///
/// Full-table scan — an explicit gardening command only, never run inside
/// hooks/recall. Resolution per distinct `target_ref` is cached so duplicated
/// targets are resolved once.
pub fn dangling(conn: &Connection) -> Result<Vec<DanglingRef>> {
    let sql =
        format!("SELECT {EDGE_COLUMNS} FROM edges ORDER BY target_ref, relation, source_doc_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], map_edge)?;

    let mut resolves: HashMap<String, bool> = HashMap::new();
    let mut out = Vec::new();
    for row in rows {
        let e = row?;
        let ok = match resolves.get(&e.target_ref) {
            Some(v) => *v,
            None => {
                let ok = resolve_ref_to_doc(conn, &e.target_ref)?.is_some();
                resolves.insert(e.target_ref.clone(), ok);
                ok
            }
        };
        if !ok {
            let source = doc_relative_path(conn, e.source_doc_id)?
                .unwrap_or_else(|| format!("#{}", e.source_doc_id));
            out.push(DanglingRef {
                target_ref: e.target_ref,
                relation: e.relation,
                source,
            });
        }
    }
    Ok(out)
}

/// A graph node ranked by degree centrality, with a per-relation breakdown of the
/// edges incident to it (as source or target).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Hub {
    pub entity: String,
    pub in_degree: usize,
    pub out_degree: usize,
    pub by_relation: Vec<(String, usize)>,
}

#[derive(Default)]
struct HubAcc {
    in_degree: usize,
    out_degree: usize,
    by_relation: BTreeMap<String, usize>,
}

/// Top-`limit` entities by total degree (in + out), each with a per-relation
/// breakdown. `relation` restricts the scan to a single relation type; `limit`
/// of 0 returns all. Ties break on entity key for determinism.
///
/// Full-table scan — an explicit command only, never run inside hooks/recall.
pub fn hubs(conn: &Connection, relation: Option<&str>, limit: usize) -> Result<Vec<Hub>> {
    let sql = format!("SELECT {EDGE_COLUMNS} FROM edges");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], map_edge)?;

    let mut acc: HashMap<String, HubAcc> = HashMap::new();
    for row in rows {
        let e = row?;
        if let Some(rel) = relation {
            if e.relation != rel {
                continue;
            }
        }
        let source = doc_relative_path(conn, e.source_doc_id)?
            .unwrap_or_else(|| format!("#{}", e.source_doc_id));
        let target = canonical_key(conn, &e.target_ref)?;

        let s = acc.entry(source).or_default();
        s.out_degree += 1;
        *s.by_relation.entry(e.relation.clone()).or_default() += 1;

        let t = acc.entry(target).or_default();
        t.in_degree += 1;
        *t.by_relation.entry(e.relation.clone()).or_default() += 1;
    }

    let mut hubs: Vec<Hub> = acc
        .into_iter()
        .map(|(entity, a)| Hub {
            entity,
            in_degree: a.in_degree,
            out_degree: a.out_degree,
            by_relation: a.by_relation.into_iter().collect(),
        })
        .collect();
    hubs.sort_by(|x, y| {
        (y.in_degree + y.out_degree)
            .cmp(&(x.in_degree + x.out_degree))
            .then_with(|| x.entity.cmp(&y.entity))
    });
    if limit > 0 {
        hubs.truncate(limit);
    }
    Ok(hubs)
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

    #[test]
    fn test_edge_views_resolve_source_path_never_numeric() {
        let conn = setup_db();
        let doc = insert_doc(&conn, "people/stefano.md");
        add_edge(&conn, doc, "repos/mdkb", "works_on", KIND_FRONTMATTER, None).unwrap();

        let edges = get_outgoing(&conn, doc, None).unwrap();
        let views = edge_views(&conn, &edges).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].source, "people/stefano.md");
        assert_eq!(views[0].target_ref, "repos/mdkb");
        assert_eq!(views[0].relation, "works_on");
    }

    #[test]
    fn test_neighbors_carry_via_relations() {
        let conn = setup_db();
        let a = insert_doc(&conn, "a.md");
        let _b = insert_doc(&conn, "b.md");
        let _c = insert_doc(&conn, "c.md");
        add_edge(&conn, a, "b", "owner", KIND_FRONTMATTER, None).unwrap();
        add_edge(&conn, _b, "c", "themes", KIND_FRONTMATTER, None).unwrap();

        let nbrs = neighbors(&conn, a, None, 2).unwrap();
        assert!(!nbrs.is_empty());
        for n in &nbrs {
            assert!(
                !n.via.is_empty(),
                "every neighbor must carry the relation it was reached through: {n:?}"
            );
        }
        let b = nbrs.iter().find(|n| n.entity == "b.md").unwrap();
        assert_eq!(b.via, vec!["owner".to_string()]);
    }

    #[test]
    fn test_resolve_entity_ref_accepts_collection_prefix() {
        let conn = setup_db();
        // collection 'docs' lives at './map' — a reference may be written map/people/x.md.
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at)
             VALUES ('mapcoll', './map', '**/*.md', 1, 1)",
            [],
        )
        .unwrap();
        let doc = insert_doc(&conn, "people/x.md");

        assert_eq!(
            resolve_entity_ref(&conn, "map/people/x.md").unwrap(),
            Some(doc),
            "collection-prefixed path must resolve like the bare path"
        );
        assert_eq!(resolve_entity_ref(&conn, "people/x").unwrap(), Some(doc));

        let forms = resolvable_forms(&conn, "map/people/x.md");
        assert!(
            forms.iter().any(|f| f == "people/x.md"),
            "tried forms must include the stripped form: {forms:?}"
        );
    }

    #[test]
    fn test_dangling_lists_unresolved_targets() {
        let conn = setup_db();
        let project = insert_doc(&conn, "projects/x.md");
        let target = insert_doc(&conn, "people/alice.md");
        add_edge(
            &conn,
            project,
            "people/alice",
            "owner",
            KIND_FRONTMATTER,
            None,
        )
        .unwrap();
        add_edge(
            &conn,
            project,
            "teams/wiz",
            "related",
            KIND_FRONTMATTER,
            None,
        )
        .unwrap();
        let _ = target;

        let dangling = dangling(&conn).unwrap();
        assert_eq!(dangling.len(), 1, "only teams/wiz is unresolved");
        assert_eq!(dangling[0].target_ref, "teams/wiz");
        assert_eq!(dangling[0].relation, "related");
        assert_eq!(dangling[0].source, "projects/x.md");
    }

    #[test]
    fn test_hubs_rank_by_degree_with_relation_breakdown() {
        let conn = setup_db();
        let a = insert_doc(&conn, "a.md");
        let b = insert_doc(&conn, "b.md");
        let hub = insert_doc(&conn, "hub.md");
        // Everyone points at hub → hub has the highest in-degree.
        add_edge(&conn, a, "hub", "owner", KIND_FRONTMATTER, None).unwrap();
        add_edge(&conn, b, "hub", "owner", KIND_FRONTMATTER, None).unwrap();
        add_edge(&conn, hub, "a", "related", KIND_FRONTMATTER, None).unwrap();

        let ranked = hubs(&conn, None, 10).unwrap();
        assert_eq!(
            ranked[0].entity, "hub.md",
            "hub ranks first by total degree"
        );
        assert_eq!(ranked[0].in_degree, 2);
        assert_eq!(ranked[0].out_degree, 1);
        assert!(ranked[0].by_relation.iter().any(|(r, _)| r == "owner"));

        // Relation filter narrows the scan.
        let owners = hubs(&conn, Some("owner"), 10).unwrap();
        let hub_row = owners.iter().find(|h| h.entity == "hub.md").unwrap();
        assert_eq!(hub_row.in_degree, 2);
        assert_eq!(
            hub_row.out_degree, 0,
            "hub's out-edge is 'related', filtered out"
        );
    }
}
