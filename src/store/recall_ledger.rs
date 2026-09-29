//! What memory recall offered on each prompt, and what became of it.
//!
//! One `recall_prompts` row per prompt that ran recall, one `recall_candidates`
//! row per entry the search returned at or above the candidate floor — whether
//! or not it was injected. Settlement at Stop fills `outcome`; `mdkb stats`
//! aggregates it. Story 181-63ba.
//!
//! Privacy: no column holds prompt text. A prompt is known only by its
//! session, time and mode; a candidate by the id of the entry it names.

use rusqlite::{Connection, params};

use crate::error::Result;

/// How recall ran on the prompt: `sigil`, `automatic` or `shadow`.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallPrompt {
    pub session: String,
    pub mode: &'static str,
    /// The floor an entry had to clear to be injected.
    pub floor: f32,
    /// The floor an entry had to clear to be recorded here.
    pub candidate_floor: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecallCandidate {
    pub entry_id: String,
    /// 0-based position in the search result.
    pub rank: u16,
    /// `None` when the entry was found by FTS alone and has no vector score.
    pub cosine: Option<f32>,
    pub entry_type: String,
    pub age_days: u32,
    /// Prompt identifiers and paths found in the entry.
    pub overlap: u8,
    pub injected: bool,
    pub holdout: bool,
}

/// Record one prompt and its candidates, after pruning rows older than
/// `retention_days`. Returns the prompt id.
pub fn record_prompt(
    conn: &Connection,
    prompt: &RecallPrompt,
    candidates: &[RecallCandidate],
    retention_days: u32,
    now: i64,
) -> Result<i64> {
    prune(conn, retention_days, now)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO recall_prompts (session, mode, floor, candidate_floor, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            prompt.session,
            prompt.mode,
            prompt.floor,
            prompt.candidate_floor,
            now
        ],
    )?;
    let prompt_id = tx.last_insert_rowid();
    {
        let mut insert = tx.prepare_cached(
            "INSERT OR IGNORE INTO recall_candidates \
             (prompt_id, entry_id, rank, cosine, entry_type, age_days, overlap, injected, holdout) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for c in candidates {
            insert.execute(params![
                prompt_id,
                c.entry_id,
                c.rank,
                c.cosine,
                c.entry_type,
                c.age_days,
                c.overlap,
                c.injected,
                c.holdout,
            ])?;
        }
    }
    tx.commit()?;
    Ok(prompt_id)
}

/// Delete prompts older than the retention window; their candidates go with
/// them.
pub fn prune(conn: &Connection, retention_days: u32, now: i64) -> Result<usize> {
    let cutoff = now - i64::from(retention_days) * 24 * 60 * 60;
    Ok(conn.execute(
        "DELETE FROM recall_prompts WHERE created_at < ?1",
        params![cutoff],
    )?)
}

