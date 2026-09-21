//! Collections, the document graph, and document evolution.
//!
//! Registering a collection, walking link edges, and recording that one
//! document supersedes another are all store operations. The MCP graph tools
//! and the CLI are two callers of the same thing.

use std::path::Path;

use crate::core::Context;
use crate::domain::Collection;
use crate::error::{Error, ErrorKind, Result};
use crate::store::evolution::{Evolution, RelationshipType};
use crate::store::{collections, documents, evolution};

/// Handle `mdkb collection add` command.
pub fn handle_collection_add(ctx: &Context, name: &str, path: &str, pattern: &str) -> Result<()> {
    validate_collection_name(name)?;
    validate_collection_path(ctx.root(), path)?;

    let now = chrono::Utc::now().timestamp();
    let collection = Collection {
        name: name.to_string(),
        path: path.to_string(),
        pattern: pattern.to_string(),
        source: crate::domain::COLLECTION_SOURCE_MANUAL.to_string(),
        created_at: now,
        updated_at: now,
    };

    collections::add_collection(&ctx.conn, &collection)?;
    Ok(())
}
/// Handle `mdkb collection update` command — change a collection's path or
/// pattern without dropping what it already holds.
///
/// `documents.collection` cascades on delete, so the only way to fix a wrong
/// pattern used to be `remove` + `add`, which erased every indexed document and
/// forced a full re-embed of the whole collection — minutes of local ONNX
/// inference to correct one glob (issue #11). Rewriting the row in place keeps
/// the documents and their embeddings; the next `mdkb update` reconciles
/// incrementally, indexing what the new pattern newly matches and dropping what
/// it no longer does.
///
/// That saving is real for `--pattern` only. A document is keyed by its path
/// *relative to the collection's base*, so `--path` does not carry the old rows
/// over to the new base: they name files the next walk will not find and are
/// reconciled away, while the new base is indexed — and embedded — from
/// scratch. (A relative path that exists under both bases keeps its row, but it
/// now describes a different file and is re-read anyway.) The command still
/// beats `remove` + `add` there because it keeps the collection's identity
/// (`created_at`, `source`), not because it saves the inference.
///
/// Returns the collection as it now stands. `None` for either field means
/// "leave it alone"; naming neither is refused rather than reported as a
/// successful update that changed nothing.
pub fn handle_collection_update(
    ctx: &Context,
    name: &str,
    path: Option<&str>,
    pattern: Option<&str>,
) -> Result<Collection> {
    if path.is_none() && pattern.is_none() {
        return Err(Error::other(
            "nothing to update: pass --pattern, --path, or both".to_string(),
        ));
    }

    let Some(existing) = collections::get_collection(&ctx.conn, name)? else {
        return Err(ErrorKind::CollectionNotFound {
            name: name.to_string(),
        }
        .into());
    };

    if let Some(path) = path {
        validate_collection_path(ctx.root(), path)?;
    }
    if let Some(pattern) = pattern {
        crate::core::indexing::compile_collection_matcher(pattern)
            .map_err(|e| Error::other(format!("Invalid glob pattern '{pattern}': {e}")))?;
    }

    // The upsert keeps `created_at` and `source`: this is the same collection,
    // pointed somewhere else. A convention collection retargeted by hand stays
    // a convention collection, so `apply_conventions` still leaves it alone.
    let updated = Collection {
        path: path.unwrap_or(&existing.path).to_string(),
        pattern: pattern.unwrap_or(&existing.pattern).to_string(),
        updated_at: chrono::Utc::now().timestamp(),
        ..existing
    };
    collections::add_collection(&ctx.conn, &updated)?;
    Ok(updated)
}
/// Handle `mdkb collection remove` command.
pub fn handle_collection_remove(ctx: &Context, name: &str) -> Result<bool> {
    collections::remove_collection(&ctx.conn, name)
}
/// Handle `mdkb collection list` command.
pub fn handle_collection_list(ctx: &Context) -> Result<Vec<CollectionInfo>> {
    let colls = collections::list_collections(&ctx.conn)?;
    let mut out = Vec::with_capacity(colls.len());
    for c in colls {
        let doc_count = collections::get_collection_document_count(&ctx.conn, &c.name)?;
        out.push(CollectionInfo {
            name: c.name,
            path: c.path,
            pattern: c.pattern,
            doc_count,
        });
    }
    Ok(out)
}
/// Handle `mdkb collection rename` command.
pub fn handle_collection_rename(ctx: &Context, old_name: &str, new_name: &str) -> Result<()> {
    validate_collection_name(new_name)?;
    collections::rename_collection(&ctx.conn, old_name, new_name)
}
fn validate_collection_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_COLLECTION_NAME_LEN {
        return Err(Error::other(format!(
            "Collection name must be 1-{MAX_COLLECTION_NAME_LEN} chars"
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(Error::other(
            "Collection name must be lowercase alphanumeric with hyphens or underscores"
                .to_string(),
        ));
    }
    Ok(())
}
/// Validate that a collection path stays within the project root.
///
/// Uses `canonicalize` when the path exists on disk (resolves symlinks).
/// Falls back to lexical `..` rejection for paths that don't exist yet —
/// the robust canonicalize check in `handle_update_files` catches these
/// at index time regardless.
fn validate_collection_path(root: &Path, path: &str) -> Result<()> {
    let candidate = root.join(path);
    if let Ok(canonical) = candidate.canonicalize() {
        let canonical_root = root
            .canonicalize()
            .map_err(|e| Error::other(format!("Failed to canonicalize root: {e}")))?;
        if !canonical.starts_with(&canonical_root) {
            return Err(Error::other(format!(
                "Collection path '{}' escapes root directory (path traversal blocked)",
                path
            )));
        }
    } else if path.contains("..") {
        return Err(Error::other(format!(
            "Collection path '{}' contains path traversal pattern '..'",
            path
        )));
    }
    Ok(())
}
/// Handle `mdkb evolve supersedes` command.
pub fn handle_evolve_supersedes(
    ctx: &Context,
    new: &str,
    old: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Supersedes,
        None,
        reason,
    )
}
/// Handle `mdkb evolve updates` command.
pub fn handle_evolve_updates(
    ctx: &Context,
    new: &str,
    old: &str,
    scope: Option<&str>,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Updates,
        scope,
        reason,
    )
}
/// Handle `mdkb evolve corrects` command.
pub fn handle_evolve_corrects(
    ctx: &Context,
    new: &str,
    old: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Corrects,
        None,
        reason,
    )
}
/// Handle `mdkb evolve retracts` command.
pub fn handle_evolve_retracts(
    ctx: &Context,
    new: &str,
    old: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Retracts,
        None,
        reason,
    )
}
/// Handle `mdkb evolve extends` command.
pub fn handle_evolve_extends(
    ctx: &Context,
    new: &str,
    old: &str,
    reason: Option<&str>,
) -> Result<i64> {
    let new_id = resolve_document_id(ctx, new)?;
    let old_id = resolve_document_id(ctx, old)?;

    evolution::add_evolution(
        &ctx.conn,
        new_id,
        old_id,
        RelationshipType::Extends,
        None,
        reason,
    )
}
/// Handle `mdkb superseded-by` command - show what replaced this doc.
pub fn handle_superseded_by(ctx: &Context, path_or_id: &str) -> Result<Vec<Evolution>> {
    let doc_id = resolve_document_id(ctx, path_or_id)?;
    evolution::get_superseded_by(&ctx.conn, doc_id)
}
/// Outgoing edges from an entity (the entity must be an indexed document).
/// Edge sources are resolved to `relative_path` so output never leaks numeric ids.
pub fn handle_graph_links(
    ctx: &Context,
    entity: &str,
    relation: Option<&str>,
) -> Result<Vec<crate::store::graph::EdgeView>> {
    let doc_id = resolve_graph_entity(ctx, entity)?;
    let edges = crate::store::graph::get_outgoing(&ctx.conn, doc_id, relation)?;
    crate::store::graph::edge_views(&ctx.conn, &edges)
}
/// Incoming edges to an entity. Accepts a dangling slug — no document required.
/// Edge sources are resolved to `relative_path` so output never leaks numeric ids.
pub fn handle_graph_backlinks(
    ctx: &Context,
    entity: &str,
    relation: Option<&str>,
) -> Result<Vec<crate::store::graph::EdgeView>> {
    let edges = crate::store::graph::get_incoming(&ctx.conn, entity, relation)?;
    crate::store::graph::edge_views(&ctx.conn, &edges)
}
/// Adjacent entities up to `depth` hops (undirected); start must be a document.
pub fn handle_graph_neighbors(
    ctx: &Context,
    entity: &str,
    relation: Option<&str>,
    depth: u32,
) -> Result<Vec<crate::store::graph::Neighbor>> {
    let doc_id = resolve_graph_entity(ctx, entity)?;
    crate::store::graph::neighbors(&ctx.conn, doc_id, relation, depth)
}
/// Shortest undirected path from `a` (a document) to `b` (any entity).
pub fn handle_graph_path(
    ctx: &Context,
    a: &str,
    b: &str,
    max_hops: u32,
) -> Result<Option<Vec<String>>> {
    let start = resolve_graph_entity(ctx, a)?;
    let target = resolve_target_key(ctx, b)?;
    crate::store::graph::shortest_path(&ctx.conn, start, &target, max_hops)
}
/// References that resolve to no indexed document (full-table scan).
pub fn handle_graph_dangling(
    ctx: &Context,
    collection: Option<&str>,
) -> Result<Vec<crate::store::graph::DanglingRef>> {
    crate::store::graph::dangling(&ctx.conn, collection)
}
/// Entities ranked by degree centrality (full-table scan).
pub fn handle_graph_hubs(
    ctx: &Context,
    relation: Option<&str>,
    limit: usize,
) -> Result<Vec<crate::store::graph::Hub>> {
    crate::store::graph::hubs(&ctx.conn, relation, limit)
}
/// Resolve a document path or ID to a document ID.
pub(crate) fn resolve_document_id(ctx: &Context, path_or_id: &str) -> Result<i64> {
    // Try to parse as ID first
    if let Ok(id) = path_or_id.parse::<i64>() {
        // Verify it exists
        if documents::get_document(&ctx.conn, id)?.is_some() {
            return Ok(id);
        }
    }

    // Try as path - search across all collections
    let all_collections = collections::list_collections(&ctx.conn)?;
    for coll in &all_collections {
        if let Some(doc) = documents::get_document_by_path(&ctx.conn, &coll.name, path_or_id)? {
            return Ok(doc.id);
        }
    }

    Err(Error::from(ErrorKind::DocumentNotFound {
        id: path_or_id.to_string(),
    }))
}
/// Resolve a graph entity argument to a document id. Extends `resolve_document_id`
/// with the `.md`-form tolerance the rest of the graph layer uses, so that
/// `links`/`neighbors`/`path` accept a bare slug (`people/x`) exactly like
/// `backlinks` does — not only `people/x.md` or a numeric id.
fn resolve_graph_entity(ctx: &Context, entity: &str) -> Result<i64> {
    if let Ok(id) = resolve_document_id(ctx, entity) {
        return Ok(id);
    }
    // Tolerate collection-prefixed paths (`map/people/x.md` == `people/x.md`).
    if let Some(id) = crate::store::graph::resolve_entity_ref(&ctx.conn, entity)? {
        return Ok(id);
    }
    let tried = crate::store::graph::resolvable_forms(&ctx.conn, entity).join(", ");
    Err(Error::from(ErrorKind::DocumentNotFound {
        id: format!("{entity} (tried: {tried})"),
    }))
}
/// Canonical key for a path target. When `b` names an existing document — by
/// numeric id or path, exactly as the start argument is resolved — use that
/// document's `relative_path` so traversal can match it. Otherwise keep `b`
/// verbatim, preserving the dangling-target semantics (an unreachable slug).
fn resolve_target_key(ctx: &Context, b: &str) -> Result<String> {
    if let Ok(id) = resolve_document_id(ctx, b) {
        if let Some(doc) = documents::get_document(&ctx.conn, id)? {
            return Ok(doc.relative_path);
        }
    }
    Ok(b.to_string())
}
/// A collection with its indexed-document count, for `collection list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CollectionInfo {
    pub name: String,
    pub path: String,
    pub pattern: String,
    pub doc_count: i64,
}
const MAX_COLLECTION_NAME_LEN: usize = 100;

