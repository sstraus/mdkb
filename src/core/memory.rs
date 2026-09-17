//! Memory entry lifecycle: create, read, retire, relate, prune.
//!
//! The knowledge store's own operations, independent of how they were asked
//! for. `mdkb memory add`, the MCP `memory_write` tool and the daemon hook path
//! are three ways of invoking the same thing, so it belongs to none of them.

#[cfg(feature = "llm")]
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::core::Context;
use crate::core::indexing::with_transaction;
use crate::core::memory_sync::{
    archive_then_remove, generate_memory_index, project_after_write, project_entry,
};
use crate::error::{Error, ErrorKind, Result};
use crate::store::memory::{self, EntryStatus, EntryType, MemoryEntry};
use crate::store::memory_graph::{self, MemoryRelation, TargetKind};
use serde::Deserialize;

/// One typed edge written atomically with a memory entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WriteRelation {
    pub relation: String,
    pub target: String,
    pub target_kind: String,
}

impl std::str::FromStr for WriteRelation {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let mut parts = value.splitn(3, ':');
        let relation = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        let target_kind = parts.next().unwrap_or("memory");
        if relation.is_empty() || target.is_empty() {
            return Err(format!(
                "invalid relation '{value}': expected RELATION:TARGET[:memory|doc]"
            ));
        }
        Ok(Self {
            relation: relation.to_string(),
            target: target.to_string(),
            target_kind: target_kind.to_string(),
        })
    }
}

/// Everything the shared memory-write operation can persist.
#[derive(Debug)]
pub struct WriteMemoryInput<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub content: &'a str,
    pub entry_type: &'a str,
    pub source_type: Option<&'a str>,
    pub tags: &'a [String],
    pub ttl: Option<u64>,
    pub due_in: Option<u64>,
    pub embedding: Option<&'a [f32]>,
    pub source_path: Option<&'a str>,
    pub relates: &'a [WriteRelation],
    pub session: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub on_conflict: Option<&'a str>,
    pub dry_run: bool,
}

