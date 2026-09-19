//! The SQL half of `mdkb memory audit`: which entries to look at, which pairs
//! the store already considers related, and the one write the audit is allowed
//! to make.
//!
//! Nothing here decides whether an entry is still true. That is the whole point
//! of the feature. An AI sweep over stored decisions has no ground truth to
//! check them against and would stamp confident "still valid" verdicts on
//! entries nobody verified, which story 092 already ruled out under a different
//! name: a prior does not gain confidence from silence. So the audit SELECTS
//! from mechanical signals — a file that is gone, a commit under a path an
//! entry cites, two entries the embedding cannot tell apart, a lifecycle record
//! past its age — and hands the list to a human.
//!
//! The one write is [`stamp_audited`], which sets `last_audited_at` and nothing
//! else. It is deliberately not `last_confirmed_at`: that column is the decay
//! reference in [`crate::store::memory::MemoryEntry::confidence_at`], so
//! writing it would refresh the confidence of every entry a sweep merely
//! looked at.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::store::memory::EntryType;

/// How many neighbours each entry's near-duplicate probe inspects.
///
/// The question is "is anything else here the same memory", not "what is
/// nearby", and the probe runs once per entry, so a short list keeps an audit
/// of a few thousand entries to a few thousand indexed lookups.
const NEAR_DUPLICATE_NEIGHBOURS: usize = 5;

/// The columns the audit reads off an entry. Deliberately not a
/// [`crate::store::memory::MemoryEntry`]: the audit needs `last_audited_at`,
/// which that struct does not carry, and none of the rest.
#[derive(Debug, Clone)]
pub struct AuditRow {
    pub rowid: i64,
    pub id: String,
    pub title: String,
    pub entry_type: String,
    pub content: String,
    /// Last write to the entry — the reference for "has the code moved since
    /// this was measured".
    pub updated_at: i64,
    pub last_accessed: Option<i64>,
    pub expires_at: Option<i64>,
    /// When a previous audit last looked at this entry. `None` = never.
    pub last_audited_at: Option<i64>,
}

