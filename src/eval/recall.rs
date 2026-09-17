//! Recall-quality evaluation for memory retrieval (LoCoMo-style).
//!
//! Every query goes through the production memory search
//! (`store::memory::search_entries_hybrid_fts`: BM25 leg, vector leg, RRF
//! fusion, access-recency signal, absolute admission, confidence re-rank) with
//! the production `[search.memory]` weights AND the production FTS expression,
//! so the CLI, the MCP tool and the hook are all measured at once. The only
//! knob is [`Mode`], which decides which legs get input. This is the yardstick
//! every retrieval change is measured against — a change that drops recall@k
//! or precision is a regression, not a win.

use crate::config::SearchMemoryConfig;
use crate::error::{Error, Result};
use crate::llm::EmbeddingService;
use crate::store::memory::{self, MemoryEntry};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Which retrieval legs receive the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// FTS5 leg only: no query embedding, the fallback production takes when
    /// the model is cold.
    Bm25,
    /// Vector leg only: the BM25 leg is starved with a term no memory holds,
    /// so fusion and re-rank still run, over the vector candidates alone.
    Embedding,
    /// Both legs, fused — production with a warm model.
    Hybrid,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Bm25, Mode::Embedding, Mode::Hybrid];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Bm25 => "bm25",
            Mode::Embedding => "embedding",
            Mode::Hybrid => "hybrid",
        }
    }

    /// True when the mode cannot run without the ONNX model.
    pub fn needs_model(self) -> bool {
        !matches!(self, Mode::Bm25)
    }
}

impl std::str::FromStr for Mode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Mode::ALL
            .into_iter()
            .find(|m| m.as_str() == s)
            .ok_or_else(|| Error::other(format!("unknown eval mode '{s}'")))
    }
}

/// An FTS5 term that no fixture memory contains. Feeding it to the BM25 leg
/// yields zero BM25 candidates without tripping the empty-query short-circuit,
/// which is how [`Mode::Embedding`] isolates the vector leg inside the
/// production function instead of calling the vector store directly.
const NO_BM25_MATCH: &str = "\"mdkbevalnobm25leg\"";

/// One production retrieval configuration: which legs run, with what weights.
#[derive(Debug)]
pub struct Retrieval<'a> {
    pub mode: Mode,
    /// Required by every mode that [`Mode::needs_model`].
    pub embedder: Option<&'a EmbeddingService>,
    /// Production `[search.memory]` weights (access-recency signal).
    pub memory_cfg: &'a SearchMemoryConfig,
}

impl Retrieval<'_> {
    /// BM25 only, with the production defaults — needs no model.
    pub fn bm25(memory_cfg: &SearchMemoryConfig) -> Retrieval<'_> {
        Retrieval {
            mode: Mode::Bm25,
            embedder: None,
            memory_cfg,
        }
    }

    /// Run one query through the production memory search.
    ///
    /// `fts_query` is the caller's escaped FTS5 expression for `text` (token-AND
    /// for a search-tool query, OR-expanded for a prompt); `text` is what gets
    /// embedded, exactly as the MCP layer embeds the raw query.
    pub fn search(
        &self,
        conn: &Connection,
        fts_query: &str,
        text: &str,
        k: usize,
    ) -> Result<Vec<MemoryEntry>> {
        let embedding = if self.mode.needs_model() {
            let svc = self.embedder.ok_or_else(|| {
                Error::other(format!(
                    "eval mode {} needs an embedder",
                    self.mode.as_str()
                ))
            })?;
            Some(svc.embed_query(text)?)
        } else {
            None
        };
        let fts = if self.mode == Mode::Embedding {
            NO_BM25_MATCH
        } else {
            fts_query
        };
        memory::search_entries_hybrid_fts(
            conn,
            fts,
            text,
            embedding.as_deref(),
            k,
            None,
            self.memory_cfg,
        )
        .map(|results| results.into_iter().map(|result| result.entry).collect())
    }
}

