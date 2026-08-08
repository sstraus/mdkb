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
pub fn handle_graph_dangling(ctx: &Context) -> Result<Vec<crate::store::graph::DanglingRef>> {
    crate::store::graph::dangling(&ctx.conn)
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