/// Every active entry, in the order the audit reports them.
pub fn auditable_entries(conn: &Connection) -> Result<Vec<AuditRow>> {
    let mut stmt = conn.prepare(
        "SELECT rowid, id, title, entry_type, content, updated_at, last_accessed, expires_at, last_audited_at
         FROM memory_entries
         WHERE status = 'active'
         ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AuditRow {
            rowid: row.get(0)?,
            id: row.get(1)?,
            title: row.get(2)?,
            entry_type: row.get(3)?,
            content: row.get(4)?,
            updated_at: row.get(5)?,
            last_accessed: row.get(6)?,
            expires_at: row.get(7)?,
            last_audited_at: row.get(8)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Record that `ids` were looked at, and nothing else about them.
///
/// One statement, no transaction of its own and no revision: an audit that
/// changes nobody's mind must not appear in the edit history of the entries it
/// read. `updated_at` is untouched too, so the markdown projection does not
/// rewrite a git-tracked file to record a local sweep.
pub fn stamp_audited(conn: &Connection, ids: &[String], now: i64) -> Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let mut stmt = conn.prepare("UPDATE memory_entries SET last_audited_at = ?1 WHERE id = ?2")?;
    let mut stamped = 0;
    for id in ids {
        stamped += stmt.execute(params![now, id])?;
    }
    Ok(stamped)
}

/// When `id` was last audited, or `None` if it never was.
pub fn last_audited_at(conn: &Connection, id: &str) -> Result<Option<i64>> {
    let stamp: Option<Option<i64>> = conn
        .query_row(
            "SELECT last_audited_at FROM memory_entries WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stamp.flatten())
}

/// Pairs of active entries already linked by a `contradicts` edge.
///
/// The cheap half of "contradicting pairs": somebody, at write time, said these
/// two disagree, and nobody has resolved it since. No model, no threshold.
pub fn contradicting_pairs(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT e.source_id, e.target_ref
         FROM memory_edges e
         JOIN memory_entries src ON src.id = e.source_id
         JOIN memory_entries tgt ON tgt.id = e.target_ref
         WHERE e.relation = 'contradicts'
           AND e.target_kind = 'memory'
           AND src.status = 'active'
           AND tgt.status = 'active'
         ORDER BY e.source_id, e.target_ref",
    )?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Pairs of active entries whose embeddings sit at or above `min_similarity`.
///
/// This is the write path's own duplicate rule applied in both directions.
/// `find_duplicate` refuses a NEW entry that lands this close to an existing
/// one, but two entries written far enough apart — or before the rule existed —
/// never met each other. The audit is where they do.
///
/// A store with no embeddings (no model pulled, or `mdkb embed` never run)
/// yields an empty list rather than an error: the other three signals need no
/// model, and an audit must not require one.
pub fn near_duplicate_pairs(
    conn: &Connection,
    min_similarity: f32,
) -> Result<Vec<(String, String, f64)>> {
    let embeddings = active_memory_embeddings(conn)?;
    if embeddings.is_empty() {
        return Ok(Vec::new());
    }
    let ids = active_ids_by_rowid(conn)?;

    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (rowid, embedding) in &embeddings {
        let neighbours = crate::store::vectors::memory_vector_search(
            conn,
            embedding,
            NEAR_DUPLICATE_NEIGHBOURS,
            None,
        )?;
        for (other_rowid, distance) in neighbours {
            if other_rowid == *rowid {
                continue;
            }
            let similarity = crate::store::hybrid::cosine_from_distance(distance);
            if similarity < f64::from(min_similarity) {
                continue;
            }
            // Each unordered pair once, keyed on the rowids rather than on the
            // slugs: the KNN reports a→b and b→a with the same distance.
            let key = (*rowid.min(&other_rowid), *rowid.max(&other_rowid));
            if !seen.insert(key) {
                continue;
            }
            let (Some(a), Some(b)) = (ids.get(rowid), ids.get(&other_rowid)) else {
                continue; // a neighbour that is no longer active
            };
            out.push((a.clone(), b.clone(), similarity));
        }
    }
    out.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

/// Embeddings of the active entries, read from the plain mirror table rather
/// than from the vec0 index — `memory_embeddings` holds the same bytes and
/// answers an ordinary `SELECT`.
///
/// The table is created by `vectors::init_vector_schema`, not by
/// `schema::init_schema`, so a connection that never went through a full
/// `Context::open` does not have it. That is an absent signal, not an error.
fn active_memory_embeddings(conn: &Connection) -> Result<Vec<(i64, Vec<f32>)>> {
    let has_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'memory_embeddings'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !has_table {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT m.memory_rowid, m.embedding
         FROM memory_embeddings m
         JOIN memory_entries e ON e.rowid = m.memory_rowid
         WHERE e.status = 'active'
         ORDER BY m.memory_rowid",
    )?;
    let rows = stmt.query_map([], |row| {
        let rowid: i64 = row.get(0)?;
        let bytes: Vec<u8> = row.get(1)?;
        Ok((rowid, bytes))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (rowid, bytes) = row?;
        let floats = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        out.push((rowid, floats));
    }
    Ok(out)
}

fn active_ids_by_rowid(conn: &Connection) -> Result<HashMap<i64, String>> {
    let mut stmt = conn.prepare("SELECT rowid, id FROM memory_entries WHERE status = 'active'")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (rowid, id) = row?;
        out.insert(rowid, id);
    }
    Ok(out)
}

/// Active entries that a prune would take: expired by `expires_at`, or a
/// lifecycle record (reminder, prior, handoff) untouched for `aged_days`.
///
/// Durable types — topic, problem, decision — are never selected for age, the
/// same rule `crate::store::memory::prunable_entry_ids` applies, because age is
/// not evidence that a decision stopped holding.
pub fn expired_or_aged(
    conn: &Connection,
    aged_days: u32,
    now: i64,
) -> Result<Vec<(String, LifecycleReason)>> {
    let cutoff = now - (i64::from(aged_days) * 86_400);
    let lifecycle = EntryType::sql_list(|t| !t.is_durable());
    let mut stmt = conn.prepare(&format!(
        "SELECT id,
                CASE WHEN expires_at IS NOT NULL AND expires_at <= ?2 THEN 1 ELSE 0 END AS expired,
                expires_at,
                COALESCE(last_accessed, created_at) AS touched
         FROM memory_entries
         WHERE status = 'active'
           AND (
                (expires_at IS NOT NULL AND expires_at <= ?2)
                OR (
                    entry_type IN ({lifecycle})
                    AND COALESCE(last_accessed, created_at) < ?1
                    AND (due_at IS NULL OR due_at < ?1)
                )
           )
         ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![cutoff, now], |row| {
        let id: String = row.get(0)?;
        let expired: i64 = row.get(1)?;
        let expires_at: Option<i64> = row.get(2)?;
        let touched: i64 = row.get(3)?;
        Ok((id, expired == 1, expires_at, touched))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, expired, expires_at, touched) = row?;
        let reason = if expired {
            LifecycleReason::Expired {
                expires_at: expires_at.unwrap_or(now),
            }
        } else {
            LifecycleReason::Aged {
                last_touched: touched,
            }
        };
        out.push((id, reason));
    }
    Ok(out)
}

/// Why a lifecycle entry was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleReason {
    /// `expires_at` has passed.
    Expired { expires_at: i64 },
    /// A reminder, prior or handoff nobody has read for the configured age.
    Aged { last_touched: i64 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema;

    fn store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_schema(&conn).unwrap();
        conn
    }

    fn add(conn: &Connection, id: &str, entry_type: &str, now: i64) {
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
             VALUES (?1, ?1, 'body', ?2, ?3, ?3)",
            params![id, entry_type, now],
        )
        .unwrap();
    }

    #[test]
    fn the_stamp_records_the_look_and_touches_nothing_else() {
        let conn = store();
        add(&conn, "a", "decision", 1_000);
        let before: (i64, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT updated_at, last_confirmed_at, last_refuted_at FROM memory_entries WHERE id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(stamp_audited(&conn, &["a".to_string()], 9_000).unwrap(), 1);

        let after: (i64, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT updated_at, last_confirmed_at, last_refuted_at FROM memory_entries WHERE id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            before, after,
            "an audit writes only last_audited_at — everything the confidence \
             model reads must be untouched"
        );
        assert_eq!(last_audited_at(&conn, "a").unwrap(), Some(9_000));
    }

    #[test]
    fn an_unaudited_entry_has_no_stamp() {
        let conn = store();
        add(&conn, "a", "topic", 1_000);
        assert_eq!(
            last_audited_at(&conn, "a").unwrap(),
            None,
            "never audited must be distinguishable from audited long ago"
        );
    }

    #[test]
    fn age_selects_a_lifecycle_entry_and_spares_a_decision() {
        let conn = store();
        let now = 1_000_000_000;
        let old = now - 200 * 86_400;
        add(&conn, "stale-reminder", "reminder", old);
        add(&conn, "old-decision", "decision", old);

        let selected = expired_or_aged(&conn, 90, now).unwrap();
        let ids: Vec<&str> = selected.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["stale-reminder"],
            "age is not evidence that a decision stopped holding"
        );
        assert!(matches!(selected[0].1, LifecycleReason::Aged { .. }));
    }

    #[test]
    fn an_expired_entry_is_selected_whatever_its_type() {
        let conn = store();
        let now = 1_000_000_000;
        add(&conn, "ttl-decision", "decision", now);
        conn.execute(
            "UPDATE memory_entries SET expires_at = ?1 WHERE id = 'ttl-decision'",
            params![now - 1],
        )
        .unwrap();

        let selected = expired_or_aged(&conn, 90, now).unwrap();
        assert_eq!(selected.len(), 1);
        assert!(matches!(selected[0].1, LifecycleReason::Expired { .. }));
    }

    #[test]
    fn a_contradicts_edge_between_live_entries_is_a_pair() {
        let conn = store();
        add(&conn, "a", "decision", 1_000);
        add(&conn, "b", "decision", 1_000);
        add(&conn, "retired", "decision", 1_000);
        conn.execute(
            "UPDATE memory_entries SET status = 'archived' WHERE id = 'retired'",
            [],
        )
        .unwrap();
        for target in ["b", "retired"] {
            conn.execute(
                "INSERT INTO memory_edges (source_id, target_ref, target_kind, relation, created_at)
                 VALUES ('a', ?1, 'memory', 'contradicts', 1000)",
                params![target],
            )
            .unwrap();
        }

        assert_eq!(
            contradicting_pairs(&conn).unwrap(),
            vec![("a".to_string(), "b".to_string())],
            "a disagreement with a retired entry is settled, not open"
        );
    }

    #[test]
    fn a_store_without_embeddings_reports_no_near_duplicates() {
        let conn = store();
        add(&conn, "a", "topic", 1_000);
        add(&conn, "b", "topic", 1_000);
        assert!(
            near_duplicate_pairs(&conn, 0.9488).unwrap().is_empty(),
            "three of the four signals need no model; the audit must run without one"
        );
    }
}
