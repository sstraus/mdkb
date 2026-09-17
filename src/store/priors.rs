//! Behavioral-prior mining storage: `prior_candidates` and `prior_clusters`.
//!
//! Candidates are per-session observed episodes distilled into a reusable
//! lesson. Clusters dedup semantically-equivalent candidates and accumulate
//! recurrence evidence; a cluster is promoted into `memory_entries`
//! (entry_type=prior) once it clears the recurrence/confirmation gate. Neither
//! candidates nor un-promoted clusters are ever injected — only promoted priors
//! surface, and only when their trigger matches the current context.
//!
//! Phase 1 (this module) provides the data model and round-trippable
//! persistence. Detection, LLM distillation, promotion, and trigger-matched
//! injection are built in later phases on top of these tables.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::domain::prior_distill::DistilledPrior;
use crate::error::Result;
use crate::llm::cosine_similarity;
use crate::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};

/// A deduped, promotable behavioral lesson accumulating recurrence evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorCluster {
    /// Canonical trigger-key hash (primary key / dedup identity).
    pub id: String,
    /// Normalized trigger identity used to dedup equivalent candidates.
    pub canonical_trigger_key: String,
    /// `prompt` | `pre_tool` | `post_tool` | `stop` | `repo`.
    pub trigger_kind: String,
    /// JSON: machine-matchable condition (path glob, tool name, prompt terms).
    pub trigger_matcher: String,
    /// Imperative lesson (<=160 chars).
    pub lesson: String,
    /// JSON: `{repo, languages, paths}`.
    pub scope: String,
    pub evidence_count: i64,
    pub distinct_sessions: i64,
    pub injected_count: i64,
    pub confirmed_count: i64,
    pub refuted_count: i64,
    /// `candidate` | `promoted` | `refuted` | `expired`.
    pub state: String,
    /// `memory_entries.id` once promoted; `None` while un-promoted.
    pub promoted_memory_id: Option<String>,
    pub created_at: i64,
    pub last_seen_at: i64,
    /// The tool-error signature this lesson exists to prevent, captured from the
    /// episode that first mined it. A session where the prior was injected and
    /// this signature did NOT come back confirms it; one where it did refutes it.
    /// `None` for clusters mined before the loop existed, and for clusters mined
    /// from a user correction rather than an error.
    pub error_signature: Option<String>,
}

/// A single observed episode feeding a cluster. Never injected directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorCandidate {
    pub id: String,
    /// Assigned when merged into a cluster; `None` until then.
    pub cluster_id: Option<String>,
    /// `candidate` | `promoted` | `refuted` | `expired`.
    pub state: String,
    pub trigger_kind: String,
    /// JSON: machine-matchable condition.
    pub trigger_matcher: String,
    pub lesson: String,
    /// JSON scope.
    pub scope: String,
    pub evidence_failure: Option<String>,
    pub evidence_fix: Option<String>,
    pub source_session: Option<String>,
    pub created_at: i64,
}

/// Minimum injection score for a promoted cluster to surface. A fresh cluster
/// seen in exactly 2 distinct sessions with no confirmations sits just above
/// this, so recurrence alone (the promotion gate) makes a prior injectable;
/// refutations or staleness push it back under.
pub const INJECT_SCORE_THRESHOLD: f64 = 0.3;

/// Injection score in `[0, 1]` for a prior cluster, decoupled from the
/// per-entry `source_authority` that made honestly-tagged AI priors
/// permanently non-injectable (they could never reach the 0.7 memory gate).
///
/// `score = recurrence × freshness × belief`, where
/// - `recurrence = sessions / (sessions + 1)` — saturating; rewards a lesson
///   observed across independent sessions, not one noisy transcript.
/// - `freshness = exp(-days_since_seen / (90 × strength))`, `strength` grows
///   with accumulated evidence so well-established lessons decay slower.
/// - `belief = (1 + confirmed) / (2 + confirmed + refuted)` — a Beta posterior
///   where refutations are genuine negative evidence (not a floored decrement).
pub fn cluster_injection_score(c: &PriorCluster, now: i64) -> f64 {
    let sessions = c.distinct_sessions.max(0) as f64;
    let recurrence = sessions / (sessions + 1.0);

    let days = ((now - c.last_seen_at) as f64 / 86_400.0).max(0.0);
    let strength = 1.0 + (1.0 + c.evidence_count.max(0) as f64).ln();
    let freshness = (-days / (90.0 * strength)).exp();

    let confirmed = c.confirmed_count.max(0) as f64;
    let refuted = c.refuted_count.max(0) as f64;
    let belief = (1.0 + confirmed) / (2.0 + confirmed + refuted);

    recurrence * freshness * belief
}

/// Whether a cluster may be injected: only a `promoted` cluster whose score
/// clears the threshold. Candidates and un-promoted clusters never inject.
pub fn is_injectable(c: &PriorCluster, now: i64) -> bool {
    c.state == "promoted" && cluster_injection_score(c, now) >= INJECT_SCORE_THRESHOLD
}

/// Insert or update a prior cluster. Uses `ON CONFLICT DO UPDATE` (not
/// `INSERT OR REPLACE`) so an update does NOT delete+reinsert the row — a delete
/// would fire `ON DELETE SET NULL` and silently unlink every candidate.
pub fn upsert_cluster(conn: &Connection, c: &PriorCluster) -> Result<()> {
    conn.execute(
        "INSERT INTO prior_clusters (
            id, canonical_trigger_key, trigger_kind, trigger_matcher, lesson, scope,
            evidence_count, distinct_sessions, injected_count, confirmed_count, refuted_count,
            state, promoted_memory_id, created_at, last_seen_at, error_signature
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
        ON CONFLICT(id) DO UPDATE SET
            canonical_trigger_key = excluded.canonical_trigger_key,
            trigger_kind = excluded.trigger_kind,
            trigger_matcher = excluded.trigger_matcher,
            lesson = excluded.lesson,
            scope = excluded.scope,
            evidence_count = excluded.evidence_count,
            distinct_sessions = excluded.distinct_sessions,
            injected_count = excluded.injected_count,
            confirmed_count = excluded.confirmed_count,
            refuted_count = excluded.refuted_count,
            state = excluded.state,
            promoted_memory_id = excluded.promoted_memory_id,
            created_at = excluded.created_at,
            last_seen_at = excluded.last_seen_at,
            error_signature = excluded.error_signature",
        params![
            c.id,
            c.canonical_trigger_key,
            c.trigger_kind,
            c.trigger_matcher,
            c.lesson,
            c.scope,
            c.evidence_count,
            c.distinct_sessions,
            c.injected_count,
            c.confirmed_count,
            c.refuted_count,
            c.state,
            c.promoted_memory_id,
            c.created_at,
            c.last_seen_at,
            c.error_signature,
        ],
    )?;
    Ok(())
}

/// Fetch a cluster by id.
pub fn get_cluster(conn: &Connection, id: &str) -> Result<Option<PriorCluster>> {
    let cluster = conn
        .query_row(
            "SELECT id, canonical_trigger_key, trigger_kind, trigger_matcher, lesson, scope,
                    evidence_count, distinct_sessions, injected_count, confirmed_count, refuted_count,
                    state, promoted_memory_id, created_at, last_seen_at, error_signature
             FROM prior_clusters WHERE id = ?1",
            params![id],
            |row| {
                Ok(PriorCluster {
                    id: row.get(0)?,
                    canonical_trigger_key: row.get(1)?,
                    trigger_kind: row.get(2)?,
                    trigger_matcher: row.get(3)?,
                    lesson: row.get(4)?,
                    scope: row.get(5)?,
                    evidence_count: row.get(6)?,
                    distinct_sessions: row.get(7)?,
                    injected_count: row.get(8)?,
                    confirmed_count: row.get(9)?,
                    refuted_count: row.get(10)?,
                    state: row.get(11)?,
                    promoted_memory_id: row.get(12)?,
                    created_at: row.get(13)?,
                    last_seen_at: row.get(14)?,
                    error_signature: row.get(15)?,
                })
            },
        )
        .optional()?;
    Ok(cluster)
}

/// Insert or update a prior candidate (`ON CONFLICT DO UPDATE`, not
/// `INSERT OR REPLACE`, to avoid delete+reinsert churn).
pub fn upsert_candidate(conn: &Connection, c: &PriorCandidate) -> Result<()> {
    conn.execute(
        "INSERT INTO prior_candidates (
            id, cluster_id, state, trigger_kind, trigger_matcher, lesson, scope,
            evidence_failure, evidence_fix, source_session, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        ON CONFLICT(id) DO UPDATE SET
            cluster_id = excluded.cluster_id,
            state = excluded.state,
            trigger_kind = excluded.trigger_kind,
            trigger_matcher = excluded.trigger_matcher,
            lesson = excluded.lesson,
            scope = excluded.scope,
            evidence_failure = excluded.evidence_failure,
            evidence_fix = excluded.evidence_fix,
            source_session = excluded.source_session,
            created_at = excluded.created_at",
        params![
            c.id,
            c.cluster_id,
            c.state,
            c.trigger_kind,
            c.trigger_matcher,
            c.lesson,
            c.scope,
            c.evidence_failure,
            c.evidence_fix,
            c.source_session,
            c.created_at,
        ],
    )?;
    Ok(())
}

/// Fetch a candidate by id.
pub fn get_candidate(conn: &Connection, id: &str) -> Result<Option<PriorCandidate>> {
    let candidate = conn
        .query_row(
            "SELECT id, cluster_id, state, trigger_kind, trigger_matcher, lesson, scope,
                    evidence_failure, evidence_fix, source_session, created_at
             FROM prior_candidates WHERE id = ?1",
            params![id],
            |row| {
                Ok(PriorCandidate {
                    id: row.get(0)?,
                    cluster_id: row.get(1)?,
                    state: row.get(2)?,
                    trigger_kind: row.get(3)?,
                    trigger_matcher: row.get(4)?,
                    lesson: row.get(5)?,
                    scope: row.get(6)?,
                    evidence_failure: row.get(7)?,
                    evidence_fix: row.get(8)?,
                    source_session: row.get(9)?,
                    created_at: row.get(10)?,
                })
            },
        )
        .optional()?;
    Ok(candidate)
}