/// Create or update one memory row, including revisions, edges, provenance and
/// its precomputed embedding. Projection is deliberately the caller's next
/// step because it needs the complete [`Context`], not only the transaction.
pub fn write_memory(conn: &rusqlite::Connection, input: WriteMemoryInput<'_>) -> Result<String> {
    memory::validate_entry_input(input.id, input.title, input.tags, input.content)?;

    let entry_type: EntryType = input
        .entry_type
        .parse()
        .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))?;
    let source_type = input
        .source_type
        .map(str::parse::<memory::SourceType>)
        .transpose()
        .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))?;
    if input
        .on_conflict
        .is_some_and(|value| value != "contradicts")
    {
        return Err(ErrorKind::InvalidQuery(
            "on_conflict must be 'contradicts' when provided".to_string(),
        )
        .into());
    }

    if input.relates.len() > 10 {
        return Err(ErrorKind::InvalidQuery("max 10 relations per entry".to_string()).into());
    }
    let parsed_relates = input
        .relates
        .iter()
        .map(|edge| {
            let relation = edge
                .relation
                .parse::<MemoryRelation>()
                .map_err(|e| Error::from(ErrorKind::InvalidQuery(e)))?;
            let target_kind = edge
                .target_kind
                .parse::<TargetKind>()
                .map_err(|e| Error::from(ErrorKind::InvalidQuery(e)))?;
            Ok((edge.target.as_str(), target_kind, relation))
        })
        .collect::<Result<Vec<_>>>()?;

    if entry_type == EntryType::Prior && memory::is_mechanical_prior_noise(input.content) {
        return Err(ErrorKind::InvalidQuery(
            "Rejected mechanical tool-chain prior (no reusable lesson). Priors must carry a distilled, trigger-scoped lesson.".to_string(),
        )
        .into());
    }

    let existing = memory::get_entry_without_tracking(conn, input.id)?;
    if input.dry_run {
        let action = if existing.is_some() {
            "update"
        } else {
            "create"
        };
        return Ok(format!(
            "dry-run: would {action} memory entry '{}'",
            input.id
        ));
    }

    let now = chrono::Utc::now().timestamp();
    let expires_at = match (input.ttl, entry_type) {
        (Some(seconds), _) => Some(now + seconds as i64),
        (None, EntryType::Prior) => Some(now + memory::PRIOR_TTL_SECS),
        (None, _) => None,
    };
    let due_at = input.due_in.map(|seconds| now + seconds as i64);
    let is_new = existing.is_none();

    let mut contradicts_target = None;
    if is_new {
        if let Some(embedding) = input.embedding {
            for (rowid, distance) in
                crate::store::vectors::memory_vector_search(conn, embedding, 3, None)?
            {
                if distance >= 0.32 {
                    continue;
                }
                let Some(duplicate) = memory::get_entry_by_rowid(conn, rowid)? else {
                    continue;
                };
                if input.on_conflict == Some("contradicts") {
                    contradicts_target = Some(duplicate.id);
                    break;
                }
                let similarity = 1.0 - (f64::from(distance) * f64::from(distance) / 2.0);
                return Err(ErrorKind::InvalidQuery(format!(
                    "Near-duplicate entry exists: \"{}\" (id: {}, similarity: {:.0}%). Update that entry instead, or use a more distinct title/content.",
                    duplicate.title,
                    duplicate.id,
                    similarity * 100.0
                ))
                .into());
            }
        }
    }

    let tx = conn.unchecked_transaction()?;
    let mut output = if let Some(mut entry) = existing {
        if let Err(error) = memory::save_revision(
            &tx,
            input.id,
            &entry.content,
            input.content,
            entry.source_type,
        ) {
            if error.is_index_corrupt() {
                return Err(error);
            }
            tracing::warn!("Failed to save revision for {}: {error}", input.id);
        }
        let previous = entry.clone();
        entry.title = input.title.to_string();
        entry.content = input.content.to_string();
        entry.entry_type = entry_type;
        entry.tags = input.tags.to_vec();
        entry.expires_at = expires_at;
        if input.due_in.is_some() {
            entry.due_at = due_at;
        }
        if let Some(source_type) = source_type {
            entry.source_type = source_type;
        }
        if crate::store::memory_file::projection_differs(&previous, &entry) {
            entry.updated_at = now;
        }
        memory::update_entry(&tx, &entry)?;
        let revisions = memory::get_revision_summary(&tx, input.id)?;
        let suffix = if revisions.count > 0 {
            format!(" ({} revisions)", revisions.count)
        } else {
            String::new()
        };
        format!("Updated memory entry: {}{suffix}", input.id)
    } else {
        let entry = MemoryEntry {
            id: input.id.to_string(),
            title: input.title.to_string(),
            content: input.content.to_string(),
            entry_type,
            tags: input.tags.to_vec(),
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: input.source_path.map(str::to_string),
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: source_type.unwrap_or_default(),
            expires_at,
            due_at,
        };
        memory::add_entry(&tx, &entry)?;
        format!("Created memory entry: {}", input.id)
    };

    for (target, kind, relation) in parsed_relates {
        memory_graph::add_edge_in(&tx, input.id, target, kind, relation)?;
    }
    if let Some(target) = &contradicts_target {
        memory_graph::add_edge_in(
            &tx,
            input.id,
            target,
            TargetKind::Memory,
            MemoryRelation::Contradicts,
        )?;
        output.push_str(&format!(
            " — conflict with '{target}' recorded as a contradicts edge (resolve via memory_confirm)"
        ));
    }
    memory::set_provenance(&tx, input.id, input.session, input.agent)?;
    tx.commit()?;

    if let Some(embedding) = input.embedding {
        if let Some(rowid) = memory::get_rowid(conn, input.id)? {
            if let Err(error) = crate::store::vectors::store_memory_embedding(
                conn,
                rowid,
                embedding,
                crate::llm::embeddings::MODEL_NAME,
            ) {
                if error.is_index_corrupt() {
                    return Err(error);
                }
                tracing::warn!(
                    "Failed to store memory embedding for '{}': {error}",
                    input.id
                );
            }
            if is_new {
                output.push_str(&memory::find_similar_entries(
                    conn, embedding, rowid, input.id,
                )?);
            }
        }
    }

    Ok(output)
}

/// Handle `mdkb memory add` command.
#[allow(clippy::too_many_arguments)]
pub fn handle_memory_add(
    ctx: &Context,
    id: &str,
    title: &str,
    entry_type: &str,
    tags: Option<&str>,
    content: &str,
    source_path: Option<&str>,
    ttl: Option<u64>,
    due_in: Option<u64>,
    source_type: Option<&str>,
    relates: &[WriteRelation],
    agent: Option<&str>,
    on_conflict: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let tags: Vec<String> = tags
        .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();
    let embedding = if dry_run
        || !crate::config::Config::load_or_default(&ctx.config_path)
            .search
            .auto_embed_memory
    {
        None
    } else {
        crate::llm::get_cached_service()
            .ok()
            .and_then(|service| service.embed_query(&format!("{title} {content}")).ok())
    };

    write_memory(
        &ctx.conn,
        WriteMemoryInput {
            id,
            title,
            content,
            entry_type,
            source_type,
            tags: &tags,
            ttl,
            due_in,
            embedding: embedding.as_deref(),
            source_path,
            relates,
            session: None,
            agent,
            on_conflict,
            dry_run,
        },
    )?;

    if dry_run {
        return Ok(());
    }

    let now = chrono::Utc::now().timestamp();

    // Save to disk and regenerate index. Recording the projection here — not
    // leaving it to the next sync — is what keeps the write and the hash that
    // describes it in step; a file written without its hash reads back as an
    // unexplained local edit.
    project_after_write(ctx, id, now);

    Ok(())
}

