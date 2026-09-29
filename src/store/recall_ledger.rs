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
        params![prompt.session, prompt.mode, prompt.floor, prompt.candidate_floor, now],
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
        record_prompt(&conn, &prompt(), &[cand("new", 0, None, true)], 30, 40 * day).unwrap();

        assert_eq!(count(&conn, "recall_prompts"), 1, "recording prunes first");
        let left: String = conn
            .query_row("SELECT entry_id FROM recall_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, "new", "the old prompt's candidate went with it");
        assert_eq!(prune(&conn, 30, 80 * day).unwrap(), 1);
        assert_eq!(count(&conn, "recall_candidates"), 0);
    }

    /// The ledger must never become a place prompt text can land. A new column
    /// fails this test until someone decides, here, that it is safe.
    #[test]
    fn no_ledger_column_can_hold_prompt_text() {
        let conn = db();
        let cols = |t: &str| -> Vec<String> {
            let mut s = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{t}') ORDER BY cid"))
                .unwrap();
            s.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        };
        assert_eq!(
            cols("recall_prompts"),
            ["id", "session", "mode", "floor", "candidate_floor", "created_at"]
        );
        assert_eq!(
            cols("recall_candidates"),
            [
                "prompt_id", "entry_id", "rank", "cosine", "entry_type", "age_days",
                "overlap", "injected", "holdout", "outcome", "outcome_at"
            ]
        );
    }
}