// ==================== Relation-Key Detection ====================

/// Frontmatter keys the evolution subsystem owns.
///
/// They point at documents and therefore score like relations, but an edge
/// from them would duplicate a relationship `store::evolution` already models
/// with its own semantics. Excluded from detection, never derived.
pub const NEVER_DERIVED: &[&str] = &[
    "supersedes",
    "updates",
    "corrects",
    "extends",
    "retracts",
];

/// The share of a key's values that must name something the index knows before
/// the key counts as a relation.
///
/// Measured 2026-09-21 on `brainstorming/work`, 404 documents: 17 relation keys
/// scored 1.0 and 23 metadata keys (`type`, `name`, `date`, `role`, `status`,
/// `github`, `slug`, `horizon`, `source`, …) scored 0.0. There was no middle
/// band, so any threshold strictly between 0 and 1 separates them. Half is the
/// honest expression of "most of its values name real things" and leaves room
/// for a corpus with a few dangling targets.
pub const RELATION_THRESHOLD: f64 = 0.5;

/// One frontmatter key, and how much of it names things the index knows.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationCandidate {
    pub key: String,
    /// Values that resolved to a document, by path or by declared identity.
    pub hits: usize,
    /// Values examined. A key whose values are free text scores `hits == 0`.
    pub total: usize,
}

