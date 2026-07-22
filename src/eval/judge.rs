//! Answer-quality judging over retrieved memory context.
//!
//! Given a question and the top-k retrieved entries, a [`Judge`] returns a
//! [`Verdict`] on whether the context supports the expected answer.
//! [`SubstringJudge`] is the deterministic, API-free default. An LLM-backed
//! judge is a future alternate `Judge` impl — the orchestration
//! ([`run_judge`]) and aggregation ([`judge_accuracy`]) here do not change when
//! it lands, because they depend only on the trait.

use crate::error::Result;
use rusqlite::Connection;
use serde::Serialize;

/// Whether the retrieved context supports the expected answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Correct,
    Incorrect,
}

/// A judging case: a question and the answer a correct retrieval must support.
#[derive(Debug, Clone)]
pub struct JudgeCase {
    pub question: String,
    pub expected_answer: String,
}

/// Aggregate judging result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JudgeReport {
    pub accuracy: f64,
    pub n: usize,
    pub k: usize,
}

/// Strategy deciding whether retrieved context answers a question.
pub trait Judge {
    fn judge(&self, question: &str, retrieved: &[String], expected: &str) -> Result<Verdict>;
}

/// Deterministic judge: `Correct` iff `expected` appears (case-insensitively)
/// in any retrieved entry. No model, no network — this is the reproducible
/// baseline judge and the one the tests exercise.
#[derive(Debug)]
pub struct SubstringJudge;

impl Judge for SubstringJudge {
    fn judge(&self, _question: &str, retrieved: &[String], expected: &str) -> Result<Verdict> {
        let needle = expected.to_lowercase();
        let hit = retrieved.iter().any(|c| c.to_lowercase().contains(&needle));
        Ok(if hit {
            Verdict::Correct
        } else {
            Verdict::Incorrect
        })
    }
}

/// Fraction of `Correct` verdicts. Empty slice → 0.0 (never divide by zero).
pub fn judge_accuracy(verdicts: &[Verdict]) -> f64 {
    if verdicts.is_empty() {
        return 0.0;
    }
    verdicts
        .iter()
        .filter(|v| matches!(v, Verdict::Correct))
        .count() as f64
        / verdicts.len() as f64
}

/// For each case: retrieve top-k context (BM25-only, deterministic), ask the
/// judge whether it supports the expected answer, aggregate accuracy.
pub fn run_judge<J: Judge>(
    conn: &Connection,
    cases: &[JudgeCase],
    k: usize,
    judge: &J,
) -> Result<JudgeReport> {
    let mut verdicts = Vec::with_capacity(cases.len());
    for case in cases {
        // QA questions are natural language → OR retrieval (any term may match),
        // not the default token-AND which a full sentence rarely satisfies.
        let fts = crate::store::search::escape_fts5_query_or(&case.question);
        let results = if fts.is_empty() {
            Vec::new()
        } else {
            crate::store::memory::search_entries_hybrid_fts(conn, &fts, None, k, 0.0, 0)?
        };
        let context: Vec<String> = results
            .iter()
            .map(|e| format!("{}\n{}", e.title, e.content))
            .collect();
        verdicts.push(judge.judge(&case.question, &context, &case.expected_answer)?);
    }
    Ok(JudgeReport {
        accuracy: judge_accuracy(&verdicts),
        n: cases.len(),
        k,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::testkit::{add, setup_db};

    #[test]
    fn judge_accuracy_is_fraction_correct() {
        assert!(judge_accuracy(&[]).abs() < f64::EPSILON);
        assert!((judge_accuracy(&[Verdict::Correct, Verdict::Incorrect]) - 0.5).abs() < 1e-9);
        assert!(
            (judge_accuracy(&[Verdict::Correct, Verdict::Correct, Verdict::Incorrect]) - 2.0 / 3.0)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn substring_judge_matches_case_insensitively() {
        let j = SubstringJudge;
        let ctx = vec!["OAuth2 PKCE flow\nauthorization CODE exchange".to_string()];
        assert_eq!(
            j.judge("q", &ctx, "code exchange").unwrap(),
            Verdict::Correct
        );
        assert_eq!(
            j.judge("q", &ctx, "quantum entanglement").unwrap(),
            Verdict::Incorrect
        );
    }

    #[test]
    fn run_judge_scores_over_retrieved_context() {
        let conn = setup_db();
        add(
            &conn,
            "oauth",
            "OAuth2 PKCE flow",
            "authorization code exchange with PKCE",
            &["auth"],
        );
        add(
            &conn,
            "cache",
            "LRU cache eviction",
            "least recently used eviction policy",
            &["perf"],
        );

        let cases = vec![
            // Retrieved oauth context supports the expected answer → Correct.
            JudgeCase {
                question: "pkce exchange".into(),
                expected_answer: "code exchange".into(),
            },
            // Retrieved cache context does not support this answer → Incorrect.
            JudgeCase {
                question: "cache eviction".into(),
                expected_answer: "quantum entanglement".into(),
            },
        ];

        let r = run_judge(&conn, &cases, 5, &SubstringJudge).unwrap();
        assert_eq!(r.n, 2);
        assert_eq!(r.k, 5);
        assert!(
            (r.accuracy - 0.5).abs() < 1e-9,
            "one supported of two → 0.5, got {}",
            r.accuracy
        );
    }
}