/// Handle `mdkb memory show` command.
pub fn handle_memory_show(ctx: &Context, id: &str) -> Result<Option<MemoryEntry>> {
    memory::get_entry_without_tracking(&ctx.conn, id)
}
/// Handle `mdkb memory confirm <id> --outcome confirmed|refuted`.
///
/// Runs fully in-process against the local DB — no daemon required — so the
/// confirm loop is reachable on every transport (this is what the UPS recall
/// nudge points at). Confirmations live in the DB (source of truth for the
/// confidence signal); the markdown projection is refreshed on the next write.
pub fn handle_memory_confirm(ctx: &Context, id: &str, outcome: &str) -> Result<ConfirmResult> {
    let delta = memory::outcome_to_delta(outcome)?;
    let message = memory::confirm_entry(&ctx.conn, id, delta)?;
    // A promoted prior has two representations: the memory entry a person reads
    // and the cluster the injector scores. `confirm_entry` moves only the first,
    // and `cluster_injection_score` reads only the second — so before this, a
    // human confirming a prior changed nothing about whether it kept firing.
    if let Err(error) = crate::store::priors::apply_belief_from_memory(&ctx.conn, id, delta) {
        tracing::warn!("confirm {id}: could not update the owning prior cluster: {error}");
    }
    // Re-read the persisted counts so JSON callers get the exact new values.
    let counters = memory::get_entry_without_tracking(&ctx.conn, id)?
        .map(|e| (e.confirmations, e.corrections))
        .unwrap_or((0, 0));
    Ok(ConfirmResult {
        id: id.to_string(),
        outcome: outcome.to_string(),
        confirmations: counters.0,
        corrections: counters.1,
        message,
    })
}
/// Handle `mdkb memory link` command: add a typed graph edge from an existing
/// memory entry to another memory slug or a document path.
pub fn handle_memory_link(
    ctx: &Context,
    id: &str,
    relation: &str,
    target: &str,
    doc: bool,
    agent: Option<&str>,
) -> Result<()> {
    use std::str::FromStr;

    let rel = MemoryRelation::from_str(relation)
        .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))?;

    // An existence check is not a read of the entry: `memory link` must not
    // move the access counters that rank memory search.
    if memory::get_entry_without_tracking(&ctx.conn, id)?.is_none() {
        return Err(ErrorKind::InvalidQuery(format!("Memory entry not found: {id}")).into());
    }

    let kind = if doc {
        TargetKind::Doc
    } else {
        TargetKind::Memory
    };
    memory_graph::add_edge(&ctx.conn, id, target, kind, rel)?;

    if let Some(agent) = agent {
        memory::set_provenance(&ctx.conn, id, None, Some(agent))?;
    }

    Ok(())
}
/// Handle `mdkb memory list` command.
pub fn handle_memory_list(
    ctx: &Context,
    limit: usize,
    status: Option<&str>,
) -> Result<Vec<MemoryEntry>> {
    let status_filter = status
        .map(|s| {
            s.parse::<EntryStatus>()
                .map_err(|e: String| Error::from(ErrorKind::InvalidQuery(e)))
        })
        .transpose()?;

    memory::list_entries(&ctx.conn, limit, status_filter)
}
/// Handle `mdkb memory search` command.
/// The same engine the MCP `search` tool and the `UserPromptSubmit` hook use:
/// OR-expanded FTS plus the vector leg, fused, then admitted by the absolute
/// relevance floor. It used to be plain token-AND FTS, which meant a
/// paraphrase the hook recalled perfectly returned nothing on the CLI.
///
/// The embedding is best-effort, exactly as `handle_hybrid_search` treats it:
/// no model means no vector leg and no semantic admission arm, so recall
/// degrades to strong lexical matches rather than to an error.
///
/// `entry_type` narrows the corpus both legs draw from. It does not switch
/// engines: the typed and untyped answers agree on every entry of that type.
pub fn handle_memory_search(
    ctx: &Context,
    query: &str,
    limit: usize,
    entry_type: Option<&str>,
) -> Result<Vec<MemoryEntry>> {
    let query_embedding = match crate::llm::get_cached_service().and_then(|s| s.embed_query(query))
    {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::debug!("Memory search falling back to BM25-only: {e}");
            None
        }
    };
    let cfg = crate::config::Config::load_or_default(&ctx.config_path);
    let scored = memory::search_entries_recall(
        &ctx.conn,
        query,
        query_embedding.as_deref(),
        limit,
        entry_type,
        &cfg.search.memory,
    )?;
    Ok(scored.into_iter().map(|result| result.entry).collect())
}
/// Handle `mdkb memory warmup` command.
pub fn handle_memory_warmup(ctx: &Context, limit: usize) -> Result<Vec<String>> {
    memory::get_warmup_index(&ctx.conn, limit)
}
/// Handle `mdkb memory rm` command.
///
/// Existence is checked without tracking first, so that a `rm` of an unknown
/// id stays a no-op instead of retiring a stray file that has no row.
pub fn handle_memory_rm(ctx: &Context, id: &str) -> Result<bool> {
    if memory::get_entry_without_tracking(&ctx.conn, id)?.is_none() {
        return Ok(false);
    }
    archive_then_remove(ctx, std::slice::from_ref(&id.to_string()), || {
        memory::delete_entry(&ctx.conn, id)
    })
}
/// Handle `mdkb memory prune` command.
/// Archives expired entries and lifecycle entries unread for `days`; durable
/// types are never archived for age (see `memory::prunable_entry_ids`).
///
/// The selection and the write are separate store calls because the disk
/// archive has to run between them — see [`archive_then_remove`]. The ids are
/// read once, so no entry can cross the expiry boundary between the two.
pub fn handle_memory_prune(ctx: &Context, days: u32, dry_run: bool) -> Result<Vec<String>> {
    let pruned = memory::prunable_entry_ids(&ctx.conn, days)?;
    if dry_run || pruned.is_empty() {
        return Ok(pruned);
    }
    archive_then_remove(ctx, &pruned, || memory::archive_entries(&ctx.conn, &pruned))?;
    Ok(pruned)
}
/// Export all memory entries to `<dir>/<id>.md` files with YAML frontmatter.
pub fn handle_memory_export(
    ctx: &Context,
    dir: &Path,
    include_expired: bool,
    overwrite: bool,
    dry_run: bool,
) -> Result<ExportResult> {
    let now = chrono::Utc::now().timestamp();
    let entries = memory::list_entries_all(&ctx.conn)?;

    let mut result = ExportResult {
        exported: 0,
        skipped: 0,
        errors: Vec::new(),
    };

    if !dry_run {
        std::fs::create_dir_all(dir).map_err(|e| ErrorKind::Io {
            path: dir.to_path_buf(),
            operation: format!("create_dir_all: {e}"),
        })?;
    }

    for entry in &entries {
        let is_expired = entry.expires_at.map(|t| t <= now).unwrap_or(false);
        if is_expired && !include_expired {
            result.skipped += 1;
            continue;
        }

        if let Err(msg) = sanitize_export_id(&entry.id) {
            result.errors.push(format!("{}: {msg}", entry.id));
            continue;
        }

        let dest = dir.join(format!("{}.md", entry.id));

        if !overwrite && dest.exists() {
            result.skipped += 1;
            continue;
        }

        if dry_run {
            result.exported += 1;
            continue;
        }

        let text = crate::store::memory_file::to_markdown(entry);
        match std::fs::write(&dest, text) {
            Ok(()) => result.exported += 1,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: write failed: {e}", dest.display()));
            }
        }
    }

    Ok(result)
}
/// Import memory entries from a directory of `.md` files with YAML frontmatter.
pub fn handle_memory_import_dir(
    ctx: &Context,
    dir: &Path,
    dry_run: bool,
    skip_duplicates: bool,
) -> Result<ImportResult> {
    let mut result = ImportResult {
        imported: 0,
        skipped: 0,
        errors: Vec::new(),
    };

    let read_dir = std::fs::read_dir(dir).map_err(|e| ErrorKind::Io {
        path: dir.to_path_buf(),
        operation: format!("read_dir: {e}"),
    })?;

    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry_res in read_dir {
        match entry_res {
            Ok(entry) => {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    paths.push(path);
                }
            }
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: read_dir entry error: {e}", dir.display()));
            }
        }
    }
    paths.sort();

    // Phase 1: read, parse, validate, and check duplicates — no DB writes.
    let mut entries_to_insert: Vec<MemoryEntry> = Vec::new();
    for path in &paths {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: read error: {e}", path.display()));
                continue;
            }
        };

        let mf = match crate::store::memory_file::from_markdown(&text) {
            Ok(mf) => mf,
            Err(e) => {
                result
                    .errors
                    .push(format!("{}: parse error: {e}", path.display()));
                continue;
            }
        };

        let id = mf.meta.id.clone();

        if let Err(e) =
            memory::validate_entry_input(&mf.meta.id, &mf.meta.title, &mf.meta.tags, &mf.content)
        {
            result.errors.push(format!("{id}: {e}"));
            continue;
        }

        if memory::get_entry_without_tracking(&ctx.conn, &id)?.is_some() {
            if skip_duplicates {
                result.skipped += 1;
            } else {
                result.errors.push(format!(
                    "{id}: already exists (use --skip-duplicates to ignore)"
                ));
            }
            continue;
        }

        if dry_run {
            result.imported += 1;
            continue;
        }

        // Derived counters (access_count etc.) are DB-owned; fresh entry starts at 0.
        entries_to_insert.push(mf.into_fresh_entry());
    }

    // Phase 2: insert all valid entries atomically.
    if !entries_to_insert.is_empty() {
        let now = chrono::Utc::now().timestamp();
        with_transaction(&ctx.conn, || {
            for entry in &entries_to_insert {
                memory::add_entry(&ctx.conn, entry)?;
            }
            Ok(())
        })?;

        // Gated by `[search] auto_embed_memory` (default on); loaded once, not per entry.
        let embed = crate::config::Config::load_or_default(&ctx.config_path)
            .search
            .auto_embed_memory;
        for entry in &entries_to_insert {
            if let Err(e) = project_entry(ctx, entry, now) {
                tracing::warn!("Failed to save imported entry {} to disk: {e}", entry.id);
            }
            // Embed so imported entries are vector-searchable; a cold model
            // leaves them pending for `mdkb update` backfill (never fatal).
            if embed {
                if let Err(e) =
                    memory::embed_entry(&ctx.conn, &entry.id, &entry.title, &entry.content)
                {
                    tracing::warn!("Failed to embed imported entry {}: {e}", entry.id);
                }
            }
            result.imported += 1;
        }
    }

    if !dry_run && result.imported > 0 {
        if let Err(e) = generate_memory_index(ctx) {
            tracing::warn!("Failed to regenerate memory index: {e}");
        }
    }

    Ok(result)
}
/// Import ONE entry markdown file, preserving everything the file records.
///
/// The supported answer to "I have an entry file and need it back in the
/// database". `mdkb memory add` cannot do it: it stamps `created_at` and
/// `updated_at` with now(), so restoring a corpus flattens months of history
/// into one day and destroys recency ranking. The only alternative was a raw
/// `sqlite3 INSERT` against index.sqlite — which skips the connection pragmas
/// this store depends on (`busy_timeout`, WAL, `synchronous = NORMAL`) and the
/// `.mutation.lock` protocol. Doing exactly that against a live store corrupted
/// `memory_fts_data` (`Rowid out of order`, `2nd reference to page 12862`), and
/// recovery needed a pre-write file copy (story 017-a378).
///
/// So this runs on the caller's `Context` connection, which is the whole point:
/// the pragmas, the lock, and the FTS/embedding triggers all apply.
///
/// Telemetry is preserved rather than reset — see
/// [`MemoryFile::into_restored_entry`] for why a restore differs from a sync.
/// An existing id is an explicit conflict, never a silent overwrite: a restore
/// that clobbers a live entry is worse than one that refuses.
pub fn handle_memory_import_file(ctx: &Context, path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: path.to_path_buf(),
            operation: format!("read: {e}"),
        })
    })?;
    let file = crate::store::memory_file::from_markdown(&text)?;

    // The filename and the frontmatter must agree. When they do not, either
    // could be the intended id, and picking one silently writes the wrong row.
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if file.meta.id != stem {
        return Err(ErrorKind::InvalidQuery(format!(
            "{}: frontmatter declares id `{}` but the filename says `{stem}` — refusing to \
             guess which is authoritative. Rename the file to `{}.md`, or fix the frontmatter.",
            path.display(),
            file.meta.id,
            file.meta.id
        ))
        .into());
    }

    memory::validate_entry_input(
        &file.meta.id,
        &file.meta.title,
        &file.meta.tags,
        &file.content,
    )?;

    let id = file.meta.id.clone();
    if memory::get_entry_without_tracking(&ctx.conn, &id)?.is_some() {
        return Err(ErrorKind::InvalidQuery(format!(
            "memory entry `{id}` already exists — import refuses to overwrite. Remove it first \
             (`mdkb memory rm {id}`) if the file is the version you want."
        ))
        .into());
    }

    let entry = file.into_restored_entry();
    let now = chrono::Utc::now().timestamp();
    memory::add_entry(&ctx.conn, &entry)?;
    // Re-project so the recorded hash describes canonical bytes; otherwise the
    // next reconciliation reads a pre-v19 file back as a local edit.
    project_entry(ctx, &entry, now)?;

    // Embed here when the model is warm; a cold model leaves the entry pending
    // and `mdkb update`'s backfill picks it up with no manual step.
    if crate::config::Config::load_or_default(&ctx.config_path)
        .search
        .auto_embed_memory
        && let Err(e) = memory::embed_entry(&ctx.conn, &id, &entry.title, &entry.content)
    {
        tracing::warn!("imported entry {id}: embedding deferred to `mdkb update`: {e}");
    }

    if let Err(e) = generate_memory_index(ctx) {
        tracing::warn!("Failed to regenerate memory index: {e}");
    }
    Ok(())
}
/// Handle `mdkb memory import` command.
pub fn handle_memory_import(
    ctx: &Context,
    path: &str,
    dry_run: bool,
    skip_duplicates: bool,
) -> Result<ImportResult> {
    let file_content = std::fs::read_to_string(path).map_err(|e| {
        Error::from(ErrorKind::Io {
            path: std::path::PathBuf::from(path),
            operation: format!("read: {e}"),
        })
    })?;

    let import_file: ImportFile = serde_json::from_str(&file_content).map_err(|e| {
        Error::from(ErrorKind::InvalidQuery(format!(
            "Failed to parse {path}: {e}"
        )))
    })?;

    let mut result = ImportResult {
        imported: 0,
        skipped: 0,
        errors: Vec::new(),
    };
    let now = chrono::Utc::now().timestamp();

    // Phase 1: parse, validate, and check duplicates — no DB writes.
    let mut entries_to_insert: Vec<MemoryEntry> = Vec::new();
    for raw in &import_file.entries {
        // Parse entry_type
        let entry_type: EntryType = match raw.entry_type.parse() {
            Ok(t) => t,
            Err(e) => {
                result.errors.push(format!("{}: {e}", raw.id));
                continue;
            }
        };

        // Parse source_type
        let source_type: memory::SourceType = match raw.source_type.parse() {
            Ok(t) => t,
            Err(e) => {
                result.errors.push(format!("{}: {e}", raw.id));
                continue;
            }
        };

        // Validate fields
        if let Err(e) = memory::validate_entry_input(&raw.id, &raw.title, &raw.tags, &raw.content) {
            result.errors.push(format!("{}: {e}", raw.id));
            continue;
        }

        // Check for duplicates
        if memory::get_entry_without_tracking(&ctx.conn, &raw.id)?.is_some() {
            if skip_duplicates {
                result.skipped += 1;
                continue;
            }
            result.errors.push(format!(
                "{}: already exists (use --skip-duplicates to ignore)",
                raw.id
            ));
            continue;
        }

        if dry_run {
            result.imported += 1;
            continue;
        }

        entries_to_insert.push(MemoryEntry {
            id: raw.id.clone(),
            title: raw.title.clone(),
            content: raw.content.clone(),
            entry_type,
            tags: raw.tags.clone(),
            status: EntryStatus::Active,
            created_at: raw.created_at.unwrap_or(now),
            updated_at: raw.updated_at.unwrap_or(now),
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type,
            expires_at: None,
            due_at: None,
        });
    }

    // Phase 2: insert all valid entries atomically.
    if !entries_to_insert.is_empty() {
        with_transaction(&ctx.conn, || {
            for entry in &entries_to_insert {
                memory::add_entry(&ctx.conn, entry)?;
            }
            Ok(())
        })?;

        // Gated by `[search] auto_embed_memory` (default on); loaded once, not per entry.
        let embed = crate::config::Config::load_or_default(&ctx.config_path)
            .search
            .auto_embed_memory;
        for entry in &entries_to_insert {
            if let Err(e) = project_entry(ctx, entry, now) {
                tracing::warn!("Failed to save imported entry {} to disk: {e}", entry.id);
            }
            // Embed so imported entries are vector-searchable; a cold model
            // leaves them pending for `mdkb update` backfill (never fatal).
            if embed {
                if let Err(e) =
                    memory::embed_entry(&ctx.conn, &entry.id, &entry.title, &entry.content)
                {
                    tracing::warn!("Failed to embed imported entry {}: {e}", entry.id);
                }
            }
            result.imported += 1;
        }
    }

    if !dry_run && result.imported > 0 {
        if let Err(e) = generate_memory_index(ctx) {
            tracing::warn!("Failed to regenerate memory index: {e}");
        }
    }

    Ok(result)
}
/// Handle `mdkb memory condense` command.
#[cfg(feature = "llm")]
pub fn handle_memory_condense(
    ctx: &Context,
    tag_filter: Option<&str>,
    dry_run: bool,
    min_entries: usize,
) -> Result<CondenseResult> {
    let mut result = CondenseResult {
        groups: Vec::new(),
        consolidated_count: 0,
        merged_count: 0,
    };

    // Find groups of related entries
    let groups = find_related_entries(ctx, tag_filter, min_entries)?;

    if groups.is_empty() {
        return Ok(result);
    }

    for mut group in groups {
        // Get full entries for this group
        let entries: Vec<memory::MemoryEntry> = group
            .entry_ids
            .iter()
            .filter_map(|id| {
                memory::get_entry_without_tracking(&ctx.conn, id)
                    .ok()
                    .flatten()
            })
            .collect();

        if entries.len() < min_entries {
            continue;
        }

        // Generate consolidated content
        let (title, content) = generate_consolidated_content(&entries)?;
        group.proposed_title = Some(title.clone());
        group.proposed_content = Some(content.clone());

        if !dry_run {
            let now = chrono::Utc::now().timestamp();

            // Create the merged entry
            let merged_entry = memory::MemoryEntry {
                id: group.proposed_id.clone(),
                title,
                content,
                entry_type: entries
                    .first()
                    .map(|e| e.entry_type)
                    .unwrap_or(memory::EntryType::Topic),
                tags: group.common_tags.clone(),
                status: memory::EntryStatus::Active,
                created_at: now,
                updated_at: now,
                superseded_by: None,
                access_count: 0,
                last_accessed: None,
                source_path: None,
                confirmations: 0,
                corrections: 0,
                last_confirmed_at: None,
                last_refuted_at: None,
                source_type: memory::SourceType::AutoExtracted,
                expires_at: None,
                due_at: None,
            };

            // Use transaction to ensure atomicity - either all changes succeed or none
            with_transaction(&ctx.conn, || {
                memory::add_entry(&ctx.conn, &merged_entry)?;

                // Mark original entries as superseded
                for entry in &entries {
                    let mut updated = entry.clone();
                    updated.status = memory::EntryStatus::Superseded;
                    updated.superseded_by = Some(group.proposed_id.clone());
                    memory::update_entry(&ctx.conn, &updated)?;
                }
                Ok(())
            })?;
            result.merged_count += 1;
            result.consolidated_count += entries.len();
        }

        result.groups.push(group);
    }

    // Regenerate index if changes were made
    if !dry_run && result.merged_count > 0 {
        if let Err(e) = generate_memory_index(ctx) {
            tracing::warn!("Failed to regenerate memory index: {e}");
        }
    }

    Ok(result)
}
/// Outcome of a `mdkb memory confirm` command.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ConfirmResult {
    pub id: String,
    pub outcome: String,
    /// Confirmation count after applying the signal.
    pub confirmations: u32,
    /// Refutation count after applying the signal. A refuted outcome raises
    /// this and leaves `confirmations` alone, so a caller that reads only the
    /// confirmation count would see a refutation as a no-op.
    pub corrections: u32,
    pub message: String,
}
/// Result of a memory export operation.
#[derive(Debug)]
pub struct ExportResult {
    pub exported: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}