/// All candidates assigned to a cluster, oldest first.
pub fn list_candidates_for_cluster(
    conn: &Connection,
    cluster_id: &str,
) -> Result<Vec<PriorCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT id, cluster_id, state, trigger_kind, trigger_matcher, lesson, scope,
                evidence_failure, evidence_fix, source_session, created_at
         FROM prior_candidates WHERE cluster_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map(params![cluster_id], |row| {
        Ok(PriorCandidate {
            id: row.get(0)?,
            cluster_id: row.get(1)?,
            state: row.get(2)?,
            trigger_kind: row.get(3)?,
            trigger_matcher: row.get(4)?,
            lesson: row.get(5)?,
            scope: row.get(6)?,
            evidence_failure: row.get(7)?,
            evidence_fix: row.get(8)?,
            source_session: row.get(9)?,
            created_at: row.get(10)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// ============================================================================
// Phase 6 — clustering, recurrence, promotion gate
// ============================================================================

/// Distinct sessions a lesson must recur across before it may be promoted.
/// (An explicit user "remember this" bypasses recurrence — handled by callers
/// that seed the cluster pre-satisfied.)
pub const PROMOTION_MIN_SESSIONS: i64 = 2;

/// Canonicalize a JSON string so semantically-identical matchers with different
/// key order hash to the same cluster. Falls back to the trimmed raw string when
/// the matcher is not valid JSON.
fn canonical_json(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => canonicalize_value(&v),
        Err(_) => raw.trim().to_string(),
    }
}

fn canonicalize_value(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{k}:{}", canonicalize_value(&map[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(canonicalize_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Stable dedup identity for a trigger: `kind|canonical(matcher)`.
pub fn canonical_trigger_key(trigger_kind: &str, trigger_matcher: &str) -> String {
    format!("{trigger_kind}|{}", canonical_json(trigger_matcher))
}

/// Deterministic cluster id for a canonical trigger key.
pub fn cluster_id_for_key(canonical_key: &str) -> String {
    let hex = format!("{:x}", Sha256::digest(canonical_key.as_bytes()));
    format!("clu-{}", &hex[..16])
}

fn find_cluster_by_key(conn: &Connection, canonical_key: &str) -> Result<Option<PriorCluster>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM prior_clusters WHERE canonical_trigger_key = ?1",
            params![canonical_key],
            |row| row.get(0),
        )
        .optional()?;
    match id {
        Some(id) => get_cluster(conn, &id),
        None => Ok(None),
    }
}

/// Cosine-similarity threshold above which two clusters' lesson embeddings are
/// treated as the same behavioral prior and merged. The distiller is
/// non-deterministic, so equivalent lessons land on different canonical trigger
/// keys; without semantic merge a recurring lesson would never accumulate the
/// ≥2 distinct sessions that gate promotion. 0.85 is high enough to keep
/// genuinely distinct lessons apart.
const PRIOR_MERGE_SIMILARITY: f32 = 0.85;

/// Encode an embedding as little-endian f32 bytes for BLOB storage.
fn encode_embedding(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Decode a little-endian f32 BLOB back into an embedding vector.
fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|chunk| f32::from_le_bytes(*chunk))
        .collect()
}

/// Persist a cluster's lesson embedding for later semantic merge. Kept off the
/// [`PriorCluster`] struct (and its round-trip) since it is only ever read by
/// [`find_cluster_by_embedding`], never surfaced.
fn set_cluster_embedding(conn: &Connection, cluster_id: &str, embedding: &[f32]) -> Result<()> {
    conn.execute(
        "UPDATE prior_clusters SET embedding = ?2 WHERE id = ?1",
        params![cluster_id, encode_embedding(embedding)],
    )?;
    Ok(())
}

/// The id of the nearest live cluster whose stored lesson embedding is within
/// `threshold` cosine of `embedding`, if any. Refuted/expired clusters are
/// excluded so a suppressed lesson is never resurrected by a near-duplicate.
/// Brute-force cosine over embedded clusters — their count is small (one per
/// distinct lesson), so a linear scan beats a vector index.
fn find_cluster_by_embedding(
    conn: &Connection,
    embedding: &[f32],
    threshold: f32,
) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT id, embedding FROM prior_clusters
         WHERE embedding IS NOT NULL AND state NOT IN ('refuted', 'expired')",
    )?;
    let rows = stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        Ok((id, blob))
    })?;

    let mut best: Option<(String, f32)> = None;
    for row in rows {
        let (id, blob) = row?;
        let sim = cosine_similarity(embedding, &decode_embedding(&blob));
        if sim >= threshold && best.as_ref().map(|(_, b)| sim > *b).unwrap_or(true) {
            best = Some((id, sim));
        }
    }
    Ok(best.map(|(id, _)| id))
}

/// Merge a candidate into its cluster (creating the cluster on first sight),
/// then recompute recurrence evidence. Exact-key-only; callers with an embedding
/// should use [`integrate_candidate_with_embedding`] to enable semantic merge.
pub fn integrate_candidate(conn: &Connection, cand: &PriorCandidate, now: i64) -> Result<String> {
    integrate_candidate_with_embedding(conn, cand, now, None)
}

/// Merge a candidate into its cluster, then recompute recurrence evidence from
/// the cluster's linked candidates. Returns the cluster id. The candidate row is
/// (re)persisted with its `cluster_id` set. Does NOT promote or write to
/// `memory_entries` — that is a separate, explicit step gated by
/// [`should_promote`].
///
/// Cluster resolution tries, in order: (1) exact canonical-trigger-key match;
/// (2) with `embedding` present, the nearest live cluster within
/// [`PRIOR_MERGE_SIMILARITY`] — the semantic merge that lets an equivalent
/// lesson phrased with a different trigger accumulate recurrence; (3) otherwise
/// a new cluster, seeded with the embedding so future candidates can merge into
/// it.
pub fn integrate_candidate_with_embedding(
    conn: &Connection,
    cand: &PriorCandidate,
    now: i64,
    embedding: Option<&[f32]>,
) -> Result<String> {
    let key = canonical_trigger_key(&cand.trigger_kind, &cand.trigger_matcher);

    let cluster_id = if let Some(existing) = find_cluster_by_key(conn, &key)? {
        existing.id
    } else if let Some(sim_id) = match embedding {
        Some(e) => find_cluster_by_embedding(conn, e, PRIOR_MERGE_SIMILARITY)?,
        None => None,
    } {
        // Semantically equivalent to an existing cluster under a different
        // trigger key — merge into it rather than fragmenting the evidence.
        sim_id
    } else {
        let id = cluster_id_for_key(&key);
        upsert_cluster(
            conn,
            &PriorCluster {
                id: id.clone(),
                canonical_trigger_key: key.clone(),
                trigger_kind: cand.trigger_kind.clone(),
                trigger_matcher: cand.trigger_matcher.clone(),
                lesson: cand.lesson.clone(),
                scope: cand.scope.clone(),
                evidence_count: 0,
                distinct_sessions: 0,
                injected_count: 0,
                confirmed_count: 0,
                refuted_count: 0,
                state: "candidate".into(),
                promoted_memory_id: None,
                created_at: now,
                last_seen_at: now,
                error_signature: None,
            },
        )?;
        if let Some(e) = embedding {
            set_cluster_embedding(conn, &id, e)?;
        }
        id
    };

    // Link the candidate to the resolved cluster.
    let mut linked = cand.clone();
    linked.cluster_id = Some(cluster_id.clone());
    upsert_candidate(conn, &linked)?;

    // Recompute recurrence from the cluster's candidates.
    let candidates = list_candidates_for_cluster(conn, &cluster_id)?;
    let evidence_count = candidates.len() as i64;
    let distinct_sessions = candidates
        .iter()
        .filter_map(|c| c.source_session.as_deref())
        .collect::<std::collections::HashSet<_>>()
        .len() as i64;

    let mut cluster = get_cluster(conn, &cluster_id)?.expect("cluster just upserted");
    cluster.evidence_count = evidence_count;
    cluster.distinct_sessions = distinct_sessions;
    cluster.last_seen_at = now;
    upsert_cluster(conn, &cluster)?;

    Ok(cluster_id)
}

/// Whether a cluster has recurred across enough distinct sessions to promote.
pub fn should_promote(c: &PriorCluster) -> bool {
    c.state == "candidate" && c.distinct_sessions >= PROMOTION_MIN_SESSIONS
}

/// Stable `memory_entries.id` for a promoted cluster (derived from the cluster
/// id so promotion is idempotent even across a crash between the two writes).
fn promoted_memory_id_for(cluster_id: &str) -> String {
    format!("prior-{}", cluster_id.trim_start_matches("clu-"))
}