/// One evaluation query: a natural-language prompt and the id(s) a correct
/// retrieval must surface within the top-k results.
#[derive(Debug, Clone)]
pub struct EvalCase {
    pub query: String,
    pub expected_ids: Vec<String>,
}

/// Aggregate recall metrics over a case set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecallReport {
    pub mode: Mode,
    pub recall_at_k: f64,
    pub mrr: f64,
    /// Share of the queries that retrieved something which were supposed to:
    /// `hits / (hits + false_positives)`. Recall alone cannot judge the
    /// absolute relevance floor — a floor of zero scores perfect recall by
    /// answering every query, including the ones with no answer.
    ///
    /// `None` when the fixture labelled no negatives, and also when nothing
    /// was retrieved for any query at all. In the first case the formula
    /// reduces to `hits / hits` and a flat 1.000 would read as a measurement
    /// of a floor that was never tested; in the second there is nothing to be
    /// precise about, and a floor that answers no query is silent rather than
    /// accurate.
    pub precision: Option<f64>,
    pub n: usize,
    /// How many labelled negatives were scored. Zero means precision was
    /// measured against positives only and carries no information.
    pub n_negatives: usize,
    pub k: usize,
    /// The queries whose expected id did not appear in the top-k, so a drop in
    /// `recall_at_k` names the cases that caused it.
    pub misses: Vec<String>,
    /// The labelled negatives that retrieved an entry anyway, so a drop in
    /// `precision` names the queries that caused it.
    pub false_positives: Vec<String>,
}

