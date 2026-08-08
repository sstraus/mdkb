//! Indexing Claude Code session transcripts as documents.
//!
//! Driven by the MCP layer on session start and by the daemon's post-heal
//! rebuild, neither of which has anything to do with the command line. It reads
//! transcript files and writes documents; no argument is parsed here and no
//! output is formatted.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::core::Context;
use crate::domain::{Collection, Document, UpdateResult};
use crate::error::Result;
use crate::store::{collections, documents};

/// Index Claude Code session files into the `claude_sessions` collection.
///
/// Walks `*.jsonl` files in the session directory (non-recursive),
/// parses them into documents with chunking, and indexes the results.
/// Uses mtime-based dedup to skip unchanged files.
pub fn handle_session_index(
    ctx: &Context,
    sessions_path: &Path,
    project_root: &str,
) -> Result<UpdateResult> {
    use crate::domain::sessions::{SessionParseConfig, find_session_dir, parse_session_file};

    let Some(session_dir) = find_session_dir(sessions_path, project_root) else {
        tracing::debug!("No session directory found for {}", project_root);
        return Ok(UpdateResult::default());
    };

    // Ensure claude_sessions collection exists
    let collection_name = crate::domain::COLLECTION_CLAUDE_SESSIONS;
    if collections::get_collection(&ctx.conn, collection_name)?.is_none() {
        let now = chrono::Utc::now().timestamp();
        let coll = Collection {
            name: collection_name.to_string(),
            path: session_dir.to_string_lossy().to_string(),
            pattern: "*.jsonl".to_string(),
            source: crate::domain::COLLECTION_SOURCE_SESSIONS.to_string(),
            created_at: now,
            updated_at: now,
        };
        collections::add_collection(&ctx.conn, &coll)?;
        tracing::info!(
            "Created claude_sessions collection at {}",
            session_dir.display()
        );
    }

    let config = SessionParseConfig::default();
    let mut result = UpdateResult::default();

    // Pre-load existing documents for mtime-based dedup
    let existing_docs: HashMap<String, Document> =
        documents::list_documents(&ctx.conn, collection_name)?
            .into_iter()
            .map(|d| (d.relative_path.clone(), d))
            .collect();

    // Walk .jsonl files (non-recursive)
    let entries: Vec<_> = std::fs::read_dir(&session_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let path = e.path();
            path.is_file() && path.extension().map(|ext| ext == "jsonl").unwrap_or(false)
        })
        .collect();

    documents::begin_transaction(&ctx.conn)?;

    // Relative paths still produced by live transcript files this pass. Any
    // previously-indexed session doc NOT in this set has lost its source and is
    // archived below.
    let mut present_paths: HashSet<String> = HashSet::new();

    for entry in &entries {
        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };

        let file_mtime = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let session_docs = match parse_session_file(&path, &config) {
            Ok(docs) => docs,
            Err(e) => {
                result
                    .errors
                    .push(format!("Failed to parse {}: {}", file_name, e));
                continue;
            }
        };

        for sdoc in &session_docs {
            present_paths.insert(sdoc.relative_path.clone());
            let existing = existing_docs.get(&sdoc.relative_path);

            // Content-hash dedup (NOT file mtime). A transcript is append-only, so
            // its mtime bumps on every growth while all but the tail chunk keep
            // byte-identical content under a stable `{sid}-chunk-NNN` key. Skipping
            // on file mtime therefore re-embedded the ENTIRE multi-MB file on every
            // append; skipping on the per-chunk content hash re-embeds only the
            // new/changed tail. `documents.hash` is the same SHA-256 the document
            // layer stores, so this is exact, not heuristic.
            if let Some(existing_doc) = existing {
                if existing_doc.hash == documents::compute_hash(&sdoc.content) {
                    result.unchanged += 1;
                    continue;
                }
            }

            let now = chrono::Utc::now().timestamp();
            let doc = Document {
                id: 0,
                collection: collection_name.to_string(),
                relative_path: sdoc.relative_path.clone(),
                hash: String::new(), // computed by index_document_in_tx
                title: Some(sdoc.metadata.session_id.clone()),
                metadata: None,
                file_modified_at: file_mtime,
                indexed_at: now,
                status: None,
            };

            documents::index_document_in_tx(&ctx.conn, &doc, &sdoc.content)?;

            if existing.is_some() {
                result.updated += 1;
            } else {
                result.added += 1;
            }
        }
    }

    // Archive session docs whose source jsonl is gone (deleted/rotated). Kept
    // searchable via explicit --collection claude_sessions; never hard-deleted
    // here (see `mdkb compact --prune-sessions`).
    result.sessions_archived = documents::archive_missing_sessions(&ctx.conn, &present_paths)?;

    documents::commit_transaction(&ctx.conn)?;

    Ok(result)
}