/// Every candidate offered in `session` that has no outcome yet, with the
/// title of the entry it names (empty if the entry was deleted since).
pub fn open_candidates(
    conn: &Connection,
    session: &str,
) -> Result<Vec<crate::domain::recall_outcome::LedgerCandidate>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.prompt_id, p.created_at, c.entry_id, COALESCE(m.title, ''), c.injected \
         FROM recall_candidates c \
         JOIN recall_prompts p ON p.id = c.prompt_id \
         LEFT JOIN memory_entries m ON m.id = c.entry_id \
         WHERE p.session = ?1 AND c.outcome IS NULL \
         ORDER BY p.created_at, c.rank",
    )?;
    let rows = stmt.query_map([session], |r| {
        Ok(crate::domain::recall_outcome::LedgerCandidate {
            prompt_id: r.get(0)?,
            prompt_at: r.get(1)?,
            entry_id: r.get(2)?,
            title: r.get(3)?,
            injected: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Write settlement labels. A candidate that already has an outcome keeps it.
pub fn set_outcomes(
    conn: &Connection,
    labels: &[(i64, String, crate::domain::recall_outcome::RecallOutcome)],
    now: i64,
) -> Result<usize> {
    let tx = conn.unchecked_transaction()?;
    let mut written = 0;
    {
        let mut update = tx.prepare_cached(
            "UPDATE recall_candidates SET outcome = ?3, outcome_at = ?4 \
             WHERE prompt_id = ?1 AND entry_id = ?2 AND outcome IS NULL",
        )?;
        for (prompt_id, entry_id, outcome) in labels {
            written += update.execute(params![prompt_id, entry_id, outcome.as_str(), now])?;
        }
    }
    tx.commit()?;
    Ok(written)
}

/// Ledger counts for one cosine band, entry type and holdout flag.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct BandCounts {
    /// `<0.40`, `0.40-0.45` … `0.65+`, or `fts` for a hit with no cosine.
    pub band: String,
    pub entry_type: String,
    pub holdout: bool,
    pub offered: u32,
    pub injected: u32,
    /// Candidates with any outcome.
    pub labelled: u32,
    /// Injected and `used` or `confirmed`.
    pub positive: u32,
    /// Injected and `refuted` or `corrected`.
    pub negative: u32,
    /// Not injected, and reached anyway.
    pub missed: u32,
}

/// Every ledger row, counted by band, entry type and holdout.
pub fn band_counts(conn: &Connection) -> Result<Vec<BandCounts>> {
    // Rounded before comparing: the column holds an `f32` widened to REAL, so
    // an entry at exactly 0.45 reads 0.4499999 and would land a band low.
    let mut stmt = conn.prepare_cached(
        "WITH c AS (SELECT *, ROUND(cosine, 4) AS cos FROM recall_candidates) \
         SELECT CASE WHEN cos IS NULL THEN 7 WHEN cos < 0.40 THEN 0 WHEN cos < 0.45 THEN 1 \
                     WHEN cos < 0.50 THEN 2 WHEN cos < 0.55 THEN 3 WHEN cos < 0.60 THEN 4 \
                     WHEN cos < 0.65 THEN 5 ELSE 6 END AS band, \
                entry_type, holdout, COUNT(*), SUM(injected), SUM(outcome IS NOT NULL), \
                SUM(injected AND COALESCE(outcome, '') IN ('used', 'confirmed')), \
                SUM(injected AND COALESCE(outcome, '') IN ('refuted', 'corrected')), \
                SUM(NOT injected AND outcome IS 'missed') \
         FROM c GROUP BY band, entry_type, holdout ORDER BY band, entry_type, holdout",
    )?;
    const BANDS: [&str; 8] = [
        "<0.40",
        "0.40-0.45",
        "0.45-0.50",
        "0.50-0.55",
        "0.55-0.60",
        "0.60-0.65",
        "0.65+",
        "fts",
    ];
    let rows = stmt.query_map([], |r| {
        let band: usize = r.get(0)?;
        Ok(BandCounts {
            band: BANDS[band].to_string(),
            entry_type: r.get(1)?,
            holdout: r.get(2)?,
            offered: r.get(3)?,
            injected: r.get(4)?,
            labelled: r.get(5)?,
            positive: r.get(6)?,
            negative: r.get(7)?,
            missed: r.get(8)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// Prompts recorded at or after `since`.
pub fn prompts_since(conn: &Connection, since: i64) -> Result<u32> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM recall_prompts WHERE created_at >= ?1",
        [since],
        |r| r.get(0),
    )?)
}

/// Recorded prompts per recall mode.
pub fn prompts_by_mode(conn: &Connection) -> Result<std::collections::BTreeMap<String, u32>> {
    let mut stmt =
        conn.prepare_cached("SELECT mode, COUNT(*) FROM recall_prompts GROUP BY mode")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::store::schema::init_schema(&conn).unwrap();
        conn
    }

    fn prompt() -> RecallPrompt {
        RecallPrompt {
            session: "s1".into(),
            mode: "shadow",
            floor: 0.50,
            candidate_floor: 0.40,
        }
    }

    fn cand(id: &str, rank: u16, cosine: Option<f32>, injected: bool) -> RecallCandidate {
        RecallCandidate {
            entry_id: id.into(),
            rank,
            cosine,
            entry_type: "decision".into(),
            age_days: 3,
            overlap: 1,
            injected,
            holdout: false,
        }
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_prompt_keeps_every_candidate_linked_to_it() {
        let conn = db();
        let cands = [
            cand("a", 0, Some(0.71), true),
            cand("b", 1, Some(0.44), false),
            cand("c", 2, None, false),
        ];
        let id = record_prompt(&conn, &prompt(), &cands, 30, 1_000).unwrap();

        let mut stmt = conn
            .prepare(
                "SELECT entry_id, rank, cosine, injected FROM recall_candidates \
                 WHERE prompt_id = ?1 ORDER BY rank",
            )
            .unwrap();
        let rows: Vec<(String, u16, Option<f32>, bool)> = stmt
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("a".into(), 0, Some(0.71), true),
                ("b".into(), 1, Some(0.44), false),
                ("c".into(), 2, None, false),
            ]
        );
        let (session, mode): (String, String) = conn
            .query_row(
                "SELECT session, mode FROM recall_prompts WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((session.as_str(), mode.as_str()), ("s1", "shadow"));
    }

    /// A prompt with no candidates is still a row: it is the denominator the
    /// false-negative rate is computed against.
    #[test]
    fn a_prompt_with_no_candidates_is_still_recorded() {
        let conn = db();
        record_prompt(&conn, &prompt(), &[], 30, 1_000).unwrap();
        assert_eq!(count(&conn, "recall_prompts"), 1);
    }

    #[test]
    fn pruning_drops_old_prompts_with_their_candidates() {
        let conn = db();
        let day = 86_400;
        record_prompt(&conn, &prompt(), &[cand("old", 0, None, true)], 30, 0).unwrap();
        record_prompt(
            &conn,
            &prompt(),
            &[cand("new", 0, None, true)],
            30,
            40 * day,
        )
        .unwrap();

        assert_eq!(count(&conn, "recall_prompts"), 1, "recording prunes first");
        let left: String = conn
            .query_row("SELECT entry_id FROM recall_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, "new", "the old prompt's candidate went with it");
        assert_eq!(prune(&conn, 30, 80 * day).unwrap(), 1);
        assert_eq!(count(&conn, "recall_candidates"), 0);
    }

    #[test]
    fn settlement_reads_open_candidates_and_writes_each_label_once() {
        use crate::domain::recall_outcome::RecallOutcome;
        let conn = db();
        let a = record_prompt(
            &conn,
            &prompt(),
            &[cand("a", 0, None, true), cand("b", 1, None, false)],
            30,
            100,
        )
        .unwrap();
        let mut other = prompt();
        other.session = "s2".into();
        record_prompt(&conn, &other, &[cand("c", 0, None, true)], 30, 100).unwrap();

        let open = open_candidates(&conn, "s1").unwrap();
        let ids: Vec<(&str, bool, i64)> = open
            .iter()
            .map(|c| (c.entry_id.as_str(), c.injected, c.prompt_at))
            .collect();
        assert_eq!(
            ids,
            vec![("a", true, 100), ("b", false, 100)],
            "only this session's rows"
        );

        assert_eq!(
            set_outcomes(&conn, &[(a, "a".into(), RecallOutcome::Used)], 200).unwrap(),
            1
        );
        // A second settlement of the same session must not overwrite the first.
        assert_eq!(
            set_outcomes(&conn, &[(a, "a".into(), RecallOutcome::Refuted)], 300).unwrap(),
            0
        );
        let (outcome, at): (String, i64) = conn
            .query_row(
                "SELECT outcome, outcome_at FROM recall_candidates WHERE entry_id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((outcome.as_str(), at), ("used", 200));
        let still_open: Vec<String> = open_candidates(&conn, "s1")
            .unwrap()
            .into_iter()
            .map(|c| c.entry_id)
            .collect();
        assert_eq!(still_open, vec!["b"]);
    }

    #[test]
    fn counts_fall_in_the_band_of_their_cosine() {
        use crate::domain::recall_outcome::RecallOutcome;
        let conn = db();
        let p = record_prompt(
            &conn,
            &prompt(),
            &[
                cand("a", 0, Some(0.42), false),
                cand("b", 1, Some(0.47), false),
                cand("c", 2, Some(0.52), true),
                cand("d", 3, Some(0.91), true),
                cand("e", 4, None, false),
            ],
            30,
            100,
        )
        .unwrap();
        set_outcomes(
            &conn,
            &[
                (p, "b".into(), RecallOutcome::Missed),
                (p, "c".into(), RecallOutcome::Used),
                (p, "d".into(), RecallOutcome::Refuted),
            ],
            200,
        )
        .unwrap();
        let rows: Vec<(String, u32, u32, u32, u32, u32)> = band_counts(&conn)
            .unwrap()
            .into_iter()
            .map(|b| {
                (
                    b.band, b.offered, b.injected, b.positive, b.negative, b.missed,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("0.40-0.45".into(), 1, 0, 0, 0, 0),
                ("0.45-0.50".into(), 1, 0, 0, 0, 1),
                ("0.50-0.55".into(), 1, 1, 1, 0, 0),
                ("0.65+".into(), 1, 1, 0, 1, 0),
                ("fts".into(), 1, 0, 0, 0, 0),
            ]
        );
        assert_eq!(prompts_by_mode(&conn).unwrap().get("shadow"), Some(&1));
    }

    /// The ledger must never become a place prompt text can land. A new column
    /// fails this test until someone decides, here, that it is safe.
    #[test]
    fn no_ledger_column_can_hold_prompt_text() {
        let conn = db();
        let cols = |t: &str| -> Vec<String> {
            let mut s = conn
                .prepare(&format!(
                    "SELECT name FROM pragma_table_info('{t}') ORDER BY cid"
                ))
                .unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            cols("recall_prompts"),
            [
                "id",
                "session",
                "mode",
                "floor",
                "candidate_floor",
                "created_at"
            ]
        );
        assert_eq!(
            cols("recall_candidates"),
            [
                "prompt_id",
                "entry_id",
                "rank",
                "cosine",
                "entry_type",
                "age_days",
                "overlap",
                "injected",
                "holdout",
                "outcome",
                "outcome_at"
            ]
        );
    }
}
