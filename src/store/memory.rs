//! Memory entry storage operations.

use std::collections::HashMap;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, Result};

/// Maximum ID length (slug format).
pub const MAX_ID_LEN: usize = 100;
/// Maximum title length.
pub const MAX_TITLE_LEN: usize = 200;
/// Maximum number of tags per entry.
pub const MAX_TAGS: usize = 20;
/// Maximum length of a single tag.
pub const MAX_TAG_LEN: usize = 50;
/// Maximum content size (100KB).
pub const MAX_CONTENT_SIZE: usize = 100_000;

/// Validate memory entry input fields.
///
/// Checks ID format, title length, tag count/length, and content size.
pub fn validate_entry_input(id: &str, title: &str, tags: &[String], content: &str) -> Result<()> {
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return Err(ErrorKind::InvalidQuery(format!("ID must be 1-{MAX_ID_LEN} chars")).into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(ErrorKind::InvalidQuery(
            "ID must be lowercase alphanumeric with hyphens".to_string(),
        )
        .into());
    }
    if title.is_empty() || title.len() > MAX_TITLE_LEN {
        return Err(
            ErrorKind::InvalidQuery(format!("Title must be 1-{MAX_TITLE_LEN} chars")).into(),
        );
    }
    if tags.len() > MAX_TAGS {
        return Err(ErrorKind::InvalidQuery(format!("Too many tags (max {MAX_TAGS})")).into());
    }
    for tag in tags {
        if tag.len() > MAX_TAG_LEN {
            return Err(ErrorKind::InvalidQuery(format!(
                "Tag '{}' exceeds {MAX_TAG_LEN} chars",
                &tag[..20.min(tag.len())]
            ))
            .into());
        }
    }
    if content.len() > MAX_CONTENT_SIZE {
        return Err(
            ErrorKind::InvalidQuery(format!("Content exceeds {MAX_CONTENT_SIZE} bytes")).into(),
        );
    }
    Ok(())
}

/// Source type for confidence weighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    OfficialDocs,
    #[default]
    UserStatement,
    AutoExtracted,
    Inference,
}

impl std::fmt::Display for SourceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OfficialDocs => write!(f, "official_docs"),
            Self::UserStatement => write!(f, "user_statement"),
            Self::AutoExtracted => write!(f, "auto_extracted"),
            Self::Inference => write!(f, "inference"),
        }
    }
}

impl std::str::FromStr for SourceType {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "official_docs" => Ok(Self::OfficialDocs),
            "user_statement" => Ok(Self::UserStatement),
            "auto_extracted" => Ok(Self::AutoExtracted),
            "inference" => Ok(Self::Inference),
            _ => Err(format!(
                "Invalid source_type: {s}. Valid: official_docs, user_statement, auto_extracted, inference"
            )),
        }
    }
}

/// Confidence floor — entries never drop below this.
const CONFIDENCE_FLOOR: f64 = 0.05;

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
    #[serde(default)]
    pub confirmations: u32,
    #[serde(default)]
    pub last_confirmed_at: Option<i64>,
    #[serde(default)]
    pub source_type: SourceType,
}

impl MemoryEntry {
    /// Calculate confidence score [0.05, 1.0].
    ///
    /// Combines Bayesian belief, Ebbinghaus temporal decay with access
    /// reinforcement, and source type authority.
    pub fn confidence(&self) -> f64 {
        self.confidence_at(chrono::Utc::now().timestamp())
    }

    /// Calculate confidence at a specific timestamp (for testing).
    pub fn confidence_at(&self, now: i64) -> f64 {
        // Belief: sigmoid over confirmations. 0 confirms = 0.5, 10 = 0.91, 50 = 0.98.
        let belief = (1.0 + self.confirmations as f64) / (2.0 + self.confirmations as f64);

        // Temporal decay: how fresh is the verification?
        let reference_time = self.last_confirmed_at.unwrap_or(self.created_at);
        let days = (now - reference_time) as f64 / 86400.0;
        let days = days.max(0.0); // guard against negative (clock skew)
        let strength = 1.0 + (1.0 + self.access_count as f64).ln();
        let decay = (-days / (90.0 * strength)).exp();

        // Source authority multiplier
        let source_mult = match self.source_type {
            SourceType::OfficialDocs => 1.0,
            SourceType::UserStatement => 0.85,
            SourceType::AutoExtracted => 0.70,
            SourceType::Inference => 0.65,
        };

        (belief * decay * source_mult).max(CONFIDENCE_FLOOR)
    }
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

/// Sort order for listing memory entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySortOrder {
    /// Most accessed first (access_count DESC).
    Popular,
    /// Most recently accessed first (last_accessed DESC NULLS LAST).
    Recent,
    /// Most recently created first (created_at DESC).
    Newest,
}

impl std::str::FromStr for MemorySortOrder {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "popular" => Ok(Self::Popular),
            "recent" => Ok(Self::Recent),
            "newest" => Ok(Self::Newest),
            _ => Err(format!(
                "Invalid sort order: '{s}'. Valid: popular, recent, newest."
            )),
        }
    }
}

/// List memory entries with configurable sort order.
pub fn list_entries_sorted(
    conn: &Connection,
    limit: usize,
    sort: MemorySortOrder,
    status_filter: Option<EntryStatus>,
) -> Result<Vec<MemoryEntry>> {
    let order_clause = match sort {
        MemorySortOrder::Popular => "ORDER BY access_count DESC",
        MemorySortOrder::Recent => "ORDER BY COALESCE(last_accessed, 0) DESC",
        MemorySortOrder::Newest => "ORDER BY created_at DESC",
    };

    let sql = if status_filter.is_some() {
        format!(
            "SELECT id, title, content, entry_type, tags, status, created_at, updated_at, superseded_by, access_count, last_accessed, source_path, confirmations, last_confirmed_at, source_type
             FROM memory_entries WHERE status = ?1 {order_clause} LIMIT ?2"
        )
    } else {
        format!(
            "SELECT id, title, content, entry_type, tags, status, created_at, updated_at, superseded_by, access_count, last_accessed, source_path, confirmations, last_confirmed_at, source_type
             FROM memory_entries {order_clause} LIMIT ?1"
        )
    };

    let mut stmt = conn.prepare(&sql)?;

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

/// Add a new memory entry.
pub fn add_entry(conn: &Connection, entry: &MemoryEntry) -> Result<()> {
    let tags_json = serde_json::to_string(&entry.tags)?;

    conn.execute(
        "INSERT INTO memory_entries (id, title, content, entry_type, tags, status, created_at, updated_at, access_count, last_accessed, source_path, confirmations, last_confirmed_at, source_type)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
            entry.last_accessed,
            entry.source_path,
            entry.confirmations,
            entry.last_confirmed_at,
            entry.source_type.to_string(),
        ],
    )?;

    Ok(())
}

