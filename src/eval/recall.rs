//! Deterministic recall-quality evaluation for memory retrieval (LoCoMo-style).
//!
//! API-free by default: uses BM25-only hybrid search (no query embedding) so a
//! recall@k / MRR baseline is reproducible without a model or network. This is
//! the yardstick every later retrieval change (dedup, router, compression) is
//! measured against — a change that drops recall@k is a regression, not a win.

use crate::error::Result;
use rusqlite::Connection;
use serde::Serialize;

/// One evaluation query: a natural-language prompt and the id(s) a correct
/// retrieval must surface within the top-k results.
#[derive(Debug, Clone)]
pub struct EvalCase {
    pub query: String,
    pub expected_ids: Vec<String>,
    /// Optional pre-computed query embedding. `None` = BM25-only (deterministic).
    pub embedding: Option<Vec<f32>>,
}

/// Aggregate recall metrics over a case set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecallReport {
    pub recall_at_k: f64,
    pub mrr: f64,
    pub n: usize,
    pub k: usize,
}

/// Compute recall@k and MRR over `cases`.
///
/// A case is a hit if any of its `expected_ids` appears in the top-k retrieved
/// entries; MRR credits the reciprocal rank of the first such hit. An empty
/// case set yields zeroed metrics (never a divide-by-zero).
pub fn run_recall(conn: &Connection, cases: &[EvalCase], k: usize) -> Result<RecallReport> {
    let mut hits = 0usize;
    let mut reciprocal_rank = 0f64;
    for case in cases {
        let results = crate::store::memory::search_entries_hybrid(
            conn,
            &case.query,
            case.embedding.as_deref(),
            k,
            0.0,
            0,
        )?;
        if let Some(pos) = results
            .iter()
            .take(k)
            .position(|e| case.expected_ids.contains(&e.id))
        {
            hits += 1;
            reciprocal_rank += 1.0 / (pos as f64 + 1.0);
        }
    }
    let n = cases.len();
    let denom = n.max(1) as f64;
    Ok(RecallReport {
        recall_at_k: hits as f64 / denom,
        mrr: reciprocal_rank / denom,
        n,
        k,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::testkit::{add, setup_db};

    #[test]
    fn recall_and_mrr_reflect_hits_and_misses() {
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
            "retry",
            "HTTP retry backoff",
            "exponential backoff jitter for retries",
            &["net"],
        );
        add(
            &conn,
            "cache",
            "LRU cache eviction",
            "least recently used eviction policy",
            &["perf"],
        );

        let cases = vec![
            // Clear hit: "pkce" and "exchange" tokens live only in the oauth entry.
            EvalCase {
                query: "pkce exchange".into(),
                expected_ids: vec!["oauth".into()],
                embedding: None,
            },
            // Clear miss: neither token is present in any entry.
            EvalCase {
                query: "kubernetes helm".into(),
                expected_ids: vec!["oauth".into()],
                embedding: None,
            },
        ];

        let r = run_recall(&conn, &cases, 5).unwrap();
        assert_eq!(r.n, 2);
        assert_eq!(r.k, 5);
        assert!(
            (r.recall_at_k - 0.5).abs() < 1e-9,
            "one hit of two cases → 0.5, got {}",
            r.recall_at_k
        );
        // The hit is at rank 1 → MRR = (1/1 + 0) / 2 = 0.5.
        assert!(
            (r.mrr - 0.5).abs() < 1e-9,
            "rank-1 hit over two cases → 0.5, got {}",
            r.mrr
        );
    }

    #[test]
    fn empty_cases_do_not_divide_by_zero() {
        let conn = setup_db();
        let r = run_recall(&conn, &[], 5).unwrap();
        assert_eq!(
            r,
            RecallReport {
                recall_at_k: 0.0,
                mrr: 0.0,
                n: 0,
                k: 5
            }
        );
    }
}