/// Promote a cluster that has cleared the recurrence gate: write its lesson as a
/// `memory_entries` prior and flip the cluster to `promoted`, linked to that row.
///
/// Returns the promoted `memory_entries.id`, or `None` if the cluster is not
/// eligible (already promoted-but-different-state, or hasn't cleared the gate).
/// Idempotent: re-promoting an already-promoted cluster is a no-op that returns
/// the existing id, so a retried Stop-hook never double-writes.
///
/// The prior is written as [`SourceType::AutoExtracted`] on purpose: its
/// per-entry confidence therefore never clears the curated-warmup floor, so an
/// auto-mined prior never leaks into unconditional SessionStart injection — it
/// surfaces ONLY through the trigger-matched cluster path.
pub fn promote_cluster(conn: &Connection, cluster_id: &str, now: i64) -> Result<Option<String>> {
    let Some(mut cluster) = get_cluster(conn, cluster_id)? else {
        return Ok(None);
    };

    // Already promoted → idempotent no-op.
    if cluster.state == "promoted" {
        return Ok(cluster.promoted_memory_id.clone());
    }
    if !should_promote(&cluster) {
        return Ok(None);
    }

    let memory_id = promoted_memory_id_for(&cluster.id);

    // Evidence footer (if any candidate carried it) makes the stored prior
    // self-explanatory for a human reading `mdkb memory get`; the lesson alone
    // is what the trigger-matched injector surfaces.
    let evidence = list_candidates_for_cluster(conn, &cluster.id)?
        .into_iter()
        .find_map(|c| match (c.evidence_failure, c.evidence_fix) {
            (Some(f), Some(x)) if !f.trim().is_empty() && !x.trim().is_empty() => {
                Some(format!("\n\nFailure: {f}\nFix: {x}"))
            }
            _ => None,
        })
        .unwrap_or_default();

    let entry = MemoryEntry {
        id: memory_id.clone(),
        title: cluster.lesson.clone(),
        content: format!("{}{}", cluster.lesson, evidence),
        entry_type: EntryType::Prior,
        tags: vec!["auto-mined".to_string(), cluster.trigger_kind.clone()],
        status: EntryStatus::Active,
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
        source_type: SourceType::AutoExtracted,
        expires_at: Some(now + crate::store::memory::PRIOR_TTL_SECS),
        due_at: None,
    };
    add_entry(conn, &entry)?;

    cluster.state = "promoted".into();
    cluster.promoted_memory_id = Some(memory_id.clone());
    cluster.last_seen_at = now;
    upsert_cluster(conn, &cluster)?;

    Ok(Some(memory_id))
}

// ============================================================================
// Phase 7 — trigger-matched injection
// ============================================================================

/// The current context a promoted prior's trigger is matched against. Priors
/// surface only when their trigger matches here — never unconditionally.
#[derive(Debug)]
pub enum TriggerContext<'a> {
    /// A tool is about to run. `path` should be repo-relative for clean glob
    /// matching against a `src/generated/**`-style pattern.
    PreTool {
        tool: &'a str,
        path: Option<&'a str>,
        command: Option<&'a str>,
    },
    /// A tool has just run. Same fields as [`TriggerContext::PreTool`]: the
    /// difference is when the lesson is useful, not what identifies the call.
    /// "Do not edit generated files" belongs before the edit; "run the generator
    /// after touching the template" belongs after it.
    PostTool {
        tool: &'a str,
        path: Option<&'a str>,
        command: Option<&'a str>,
    },
    /// The user just submitted a prompt.
    Prompt { text: &'a str },
}

/// Whether a glob `pattern` matches `path`. Invalid globs never match (a bad
/// distilled pattern must not surface a prior everywhere).
fn glob_matches(pattern: &str, path: &str) -> bool {
    globset::Glob::new(pattern)
        .map(|g| g.compile_matcher().is_match(path))
        .unwrap_or(false)
}

/// Whether `pattern` identifies a tool call: the tool's name, a glob over its
/// path, or a substring of its command. Shared by `pre_tool` and `post_tool`,
/// which differ only in when they fire.
fn tool_call_matches(pattern: &str, tool: &str, path: Option<&str>, command: Option<&str>) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if pattern.eq_ignore_ascii_case(tool) {
        return true;
    }
    if let Some(p) = path
        && glob_matches(pattern, p)
    {
        return true;
    }
    if let Some(c) = command
        && c.contains(pattern)
    {
        return true;
    }
    false
}

/// Whether a promoted cluster's trigger matches the current context.
///
/// The matcher reads the distiller's `{"when","pattern"}` shape. For `pre_tool`
/// and `post_tool` the `pattern` matches the tool name, a path glob, or a
/// command substring; for `prompt` it (or `when`) is a case-insensitive
/// substring of the prompt. A kind is matched only in its own context, and a
/// kind with no arm here never matches anything — which is why
/// `VALID_TRIGGER_KINDS` may not contain one.
pub fn trigger_matches(kind: &str, matcher_json: &str, ctx: &TriggerContext) -> bool {
    let v: serde_json::Value = match serde_json::from_str(matcher_json) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let pattern = v
        .get("pattern")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
    let when = v.get("when").and_then(|x| x.as_str()).unwrap_or("").trim();

    match (kind, ctx) {
        (
            "pre_tool",
            TriggerContext::PreTool {
                tool,
                path,
                command,
            },
        )
        | (
            "post_tool",
            TriggerContext::PostTool {
                tool,
                path,
                command,
            },
        ) => tool_call_matches(pattern, tool, *path, *command),
        ("prompt", TriggerContext::Prompt { text }) => {
            let needle = if pattern.is_empty() { when } else { pattern };
            if needle.is_empty() {
                return false;
            }
            text.to_lowercase().contains(&needle.to_lowercase())
        }
        _ => false,
    }
}