/// Confirm a memory entry — positive confidence signal.
///
/// Increments confirmations counter, updates last_confirmed_at.
/// Auto-restores archived entries to active (strong relevance signal).
/// Returns error if entry is superseded.
pub fn confirm_entry(conn: &Connection, id: &str) -> Result<String> {
    let entry = get_entry_without_tracking(conn, id)?
        .ok_or_else(|| ErrorKind::InvalidQuery(format!("Memory entry not found: {id}")))?;

    if entry.status == EntryStatus::Superseded {
        return Err(ErrorKind::InvalidQuery(format!(
            "Cannot confirm superseded entry '{id}'. Confirm the replacement instead."
        ))
        .into());
    }

    let now = Utc::now().timestamp();
    let new_status = if entry.status == EntryStatus::Archived {
        "active".to_string()
    } else {
        entry.status.to_string()
    };

    conn.execute(
        "UPDATE memory_entries SET confirmations = confirmations + 1, last_confirmed_at = ?1, status = ?2, updated_at = ?1 WHERE id = ?3",
        params![now, new_status, id],
    )?;

    if entry.status == EntryStatus::Archived {
        Ok(format!("Confirmed and restored to active: {id}"))
    } else {
        Ok(format!(
            "Confirmed: {id} ({} confirmations)",
            entry.confirmations + 1
        ))
    }
}

/// Correct a memory entry — positive confidence signal.
///
/// Correcting = improving the entry. Always boosts confidence.
/// Optionally appends correction text. To remove bad entries, use delete.
/// Returns error if entry is superseded or archived.
pub fn correct_entry(conn: &Connection, id: &str, correction: Option<&str>) -> Result<String> {
    let entry = get_entry_without_tracking(conn, id)?
        .ok_or_else(|| ErrorKind::InvalidQuery(format!("Memory entry not found: {id}")))?;

    if entry.status == EntryStatus::Superseded {
        return Err(ErrorKind::InvalidQuery(format!(
            "Cannot correct superseded entry '{id}'. Correct the replacement instead."
        ))
        .into());
    }

    if entry.status == EntryStatus::Archived {
        return Err(ErrorKind::InvalidQuery(format!(
            "Cannot correct archived entry '{id}'. Restore it first or correct its replacement."
        ))
        .into());
    }

    let now = Utc::now().timestamp();

    // Correction = improving the entry → always boost confidence
    if let Some(text) = correction {
        // Validate correction size before allocating
        const MAX_CORRECTION_LEN: usize = MAX_CONTENT_SIZE / 2;
        if text.len() > MAX_CORRECTION_LEN {
            return Err(ErrorKind::InvalidQuery(format!(
                "Correction text exceeds {MAX_CORRECTION_LEN} bytes"
            ))
            .into());
        }
        let timestamp = chrono::DateTime::from_timestamp(now, 0)
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let correction_block = format!("\n\n## Correction ({})\n\n{}", timestamp, text);
        let new_content = format!("{}{}", entry.content, correction_block);

        if new_content.len() > MAX_CONTENT_SIZE {
            return Err(ErrorKind::InvalidQuery(format!(
                "Correction would exceed max content size ({MAX_CONTENT_SIZE} bytes)"
            ))
            .into());
        }

        conn.execute(
            "UPDATE memory_entries SET confirmations = confirmations + 1, last_confirmed_at = ?1, content = ?2, updated_at = ?1 WHERE id = ?3",
            params![now, new_content, id],
        )?;
        Ok(format!(
            "Corrected: {id} (correction appended, confidence boosted)"
        ))
    } else {
        conn.execute(
            "UPDATE memory_entries SET confirmations = confirmations + 1, last_confirmed_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![now, id],
        )?;
        Ok(format!("Corrected: {id} (confidence boosted)"))
    }
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

/// Maximum number of revisions to keep per memory entry.
const MAX_REVISIONS: usize = 3;

/// A stored revision (diff between two versions of content).
#[derive(Debug, Clone)]
pub struct Revision {
    pub id: i64,
    pub memory_id: String,
    pub diff: String,
    pub created_at: i64,
}

/// Summary of revision history for a memory entry.
#[derive(Debug, Clone)]
pub struct RevisionSummary {
    pub count: usize,
    pub dates: Vec<i64>,
}

/// Save a revision diff when a memory entry is updated.
///
/// Only saves for manually-written entries (`UserStatement`, `OfficialDocs`).
/// Keeps at most `MAX_REVISIONS` per entry, pruning the oldest.
/// Skips saving when content is identical.
pub fn save_revision(
    conn: &Connection,
    memory_id: &str,
    old_content: &str,
    new_content: &str,
    source_type: SourceType,
) -> Result<()> {
    // Only track revisions for manually-written entries
    match source_type {
        SourceType::UserStatement | SourceType::OfficialDocs => {}
        _ => return Ok(()),
    }

    // Skip if content is identical
    if old_content == new_content {
        return Ok(());
    }

    // Compute unified diff
    let text_diff = similar::TextDiff::from_lines(old_content, new_content);
    let diff = text_diff.unified_diff().context_radius(2).to_string();

    let now = Utc::now().timestamp();

    conn.execute(
        "INSERT INTO memory_revisions (memory_id, diff, created_at) VALUES (?1, ?2, ?3)",
        params![memory_id, diff, now],
    )?;

    // Prune oldest revisions beyond MAX_REVISIONS
    conn.execute(
        "DELETE FROM memory_revisions WHERE id IN (
            SELECT id FROM memory_revisions
            WHERE memory_id = ?1
            ORDER BY created_at DESC, id DESC
            LIMIT -1 OFFSET ?2
        )",
        params![memory_id, MAX_REVISIONS as i64],
    )?;

    Ok(())
}

