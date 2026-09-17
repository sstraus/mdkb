//! Evaluation fixtures: a self-contained corpus of memories plus recall and
//! judge cases, loaded from JSON. The harness seeds a fresh on-disk store from
//! the fixture (reproducible — independent of the live repo), so a recall
//! baseline is stable across machines and runs.

use crate::core::Context;
use crate::error::{Error, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde::Deserialize;
use std::path::Path;

use super::judge::JudgeCase;
use super::recall::EvalCase;
use crate::store::memory::{self, EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};

/// The synthetic corpus shipped in the binary, used when no `--fixture` is given.
const DEFAULT_FIXTURE: &str = include_str!("../../assets/eval/memory-recall.json");

/// A memory to seed into the evaluation store.
#[derive(Debug, Deserialize)]
pub struct FixtureMemory {
    pub id: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A recall case: query + the id(s) a correct top-k must surface.
#[derive(Debug, Deserialize)]
pub struct FixtureRecall {
    pub query: String,
    pub expected_ids: Vec<String>,
}

/// A judge case: question + the answer the retrieved context must support.
#[derive(Debug, Deserialize)]
pub struct FixtureJudge {
    pub question: String,
    pub expected_answer: String,
}

/// A complete evaluation fixture.
#[derive(Debug, Deserialize)]
pub struct Fixture {
    #[serde(default)]
    pub memories: Vec<FixtureMemory>,
    #[serde(default)]
    pub recall: Vec<FixtureRecall>,
    /// In-domain queries no memory answers. See `_negatives_rule` in the JSON:
    /// they carry the corpus vocabulary but not its answers, so the correct
    /// retrieval is the empty set and anything returned is a false positive.
    #[serde(default)]
    pub negatives: Vec<String>,
    #[serde(default)]
    pub judge: Vec<FixtureJudge>,
}

/// A real mdkb store in a scratch directory, seeded from a fixture.
///
/// `ctx` is declared first so the connection closes before the directory is
/// removed.
#[derive(Debug)]
pub struct EvalStore {
    pub ctx: Context,
    _dir: tempfile::TempDir,
}

impl Fixture {
    /// Parse a fixture from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// The synthetic fixture bundled in the binary (machine-independent).
    pub fn bundled() -> Result<Self> {
        Ok(serde_json::from_str(DEFAULT_FIXTURE)?)
    }

    /// Create a real store — the same `Context::init` every `mdkb init` runs,
    /// with sqlite-vec, the vector schema and the production pragmas — and
    /// seed this fixture's memories into it. Embeddings are not generated
    /// here; see [`Fixture::embed`].
    pub fn open_store(&self) -> Result<EvalStore> {
        let dir = tempfile::tempdir()?;
        let ctx = Context::init(dir.path())?;
        self.seed(&ctx.conn)?;
        Ok(EvalStore { ctx, _dir: dir })
    }

    /// Insert this fixture's memories as active `Topic` entries.
    fn seed(&self, conn: &Connection) -> Result<()> {
        let now = Utc::now().timestamp();
        for m in &self.memories {
            add_entry(
                conn,
                &MemoryEntry {
                    id: m.id.clone(),
                    title: m.title.clone(),
                    content: m.content.clone(),
                    entry_type: EntryType::Topic,
                    tags: m.tags.clone(),
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
                    source_type: SourceType::UserStatement,
                    expires_at: None,
                    due_at: None,
                },
            )?;
        }
        Ok(())
    }

    /// Embed every seeded memory through the production backfill (the one
    /// `mdkb update` runs). The caller must have loaded the model already:
    /// the backfill treats a cold model as "retry later" and embeds nothing,
    /// which here would be an eval of the wrong thing, so a short count is an
    /// error.
    pub fn embed(&self, conn: &Connection) -> Result<()> {
        let embedded = memory::backfill_memory_embeddings(conn)?;
        if embedded != self.memories.len() {
            return Err(Error::other(format!(
                "eval store: embedded {embedded} of {} memories",
                self.memories.len()
            )));
        }
        Ok(())
    }

    pub fn recall_cases(&self) -> Vec<EvalCase> {
        self.recall
            .iter()
            .map(|r| EvalCase {
                query: r.query.clone(),
                expected_ids: r.expected_ids.clone(),
            })
            .collect()
    }

    pub fn negative_queries(&self) -> Vec<String> {
        self.negatives.clone()
    }

    pub fn judge_cases(&self) -> Vec<JudgeCase> {
        self.judge
            .iter()
            .map(|j| JudgeCase {
                question: j.question.clone(),
                expected_answer: j.expected_answer.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SearchMemoryConfig;
    use crate::eval::recall::{Mode, Retrieval, run_recall};
    use std::collections::HashSet;

    fn committed_fixture() -> Fixture {
        Fixture::bundled().expect("bundled eval fixture parses")
    }

    /// Lowercased words split on every non-alphanumeric character, so
    /// `code_verifier` and `code verifier` compare equal.
    fn words(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn four_grams(text: &str) -> HashSet<Vec<String>> {
        words(text).windows(4).map(|w| w.to_vec()).collect()
    }

    #[test]
    fn committed_fixture_is_well_formed() {
        let fx = committed_fixture();
        assert!(!fx.memories.is_empty(), "fixture must seed memories");
        assert!(!fx.recall.is_empty(), "fixture must define recall cases");
        // Every expected id must reference a memory that actually exists.
        let ids: HashSet<&str> = fx.memories.iter().map(|m| m.id.as_str()).collect();
        for c in &fx.recall {
            assert!(
                !c.expected_ids.is_empty(),
                "query {:?} expects nothing",
                c.query
            );
            for e in &c.expected_ids {
                assert!(ids.contains(e.as_str()), "recall expects unknown id {e}");
            }
        }
    }

    #[test]
    fn every_memory_has_held_out_queries() {
        // The point of the fixture is to measure retrieval of every memory from
        // several angles, so a memory nobody asks about is a hole in the eval.
        let fx = committed_fixture();
        for m in &fx.memories {
            let asked = fx
                .recall
                .iter()
                .filter(|c| c.expected_ids.contains(&m.id))
                .count();
            assert!(
                asked >= 2,
                "memory {} has {asked} queries, wants >= 2",
                m.id
            );
        }
    }

    /// The authoring rule stated at the top of the fixture file: a query is
    /// written from memory of the topic, not copied from the document. A query
    /// that shares four consecutive words with its target is a copy, and a copy
    /// measures string matching, not retrieval.
    #[test]
    fn held_out_queries_share_no_4gram_with_their_target() {
        let fx = committed_fixture();
        let target_grams = |id: &str| -> HashSet<Vec<String>> {
            let m = fx.memories.iter().find(|m| m.id == id).expect("known id");
            let mut grams = four_grams(&m.title);
            grams.extend(four_grams(&m.content));
            grams
        };
        for c in &fx.recall {
            let query_grams = four_grams(&c.query);
            for id in &c.expected_ids {
                let target = target_grams(id);
                let shared: Vec<_> = query_grams.intersection(&target).collect();
                assert!(
                    shared.is_empty(),
                    "query {:?} copies {:?} from memory {id}",
                    c.query,
                    shared
                );
            }
        }
        // Judge questions retrieve too, so the same rule applies to them
        // against every memory.
        for j in &fx.judge {
            let q = four_grams(&j.question);
            for m in &fx.memories {
                let target = target_grams(&m.id);
                let shared: Vec<_> = q.intersection(&target).collect();
                assert!(
                    shared.is_empty(),
                    "judge question {:?} copies {:?} from memory {}",
                    j.question,
                    shared,
                    m.id
                );
            }
        }
    }

    #[test]
    fn open_store_is_a_real_store_on_disk_with_the_vector_schema() {
        let fx = committed_fixture();
        let store = fx.open_store().unwrap();
        assert!(store.ctx.db_path.exists(), "store must live on disk");
        let vec_tables: i64 = store
            .ctx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'vec_memory'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(vec_tables, 1, "the sqlite-vec memory table must exist");
        let seeded: i64 = store
            .ctx
            .conn
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seeded as usize, fx.memories.len());
    }

    /// The `_negatives_rule` stated in the fixture file, both halves.
    ///
    /// In-domain: a negative has to carry the corpus vocabulary, or it is an
    /// easy negative that any floor rejects and tells the calibration nothing.
    /// Unanswered: it has to obey the same no-4gram rule as the queries, or it
    /// is a positive nobody labelled and the false positive it scores is the
    /// fixture's fault, not the retriever's.
    #[test]
    fn negatives_are_in_domain_and_answered_by_no_memory() {
        let fx = committed_fixture();
        assert!(
            (30..=50).contains(&fx.negatives.len()),
            "the tau calibration wants 30 to 50 negatives, found {}",
            fx.negatives.len()
        );
        let mut corpus_words: HashSet<String> = HashSet::new();
        let mut corpus_grams: HashSet<Vec<String>> = HashSet::new();
        for m in &fx.memories {
            for field in [&m.title, &m.content] {
                corpus_words.extend(words(field));
                corpus_grams.extend(four_grams(field));
            }
        }
        for q in &fx.negatives {
            let in_domain = words(q).into_iter().any(|w| {
                corpus_words.contains(&w) && !crate::store::search::STOPWORDS.contains(&w.as_str())
            });
            assert!(
                in_domain,
                "negative {q:?} shares no content word with any memory, so it is \
                 not in-domain"
            );
            let grams = four_grams(q);
            let leaked: Vec<_> = grams.intersection(&corpus_grams).collect();
            assert!(
                leaked.is_empty(),
                "negative {q:?} copies {leaked:?} from a memory, so it is an \
                 unlabelled positive"
            );
        }
        // A negative must not also be listed as a query with an answer.
        let asked: HashSet<&str> = fx.recall.iter().map(|c| c.query.as_str()).collect();
        for q in &fx.negatives {
            assert!(
                !asked.contains(q.as_str()),
                "{q:?} is both positive and negative"
            );
        }
    }

    /// Run the committed fixture in `mode` through a real store.
    fn baseline(mode: Mode) -> crate::eval::recall::RecallReport {
        let fx = committed_fixture();
        let store = fx.open_store().unwrap();
        let cfg = SearchMemoryConfig::default();
        let embedder = if mode.needs_model() {
            let svc = crate::llm::get_cached_service().expect("model");
            fx.embed(&store.ctx.conn).unwrap();
            Some(svc)
        } else {
            None
        };
        let retrieval = Retrieval {
            mode,
            embedder: embedder.as_deref(),
            memory_cfg: &cfg,
        };
        run_recall(
            &store.ctx.conn,
            &retrieval,
            &fx.recall_cases(),
            &fx.negative_queries(),
            5,
        )
        .unwrap()
    }

    /// The precision-recall curve over the cosine floor.
    ///
    /// This is the measurement `config::MIN_RECALL_COSINE_DEFAULT` comes from:
    /// 36 held-out queries with an answer against 40 in-domain negatives with
    /// none, in the production hybrid mode, at every floor from 0.00 to 0.90.
    /// Re-run it (`--ignored`) after any change to the fixture, the fusion
    /// weights or the lexical admission arm, and record the table in
    /// `docs/retrieval-eval.md`.
    #[test]
    #[ignore = "requires ONNX model download"]
    fn print_the_precision_recall_curve_over_tau() {
        let fx = committed_fixture();
        let store = fx.open_store().unwrap();
        let svc = crate::llm::get_cached_service().expect("model");
        fx.embed(&store.ctx.conn).unwrap();
        let cases = fx.recall_cases();
        let negatives = fx.negative_queries();

        println!("  tau  recall@5  precision  hits  false positives");
        let mut curve = Vec::new();
        for step in 0..=18 {
            let tau = step as f32 * 0.05;
            let cfg = SearchMemoryConfig {
                min_recall_cosine: tau,
                ..SearchMemoryConfig::default()
            };
            let retrieval = Retrieval {
                mode: Mode::Hybrid,
                embedder: Some(svc.as_ref()),
                memory_cfg: &cfg,
            };
            let r = run_recall(&store.ctx.conn, &retrieval, &cases, &negatives, 5).unwrap();
            println!(
                " {tau:.2}     {:.3}      {:.3}    {:>2}  {}",
                r.recall_at_k,
                r.precision.expect("negatives are labelled"),
                r.n - r.misses.len(),
                r.false_positives.len()
            );
            curve.push((tau, r));
        }

        // The rule `MIN_RECALL_COSINE_DEFAULT` was chosen by: the lowest floor
        // on this curve that admits no labelled negative. Asserting the rule
        // rather than the number is what keeps the constant derived — change
        // the fixture and this fails until the constant is re-read off it.
        let chosen = crate::config::MIN_RECALL_COSINE_DEFAULT;
        let clean = |r: &crate::eval::recall::RecallReport| r.false_positives.is_empty();
        let lowest_clean = curve
            .iter()
            .find(|(_, r)| clean(r))
            .map(|(tau, _)| *tau)
            .expect("some floor on the curve admits no negative");
        assert!(
            (lowest_clean - chosen).abs() < 0.001,
            "MIN_RECALL_COSINE_DEFAULT is {chosen}, but the lowest floor that \
             admits no labelled negative is {lowest_clean}"
        );
        let at_chosen = curve
            .iter()
            .find(|(tau, _)| (*tau - chosen).abs() < 0.001)
            .map(|(_, r)| r)
            .expect("the chosen floor is a point on the curve");
        assert_eq!(at_chosen.precision, Some(1.0));
        assert!(
            at_chosen.recall_at_k >= 0.58,
            "recall@5 at the chosen floor fell to {}",
            at_chosen.recall_at_k
        );
    }

    /// Floors sit just under the numbers recorded in docs/retrieval-eval.md.
    /// A drop below one means retrieval regressed (or the fixture changed and
    /// the doc must be updated with it).
    /// BM25 alone surfaces one entry of 36 on this fixture, and nothing at all
    /// for the 40 negatives. That is the point of the floor.
    ///
    /// A BM25-only run supplies no embedding, so the distance arm of
    /// `hybrid::admits` is unavailable and a strong lexical match is the only
    /// way in. Every query and every negative reaches the BM25 result set —
    /// the expression is OR-expanded and the negatives are in-domain by
    /// construction — so this is the direct measurement of the story's
    /// constraint: membership in that set is not evidence.
    ///
    /// The one hit, `proof key for code exchange in the authorization grant`,
    /// shares two rare terms (`authorization`, `exchange`) with the oauth
    /// entry, which is the rare-term arm firing on real evidence. It used to
    /// score 0.167 (6 of 36) by ranking whatever BM25 returned.
    #[test]
    fn committed_fixture_bm25_baseline_holds() {
        let r = baseline(Mode::Bm25);
        assert_eq!(r.n, 36);
        assert_eq!(r.n_negatives, 40);
        // Counted on the miss list, not on the float: the exact statement is
        // "35 of 36 queries were rejected", and `recall_at_k` is a type that
        // should not be compared for equality.
        assert_eq!(
            r.misses.len(),
            35,
            "without an embedding only a strong lexical match may be admitted, \
             yet {} of {} queries got through (recall@5 {:.3})",
            r.n - r.misses.len(),
            r.n,
            r.recall_at_k
        );
        assert_eq!(
            r.false_positives,
            Vec::<String>::new(),
            "every negative is in the OR-expanded BM25 result set; membership \
             there must not admit it"
        );
    }

    /// Both model modes are floored at recall AND at precision.
    ///
    /// Recall alone stopped being a sufficient guard when the absolute cosine
    /// floor went in: removing the floor raises recall to 1.000 and would
    /// sail past a recall floor while putting back the behavior story 083
    /// exists to remove. The pair has to hold together — 0.583 recall@5 at
    /// precision 1.000 over the 40 labelled negatives, the operating point
    /// `MIN_RECALL_COSINE_DEFAULT` was read off (`docs/retrieval-eval.md`).
    #[test]
    #[ignore = "requires ONNX model download"]
    fn committed_fixture_embedding_baseline_holds() {
        assert_baseline(Mode::Embedding);
    }

    #[test]
    #[ignore = "requires ONNX model download"]
    fn committed_fixture_hybrid_baseline_holds() {
        assert_baseline(Mode::Hybrid);
    }

    fn assert_baseline(mode: Mode) {
        let r = baseline(mode);
        assert!(
            r.recall_at_k >= 0.58,
            "{} recall@5 dropped to {} (misses: {:?})",
            mode.as_str(),
            r.recall_at_k,
            r.misses
        );
        assert_eq!(
            r.precision,
            Some(1.0),
            "{} admitted a labelled negative: {:?}",
            mode.as_str(),
            r.false_positives
        );
    }
}