impl RelationCandidate {
    pub fn score(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.hits as f64 / self.total as f64
        }
    }

    pub fn is_relation(&self) -> bool {
        self.total > 0 && self.score() >= RELATION_THRESHOLD
    }
}

/// What a detection run looked at, not just what it concluded.
///
/// The empty case has two very different causes — a corpus whose keys are all
/// metadata, and a corpus with no identity space for anything to resolve
/// against — and a caller that cannot tell them apart will report a guess as a
/// finding. `identities` is what separates them.
#[derive(Debug, Clone)]
pub struct RelationDetection {
    /// Keys at or above [`RELATION_THRESHOLD`], best first.
    pub candidates: Vec<RelationCandidate>,
    /// Every key scored, including the ones that scored zero.
    pub examined: Vec<RelationCandidate>,
    /// Documents whose frontmatter was readable.
    pub documents: usize,
    /// Rows in `document_aliases`. Zero means nothing can resolve by name.
    pub identities: usize,
}

impl RelationDetection {
    /// True when the corpus offers nothing to resolve against, so an empty
    /// result says nothing about the keys.
    pub fn has_no_identity_space(&self) -> bool {
        self.identities == 0
    }
}

/// Which frontmatter keys name things the index knows.
///
/// Measured, not guessed. Free text can never score, so metadata cannot enter
/// the graph: `type: person` only becomes a relation if some document actually
/// declares `person` as its identity, and in a real corpus none does.
///
/// Excluded before scoring: [`NEVER_DERIVED`] and the configured
/// `identity_keys` — a key cannot be both what a document IS and what it
/// points at.
pub fn detect_relation_keys(
    conn: &rusqlite::Connection,
    cfg: &crate::config::GraphConfig,
) -> Result<RelationDetection> {
    use std::collections::BTreeMap;

    let identities: usize = conn
        .query_row("SELECT COUNT(*) FROM document_aliases", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap_or(0) as usize;

    let mut scores: BTreeMap<String, RelationCandidate> = BTreeMap::new();
    let mut documents = 0usize;

    let mut stmt = conn.prepare(
        "SELECT metadata FROM documents
         WHERE (status = 'current' OR status IS NULL) AND metadata IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;

    for row in rows {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&row?) else {
            continue;
        };
        let Some(object) = value.as_object() else {
            continue;
        };
        documents += 1;

        for key in object.keys() {
            if NEVER_DERIVED.contains(&key.as_str())
                || cfg.identity_keys.iter().any(|k| k == key)
            {
                continue;
            }
            let refs = crate::domain::frontmatter::extract_relation_refs(Some(&value), key);
            if refs.is_empty() {
                continue;
            }
            let entry = scores
                .entry(key.clone())
                .or_insert_with(|| RelationCandidate {
                    key: key.clone(),
                    hits: 0,
                    total: 0,
                });
            for target in refs {
                entry.total += 1;
                if crate::store::graph::resolve_entity_ref(conn, &target)?.is_some() {
                    entry.hits += 1;
                }
            }
        }
    }

    let mut examined: Vec<RelationCandidate> = scores.into_values().collect();
    examined.sort_by(|a, b| {
        b.score()
            .partial_cmp(&a.score())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.total.cmp(&a.total))
            .then_with(|| a.key.cmp(&b.key))
    });
    let candidates = examined
        .iter()
        .filter(|c| c.is_relation())
        .cloned()
        .collect();

    Ok(RelationDetection {
        candidates,
        examined,
        documents,
        identities,
    })
}