/// Compute recall@k, MRR and precision over `cases` and `negatives`.
///
/// A case is a hit if any of its `expected_ids` appears in the top-k retrieved
/// entries; MRR credits the reciprocal rank of the first such hit. An empty
/// case set yields zeroed metrics (never a divide-by-zero).
///
/// `negatives` are in-domain queries no stored entry answers, so the correct
/// retrieval for one is the empty set and anything it returns is a false
/// positive. They are what makes the absolute relevance floor measurable:
/// without them, dropping the floor to zero looks like a pure win.
///
/// Each query is OR-expanded with [`crate::store::search::build_recall_query`],
/// the one expression every memory surface now builds: the CLI
/// `search --scope memory`, the MCP `search` tool with `scope: memory` and the
/// `UserPromptSubmit` hook. The number this reports is therefore what an agent
/// gets, whichever door it came through.
pub fn run_recall(
    conn: &Connection,
    retrieval: &Retrieval<'_>,
    cases: &[EvalCase],
    negatives: &[String],
    k: usize,
) -> Result<RecallReport> {
    let mut hits = 0usize;
    let mut reciprocal_rank = 0f64;
    let mut misses = Vec::new();
    for case in cases {
        let Some(fts) = crate::store::search::build_recall_query(&case.query) else {
            misses.push(case.query.clone());
            continue;
        };
        let results = retrieval.search(conn, &fts, &case.query, k)?;
        match results
            .iter()
            .take(k)
            .position(|e| case.expected_ids.contains(&e.id))
        {
            Some(pos) => {
                hits += 1;
                reciprocal_rank += 1.0 / (pos as f64 + 1.0);
            }
            None => misses.push(case.query.clone()),
        }
    }
    let mut false_positives = Vec::new();
    for query in negatives {
        let Some(fts) = crate::store::search::build_recall_query(query) else {
            continue;
        };
        if !retrieval.search(conn, &fts, query, k)?.is_empty() {
            false_positives.push(query.clone());
        }
    }
    let n = cases.len();
    let denom = n.max(1) as f64;
    let admitted = hits + false_positives.len();
    Ok(RecallReport {
        mode: retrieval.mode,
        recall_at_k: hits as f64 / denom,
        mrr: reciprocal_rank / denom,
        precision: (!negatives.is_empty() && admitted > 0).then(|| hits as f64 / admitted as f64),
        n,
        n_negatives: negatives.len(),
        k,
        misses,
        false_positives,
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
            // Clear hit: the three tokens run consecutively in the oauth entry,
            // which is what the absolute recall gate wants from a BM25-only run
            // (no embedding → no distance → strong lexical is the only arm).
            EvalCase {
                query: "authorization code exchange".into(),
                expected_ids: vec!["oauth".into()],
            },
            // Clear miss: neither token is present in any entry.
            EvalCase {
                query: "kubernetes helm".into(),
                expected_ids: vec!["oauth".into()],
            },
        ];

        let cfg = SearchMemoryConfig::default();
        let r = run_recall(&conn, &Retrieval::bm25(&cfg), &cases, &[], 5).unwrap();
        assert_eq!(r.mode, Mode::Bm25);
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
        // The report names the query that failed, not just the count.
        assert_eq!(r.misses, vec!["kubernetes helm".to_string()]);
        // No labelled negatives were scored, so precision carries no signal
        // and says so with `None` rather than a flattering 1.000.
        assert_eq!(r.n_negatives, 0);
        assert_eq!(r.precision, None);
    }

    /// Precision is the metric the absolute relevance floor is tuned against,
    /// and it has to move when recall does not. Both queries here retrieve,
    /// one of them is labelled as having no answer: recall stays perfect and
    /// precision halves.
    #[test]
    fn a_negative_that_retrieves_an_entry_costs_precision_not_recall() {
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

        let cases = vec![EvalCase {
            query: "authorization code exchange".into(),
            expected_ids: vec!["oauth".into()],
        }];
        // Quotes the cache entry's own words, so the floor admits it — but it
        // is labelled as a query no entry should answer.
        let negatives = vec!["least recently used eviction".to_string()];

        let cfg = SearchMemoryConfig::default();
        let r = run_recall(&conn, &Retrieval::bm25(&cfg), &cases, &negatives, 5).unwrap();
        assert!(r.misses.is_empty(), "the positive case is a hit");
        assert!(
            (r.mrr - 1.0).abs() < 1e-9,
            "the hit is at rank 1 → MRR 1.0, got {}",
            r.mrr
        );
        assert_eq!(r.n_negatives, 1);
        assert_eq!(r.false_positives, negatives);
        assert_eq!(
            r.precision,
            Some(0.5),
            "one hit against one false positive → 0.5"
        );
    }

    #[test]
    fn empty_cases_do_not_divide_by_zero() {
        let conn = setup_db();
        let cfg = SearchMemoryConfig::default();
        let r = run_recall(&conn, &Retrieval::bm25(&cfg), &[], &[], 5).unwrap();
        assert_eq!(
            r,
            RecallReport {
                mode: Mode::Bm25,
                recall_at_k: 0.0,
                mrr: 0.0,
                precision: None,
                n: 0,
                n_negatives: 0,
                k: 5,
                misses: vec![],
                false_positives: vec![],
            }
        );
    }

    #[test]
    fn a_model_mode_without_an_embedder_is_an_error_not_a_silent_bm25_run() {
        let conn = setup_db();
        let cfg = SearchMemoryConfig::default();
        let retrieval = Retrieval {
            mode: Mode::Hybrid,
            embedder: None,
            memory_cfg: &cfg,
        };
        let err = retrieval.search(&conn, "\"x\"", "x", 5).unwrap_err();
        assert!(
            err.to_string().contains("needs an embedder"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn mode_round_trips_through_its_name() {
        for m in Mode::ALL {
            assert_eq!(m.as_str().parse::<Mode>().unwrap(), m);
        }
        assert!("vector".parse::<Mode>().is_err());
        assert!(!Mode::Bm25.needs_model());
        assert!(Mode::Embedding.needs_model());
        assert!(Mode::Hybrid.needs_model());
    }
}