/// Result of a memory import operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImportResult {
    pub imported: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}
/// JSON format for memory import (matches wiz fallback format).
#[derive(Debug, Deserialize)]
struct ImportFile {
    entries: Vec<ImportEntry>,
}
/// A single entry in the import JSON file.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportEntry {
    id: String,
    title: String,
    content: String,
    #[serde(default = "default_import_entry_type")]
    entry_type: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_import_source_type")]
    source_type: String,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
}
/// Reject entry IDs that would escape the export directory or hide dotfiles.
///
/// Accepts the same character class enforced by [`memory::validate_entry_input`]
/// (alphanumerics, `-`, `_`, `.`, `/`), then adds filename-level guards:
/// no empty string, no path separators, no `..`, no leading `.`.
fn sanitize_export_id(id: &str) -> std::result::Result<(), String> {
    if id.is_empty() {
        return Err("empty id".to_string());
    }
    if id.contains('/') || id.contains('\\') {
        return Err(format!("path separator in id: {id:?}"));
    }
    if id == "." || id == ".." || id.starts_with('.') {
        return Err(format!("dot-prefixed id: {id:?}"));
    }
    Ok(())
}
fn default_import_source_type() -> String {
    "user_statement".to_string()
}
fn default_import_entry_type() -> String {
    "topic".to_string()
}
/// Result of a condense operation.
#[cfg(feature = "llm")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CondenseResult {
    /// Groups of related entries found.
    pub groups: Vec<CondenseGroup>,
    /// Number of entries consolidated.
    pub consolidated_count: usize,
    /// Number of new merged entries created.
    pub merged_count: usize,
}
/// A group of related entries that can be condensed.
#[cfg(feature = "llm")]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CondenseGroup {
    /// IDs of entries in this group.
    pub entry_ids: Vec<String>,
    /// Common tags shared by entries.
    pub common_tags: Vec<String>,
    /// Proposed merged ID (generated from tags).
    pub proposed_id: String,
    /// Proposed title (generated by LLM or from entries).
    pub proposed_title: Option<String>,
    /// Proposed content (generated by LLM).
    pub proposed_content: Option<String>,
}
/// Find groups of related memory entries based on overlapping tags.
#[cfg(feature = "llm")]
pub fn find_related_entries(
    ctx: &Context,
    tag_filter: Option<&str>,
    min_entries: usize,
) -> Result<Vec<CondenseGroup>> {
    // Get all active entries
    let entries = memory::list_entries(&ctx.conn, 1000, Some(memory::EntryStatus::Active))?;

    // Build tag -> entries index
    let mut tag_index: HashMap<String, Vec<&memory::MemoryEntry>> = HashMap::new();
    for entry in &entries {
        for tag in &entry.tags {
            // If filtering by tag, only include matching entries
            if let Some(filter) = tag_filter {
                if tag != filter {
                    continue;
                }
            }
            tag_index.entry(tag.clone()).or_default().push(entry);
        }
    }

    // Find groups with overlapping tags (use the largest tag groups first)
    let mut groups: Vec<CondenseGroup> = Vec::new();
    let mut processed_ids: HashSet<String> = HashSet::new();

    // Sort tags by entry count (descending)
    let mut sorted_tags: Vec<_> = tag_index.iter().collect();
    sorted_tags.sort_by_key(|entry| std::cmp::Reverse(entry.1.len()));

    for (tag, tag_entries) in sorted_tags {
        // Skip if not enough entries
        if tag_entries.len() < min_entries {
            continue;
        }

        // Filter out already-processed entries
        let available: Vec<_> = tag_entries
            .iter()
            .filter(|e| !processed_ids.contains(&e.id))
            .copied()
            .collect();

        if available.len() < min_entries {
            continue;
        }

        // Find common tags among these entries
        let common_tags = find_common_tags(&available);

        // Generate proposed ID from common tags
        let proposed_id = if common_tags.len() > 1 {
            format!("{}-consolidated", common_tags.join("-"))
        } else {
            format!("{}-consolidated", tag)
        };

        let entry_ids: Vec<String> = available.iter().map(|e| e.id.clone()).collect();

        // Mark these entries as processed
        for id in &entry_ids {
            processed_ids.insert(id.clone());
        }

        groups.push(CondenseGroup {
            entry_ids,
            common_tags,
            proposed_id,
            proposed_title: None,
            proposed_content: None,
        });
    }

    Ok(groups)
}
/// Find common tags among a set of entries.
#[cfg(feature = "llm")]
fn find_common_tags(entries: &[&memory::MemoryEntry]) -> Vec<String> {
    if entries.is_empty() {
        return Vec::new();
    }

    let first_tags: HashSet<_> = entries[0].tags.iter().cloned().collect();
    let mut common: HashSet<_> = first_tags;

    for entry in entries.iter().skip(1) {
        let entry_tags: HashSet<_> = entry.tags.iter().cloned().collect();
        common = common.intersection(&entry_tags).cloned().collect();
    }

    let mut result: Vec<_> = common.into_iter().collect();
    result.sort();
    result
}
/// Generate consolidated content for memory entries.
/// Currently uses heuristic-based concatenation.
/// TODO: When llama-cpp-rs is integrated, use LLM for smarter consolidation.
#[cfg(feature = "llm")]
pub fn generate_consolidated_content(entries: &[memory::MemoryEntry]) -> Result<(String, String)> {
    // Generate title from common elements
    let title = if entries.len() == 1 {
        entries[0].title.clone()
    } else {
        // Find common words in titles
        let first_words: HashSet<_> = entries[0]
            .title
            .split_whitespace()
            .map(|s| s.to_lowercase())
            .collect();
        let common_words: Vec<_> = first_words
            .iter()
            .filter(|w| {
                entries[1..]
                    .iter()
                    .all(|e| e.title.to_lowercase().contains(w.as_str()))
            })
            .take(3)
            .cloned()
            .collect();

        if common_words.is_empty() {
            format!("{} (consolidated)", entries[0].title)
        } else {
            format!("{} - Complete Guide", common_words.join(" ").to_uppercase())
        }
    };

    // Generate content (simple concatenation for now)
    let mut content = String::new();
    content.push_str(&format!("# {}\n\n", title));
    content.push_str("*This entry consolidates multiple related entries.*\n\n");

    for entry in entries {
        content.push_str(&format!("## From: {}\n\n", entry.title));
        // Skip the first line if it's a title
        let entry_content = entry
            .content
            .lines()
            .skip_while(|l| l.starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n");
        content.push_str(&entry_content);
        content.push_str("\n\n");
    }

    Ok((title, content))
}