#[cfg(test)]
mod relation_detection_tests {
    use super::*;
    use crate::config::GraphConfig;
    use crate::store::schema::init_schema;
    use rusqlite::{Connection, params};

    fn setup() -> Connection {
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

    /// Index a document at `path` carrying `metadata`, and record every
    /// identity it declares, exactly as `process_identities` would.
    fn doc(conn: &Connection, path: &str, metadata: &str) -> i64 {
        let hash = format!("h-{path}");
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?1, '# x', 1)",
            params![hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, metadata, file_modified_at, indexed_at)
             VALUES ('docs', ?1, ?2, ?3, 1, 1)",
            params![path, hash, metadata],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        let parsed: serde_json::Value = serde_json::from_str(metadata).unwrap();
        for key in ["id", "aliases"] {
            for alias in crate::domain::frontmatter::extract_relation_refs(Some(&parsed), key) {
                crate::store::graph::add_alias(conn, id, &alias, key).unwrap();
            }
        }
        id
    }

    /// A miniature of the measured corpus: typed people and orgs that declare
    /// their names, meetings that point at them, and metadata that does not.
    fn seed_typed_corpus(conn: &Connection) {
        doc(conn, "people/alice.md", r#"{"id":"person:alice","type":"person","name":"Alice","role":"Data Scientist"}"#);
        doc(conn, "people/bob.md", r#"{"id":"person:bob","type":"person","name":"Bob","role":"Engineer"}"#);
        doc(conn, "orgs/acme.md", r#"{"id":"org:acme","type":"org","name":"Acme"}"#);
        doc(
            conn,
            "meetings/m1.md",
            r#"{"id":"meeting:m1","type":"meeting","date":"2026-05-01","status":"done","attendees":["person:alice","person:bob"],"org":["org:acme"],"supersedes":["person:alice"]}"#,
        );
        doc(
            conn,
            "meetings/m2.md",
            r#"{"id":"meeting:m2","type":"meeting","date":"2026-05-02","status":"done","attendees":["person:alice"],"org":["org:acme"]}"#,
        );
    }

    #[test]
    fn relation_keys_score_one_and_metadata_keys_score_zero() {
        let conn = setup();
        seed_typed_corpus(&conn);

        let found = detect_relation_keys(&conn, &GraphConfig::default()).unwrap();
        let score = |k: &str| {
            found
                .examined
                .iter()
                .find(|c| c.key == k)
                .unwrap_or_else(|| panic!("key {k} was never examined"))
                .score()
        };

        assert_eq!(score("attendees"), 1.0);
        assert_eq!(score("org"), 1.0);
        assert_eq!(score("type"), 0.0, "`type: person` must not become an edge");
        assert_eq!(score("name"), 0.0);
        assert_eq!(score("role"), 0.0);
        assert_eq!(score("date"), 0.0);
        assert_eq!(score("status"), 0.0);

        let keys: Vec<&str> = found.candidates.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec!["attendees", "org"]);
    }

    #[test]
    fn evolution_keys_are_never_derived_even_when_they_resolve() {
        // `supersedes: [person:alice]` in the fixture resolves perfectly. It
        // still must not appear: evolution already models that relationship.
        let conn = setup();
        seed_typed_corpus(&conn);

        let found = detect_relation_keys(&conn, &GraphConfig::default()).unwrap();
        for key in NEVER_DERIVED {
            assert!(
                !found.examined.iter().any(|c| &c.key == key),
                "{key} must not even be scored"
            );
        }
    }

    #[test]
    fn identity_keys_are_not_relation_candidates() {
        // `id` resolves to the document that wrote it — itself. A key cannot be
        // both what a document IS and what it points at.
        let conn = setup();
        seed_typed_corpus(&conn);

        let found = detect_relation_keys(&conn, &GraphConfig::default()).unwrap();
        assert!(!found.examined.iter().any(|c| c.key == "id"));
        assert!(!found.candidates.iter().any(|c| c.key == "id"));
    }

    #[test]
    fn a_configured_identity_key_is_excluded_too() {
        let conn = setup();
        seed_typed_corpus(&conn);
        let cfg = GraphConfig {
            identity_keys: vec!["type".to_string()],
            ..GraphConfig::default()
        };

        let found = detect_relation_keys(&conn, &cfg).unwrap();
        assert!(!found.examined.iter().any(|c| c.key == "type"));
        assert!(
            found.examined.iter().any(|c| c.key == "id"),
            "`id` is only excluded because it is configured as an identity key"
        );
    }

    #[test]
    fn a_corpus_with_no_identity_space_proposes_nothing_and_says_so() {
        // Untyped documents, plain-text frontmatter. Returning candidates here
        // would be a guess dressed as a measurement.
        let conn = setup();
        let hash = "h-plain";
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?1, '# x', 1)",
            params![hash],
        )
        .unwrap();
        let metadata = r#"{"owner":"Alice Smith","status":"draft"}"#;
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, metadata, file_modified_at, indexed_at)
             VALUES ('docs', 'a.md', ?1, ?2, 1, 1)",
            params![hash, metadata],
        )
        .unwrap();

        let found = detect_relation_keys(&conn, &GraphConfig::default()).unwrap();
        assert!(found.candidates.is_empty());
        assert!(
            found.has_no_identity_space(),
            "the caller must be able to say WHY nothing was found"
        );
        assert_eq!(found.documents, 1);
    }

    #[test]
    fn an_empty_result_with_an_identity_space_is_a_real_finding() {
        // The other empty case: things CAN resolve, and these keys still do
        // not. That is a measurement, not a missing precondition.
        let conn = setup();
        doc(&conn, "people/alice.md", r#"{"id":"person:alice","status":"draft"}"#);

        let found = detect_relation_keys(&conn, &GraphConfig::default()).unwrap();
        assert!(found.candidates.is_empty());
        assert!(!found.has_no_identity_space());
        assert_eq!(found.identities, 1);
    }

    #[test]
    fn a_partly_dangling_key_is_still_a_relation() {
        // Real corpora point at documents that are not indexed yet. A key must
        // not stop being a relation because one of its targets is missing.
        let conn = setup();
        doc(&conn, "people/alice.md", r#"{"id":"person:alice"}"#);
        doc(
            &conn,
            "m.md",
            r#"{"attendees":["person:alice","person:ghost"]}"#,
        );

        let found = detect_relation_keys(&conn, &GraphConfig::default()).unwrap();
        let attendees = found
            .examined
            .iter()
            .find(|c| c.key == "attendees")
            .expect("scored");
        assert_eq!((attendees.hits, attendees.total), (1, 2));
        assert_eq!(attendees.score(), 0.5);
        assert!(
            attendees.is_relation(),
            "at the threshold, not below it: half its targets are real"
        );
    }

    #[test]
    fn a_path_valued_key_resolves_without_any_identity() {
        // Detection is identity-aware, not identity-only: a key whose values
        // are relative paths was always resolvable and must still score.
        let conn = setup();
        doc(&conn, "b.md", "{}");
        doc(&conn, "a.md", r#"{"related":["b.md"]}"#);

        let found = detect_relation_keys(&conn, &GraphConfig::default()).unwrap();
        assert_eq!(
            found
                .examined
                .iter()
                .find(|c| c.key == "related")
                .unwrap()
                .score(),
            1.0
        );
    }
}