/// All clusters currently in the `promoted` state.
pub fn list_promoted_clusters(conn: &Connection) -> Result<Vec<PriorCluster>> {
    let mut stmt = conn.prepare(
        "SELECT id, canonical_trigger_key, trigger_kind, trigger_matcher, lesson, scope,
                evidence_count, distinct_sessions, injected_count, confirmed_count, refuted_count,
                state, promoted_memory_id, created_at, last_seen_at, error_signature
         FROM prior_clusters WHERE state = 'promoted'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PriorCluster {
            id: row.get(0)?,
            canonical_trigger_key: row.get(1)?,
            trigger_kind: row.get(2)?,
            trigger_matcher: row.get(3)?,
            lesson: row.get(4)?,
            scope: row.get(5)?,
            evidence_count: row.get(6)?,
            distinct_sessions: row.get(7)?,
            injected_count: row.get(8)?,
            confirmed_count: row.get(9)?,
            refuted_count: row.get(10)?,
            state: row.get(11)?,
            promoted_memory_id: row.get(12)?,
            created_at: row.get(13)?,
            last_seen_at: row.get(14)?,
            error_signature: row.get(15)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Promoted priors that are both injectable (score gate) and trigger-matched to
/// `ctx`, highest injection score first, capped at `max`. This is the single
/// entry point the PreToolUse / UserPromptSubmit hooks use to surface priors.
pub fn match_injectable(
    conn: &Connection,
    ctx: &TriggerContext,
    now: i64,
    max: usize,
) -> Result<Vec<PriorCluster>> {
    if max == 0 {
        return Ok(Vec::new());
    }
    let mut matched: Vec<PriorCluster> = list_promoted_clusters(conn)?
        .into_iter()
        .filter(|c| {
            is_injectable(c, now) && trigger_matches(&c.trigger_kind, &c.trigger_matcher, ctx)
        })
        .collect();
    matched.sort_by(|a, b| {
        cluster_injection_score(b, now)
            .partial_cmp(&cluster_injection_score(a, now))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    matched.truncate(max);
    Ok(matched)
}

// ============================================================================
// Phase 5 → 6 — persist a distilled prior and promote on recurrence
// ============================================================================

/// Deterministic candidate id for a `(trigger, session)` pair. Making it stable
/// means re-running the Stop hook for the same session+trigger UPSERTs the same
/// row instead of inflating a cluster's evidence — one session contributes at
/// most one candidate, so `distinct_sessions` stays honest.
fn candidate_id_for(canonical_key: &str, session: &str) -> String {
    let hex = format!(
        "{:x}",
        Sha256::digest(format!("{canonical_key}|{session}").as_bytes())
    );
    format!("cand-{}", &hex[..16])
}

/// Persist a model-distilled prior as a candidate, cluster it by trigger, and
/// promote the cluster if it has now recurred across enough distinct sessions.
///
/// This is the whole post-distillation pipeline, isolated from the model call so
/// it is unit-testable with a fixture `DistilledPrior` (no live model). Returns
/// the promoted `memory_entries.id` when this observation tipped the cluster over
/// the recurrence gate (or it was already promoted), else `None`.
///
/// `lesson_embedding` (the distilled lesson embedded by the caller) enables
/// semantic cluster-merge: two sessions that teach the same lesson but whose
/// distiller emitted different triggers still converge on one cluster and can
/// promote. Pass `None` to fall back to exact-trigger-key clustering only.
///
/// `error_signature` is the tool-error signature of the episode that produced
/// this observation. It is what later sessions are checked against to decide
/// whether an injected prior worked, so it is recorded on the cluster the first
/// time one is seen and never overwritten afterwards.
pub fn integrate_distilled(
    conn: &Connection,
    d: &DistilledPrior,
    session: &str,
    now: i64,
    lesson_embedding: Option<&[f32]>,
    error_signature: Option<&str>,
) -> Result<Option<String>> {
    let key = canonical_trigger_key(&d.trigger_kind, &d.trigger_matcher);
    let cand = PriorCandidate {
        id: candidate_id_for(&key, session),
        cluster_id: None,
        state: "candidate".into(),
        trigger_kind: d.trigger_kind.clone(),
        trigger_matcher: d.trigger_matcher.clone(),
        lesson: d.lesson.clone(),
        scope: d.scope.clone(),
        evidence_failure: Some(d.evidence_failure.clone()),
        evidence_fix: Some(d.evidence_fix.clone()),
        source_session: Some(session.to_string()),
        created_at: now,
    };
    let cluster_id = integrate_candidate_with_embedding(conn, &cand, now, lesson_embedding)?;
    if let Some(sig) = error_signature.map(str::trim).filter(|s| !s.is_empty()) {
        set_cluster_error_signature(conn, &cluster_id, sig)?;
    }
    promote_cluster(conn, &cluster_id, now)
}

/// Record the failure a cluster exists to prevent, the first time one is seen.
///
/// Never overwrites: a cluster accumulates observations from several sessions
/// and the later ones are the *same* lesson by construction, so re-writing the
/// signature would just make it track whichever session ran last.
fn set_cluster_error_signature(conn: &Connection, cluster_id: &str, signature: &str) -> Result<()> {
    conn.execute(
        "UPDATE prior_clusters SET error_signature = ?2
         WHERE id = ?1 AND (error_signature IS NULL OR TRIM(error_signature) = '')",
        params![cluster_id, signature],
    )?;
    Ok(())
}

/// Record that a cluster's prior was injected into `session`.
///
/// Two writes, and both matter: the cluster counter is lifecycle telemetry, and
/// the `prior_injections` row is the open question the next Stop hook answers —
/// did this lesson hold? The row is inserted once per (cluster, session); a
/// second injection in the same session keeps the first `injected_at`, because
/// an error recurring after injection #1 refutes the prior regardless of how
/// many times it was repeated afterwards.
///
/// It deliberately does **not** touch `last_seen_at`. That field feeds the
/// freshness term of [`cluster_injection_score`], so a prior that stamped it on
/// every injection held its own score up without any new evidence — injected
/// because it was fresh, fresh because it was injected. Only mining moves it,
/// and mining moves it because it saw the pattern happen again.
pub fn record_injection(
    conn: &Connection,
    cluster_id: &str,
    session: &str,
    now: i64,
) -> Result<()> {
    conn.execute(
        "UPDATE prior_clusters SET injected_count = injected_count + 1 WHERE id = ?1",
        params![cluster_id],
    )?;
    conn.execute(
        "INSERT INTO prior_injections (cluster_id, session, injected_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(cluster_id, session) DO NOTHING",
        params![cluster_id, session, now],
    )?;
    Ok(())
}

// ============================================================================
// The belief loop — an injected prior is confirmed or refuted by what followed
// ============================================================================

/// Tokens of an error signature, for comparing one failure with another:
/// lowercased alphanumeric runs, with pure numbers dropped. Line numbers, byte
/// offsets and timing differ between two occurrences of the same failure, so
/// keeping them would make every recurrence look like a new error.
fn signature_tokens(signature: &str) -> std::collections::HashSet<String> {
    signature
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1 && !t.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_lowercase)
        .collect()
}

/// Minimum share of a cluster's signature tokens that must reappear in an
/// observed error for the two to count as the same failure. Containment, not
/// Jaccard: the observed error is often the same failure wrapped in extra
/// context, and that extra context must not dilute the match.
const SIGNATURE_MATCH_RATIO: f64 = 0.6;

/// Whether `observed` is a recurrence of `known` — the same failure, not
/// necessarily the same bytes.
pub fn signatures_match(known: &str, observed: &str) -> bool {
    let known_tokens = signature_tokens(known);
    if known_tokens.is_empty() {
        return false;
    }
    let observed_tokens = signature_tokens(observed);
    let shared = known_tokens
        .iter()
        .filter(|t| observed_tokens.contains(*t))
        .count();
    shared as f64 / known_tokens.len() as f64 >= SIGNATURE_MATCH_RATIO
}

/// One error observed in a session, as the settling pass needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedError {
    /// The error output signature.
    pub signature: String,
    /// When it happened, when the transcript said so. `None` means the record
    /// carried no timestamp, and the error is then treated as in-scope: a
    /// recurrence we cannot place is still a recurrence.
    pub at: Option<i64>,
}

/// What settling one session's injections did.
///
/// Three outcomes, and only the first is evidence. Splitting the other two is
/// the point: a settlement that moves no counter still has to be recorded, or
/// the next Stop hook reopens the same question forever.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SettleReport {
    /// The failure the prior warns about happened after it was shown.
    pub refuted: Vec<String>,
    /// The cluster has no error signature, so nothing could have been observed.
    pub unobservable: Vec<String>,
    /// Observable, and the failure did not come back — which is not the same as
    /// the prior having worked. See [`settle_injections`].
    pub unrefuted: Vec<String>,
}

impl SettleReport {
    /// Nothing was open to settle.
    pub fn is_empty(&self) -> bool {
        self.refuted.is_empty() && self.unobservable.is_empty() && self.unrefuted.is_empty()
    }
}

/// Close the loop for every prior injected into `session` and not yet settled.
///
/// Three outcomes, of which exactly one is evidence:
///
/// * **refuted** — the prior's own error signature recurred after it was
///   injected. It was surfaced, and the failure it warns about happened anyway.
/// * **unobservable** — the cluster has no error signature. A prior mined from
///   a user correction has nothing that can recur, so no session can be
///   evidence about it. 61 of the 66 clusters in the live store are here.
/// * **unrefuted** — the signature existed and did not recur.
///
/// **A session never confirms a prior.** Only a person does, through
/// [`apply_belief_from_memory`]. The tempting automated rule — "the operation
/// ran and the failure did not follow, so the lesson worked" — is not weak
/// evidence but inverted evidence, for a reason the hook shape makes plain: the
/// PreToolUse hook returns `additionalContext` and never a deny, so the command
/// is already committed when the prior is shown. An injection therefore means
/// the warned-about operation ran *with the lesson not applied to it*. The
/// heeded case — the agent rewrites the command, the matcher stops firing —
/// produces no injection at all and so leaves no row to settle. An automated
/// `confirmed` would be fed exclusively by the un-heeded case, and would rise
/// fastest on the priors whose failure is least deterministic.
///
/// The matcher makes that concrete: [`tool_call_matches`] accepts a bare tool
/// name, which the distiller prompt offers as a pattern form, so a cluster with
/// `pattern: "Bash"` fires on every shell call. Under an automated positive
/// rule it would bank a confirmation in every session that ran any command
/// without reproducing the signature.
///
/// Before this split, a missing signature mapped through `unwrap_or(false)` to
/// "did not recur" and therefore to `confirmed`: every injection of a
/// correction-mined prior was a free confirmation, and the belief term rose on
/// silence. A prior that is never re-mined, never refuted and never confirmed by
/// a person now decays out of injection range, which is the intended TTL.
///
/// Called from the Stop hook, before the mining gate: most sessions teach
/// nothing new and are gated out, but they still answer the question about the
/// priors they were shown. Each (cluster, session) settles exactly once — the
/// `prior_injections` primary key plus the `outcome IS NULL` filter — so a
/// retried Stop hook cannot inflate the counter.
pub fn settle_injections(
    conn: &Connection,
    session: &str,
    now: i64,
    errors: &[ObservedError],
) -> Result<SettleReport> {
    let mut stmt = conn.prepare(
        "SELECT i.cluster_id, i.injected_at, c.error_signature
         FROM prior_injections i JOIN prior_clusters c ON c.id = i.cluster_id
         WHERE i.session = ?1 AND i.outcome IS NULL",
    )?;
    let open: Vec<(String, i64, Option<String>)> = stmt
        .query_map(params![session], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    let mut report = SettleReport::default();
    for (cluster_id, injected_at, known_signature) in open {
        // A blank signature is the same absence as a missing one: it matches
        // nothing, so nothing about it can be observed.
        let signature = known_signature
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let outcome = match signature {
            None => "unobservable",
            Some(known) => {
                let recurred = errors
                    .iter()
                    .filter(|e| e.at.is_none_or(|at| at >= injected_at))
                    .any(|e| signatures_match(known, &e.signature));
                if recurred { "refuted" } else { "unrefuted" }
            }
        };

        // Guarded by the same `outcome IS NULL` predicate as the SELECT, so two
        // Stop hooks racing on one session settle it once between them.
        let claimed = conn.execute(
            "UPDATE prior_injections SET outcome = ?3, settled_at = ?4
             WHERE cluster_id = ?1 AND session = ?2 AND outcome IS NULL",
            params![cluster_id, session, outcome, now],
        )?;
        if claimed == 0 {
            continue;
        }
        // Only a refutation moves a counter. The other two are recorded on the
        // injection row and nowhere else. `confirmed_count` still exists and is
        // still read by the belief term — only a person writes it now.
        if outcome == "refuted" {
            conn.execute(
                "UPDATE prior_clusters SET refuted_count = refuted_count + 1 WHERE id = ?1",
                params![cluster_id],
            )?;
        }
        match outcome {
            "refuted" => report.refuted.push(cluster_id),
            "unobservable" => report.unobservable.push(cluster_id),
            _ => report.unrefuted.push(cluster_id),
        }
    }
    Ok(report)
}

/// Apply a human's `mdkb memory confirm/refute` verdict on a promoted prior's
/// projection to the cluster behind it. Returns the cluster id when one owns
/// `memory_id`, `None` when the entry is an ordinary memory.
///
/// Without this the two halves disagreed: confirming the projection moved
/// `memory_entries.confirmations`, which the injection score does not read,
/// while `cluster_injection_score` kept dividing by a belief that no one could
/// move. A person who says the lesson works is the strongest signal there is.
pub fn apply_belief_from_memory(
    conn: &Connection,
    memory_id: &str,
    delta: i32,
) -> Result<Option<String>> {
    if delta == 0 {
        return Ok(None);
    }
    let cluster_id: Option<String> = conn
        .query_row(
            "SELECT id FROM prior_clusters WHERE promoted_memory_id = ?1",
            params![memory_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(cluster_id) = cluster_id else {
        return Ok(None);
    };
    let column = if delta > 0 {
        "confirmed_count"
    } else {
        "refuted_count"
    };
    conn.execute(
        &format!("UPDATE prior_clusters SET {column} = {column} + 1 WHERE id = ?1"),
        params![cluster_id],
    )?;
    Ok(Some(cluster_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::init_schema;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn sample_cluster() -> PriorCluster {
        PriorCluster {
            id: "clu-abc123".into(),
            canonical_trigger_key: "pre_tool|edit|src/generated/**".into(),
            trigger_kind: "pre_tool".into(),
            trigger_matcher: r#"{"tool":"Edit","path":"src/generated/**"}"#.into(),
            lesson: "Do not edit generated files; change the generator and regenerate.".into(),
            scope: r#"{"repo":"current","languages":["rust"],"paths":["src/generated/**"]}"#.into(),
            evidence_count: 2,
            distinct_sessions: 2,
            injected_count: 0,
            confirmed_count: 0,
            refuted_count: 0,
            state: "candidate".into(),
            promoted_memory_id: None,
            created_at: 100,
            last_seen_at: 200,
            error_signature: None,
        }
    }

    fn sample_candidate(cluster_id: Option<&str>) -> PriorCandidate {
        PriorCandidate {
            id: "cand-1".into(),
            cluster_id: cluster_id.map(String::from),
            state: "candidate".into(),
            trigger_kind: "pre_tool".into(),
            trigger_matcher: r#"{"tool":"Edit","path":"src/generated/**"}"#.into(),
            lesson: "Do not edit generated files; change the generator and regenerate.".into(),
            scope: r#"{"repo":"current"}"#.into(),
            evidence_failure: Some("Direct edit was overwritten by regeneration.".into()),
            evidence_fix: Some("Edited the generator template instead.".into()),
            source_session: Some("sess-xyz".into()),
            created_at: 150,
        }
    }

    #[test]
    fn cluster_round_trips() {
        let conn = conn();
        let c = sample_cluster();
        upsert_cluster(&conn, &c).unwrap();
        assert_eq!(get_cluster(&conn, &c.id).unwrap().as_ref(), Some(&c));
        assert_eq!(get_cluster(&conn, "missing").unwrap(), None);
    }

    #[test]
    fn cluster_upsert_replaces() {
        let conn = conn();
        let mut c = sample_cluster();
        upsert_cluster(&conn, &c).unwrap();
        // Recurrence grows and the cluster advances state on re-observation.
        c.state = "promoted".into();
        c.evidence_count = 5;
        c.distinct_sessions = 3;
        upsert_cluster(&conn, &c).unwrap();
        let got = get_cluster(&conn, &c.id).unwrap().unwrap();
        assert_eq!(got.state, "promoted");
        assert_eq!(got.evidence_count, 5);
        assert_eq!(got.distinct_sessions, 3);
    }

    #[test]
    fn candidate_round_trips_and_lists_by_cluster() {
        let conn = conn();
        let cluster = sample_cluster();
        upsert_cluster(&conn, &cluster).unwrap();
        let cand = sample_candidate(Some(&cluster.id));
        upsert_candidate(&conn, &cand).unwrap();

        assert_eq!(
            get_candidate(&conn, &cand.id).unwrap().as_ref(),
            Some(&cand)
        );
        let listed = list_candidates_for_cluster(&conn, &cluster.id).unwrap();
        assert_eq!(listed, vec![cand]);
    }

    #[test]
    fn promoted_cluster_with_two_sessions_is_injectable() {
        let mut c = sample_cluster(); // distinct_sessions=2, no refutations
        c.state = "promoted".into();
        // "now" == last_seen_at → maximally fresh.
        assert!(is_injectable(&c, c.last_seen_at));
    }

    #[test]
    fn candidate_is_never_injectable() {
        let c = sample_cluster(); // state == "candidate"
        assert_eq!(c.state, "candidate");
        assert!(!is_injectable(&c, c.last_seen_at));
    }

    #[test]
    fn refutations_push_a_promoted_prior_below_threshold() {
        let mut c = sample_cluster();
        c.state = "promoted".into();
        c.refuted_count = 3;
        assert!(!is_injectable(&c, c.last_seen_at));
    }

    #[test]
    fn staleness_decays_a_promoted_prior_out_of_injection() {
        let mut c = sample_cluster();
        c.state = "promoted".into();
        let now = c.last_seen_at + 400 * 86_400; // ~400 days later
        assert!(is_injectable(&c, c.last_seen_at), "fresh: injectable");
        assert!(!is_injectable(&c, now), "stale: no longer injectable");
    }

    #[test]
    fn deleting_cluster_nulls_candidate_fk() {
        // ON DELETE SET NULL keeps the observed episode as evidence even after
        // its cluster is pruned.
        let conn = conn();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let cluster = sample_cluster();
        upsert_cluster(&conn, &cluster).unwrap();
        upsert_candidate(&conn, &sample_candidate(Some(&cluster.id))).unwrap();

        conn.execute(
            "DELETE FROM prior_clusters WHERE id = ?1",
            params![cluster.id],
        )
        .unwrap();

        let cand = get_candidate(&conn, "cand-1").unwrap().unwrap();
        assert_eq!(cand.cluster_id, None);
    }

    fn cand_with(id: &str, session: &str, matcher: &str) -> PriorCandidate {
        PriorCandidate {
            id: id.into(),
            cluster_id: None,
            state: "candidate".into(),
            trigger_kind: "pre_tool".into(),
            trigger_matcher: matcher.into(),
            lesson: "Do not edit generated files; change the generator.".into(),
            scope: r#"{"repo":"current"}"#.into(),
            evidence_failure: None,
            evidence_fix: None,
            source_session: Some(session.into()),
            created_at: 100,
        }
    }

    const MATCHER: &str = r#"{"tool":"Edit","path":"src/generated/**"}"#;

    #[test]
    fn recurrence_across_two_sessions_promotes() {
        let conn = conn();
        let id1 = integrate_candidate(&conn, &cand_with("c1", "s1", MATCHER), 1000).unwrap();
        let id2 = integrate_candidate(&conn, &cand_with("c2", "s2", MATCHER), 2000).unwrap();
        assert_eq!(id1, id2, "same trigger → same cluster");

        let cluster = get_cluster(&conn, &id1).unwrap().unwrap();
        assert_eq!(cluster.evidence_count, 2);
        assert_eq!(cluster.distinct_sessions, 2);
        assert_eq!(cluster.last_seen_at, 2000);
        assert!(should_promote(&cluster));
    }

    #[test]
    fn same_session_twice_does_not_promote() {
        let conn = conn();
        integrate_candidate(&conn, &cand_with("c1", "s1", MATCHER), 1000).unwrap();
        let id = integrate_candidate(&conn, &cand_with("c2", "s1", MATCHER), 1500).unwrap();
        let cluster = get_cluster(&conn, &id).unwrap().unwrap();
        assert_eq!(cluster.evidence_count, 2);
        assert_eq!(
            cluster.distinct_sessions, 1,
            "one session cannot self-promote"
        );
        assert!(!should_promote(&cluster));
    }

    #[test]
    fn different_triggers_form_different_clusters() {
        let conn = conn();
        let a = integrate_candidate(&conn, &cand_with("c1", "s1", MATCHER), 1000).unwrap();
        let b = integrate_candidate(
            &conn,
            &cand_with("c2", "s1", r#"{"tool":"Bash","command":"cargo test"}"#),
            1000,
        )
        .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn canonical_key_ignores_json_key_order() {
        let a = canonical_trigger_key("pre_tool", r#"{"tool":"Edit","path":"x"}"#);
        let b = canonical_trigger_key("pre_tool", r#"{"path":"x","tool":"Edit"}"#);
        assert_eq!(a, b);
        assert_eq!(cluster_id_for_key(&a), cluster_id_for_key(&b));
    }

    #[test]
    fn promoting_a_gated_cluster_writes_a_prior_memory_entry() {
        use crate::store::memory::{EntryType, SourceType, get_entry_without_tracking};
        let conn = conn();
        // Two distinct sessions → clears the recurrence gate.
        integrate_candidate(&conn, &cand_with("c1", "s1", MATCHER), 1000).unwrap();
        let cluster_id = integrate_candidate(&conn, &cand_with("c2", "s2", MATCHER), 2000).unwrap();

        let mem_id = promote_cluster(&conn, &cluster_id, 3000)
            .unwrap()
            .expect("a gated cluster promotes");

        let entry = get_entry_without_tracking(&conn, &mem_id).unwrap().unwrap();
        assert_eq!(entry.entry_type, EntryType::Prior);
        // AutoExtracted authority keeps it below the curated-warmup floor.
        assert_eq!(entry.source_type, SourceType::AutoExtracted);
        assert!(entry.content.contains("Do not edit generated files"));
        assert!(entry.tags.contains(&"auto-mined".to_string()));
        // A mined prior ages out like a written one: what it observed stops
        // being true when the code it was mined from goes away, and nothing
        // else retires it.
        assert_eq!(
            entry.expires_at,
            Some(3000 + crate::store::memory::PRIOR_TTL_SECS),
            "a mined prior carries the same TTL as a written one"
        );

        let cluster = get_cluster(&conn, &cluster_id).unwrap().unwrap();
        assert_eq!(cluster.state, "promoted");
        assert_eq!(cluster.promoted_memory_id.as_deref(), Some(mem_id.as_str()));
    }

    #[test]
    fn promotion_is_idempotent() {
        let conn = conn();
        integrate_candidate(&conn, &cand_with("c1", "s1", MATCHER), 1000).unwrap();
        let cluster_id = integrate_candidate(&conn, &cand_with("c2", "s2", MATCHER), 2000).unwrap();

        let first = promote_cluster(&conn, &cluster_id, 3000).unwrap().unwrap();
        // A retried Stop-hook must not double-write or error on the UNIQUE id.
        let second = promote_cluster(&conn, &cluster_id, 4000).unwrap().unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn one_session_cluster_does_not_promote() {
        let conn = conn();
        integrate_candidate(&conn, &cand_with("c1", "s1", MATCHER), 1000).unwrap();
        let cluster_id = integrate_candidate(&conn, &cand_with("c2", "s1", MATCHER), 1500).unwrap();
        assert_eq!(
            promote_cluster(&conn, &cluster_id, 3000).unwrap(),
            None,
            "one session must not promote"
        );
        let cluster = get_cluster(&conn, &cluster_id).unwrap().unwrap();
        assert_eq!(cluster.state, "candidate");
    }

    #[test]
    fn promoting_a_missing_cluster_is_none() {
        let conn = conn();
        assert_eq!(promote_cluster(&conn, "clu-nope", 3000).unwrap(), None);
    }

    // --- Phase 7: trigger matching + injection selection ---

    #[test]
    fn pre_tool_trigger_matches_path_glob_tool_and_command() {
        let matcher = r#"{"when":"editing generated code","pattern":"src/generated/**"}"#;
        let ctx = TriggerContext::PreTool {
            tool: "Edit",
            path: Some("src/generated/api.rs"),
            command: None,
        };
        assert!(trigger_matches("pre_tool", matcher, &ctx));

        // Non-matching path does not fire.
        let ctx_miss = TriggerContext::PreTool {
            tool: "Edit",
            path: Some("src/handwritten.rs"),
            command: None,
        };
        assert!(!trigger_matches("pre_tool", matcher, &ctx_miss));

        // Tool-name pattern.
        let tool_matcher = r#"{"pattern":"Bash"}"#;
        assert!(trigger_matches(
            "pre_tool",
            tool_matcher,
            &TriggerContext::PreTool {
                tool: "Bash",
                path: None,
                command: Some("ls")
            }
        ));

        // Command-substring pattern.
        let cmd_matcher = r#"{"pattern":"cargo test"}"#;
        assert!(trigger_matches(
            "pre_tool",
            cmd_matcher,
            &TriggerContext::PreTool {
                tool: "Bash",
                path: None,
                command: Some("cargo test --lib")
            }
        ));
    }

    /// The distiller's accepted kinds and the kinds the matcher can act on MUST
    /// be the same set.
    ///
    /// They were not: the validator accepted `stop` and `repo`, `trigger_matches`
    /// handled neither, and every cluster mined under those kinds — 32 of 58,
    /// including both that had been promoted — sat in the store unable to fire.
    /// A prior nothing can ever inject is worse than no prior: it consumes a
    /// promotion, reports as working, and teaches nobody.
    #[test]
    fn every_accepted_trigger_kind_has_an_injection_point() {
        // One context per kind, with a matcher that must fire in it.
        let matchable: &[(&str, &str, TriggerContext)] = &[
            (
                "prompt",
                r#"{"pattern":"ripgrep"}"#,
                TriggerContext::Prompt {
                    text: "use ripgrep here",
                },
            ),
            (
                "pre_tool",
                r#"{"pattern":"src/generated/**"}"#,
                TriggerContext::PreTool {
                    tool: "Edit",
                    path: Some("src/generated/api.rs"),
                    command: None,
                },
            ),
            (
                "post_tool",
                r#"{"pattern":"src/generated/**"}"#,
                TriggerContext::PostTool {
                    tool: "Edit",
                    path: Some("src/generated/api.rs"),
                    command: None,
                },
            ),
        ];

        for (kind, matcher, ctx) in matchable {
            assert!(
                trigger_matches(kind, matcher, ctx),
                "{kind} is accepted by the distiller but cannot be matched"
            );
        }

        let injectable: Vec<&str> = matchable.iter().map(|(k, _, _)| *k).collect();
        let mut accepted = crate::domain::prior_distill::VALID_TRIGGER_KINDS.to_vec();
        let mut expected = injectable.clone();
        accepted.sort_unstable();
        expected.sort_unstable();
        assert_eq!(
            accepted, expected,
            "VALID_TRIGGER_KINDS must equal the kinds trigger_matches handles: \
             accepting one more mines priors that can never fire, accepting one \
             fewer throws away a lesson the matcher could have used"
        );
    }

    #[test]
    fn prompt_trigger_matches_case_insensitive_substring() {
        let matcher = r#"{"when":"asks about ripgrep","pattern":"ripgrep"}"#;
        assert!(trigger_matches(
            "prompt",
            matcher,
            &TriggerContext::Prompt {
                text: "Should I use RipGrep or grep here?"
            }
        ));
        assert!(!trigger_matches(
            "prompt",
            matcher,
            &TriggerContext::Prompt {
                text: "unrelated question"
            }
        ));
    }

    #[test]
    fn wrong_kind_or_invalid_matcher_never_matches() {
        let ctx = TriggerContext::PreTool {
            tool: "Edit",
            path: Some("src/generated/x.rs"),
            command: None,
        };
        // prompt-kind trigger against a pre_tool context: no match.
        assert!(!trigger_matches(
            "prompt",
            r#"{"pattern":"src/generated/**"}"#,
            &ctx
        ));
        // Unparseable matcher: no match (must not surface a prior everywhere).
        assert!(!trigger_matches("pre_tool", "not json", &ctx));
        // Invalid glob: no match.
        assert!(!trigger_matches("pre_tool", r#"{"pattern":"["}"#, &ctx));
    }

    fn promoted_cluster(id: &str, matcher: &str, sessions: i64) -> PriorCluster {
        PriorCluster {
            id: id.into(),
            canonical_trigger_key: format!("pre_tool|{matcher}"),
            trigger_kind: "pre_tool".into(),
            trigger_matcher: matcher.into(),
            lesson: "Do not edit generated files; edit the generator.".into(),
            scope: r#"{"repo":"current"}"#.into(),
            evidence_count: sessions,
            distinct_sessions: sessions,
            injected_count: 0,
            confirmed_count: 0,
            refuted_count: 0,
            state: "promoted".into(),
            // No FK row needed: the injection path reads the cluster's lesson
            // directly, not the promoted memory entry.
            promoted_memory_id: None,
            created_at: 100,
            last_seen_at: 200,
            error_signature: None,
        }
    }

    #[test]
    fn match_injectable_returns_only_trigger_matched_promoted_priors() {
        let conn = conn();
        let now = 200;
        // Promoted + matching trigger.
        upsert_cluster(
            &conn,
            &promoted_cluster("clu-a", r#"{"pattern":"src/generated/**"}"#, 2),
        )
        .unwrap();
        // Promoted but non-matching trigger.
        upsert_cluster(
            &conn,
            &promoted_cluster("clu-b", r#"{"pattern":"docs/**"}"#, 2),
        )
        .unwrap();
        // Matching trigger but only a candidate (never injectable).
        let mut cand = promoted_cluster("clu-c", r#"{"pattern":"src/generated/**"}"#, 2);
        cand.state = "candidate".into();
        upsert_cluster(&conn, &cand).unwrap();

        let ctx = TriggerContext::PreTool {
            tool: "Edit",
            path: Some("src/generated/api.rs"),
            command: None,
        };
        let hits = match_injectable(&conn, &ctx, now, 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "clu-a");
    }

    #[test]
    fn match_injectable_respects_the_cap_and_ranks_by_score() {
        let conn = conn();
        let now = 200;
        // Two matching promoted priors; the one seen in more sessions scores higher.
        upsert_cluster(
            &conn,
            &promoted_cluster("clu-lo", r#"{"pattern":"src/generated/**"}"#, 2),
        )
        .unwrap();
        upsert_cluster(
            &conn,
            &promoted_cluster("clu-hi", r#"{"pattern":"src/**"}"#, 8),
        )
        .unwrap();

        let ctx = TriggerContext::PreTool {
            tool: "Edit",
            path: Some("src/generated/api.rs"),
            command: None,
        };
        let hits = match_injectable(&conn, &ctx, now, 1).unwrap();
        assert_eq!(hits.len(), 1, "cap honored");
        assert_eq!(hits[0].id, "clu-hi", "higher-recurrence prior ranks first");
    }

    fn sample_distilled() -> DistilledPrior {
        DistilledPrior {
            trigger_kind: "pre_tool".into(),
            trigger_matcher: r#"{"pattern":"src/generated/**"}"#.into(),
            lesson: "Do not edit generated files; edit the generator instead.".into(),
            scope: r#"{"repo":"current","languages":["rust"]}"#.into(),
            evidence_failure: "Direct edit was overwritten by regeneration.".into(),
            evidence_fix: "Edited the generator template instead.".into(),
            ttl_days: Some(30),
        }
    }

    #[test]
    fn integrate_distilled_promotes_only_after_two_sessions() {
        let conn = conn();
        let d = sample_distilled();

        // First session: candidate stored, not yet promoted.
        assert_eq!(
            integrate_distilled(&conn, &d, "sess-1", 1000, None, None).unwrap(),
            None,
            "one session must not promote"
        );
        // Second, distinct session: recurrence gate cleared → promoted.
        let mem = integrate_distilled(&conn, &d, "sess-2", 2000, None, None)
            .unwrap()
            .expect("two distinct sessions promote");

        use crate::store::memory::{EntryType, get_entry_without_tracking};
        let entry = get_entry_without_tracking(&conn, &mem).unwrap().unwrap();
        assert_eq!(entry.entry_type, EntryType::Prior);
        assert!(entry.content.contains("Do not edit generated files"));
    }

    #[test]
    fn integrate_distilled_same_session_twice_does_not_promote() {
        let conn = conn();
        let d = sample_distilled();
        assert_eq!(
            integrate_distilled(&conn, &d, "sess-1", 1000, None, None).unwrap(),
            None
        );
        // Same session re-emitting the same trigger: deterministic candidate id
        // upserts, so distinct_sessions stays 1 — no self-promotion.
        assert_eq!(
            integrate_distilled(&conn, &d, "sess-1", 1500, None, None).unwrap(),
            None,
            "one session cannot promote itself by repeating"
        );
        let key = canonical_trigger_key(&d.trigger_kind, &d.trigger_matcher);
        let cluster = get_cluster(&conn, &cluster_id_for_key(&key))
            .unwrap()
            .unwrap();
        assert_eq!(cluster.distinct_sessions, 1);
        assert_eq!(cluster.evidence_count, 1, "same candidate id upserted once");
    }

    fn cluster_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM prior_clusters", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn integrate_distilled_merges_semantically_similar_across_sessions() {
        let conn = conn();

        // Session 1: the distiller emits a pre_tool trigger for the lesson.
        let d1 = sample_distilled(); // trigger {"pattern":"src/generated/**"}
        let emb1 = [1.0_f32, 0.0, 0.0];
        assert_eq!(
            integrate_distilled(&conn, &d1, "sess-1", 1000, Some(&emb1), None).unwrap(),
            None,
            "one session must not promote"
        );

        // Session 2: SAME lesson, but the non-deterministic distiller emits a
        // DIFFERENT trigger (kind + matcher) → different canonical key. The
        // near-identical lesson embedding must merge it into cluster 1.
        let d2 = DistilledPrior {
            trigger_kind: "post_tool".into(),
            trigger_matcher: r#"{"pattern":"src/generated/*","when":"after editing generated"}"#
                .into(),
            lesson: "Regenerate generated sources; never hand-edit auto-generated files.".into(),
            scope: r#"{"repo":"current"}"#.into(),
            evidence_failure: "Manual edit overwritten on rebuild.".into(),
            evidence_fix: "Ran the generator instead.".into(),
            ttl_days: Some(30),
        };
        let emb2 = [0.96_f32, 0.28, 0.0]; // cosine ~0.96 with emb1 → merges
        let mem = integrate_distilled(&conn, &d2, "sess-2", 2000, Some(&emb2), None)
            .unwrap()
            .expect("semantically equivalent second session promotes the merged cluster");

        assert_eq!(
            cluster_count(&conn),
            1,
            "equivalent lessons form ONE cluster"
        );
        use crate::store::memory::{EntryType, get_entry_without_tracking};
        let entry = get_entry_without_tracking(&conn, &mem).unwrap().unwrap();
        assert_eq!(entry.entry_type, EntryType::Prior);
    }

    #[test]
    fn integrate_distilled_keeps_dissimilar_lessons_in_separate_clusters() {
        let conn = conn();
        let d1 = sample_distilled();
        let emb1 = [1.0_f32, 0.0, 0.0];
        integrate_distilled(&conn, &d1, "sess-1", 1000, Some(&emb1), None).unwrap();

        // A different trigger AND an orthogonal embedding: must NOT merge.
        let d2 = DistilledPrior {
            trigger_kind: "prompt".into(),
            trigger_matcher: r#"{"pattern":"deploy"}"#.into(),
            lesson: "Always run the smoke test before deploying to production.".into(),
            scope: r#"{"repo":"current"}"#.into(),
            evidence_failure: "Broken deploy reached prod.".into(),
            evidence_fix: "Added a pre-deploy smoke test.".into(),
            ttl_days: Some(30),
        };
        let emb2 = [0.0_f32, 1.0, 0.0]; // cosine 0.0 with emb1 → distinct
        assert_eq!(
            integrate_distilled(&conn, &d2, "sess-2", 2000, Some(&emb2), None).unwrap(),
            None,
            "an unrelated lesson from a second session must not promote either cluster"
        );
        assert_eq!(cluster_count(&conn), 2, "distinct lessons stay separate");
    }

    #[test]
    fn record_injection_increments_counter() {
        let conn = conn();
        upsert_cluster(
            &conn,
            &promoted_cluster("clu-a", r#"{"pattern":"src/**"}"#, 2),
        )
        .unwrap();
        let seen_before = get_cluster(&conn, "clu-a").unwrap().unwrap().last_seen_at;
        record_injection(&conn, "clu-a", "sess-1", 999).unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();
        let c = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(c.injected_count, 2);
        // Story 092: the counter is telemetry, `last_seen_at` is evidence. This
        // assertion used to read `1000` — the prior refreshing itself by being
        // shown, which is what kept a stale cluster looking fresh.
        assert_eq!(c.last_seen_at, seen_before);
    }

    // ========================================================================
    // The belief loop
    // ========================================================================

    /// A cluster with a known signature, promoted and injectable.
    fn cluster_with_signature(id: &str, signature: Option<&str>) -> PriorCluster {
        let mut c = promoted_cluster(id, r#"{"pattern":"src/**"}"#, 2);
        c.error_signature = signature.map(str::to_string);
        c
    }

    fn err(signature: &str, at: Option<i64>) -> ObservedError {
        ObservedError {
            signature: signature.to_string(),
            at,
        }
    }

    /// Story 092, criterion 6. A different error is not the prior's error, so
    /// the session did not refute it — and that is all the session says. The
    /// PreToolUse hook cannot deny the call, so the prior was shown *after* the
    /// operation was committed: a quiet session is the un-heeded case, never
    /// proof the lesson worked. Only a person confirms.
    #[test]
    fn a_quiet_session_does_not_confirm_the_prior_it_was_shown() {
        let conn = conn();
        upsert_cluster(
            &conn,
            &cluster_with_signature("clu-a", Some("error[E0433]: failed to resolve `foo`")),
        )
        .unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();

        let report = settle_injections(
            &conn,
            "sess-1",
            2000,
            &[err("warning: unused variable `x`", Some(1500))],
        )
        .unwrap();

        assert_eq!(report.unrefuted, vec!["clu-a".to_string()]);
        assert!(report.refuted.is_empty());
        let c = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(c.confirmed_count, 0, "no session moves this counter");
        assert_eq!(c.refuted_count, 0);
    }

    #[test]
    fn the_error_coming_back_after_the_warning_refutes_it() {
        let conn = conn();
        upsert_cluster(
            &conn,
            &cluster_with_signature("clu-a", Some("error[E0433]: failed to resolve `foo`")),
        )
        .unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();

        // Same failure, different surrounding text and a different line number:
        // a recurrence, not a byte-identical repeat.
        let report = settle_injections(
            &conn,
            "sess-1",
            2000,
            &[err(
                "src/lib.rs:88 error[E0433]: failed to resolve `foo` in this scope",
                Some(1500),
            )],
        )
        .unwrap();

        assert_eq!(report.refuted, vec!["clu-a".to_string()]);
        assert!(report.unrefuted.is_empty());
        let c = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(c.refuted_count, 1);
        assert_eq!(c.confirmed_count, 0);
    }

    /// The error that was already happening when the prior was injected is what
    /// the prior is warning about, not evidence that the warning failed.
    #[test]
    fn an_error_before_the_injection_does_not_refute() {
        let conn = conn();
        upsert_cluster(
            &conn,
            &cluster_with_signature("clu-a", Some("error[E0433]: failed to resolve `foo`")),
        )
        .unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();

        let report = settle_injections(
            &conn,
            "sess-1",
            2000,
            &[err("error[E0433]: failed to resolve `foo`", Some(900))],
        )
        .unwrap();

        assert_eq!(report.unrefuted, vec!["clu-a".to_string()]);
        assert!(report.refuted.is_empty());
    }

    /// An error the transcript did not timestamp cannot be placed before the
    /// injection, and a recurrence we cannot place is still a recurrence.
    #[test]
    fn an_undated_recurrence_still_refutes() {
        let conn = conn();
        upsert_cluster(
            &conn,
            &cluster_with_signature("clu-a", Some("error[E0433]: failed to resolve `foo`")),
        )
        .unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();

        let report = settle_injections(
            &conn,
            "sess-1",
            2000,
            &[err("error[E0433]: failed to resolve `foo`", None)],
        )
        .unwrap();

        assert_eq!(report.refuted, vec!["clu-a".to_string()]);
    }

    /// Story 092, criterion 2. A cluster mined from a user correction has no
    /// error to recur, so there is nothing a quiet session can be evidence of.
    /// 61 of the 66 clusters in the live store are in exactly this state, so
    /// this is the normal path, not an edge case.
    #[test]
    fn a_cluster_with_no_error_signature_settles_unobservable() {
        let conn = conn();
        upsert_cluster(&conn, &cluster_with_signature("clu-a", None)).unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();

        let report = settle_injections(&conn, "sess-1", 2000, &[]).unwrap();

        assert_eq!(report.unobservable, vec!["clu-a".to_string()]);
        assert!(report.refuted.is_empty());
        let c = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(c.confirmed_count, 0, "neither counter moves");
        assert_eq!(c.refuted_count, 0);
    }

    /// The distiller writes `""` as readily as it omits the field, and an empty
    /// signature matches nothing, so it is the same absence.
    #[test]
    fn an_empty_error_signature_is_as_unobservable_as_a_missing_one() {
        let conn = conn();
        upsert_cluster(&conn, &cluster_with_signature("clu-a", Some("  "))).unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();

        let report = settle_injections(&conn, "sess-1", 2000, &[]).unwrap();

        assert_eq!(report.unobservable, vec!["clu-a".to_string()]);
        assert_eq!(
            get_cluster(&conn, "clu-a")
                .unwrap()
                .unwrap()
                .confirmed_count,
            0
        );
    }

    /// Story 092, criterion 3. A `prompt` prior is injected because the prompt
    /// matched, which says nothing about whether the operation it warns about
    /// ever ran. Nothing recorded can answer that — `call_log` keeps tool names
    /// and timestamps, never arguments — so the session is not evidence either
    /// way. The `pre_tool` case settles the same way for a different reason:
    /// the operation provably ran, and ran *unchanged*, because the hook cannot
    /// deny it.
    #[test]
    fn no_recurrence_settles_unrefuted_whatever_the_trigger_kind() {
        let conn = conn();
        for (id, kind, key) in [
            ("clu-prompt", "prompt", "prompt|{pattern:src/**}"),
            ("clu-pre", "pre_tool", "pre_tool|{pattern:Bash}"),
        ] {
            let mut c = cluster_with_signature(id, Some("error[E0433]: failed to resolve"));
            c.trigger_kind = kind.into();
            c.canonical_trigger_key = key.into();
            upsert_cluster(&conn, &c).unwrap();
            record_injection(&conn, id, "sess-1", 1000).unwrap();
        }

        let report = settle_injections(&conn, "sess-1", 2000, &[]).unwrap();

        let mut unrefuted = report.unrefuted.clone();
        unrefuted.sort();
        assert_eq!(unrefuted, vec!["clu-pre", "clu-prompt"]);
        for id in ["clu-prompt", "clu-pre"] {
            let c = get_cluster(&conn, id).unwrap().unwrap();
            assert_eq!(c.confirmed_count, 0, "{id} banked a confirmation");
            assert_eq!(c.refuted_count, 0);
        }
    }

    /// A recurrence refutes whatever the kind: the failure is observed, not
    /// inferred, so no opportunity question arises.
    #[test]
    fn a_prompt_injection_is_still_refuted_by_a_recurrence() {
        let conn = conn();
        let mut c = cluster_with_signature("clu-a", Some("error[E0433]: failed to resolve"));
        c.trigger_kind = "prompt".into();
        c.canonical_trigger_key = "prompt|{pattern:src/**}".into();
        upsert_cluster(&conn, &c).unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();

        let report = settle_injections(
            &conn,
            "sess-1",
            2000,
            &[err("error[E0433]: failed to resolve `foo`", Some(1500))],
        )
        .unwrap();

        assert_eq!(report.refuted, vec!["clu-a".to_string()]);
        assert_eq!(
            get_cluster(&conn, "clu-a").unwrap().unwrap().refuted_count,
            1
        );
    }

    /// Story 092, criterion 4. `last_seen_at` feeds the freshness term of
    /// `cluster_injection_score`, so a prior that refreshed itself by being
    /// injected would hold its own score up without any new evidence. Only
    /// mining, which sees the pattern happen again, may move it.
    #[test]
    fn injection_does_not_refresh_the_cluster() {
        let conn = conn();
        upsert_cluster(&conn, &cluster_with_signature("clu-a", None)).unwrap();
        let before = get_cluster(&conn, "clu-a").unwrap().unwrap().last_seen_at;

        record_injection(&conn, "clu-a", "sess-1", before + 90 * 86_400).unwrap();

        let after = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(
            after.last_seen_at, before,
            "being shown is not new evidence"
        );
        assert_eq!(after.injected_count, 1, "the injection is still counted");
    }

    /// Both this test and the next one used to count confirmations, which no
    /// session can produce any more. The claim they make is about the *ballot*,
    /// not the verdict, so a refutation carries it unchanged.
    #[test]
    fn settling_twice_counts_once_per_session() {
        let conn = conn();
        upsert_cluster(&conn, &cluster_with_signature("clu-a", Some("boom failed"))).unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1100).unwrap();

        let errors = [err("boom failed", Some(1500))];
        settle_injections(&conn, "sess-1", 2000, &errors).unwrap();
        let second = settle_injections(&conn, "sess-1", 2100, &errors).unwrap();

        assert!(second.is_empty(), "a settled session has nothing left open");
        let c = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(c.refuted_count, 1, "two injections, one session, one vote");
        assert_eq!(c.injected_count, 2, "telemetry still counts both");
    }

    #[test]
    fn a_second_session_votes_again() {
        let conn = conn();
        upsert_cluster(&conn, &cluster_with_signature("clu-a", Some("boom failed"))).unwrap();
        let errors = [err("boom failed", None)];
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();
        settle_injections(&conn, "sess-1", 2000, &errors).unwrap();
        record_injection(&conn, "clu-a", "sess-2", 3000).unwrap();
        settle_injections(&conn, "sess-2", 4000, &errors).unwrap();

        let c = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(c.refuted_count, 2);
    }

    /// Clusters mined before the loop existed carry no signature. They must
    /// still *settle* rather than staying open forever — which was the original
    /// claim here, and remains true. What changed with story 092 is the verdict:
    /// settling them as `confirmed` turned "we could not look" into "it held",
    /// so they now settle `unobservable` and the injection row closes without
    /// either counter moving. Unrelated errors in the session change nothing,
    /// because there is no signature to compare them against.
    #[test]
    fn a_cluster_with_no_signature_settles_without_becoming_evidence() {
        let conn = conn();
        upsert_cluster(&conn, &cluster_with_signature("clu-a", None)).unwrap();
        record_injection(&conn, "clu-a", "sess-1", 1000).unwrap();

        let report =
            settle_injections(&conn, "sess-1", 2000, &[err("anything at all", Some(1500))])
                .unwrap();

        assert_eq!(report.unobservable, vec!["clu-a".to_string()]);
        assert!(report.refuted.is_empty());
        let c = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!((c.confirmed_count, c.refuted_count), (0, 0));

        // Settled means settled: a second Stop hook finds nothing open.
        let again = settle_injections(&conn, "sess-1", 2100, &[]).unwrap();
        assert!(again.is_empty(), "the question was closed, not reopened");
    }

    #[test]
    fn a_different_failure_is_not_a_recurrence() {
        assert!(!signatures_match(
            "error[E0433]: failed to resolve `foo`",
            "error[E0599]: no method named `bar` found"
        ));
        assert!(signatures_match(
            "error[E0433]: failed to resolve `foo`",
            "at line 12: error[E0433]: failed to resolve `foo` — did you mean?"
        ));
    }

    /// The loop is only worth building if it moves the number the injector
    /// reads. A promoted cluster starts at 0.33 against a 0.3 threshold: three
    /// refutations push it under, and confirmations bring it back.
    ///
    /// Story 092 changed who may push it back up. Quiet sessions no longer can —
    /// they are the un-heeded case — so the recovery half now runs through the
    /// human verdict, which is the only thing that raises belief.
    #[test]
    fn belief_carries_the_score_across_the_threshold_both_ways() {
        let conn = conn();
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES ('prior-a', 'l', 'l', 'prior', '[]', 100, 100)",
            [],
        )
        .unwrap();
        let mut seed = cluster_with_signature("clu-a", Some("boom failed"));
        seed.promoted_memory_id = Some("prior-a".into());
        upsert_cluster(&conn, &seed).unwrap();
        let now = 1000;

        let fresh = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert!(
            is_injectable(&fresh, now),
            "a freshly promoted prior injects: {}",
            cluster_injection_score(&fresh, now)
        );

        // Three sessions in which the error came back anyway.
        for (i, session) in ["s1", "s2", "s3"].iter().enumerate() {
            record_injection(&conn, "clu-a", session, now).unwrap();
            settle_injections(
                &conn,
                session,
                now + i as i64,
                &[err("boom failed", Some(now))],
            )
            .unwrap();
        }
        let refuted = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(refuted.refuted_count, 3);
        assert!(
            !is_injectable(&refuted, now),
            "three refutations must stop it firing: {}",
            cluster_injection_score(&refuted, now)
        );

        // Quiet sessions afterwards change nothing: silence is not a recovery.
        for (i, session) in ["s4", "s5", "s6", "s7"].iter().enumerate() {
            record_injection(&conn, "clu-a", session, now).unwrap();
            settle_injections(&conn, session, now + i as i64, &[]).unwrap();
        }
        let still_out = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(still_out.confirmed_count, 0, "no session confirms");
        assert!(!is_injectable(&still_out, now), "silence is not a recovery");

        // Four human confirmations bring it back over the line.
        for _ in 0..4 {
            apply_belief_from_memory(&conn, "prior-a", 1).unwrap();
        }
        let recovered = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(recovered.confirmed_count, 4);
        assert!(
            is_injectable(&recovered, now),
            "confirmations must bring it back: {}",
            cluster_injection_score(&recovered, now)
        );
    }

    #[test]
    fn a_human_verdict_on_the_projection_moves_the_cluster() {
        let conn = conn();
        // A real row: `promoted_memory_id` carries a foreign key into
        // `memory_entries`, and the schema turns foreign keys on.
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES ('prior-a', 'l', 'l', 'prior', '[]', 100, 100)",
            [],
        )
        .unwrap();
        let mut c = cluster_with_signature("clu-a", Some("boom failed"));
        c.promoted_memory_id = Some("prior-a".into());
        upsert_cluster(&conn, &c).unwrap();

        assert_eq!(
            apply_belief_from_memory(&conn, "prior-a", 1).unwrap(),
            Some("clu-a".to_string())
        );
        assert_eq!(
            apply_belief_from_memory(&conn, "prior-a", -1).unwrap(),
            Some("clu-a".to_string())
        );
        // An ordinary memory entry owns no cluster.
        assert_eq!(
            apply_belief_from_memory(&conn, "some-topic", 1).unwrap(),
            None
        );

        let after = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(after.confirmed_count, 1);
        assert_eq!(after.refuted_count, 1);
    }

    #[test]
    fn the_first_observed_signature_is_the_one_that_sticks() {
        let conn = conn();
        upsert_cluster(&conn, &cluster_with_signature("clu-a", None)).unwrap();

        set_cluster_error_signature(&conn, "clu-a", "first failure").unwrap();
        set_cluster_error_signature(&conn, "clu-a", "second failure").unwrap();

        let c = get_cluster(&conn, "clu-a").unwrap().unwrap();
        assert_eq!(c.error_signature.as_deref(), Some("first failure"));
    }
}