/// Get all revisions for a memory entry, ordered oldest first.
pub fn get_revisions(conn: &Connection, memory_id: &str) -> Result<Vec<Revision>> {
    let mut stmt = conn.prepare(
        "SELECT id, memory_id, diff, created_at FROM memory_revisions
         WHERE memory_id = ?1 ORDER BY created_at ASC, id ASC",
    )?;
    let revisions = stmt
        .query_map(params![memory_id], |row| {
            Ok(Revision {
                id: row.get(0)?,
                memory_id: row.get(1)?,
                diff: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(revisions)
}

/// Get a summary of revisions (count + dates) for display metadata.
pub fn get_revision_summary(conn: &Connection, memory_id: &str) -> Result<RevisionSummary> {
    let mut stmt = conn.prepare(
        "SELECT created_at FROM memory_revisions
         WHERE memory_id = ?1 ORDER BY created_at ASC",
    )?;
    let dates: Vec<i64> = stmt
        .query_map(params![memory_id], |row| row.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(RevisionSummary {
        count: dates.len(),
        dates,
    })
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
        "SELECT id, title, content, entry_type, tags, status, created_at, updated_at, superseded_by, access_count, last_accessed, source_path, confirmations, last_confirmed_at, source_type
         FROM memory_entries WHERE id = ?1"
    )?;

    let entry = stmt.query_row(params![id], row_to_entry).optional()?;

    Ok(entry)
}

/// Delete a memory entry.
pub fn delete_entry(conn: &Connection, id: &str) -> Result<bool> {
    let rows = conn.execute("DELETE FROM memory_entries WHERE id = ?1", params![id])?;
    Ok(rows > 0)
}

/// List memory entries sorted by access count (most popular first).
///
/// This is a convenience wrapper around `list_entries_sorted` with `Popular` sort order.
pub fn list_entries(
    conn: &Connection,
    limit: usize,
    status_filter: Option<EntryStatus>,
) -> Result<Vec<MemoryEntry>> {
    list_entries_sorted(conn, limit, MemorySortOrder::Popular, status_filter)
}

/// Search memory entries using full-text search.
pub fn search_entries(conn: &Connection, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
    let fts_query = crate::store::search::escape_fts5_query(query);
    let mut stmt = conn.prepare(
        "SELECT m.id, m.title, m.content, m.entry_type, m.tags, m.status, m.created_at, m.updated_at, m.superseded_by, m.access_count, m.last_accessed, m.source_path, m.confirmations, m.last_confirmed_at, m.source_type
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

/// BM25 search returning (rowid, entry) pairs for RRF fusion.
fn bm25_search_with_rowid(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<(i64, MemoryEntry)>> {
    let fts_query = crate::store::search::escape_fts5_query(query);
    let mut stmt = conn.prepare(
        "SELECT m.rowid, m.id, m.title, m.content, m.entry_type, m.tags, m.status, m.created_at, m.updated_at, m.superseded_by, m.access_count, m.last_accessed, m.source_path, m.confirmations, m.last_confirmed_at, m.source_type
         FROM memory_entries m
         JOIN memory_fts f ON m.rowid = f.rowid
         WHERE memory_fts MATCH ?1
         ORDER BY bm25(memory_fts)
         LIMIT ?2"
    )?;

    let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
        let rowid: i64 = row.get(0)?;
        let entry = row_to_entry_offset(row, 1)?;
        Ok((rowid, entry))
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

/// L2 distance threshold for duplicate detection (~cosine similarity > 0.85).
const SIMILARITY_THRESHOLD: f32 = 0.55;

/// Weight for RRF/relevance score in confidence-weighted ranking.
const RELEVANCE_WEIGHT: f64 = 0.7;
/// Weight for confidence score in confidence-weighted ranking.
const CONFIDENCE_WEIGHT: f64 = 0.3;

/// Find memory entries similar to the given embedding, excluding `exclude_rowid`.
///
/// Returns a formatted warning string for any matches above the similarity threshold.
pub fn find_similar_entries(
    conn: &Connection,
    embedding: &[f32],
    exclude_rowid: i64,
    exclude_id: &str,
) -> String {
    let mut warnings = String::new();
    if let Ok(similar) = crate::store::vectors::memory_vector_search(conn, embedding, 5) {
        for (sim_rowid, distance) in &similar {
            if *sim_rowid == exclude_rowid || *distance > SIMILARITY_THRESHOLD {
                continue;
            }
            if let Ok(Some(sim_entry)) = get_entry_by_rowid(conn, *sim_rowid) {
                if sim_entry.id != exclude_id {
                    let similarity = 1.0 - (*distance as f64 * *distance as f64 / 2.0);
                    warnings.push_str(&format!(
                        "\nSimilar entry exists: {} (similarity: {:.2}). Consider updating it instead.",
                        sim_entry.id, similarity
                    ));
                }
            }
        }
    }
    warnings
}

/// Get memory entry by rowid (internal, for hybrid search).
fn get_entry_by_rowid(conn: &Connection, rowid: i64) -> Result<Option<MemoryEntry>> {
    let entry = conn
        .query_row(
            "SELECT id, title, content, entry_type, tags, status, created_at, updated_at, superseded_by, access_count, last_accessed, source_path, confirmations, last_confirmed_at, source_type
             FROM memory_entries WHERE rowid = ?1",
            params![rowid],
            row_to_entry,
        )
        .optional()?;
    Ok(entry)
}

/// Batch fetch memory entries by rowids in a single query.
fn get_entries_by_rowids(conn: &Connection, rowids: &[i64]) -> Result<HashMap<i64, MemoryEntry>> {
    if rowids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders: Vec<String> = (1..=rowids.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT rowid, id, title, content, entry_type, tags, status, created_at, updated_at, superseded_by, access_count, last_accessed, source_path, confirmations, last_confirmed_at, source_type
         FROM memory_entries WHERE rowid IN ({})",
        placeholders.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        rowids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        let rowid: i64 = row.get(0)?;
        let entry = row_to_entry_offset(row, 1)?;
        Ok((rowid, entry))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (rowid, entry) = row?;
        map.insert(rowid, entry);
    }
    Ok(map)
}

/// Hybrid search for memory entries: BM25 + vector with RRF fusion.
///
/// Falls back to BM25-only if no embeddings exist or embedding service is unavailable.
pub fn search_entries_hybrid(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&[f32]>,
    limit: usize,
) -> Result<Vec<MemoryEntry>> {
    use crate::store::{hybrid, vectors};

    // BM25 search (get more for fusion)
    let bm25_results = bm25_search_with_rowid(conn, query, limit * 2)?;

    // If no embedding provided, fall back to BM25-only
    let query_embedding = match query_embedding {
        Some(emb) => emb,
        None => {
            return Ok(bm25_results
                .into_iter()
                .take(limit)
                .map(|(_, e)| e)
                .collect());
        }
    };

    // Vector search
    let vector_results = vectors::memory_vector_search(conn, query_embedding, limit * 2)?;

    // If no vector results, fall back to BM25-only
    if vector_results.is_empty() {
        return Ok(bm25_results
            .into_iter()
            .take(limit)
            .map(|(_, e)| e)
            .collect());
    }

    // Build SearchResult wrappers for BM25 (RRF needs SearchResult with i64 id)
    let bm25_for_rrf: Vec<crate::domain::SearchResult> = bm25_results
        .iter()
        .enumerate()
        .map(|(_, (rowid, _))| crate::domain::SearchResult {
            id: *rowid,
            collection: String::new(),
            path: String::new(),
            title: None,
            score: 0.0,
            snippets: vec![],
            status: None,
            superseded_by: None,
            repo_root: None,
        })
        .collect();

    // RRF fusion
    let config = hybrid::HybridConfig::default();
    let mut fused = hybrid::rrf_fusion(&bm25_for_rrf, &vector_results, &config);
    hybrid::normalize_scores(&mut fused);

    // Build a lookup map from rowid -> MemoryEntry (from BM25 results)
    let mut entry_map: HashMap<i64, MemoryEntry> = bm25_results.into_iter().collect();

    // Batch-fetch vector-only entries (not in BM25 results) in a single query
    let vector_only_rowids: Vec<i64> = fused
        .iter()
        .filter(|(rowid, _)| !entry_map.contains_key(rowid))
        .map(|(rowid, _)| *rowid)
        .collect();
    let mut vector_entries = get_entries_by_rowids(conn, &vector_only_rowids)?;

    // Resolve fused results to MemoryEntry
    // Apply confidence-weighted re-ranking: final = rrf_norm * 0.7 + confidence * 0.3
    let mut scored_results: Vec<(MemoryEntry, f64)> = Vec::new();
    for (rowid, rrf_score) in fused {
        let entry = if let Some(e) = entry_map.remove(&rowid) {
            e
        } else if let Some(e) = vector_entries.remove(&rowid) {
            e
        } else {
            continue;
        };
        let final_score = rrf_score * RELEVANCE_WEIGHT + entry.confidence() * CONFIDENCE_WEIGHT;
        scored_results.push((entry, final_score));
    }

    // Re-sort by final score descending
    scored_results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    Ok(scored_results
        .into_iter()
        .take(limit)
        .map(|(e, _)| e)
        .collect())
}

/// Get rowid for a memory entry by its slug ID.
pub fn get_rowid(conn: &Connection, id: &str) -> Result<Option<i64>> {
    let rowid = conn
        .query_row(
            "SELECT rowid FROM memory_entries WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(rowid)
}

/// Get warmup index - compact list of top entries by access count.
///
/// Uses a targeted query selecting only needed columns (no content).
pub fn get_warmup_index(conn: &Connection, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, entry_type, tags FROM memory_entries
         WHERE status = 'active'
         ORDER BY access_count DESC
         LIMIT ?1",
    )?;

    let index: Vec<String> = stmt
        .query_map(params![limit as i64], |row| {
            let id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let entry_type: String = row.get(2)?;
            let tags_json: String = row.get(3)?;
            let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_default();
            let tags_str = tags
                .iter()
                .map(|t| format!("#{t}"))
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!("[{entry_type}] {id}: {title} {tags_str}"))
        })?
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("Failed to read warmup entry: {e}");
                None
            }
        })
        .collect();

    Ok(index)
}

/// Get total count of memory entries.
pub fn count_entries(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM memory_entries", [], |row| row.get(0))?;
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
        .filter_map(|r| match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!("Failed to read prunable entry ID: {e}");
                None
            }
        })
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
    row_to_entry_offset(row, 0)
}

/// Parse a MemoryEntry from a row with a column offset (for queries that prepend extra columns).
fn row_to_entry_offset(row: &rusqlite::Row<'_>, off: usize) -> rusqlite::Result<MemoryEntry> {
    let tags_json: String = row.get(off + 4)?;
    let tags: Vec<String> = serde_json::from_str(&tags_json).unwrap_or_else(|e| {
        tracing::warn!("Failed to deserialize tags, defaulting to empty: {e}");
        Vec::new()
    });
    let entry_type_str: String = row.get(off + 3)?;
    let status_str: String = row.get(off + 5)?;
    let source_type_str: String = row.get::<_, Option<String>>(off + 14)?.unwrap_or_default();

    Ok(MemoryEntry {
        id: row.get(off)?,
        title: row.get(off + 1)?,
        content: row.get(off + 2)?,
        entry_type: entry_type_str.parse().unwrap_or_else(|_| {
            tracing::warn!(
                "Unknown entry_type '{}', defaulting to Topic",
                entry_type_str
            );
            EntryType::Topic
        }),
        tags,
        status: status_str.parse().unwrap_or_else(|_| {
            tracing::warn!("Unknown status '{}', defaulting to Active", status_str);
            EntryStatus::Active
        }),
        created_at: row.get(off + 6)?,
        updated_at: row.get(off + 7)?,
        superseded_by: row.get(off + 8)?,
        access_count: u64::try_from(row.get::<_, i64>(off + 9)?).unwrap_or(0),
        last_accessed: row.get(off + 10)?,
        source_path: row.get(off + 11)?,
        confirmations: row.get::<_, Option<i64>>(off + 12)?.unwrap_or(0) as u32,
        last_confirmed_at: row.get(off + 13)?,
        source_type: source_type_str.parse().unwrap_or_else(|_| {
            tracing::warn!(
                "Unknown source_type '{}', defaulting to UserStatement",
                source_type_str
            );
            SourceType::UserStatement
        }),
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };

        add_entry(&conn, &entry).unwrap();

        entry.content = "Updated content with solution".to_string();
        entry.tags.push("fixed".to_string());
        update_entry(&conn, &entry).unwrap();

        let retrieved = get_entry_without_tracking(&conn, "bug-fix")
            .unwrap()
            .unwrap();
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };

        add_entry(&conn, &entry).unwrap();
        assert!(
            get_entry_without_tracking(&conn, "to-delete")
                .unwrap()
                .is_some()
        );

        let deleted = delete_entry(&conn, "to-delete").unwrap();
        assert!(deleted);
        assert!(
            get_entry_without_tracking(&conn, "to-delete")
                .unwrap()
                .is_none()
        );
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
                confirmations: 0,
                last_confirmed_at: None,
                source_type: SourceType::UserStatement,
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };

        add_entry(&conn, &entry1).unwrap();
        add_entry(&conn, &entry2).unwrap();

        let results = search_entries(&conn, "authentication", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "auth-jwt");
    }

    #[test]
    fn test_search_entries_by_tag() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let entry = MemoryEntry {
            id: "tagged-entry".to_string(),
            title: "Some title".to_string(),
            content: "Unrelated content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec!["kubernetes".to_string(), "deployment".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &entry).unwrap();

        // Search by tag name — should find via FTS tags column
        let results = search_entries(&conn, "kubernetes", 10).unwrap();
        assert_eq!(results.len(), 1, "Should find entry by tag");
        assert_eq!(results[0].id, "tagged-entry");
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };

        add_entry(&conn, &entry).unwrap();

        let index = get_warmup_index(&conn, 50).unwrap();
        assert_eq!(index.len(), 1);
        assert!(
            index[0].starts_with("[problem]"),
            "Should start with type prefix, got: {}",
            index[0]
        );
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
                status: if i == 2 {
                    EntryStatus::Archived
                } else {
                    EntryStatus::Active
                },
                created_at: now,
                updated_at: now,
                superseded_by: None,
                access_count: 0,
                last_accessed: None,
                source_path: None,
                confirmations: 0,
                last_confirmed_at: None,
                source_type: SourceType::UserStatement,
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &entry).unwrap();

        // Prune with 30 days - should find nothing
        let pruned = prune_entries(&conn, 30, false).unwrap();
        assert!(pruned.is_empty());

        // Entry should still be active
        let retrieved = get_entry_without_tracking(&conn, "recent")
            .unwrap()
            .unwrap();
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &entry).unwrap();

        // Dry run - should report but not change
        let pruned = prune_entries(&conn, 30, true).unwrap();
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0], "stale-dry");

        // Entry should still be active (dry run)
        let retrieved = get_entry_without_tracking(&conn, "stale-dry")
            .unwrap()
            .unwrap();
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
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

    #[test]
    fn test_list_entries_sorted_popular() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let high = MemoryEntry {
            id: "high".to_string(),
            title: "High".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now - 100,
            updated_at: now,
            superseded_by: None,
            access_count: 50,
            last_accessed: Some(now - 200),
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        let low = MemoryEntry {
            id: "low".to_string(),
            title: "Low".to_string(),
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
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &low).unwrap();
        add_entry(&conn, &high).unwrap();

        let entries = list_entries_sorted(
            &conn,
            10,
            MemorySortOrder::Popular,
            Some(EntryStatus::Active),
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].id, "high",
            "Popular sort: highest access_count first"
        );
        assert_eq!(entries[1].id, "low");
    }

    #[test]
    fn test_list_entries_sorted_recent() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let old_accessed = MemoryEntry {
            id: "old".to_string(),
            title: "Old".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 100,
            last_accessed: Some(now - 1000),
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        let recent = MemoryEntry {
            id: "recent".to_string(),
            title: "Recent".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now - 500,
            updated_at: now,
            superseded_by: None,
            access_count: 1,
            last_accessed: Some(now),
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &old_accessed).unwrap();
        add_entry(&conn, &recent).unwrap();

        let entries = list_entries_sorted(
            &conn,
            10,
            MemorySortOrder::Recent,
            Some(EntryStatus::Active),
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].id, "recent",
            "Recent sort: most recently accessed first"
        );
        assert_eq!(entries[1].id, "old");
    }

    #[test]
    fn test_list_entries_sorted_newest() {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        let older = MemoryEntry {
            id: "older".to_string(),
            title: "Older".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now - 1000,
            updated_at: now,
            superseded_by: None,
            access_count: 100,
            last_accessed: Some(now),
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        let newer = MemoryEntry {
            id: "newer".to_string(),
            title: "Newer".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 1,
            last_accessed: Some(now - 500),
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &older).unwrap();
        add_entry(&conn, &newer).unwrap();

        let entries = list_entries_sorted(
            &conn,
            10,
            MemorySortOrder::Newest,
            Some(EntryStatus::Active),
        )
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].id, "newer",
            "Newest sort: most recently created first"
        );
        assert_eq!(entries[1].id, "older");
    }

    #[test]
    fn test_validate_entry_valid() {
        validate_entry_input("auth-jwt", "JWT Auth", &["auth".into()], "content").unwrap();
    }

    #[test]
    fn test_validate_entry_empty_id() {
        let err = validate_entry_input("", "Title", &[], "content").unwrap_err();
        assert!(err.to_string().contains("ID must be"), "{err}");
    }

    #[test]
    fn test_validate_entry_id_too_long() {
        let long_id = "a".repeat(MAX_ID_LEN + 1);
        let err = validate_entry_input(&long_id, "Title", &[], "content").unwrap_err();
        assert!(err.to_string().contains("ID must be"), "{err}");
    }

    #[test]
    fn test_validate_entry_id_invalid_chars() {
        let err = validate_entry_input("Auth_JWT", "Title", &[], "content").unwrap_err();
        assert!(err.to_string().contains("lowercase"), "{err}");
    }

    #[test]
    fn test_validate_entry_empty_title() {
        let err = validate_entry_input("ok-id", "", &[], "content").unwrap_err();
        assert!(err.to_string().contains("Title must be"), "{err}");
    }

    #[test]
    fn test_validate_entry_title_too_long() {
        let long_title = "x".repeat(MAX_TITLE_LEN + 1);
        let err = validate_entry_input("ok-id", &long_title, &[], "content").unwrap_err();
        assert!(err.to_string().contains("Title must be"), "{err}");
    }

    #[test]
    fn test_validate_entry_too_many_tags() {
        let tags: Vec<String> = (0..MAX_TAGS + 1).map(|i| format!("tag-{i}")).collect();
        let err = validate_entry_input("ok-id", "Title", &tags, "content").unwrap_err();
        assert!(err.to_string().contains("Too many tags"), "{err}");
    }

    #[test]
    fn test_validate_entry_tag_too_long() {
        let tags = vec!["x".repeat(MAX_TAG_LEN + 1)];
        let err = validate_entry_input("ok-id", "Title", &tags, "content").unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
    }

    #[test]
    fn test_validate_entry_content_too_large() {
        let big = "x".repeat(MAX_CONTENT_SIZE + 1);
        let err = validate_entry_input("ok-id", "Title", &[], &big).unwrap_err();
        assert!(err.to_string().contains("Content exceeds"), "{err}");
    }

    // ==================== Hybrid Search Tests ====================

    fn setup_db_with_vectors() -> Connection {
        use crate::store::vectors;
        vectors::init_sqlite_vec();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::store::schema::init_schema(&conn).unwrap();
        vectors::init_vector_schema(&conn).unwrap();
        conn
    }

    fn test_embedding(seed: f32) -> Vec<f32> {
        (0..crate::store::vectors::EMBEDDING_DIM)
            .map(|i| seed + i as f32 * 0.001)
            .collect()
    }

    #[test]
    fn test_hybrid_search_falls_back_to_bm25_without_embedding() {
        let conn = setup_db_with_vectors();

        let entry = MemoryEntry {
            id: "test-entry".to_string(),
            title: "OAuth PKCE Flow".to_string(),
            content: "How we handle authentication with PKCE protocol".to_string(),
            entry_type: EntryType::Topic,
            tags: vec!["auth".to_string()],
            status: EntryStatus::Active,
            created_at: 1000,
            updated_at: 1000,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &entry).unwrap();

        // Search without embedding — should fall back to BM25
        let results = search_entries_hybrid(&conn, "OAuth PKCE", None, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "test-entry");
    }

    #[test]
    fn test_hybrid_search_finds_semantic_match() {
        let conn = setup_db_with_vectors();
        use crate::store::vectors;

        // Entry 1: keyword match for "authentication"
        let e1 = MemoryEntry {
            id: "auth-basic".to_string(),
            title: "Basic Authentication Setup".to_string(),
            content: "How to configure basic authentication".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: 1000,
            updated_at: 1000,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &e1).unwrap();
        let rowid1 = get_rowid(&conn, "auth-basic").unwrap().unwrap();
        vectors::store_memory_embedding(&conn, rowid1, &test_embedding(0.1), "test").unwrap();

        // Entry 2: different keywords but semantically similar embedding
        let e2 = MemoryEntry {
            id: "jwt-refresh".to_string(),
            title: "JWT Token Refresh Strategy".to_string(),
            content: "Design for token expiration and refresh flow".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: 1000,
            updated_at: 1000,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &e2).unwrap();
        let rowid2 = get_rowid(&conn, "jwt-refresh").unwrap().unwrap();
        // Give jwt-refresh a similar embedding to the query
        vectors::store_memory_embedding(&conn, rowid2, &test_embedding(0.11), "test").unwrap();

        // Entry 3: unrelated
        let e3 = MemoryEntry {
            id: "db-tuning".to_string(),
            title: "Database Tuning Notes".to_string(),
            content: "SQLite WAL mode and pragma settings".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: 1000,
            updated_at: 1000,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &e3).unwrap();
        let rowid3 = get_rowid(&conn, "db-tuning").unwrap().unwrap();
        vectors::store_memory_embedding(&conn, rowid3, &test_embedding(0.9), "test").unwrap();

        // Query: "token expiration" — BM25 matches jwt-refresh, vector matches auth-basic+jwt-refresh
        // Query embedding close to auth entries
        let query_emb = test_embedding(0.105);
        let results =
            search_entries_hybrid(&conn, "token expiration", Some(&query_emb), 10).unwrap();

        // jwt-refresh should be found (has both BM25 keyword match and vector similarity)
        let result_ids: Vec<&str> = results.iter().map(|e| e.id.as_str()).collect();
        assert!(
            result_ids.contains(&"jwt-refresh"),
            "jwt-refresh should be found via BM25+vector"
        );
        // auth-basic should be found via vector similarity even without keyword match
        assert!(
            result_ids.contains(&"auth-basic"),
            "auth-basic should be found via vector similarity"
        );
    }

    #[test]
    fn test_hybrid_search_respects_limit() {
        let conn = setup_db_with_vectors();
        use crate::store::vectors;

        for i in 1..=10 {
            let entry = MemoryEntry {
                id: format!("entry-{i}"),
                title: format!("Test Entry {i}"),
                content: format!("Content for searchable entry number {i}"),
                entry_type: EntryType::Topic,
                tags: vec![],
                status: EntryStatus::Active,
                created_at: 1000,
                updated_at: 1000,
                superseded_by: None,
                access_count: 0,
                last_accessed: None,
                source_path: None,
                confirmations: 0,
                last_confirmed_at: None,
                source_type: SourceType::UserStatement,
            };
            add_entry(&conn, &entry).unwrap();
            let rowid = get_rowid(&conn, &format!("entry-{i}")).unwrap().unwrap();
            vectors::store_memory_embedding(&conn, rowid, &test_embedding(i as f32 * 0.1), "test")
                .unwrap();
        }

        let query_emb = test_embedding(0.5);
        let results =
            search_entries_hybrid(&conn, "searchable entry", Some(&query_emb), 3).unwrap();
        assert_eq!(results.len(), 3, "Should respect limit of 3");
    }

    #[test]
    fn test_get_rowid() {
        let conn = setup_db_with_vectors();
        let entry = MemoryEntry {
            id: "my-entry".to_string(),
            title: "My Entry".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: 1000,
            updated_at: 1000,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
        };
        add_entry(&conn, &entry).unwrap();

        let rowid = get_rowid(&conn, "my-entry").unwrap();
        assert!(rowid.is_some());

        let missing = get_rowid(&conn, "nonexistent").unwrap();
        assert!(missing.is_none());
    }

    // ==================== Confidence Formula Tests ====================

    fn make_entry_at(
        created: i64,
        confirmations: u32,
        access_count: u64,
        last_confirmed: Option<i64>,
        source: SourceType,
    ) -> MemoryEntry {
        MemoryEntry {
            id: "test".to_string(),
            title: "Test".to_string(),
            content: "Content".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: created,
            updated_at: created,
            superseded_by: None,
            access_count,
            last_accessed: None,
            source_path: None,
            confirmations,
            last_confirmed_at: last_confirmed,
            source_type: source,
        }
    }

    #[test]
    fn test_confidence_new_user_statement() {
        let now = 1000;
        let entry = make_entry_at(now, 0, 0, None, SourceType::UserStatement);
        let conf = entry.confidence_at(now);
        // belief=0.5, decay=1.0, source=0.85 → 0.425
        assert!(
            (conf - 0.425).abs() < 0.01,
            "New user_statement should be ~0.425, got {conf}"
        );
    }

    #[test]
    fn test_confidence_90_day_stale() {
        let created = 0;
        let now = 90 * 86400; // 90 days later
        let entry = make_entry_at(created, 0, 0, None, SourceType::UserStatement);
        let conf = entry.confidence_at(now);
        // belief=0.5, decay=e^(-1)≈0.368, source=0.85 → ~0.156
        assert!(
            (conf - 0.156).abs() < 0.02,
            "90-day stale should be ~0.156, got {conf}"
        );
    }

    #[test]
    fn test_confidence_heavily_confirmed() {
        let now = 1000;
        let entry = make_entry_at(now, 5, 0, Some(now), SourceType::OfficialDocs);
        let conf = entry.confidence_at(now);
        // belief=6/7≈0.857, decay=1.0, source=1.0 → ~0.857
        assert!(
            (conf - 0.857).abs() < 0.01,
            "Heavily confirmed should be ~0.857, got {conf}"
        );
    }

    #[test]
    fn test_confidence_access_slows_decay() {
        let created = 0;
        let now = 90 * 86400;
        let no_access = make_entry_at(created, 0, 0, None, SourceType::UserStatement);
        let high_access = make_entry_at(created, 0, 10, None, SourceType::UserStatement);
        let conf_no = no_access.confidence_at(now);
        let conf_hi = high_access.confidence_at(now);
        assert!(
            conf_hi > conf_no,
            "High access should slow decay: {conf_hi} > {conf_no}"
        );
    }

    #[test]
    fn test_confidence_floor() {
        let created = 0;
        let now = 365 * 5 * 86400; // 5 years
        let entry = make_entry_at(created, 0, 0, None, SourceType::Inference);
        let conf = entry.confidence_at(now);
        assert!(
            (conf - CONFIDENCE_FLOOR).abs() < 0.001,
            "Very old entry should hit floor {CONFIDENCE_FLOOR}, got {conf}"
        );
    }

    #[test]
    fn test_confidence_inference_lower_than_user() {
        let now = 1000;
        let user = make_entry_at(now, 0, 0, None, SourceType::UserStatement);
        let infer = make_entry_at(now, 0, 0, None, SourceType::Inference);
        assert!(
            user.confidence_at(now) > infer.confidence_at(now),
            "user_statement should score higher than inference"
        );
    }

    #[test]
    fn test_confidence_new_auto_extracted() {
        let now = 1000;
        let entry = make_entry_at(now, 0, 0, None, SourceType::AutoExtracted);
        let conf = entry.confidence_at(now);
        // belief=0.5, decay=1.0, source=0.70 → 0.35
        assert!(
            (conf - 0.35).abs() < 0.01,
            "New auto_extracted should be ~0.35, got {conf}"
        );
    }

    #[test]
    fn test_confidence_auto_extracted_90_day_stale() {
        let created = 0;
        let now = 90 * 86400;
        let entry = make_entry_at(created, 0, 0, None, SourceType::AutoExtracted);
        let conf = entry.confidence_at(now);
        // belief=0.5, decay=e^(-1)≈0.368, source=0.70 → ~0.129
        assert!(
            (conf - 0.129).abs() < 0.02,
            "90-day auto_extracted should be ~0.129, got {conf}"
        );
    }

    #[test]
    fn test_confidence_auto_extracted_after_confirmation() {
        let now = 1000;
        let entry = make_entry_at(now, 1, 0, Some(now), SourceType::AutoExtracted);
        let conf = entry.confidence_at(now);
        // belief=2/3≈0.667, decay=1.0, source=0.70 → ~0.467
        assert!(
            (conf - 0.467).abs() < 0.01,
            "Confirmed auto_extracted should be ~0.467, got {conf}"
        );
    }

    #[test]
    fn test_confidence_auto_extracted_between_inference_and_user() {
        let now = 1000;
        let auto = make_entry_at(now, 0, 0, None, SourceType::AutoExtracted);
        let user = make_entry_at(now, 0, 0, None, SourceType::UserStatement);
        let infer = make_entry_at(now, 0, 0, None, SourceType::Inference);
        let conf_auto = auto.confidence_at(now);
        let conf_user = user.confidence_at(now);
        let conf_infer = infer.confidence_at(now);
        assert!(
            conf_user > conf_auto,
            "user > auto_extracted: {conf_user} > {conf_auto}"
        );
        assert!(
            conf_auto > conf_infer,
            "auto_extracted > inference: {conf_auto} > {conf_infer}"
        );
    }

    #[test]
    fn test_source_type_auto_extracted_roundtrip() {
        let parsed: SourceType = "auto_extracted".parse().unwrap();
        assert_eq!(parsed, SourceType::AutoExtracted);
        assert_eq!(parsed.to_string(), "auto_extracted");
    }

    // ==================== Revision History Tests ====================

    #[test]
    fn test_save_revision_creates_diff() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        save_revision(
            &conn,
            "test",
            "Old content",
            "New content",
            SourceType::UserStatement,
        )
        .unwrap();

        let revisions = get_revisions(&conn, "test").unwrap();
        assert_eq!(revisions.len(), 1);
        assert!(revisions[0].diff.contains("Old content"));
        assert!(revisions[0].diff.contains("New content"));
    }

    #[test]
    fn test_save_revision_skips_auto_extracted() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::AutoExtracted,
        );
        add_entry(&conn, &entry).unwrap();

        save_revision(&conn, "test", "Old", "New", SourceType::AutoExtracted).unwrap();

        let revisions = get_revisions(&conn, "test").unwrap();
        assert!(
            revisions.is_empty(),
            "auto_extracted should not create revisions"
        );
    }

    #[test]
    fn test_save_revision_skips_inference() {
        let conn = setup_db();
        let entry = make_entry_at(Utc::now().timestamp(), 0, 0, None, SourceType::Inference);
        add_entry(&conn, &entry).unwrap();

        save_revision(&conn, "test", "Old", "New", SourceType::Inference).unwrap();

        let revisions = get_revisions(&conn, "test").unwrap();
        assert!(
            revisions.is_empty(),
            "inference should not create revisions"
        );
    }

    #[test]
    fn test_save_revision_keeps_max_three() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        save_revision(&conn, "test", "v0", "v1", SourceType::UserStatement).unwrap();
        save_revision(&conn, "test", "v1", "v2", SourceType::UserStatement).unwrap();
        save_revision(&conn, "test", "v2", "v3", SourceType::UserStatement).unwrap();
        save_revision(&conn, "test", "v3", "v4", SourceType::UserStatement).unwrap();

        let revisions = get_revisions(&conn, "test").unwrap();
        assert_eq!(revisions.len(), 3, "should keep max 3 revisions");
        // Oldest should be v1→v2 (v0→v1 pruned)
        assert!(
            revisions[0].diff.contains("v1"),
            "oldest should reference v1→v2"
        );
        assert!(
            revisions[0].diff.contains("v2"),
            "oldest should reference v1→v2"
        );
    }

    #[test]
    fn test_save_revision_skips_identical_content() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        save_revision(
            &conn,
            "test",
            "Same content",
            "Same content",
            SourceType::UserStatement,
        )
        .unwrap();

        let revisions = get_revisions(&conn, "test").unwrap();
        assert!(revisions.is_empty(), "no revision for identical content");
    }

    #[test]
    fn test_revision_summary() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        save_revision(&conn, "test", "v0", "v1", SourceType::UserStatement).unwrap();
        save_revision(&conn, "test", "v1", "v2", SourceType::UserStatement).unwrap();

        let summary = get_revision_summary(&conn, "test").unwrap();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.dates.len(), 2);
    }

    #[test]
    fn test_revision_summary_empty() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        let summary = get_revision_summary(&conn, "test").unwrap();
        assert_eq!(summary.count, 0);
        assert!(summary.dates.is_empty());
    }

    #[test]
    fn test_revisions_deleted_with_entry() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        save_revision(&conn, "test", "v0", "v1", SourceType::UserStatement).unwrap();

        delete_entry(&conn, "test").unwrap();

        let revisions = get_revisions(&conn, "test").unwrap();
        assert!(revisions.is_empty(), "revisions should be cascade-deleted");
    }

    // ==================== Confirm/Correct Tests ====================

    #[test]
    fn test_confirm_entry_increments() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        let result = confirm_entry(&conn, "test").unwrap();
        assert!(result.contains("Confirmed"), "{result}");

        let updated = get_entry_without_tracking(&conn, "test").unwrap().unwrap();
        assert_eq!(updated.confirmations, 1);
        assert!(updated.last_confirmed_at.is_some());
    }

    #[test]
    fn test_confirm_archived_restores() {
        let conn = setup_db();
        let mut entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        entry.status = EntryStatus::Archived;
        add_entry(&conn, &entry).unwrap();

        let result = confirm_entry(&conn, "test").unwrap();
        assert!(result.contains("restored"), "{result}");

        let updated = get_entry_without_tracking(&conn, "test").unwrap().unwrap();
        assert_eq!(updated.status, EntryStatus::Active);
    }

    #[test]
    fn test_confirm_superseded_blocked() {
        let conn = setup_db();
        let mut entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        entry.status = EntryStatus::Superseded;
        add_entry(&conn, &entry).unwrap();

        let result = confirm_entry(&conn, "test");
        assert!(result.is_err(), "Should block confirm on superseded");
    }

    #[test]
    fn test_correct_entry_increments() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        let result = correct_entry(&conn, "test", None).unwrap();
        assert!(result.contains("Corrected"), "{result}");

        let updated = get_entry_without_tracking(&conn, "test").unwrap().unwrap();
        assert_eq!(updated.confirmations, 1, "correct always boosts confidence");
    }

    #[test]
    fn test_correct_with_text_appends_and_boosts() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        correct_entry(&conn, "test", Some("The API changed to v3")).unwrap();

        let updated = get_entry_without_tracking(&conn, "test").unwrap().unwrap();
        assert!(
            updated.content.contains("## Correction"),
            "Should have correction header"
        );
        assert!(
            updated.content.contains("The API changed to v3"),
            "Should contain correction text"
        );
        assert_eq!(
            updated.confirmations, 1,
            "correction should boost confidence"
        );
        assert!(
            updated.last_confirmed_at.is_some(),
            "should set last_confirmed_at"
        );
    }

    #[test]
    fn test_correct_without_text_boosts() {
        let conn = setup_db();
        let entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        add_entry(&conn, &entry).unwrap();

        correct_entry(&conn, "test", None).unwrap();

        let updated = get_entry_without_tracking(&conn, "test").unwrap().unwrap();
        assert_eq!(
            updated.confirmations, 1,
            "correction should boost confidence"
        );
        assert!(
            updated.last_confirmed_at.is_some(),
            "should set last_confirmed_at"
        );
    }

    #[test]
    fn test_correct_superseded_blocked() {
        let conn = setup_db();
        let mut entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        entry.status = EntryStatus::Superseded;
        add_entry(&conn, &entry).unwrap();

        assert!(correct_entry(&conn, "test", None).is_err());
    }

    #[test]
    fn test_correct_archived_blocked() {
        let conn = setup_db();
        let mut entry = make_entry_at(
            Utc::now().timestamp(),
            0,
            0,
            None,
            SourceType::UserStatement,
        );
        entry.status = EntryStatus::Archived;
        add_entry(&conn, &entry).unwrap();

        assert!(correct_entry(&conn, "test", None).is_err());
    }
}
