//! Adversarial cases for the recall rerank stage (story 202-4c67), against the
//! public `mcp::recall_rerank` surface with a scripted `Reranker`. No model.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mdkb::config::HooksConfig;
use mdkb::llm::rerank::{RerankError, Reranker};
use mdkb::mcp::recall_rerank::{Language, prompt_language, rerank_stage};
use mdkb::store::memory::MemoryEntry;

type Script = Box<dyn Fn(&[String]) -> Result<Vec<f32>, RerankError> + Send + Sync>;

struct Scripted {
    calls: AtomicUsize,
    script: Script,
}

impl std::fmt::Debug for Scripted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Scripted")
    }
}

impl Reranker for Scripted {
    fn score(&self, _query: &str, docs: &[String]) -> Result<Vec<f32>, RerankError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.script)(docs)
    }
}

fn scripted(
    script: impl Fn(&[String]) -> Result<Vec<f32>, RerankError> + Send + Sync + 'static,
) -> (Arc<Scripted>, Arc<dyn Reranker>) {
    let concrete = Arc::new(Scripted {
        calls: AtomicUsize::new(0),
        script: Box::new(script),
    });
    let erased: Arc<dyn Reranker> = concrete.clone();
    (concrete, erased)
}

fn entry(id: &str) -> MemoryEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "title": format!("title {id}"),
        "content": format!("content {id}"),
        "entry_type": "topic",
        "tags": [],
        "status": "active",
        "created_at": 0,
        "updated_at": 0,
        "superseded_by": null,
        "access_count": 0,
        "last_accessed": null,
    }))
    .unwrap()
}

fn pool(n: usize) -> Vec<MemoryEntry> {
    (0..n).map(|i| entry(&format!("e{i}"))).collect()
}

fn ids(entries: &[MemoryEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.id.as_str()).collect()
}

const BUDGET: Option<Duration> = Some(Duration::from_millis(200));

#[test]
fn an_italian_sentence_full_of_the_article_i_is_not_read_as_english() {
    // Catches: `i` listed as an English pronoun, so "controlla i test e i log"
    // votes English 2-1 and the Italian prompt is held to the English floor
    // (-1.95) instead of the Italian one (-1.05) — the gate that keeps the 38
    // fitted Italian negatives out.
    assert_eq!(prompt_language("controlla i test e i log"), Language::It);
    assert_eq!(
        prompt_language("mostra i test i log e i report"),
        Language::It
    );
}

#[tokio::test]
async fn a_reranker_that_outlasts_its_budget_is_abandoned_not_awaited() {
    // Catches: awaiting the blocking task past the budget, which hands the hook
    // back to the client after its 1 s socket timeout.
    let (_, reranker) = scripted(|docs| {
        std::thread::sleep(Duration::from_millis(1500));
        Ok(vec![0.0; docs.len()])
    });
    let started = Instant::now();
    let stage = rerank_stage(
        &reranker,
        &HooksConfig::default(),
        "how does the daemon reload",
        pool(3),
        Some(Duration::from_millis(100)),
    )
    .await;
    assert_eq!(stage.outcome, "timeout");
    assert!(stage.entries.is_none());
    assert!(started.elapsed() < Duration::from_millis(700));
}

#[tokio::test]
async fn a_panicking_reranker_falls_back_instead_of_propagating() {
    // Catches: a model panic surfacing through the JoinHandle as a hook crash
    // or as an empty (suppressing) answer instead of the MiniLM fallback.
    let (_, reranker) = scripted(|_| panic!("ort exploded"));
    let stage = rerank_stage(
        &reranker,
        &HooksConfig::default(),
        "the daemon",
        pool(2),
        BUDGET,
    )
    .await;
    assert_eq!(stage.outcome, "failed");
    assert!(stage.entries.is_none());
}

#[tokio::test]
async fn a_score_list_of_the_wrong_length_is_a_failure_not_a_partial_answer() {
    // Catches: zipping 4 scores onto 5 entries and injecting the truncated pair.
    let (_, reranker) = scripted(|docs| Ok(vec![5.0; docs.len() - 1]));
    let stage = rerank_stage(
        &reranker,
        &HooksConfig::default(),
        "the daemon",
        pool(5),
        BUDGET,
    )
    .await;
    assert_eq!(stage.outcome, "failed");
    assert!(stage.entries.is_none());
}

#[tokio::test]
async fn every_reranker_error_leaves_the_minilm_result_in_place() {
    // Catches: one RerankError variant mapped to `Some(vec![])`, which replaces
    // MiniLM's admitted entries with nothing.
    for error in [
        RerankError::Loading,
        RerankError::NotCached("/nowhere".into()),
        RerankError::LoadFailed("x".into()),
        RerankError::Busy,
        RerankError::Failed("x".into()),
    ] {
        let expected = error.outcome();
        let slot = std::sync::Mutex::new(Some(error));
        let (_, reranker) = scripted(move |_| Err(slot.lock().unwrap().take().unwrap()));
        let stage = rerank_stage(
            &reranker,
            &HooksConfig::default(),
            "the daemon",
            pool(2),
            BUDGET,
        )
        .await;
        assert_eq!(stage.outcome, expected);
        assert!(stage.entries.is_none(), "{expected} must fall back");
    }
}

#[tokio::test]
async fn non_finite_scores_are_a_failure_not_a_below_gate_verdict() {
    // Catches: a model that emits NaN for every pair being logged as
    // `below_gate` and answered with `Some(vec![])`, silently discarding the
    // MiniLM result for every prompt until the daemon restarts.
    let (_, reranker) = scripted(|docs| Ok(vec![f32::NAN; docs.len()]));
    let stage = rerank_stage(
        &reranker,
        &HooksConfig::default(),
        "the daemon",
        pool(3),
        BUDGET,
    )
    .await;
    assert!(
        stage.entries.is_none(),
        "outcome {} replaced the MiniLM result with {:?}",
        stage.outcome,
        stage.entries.as_ref().map(|e| ids(e))
    );
}

#[tokio::test]
async fn an_empty_pool_does_not_wake_the_model() {
    // Catches: the stage calling the reranker for zero candidates (the pool is
    // empty when every hit was disputed), which starts a 1 GB model load for
    // nothing.
    let (concrete, reranker) = scripted(|docs| Ok(vec![0.0; docs.len()]));
    let _ = rerank_stage(
        &reranker,
        &HooksConfig::default(),
        "the daemon",
        Vec::new(),
        BUDGET,
    )
    .await;
    assert_eq!(concrete.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_floor_is_inclusive_and_admitted_entries_come_best_first() {
    // Catches: `>` for `>=` (an entry exactly at the fitted floor dropped) and
    // an admitted list in pool order instead of score order.
    let cfg = HooksConfig::default();
    let floor = cfg.recall_rerank_min_score_en;
    let scores = vec![floor - 0.01, floor, floor + 3.0, floor + 1.0];
    let (_, reranker) = scripted(move |_| Ok(scores.clone()));
    let stage = rerank_stage(&reranker, &cfg, "what is the daemon", pool(4), BUDGET).await;
    assert_eq!(stage.outcome, "ok");
    assert_eq!(ids(&stage.entries.unwrap()), ["e2", "e3", "e1"]);
}

#[tokio::test]
async fn no_budget_never_reaches_the_reranker() {
    // Catches: a spent hook budget still starting a blocking task that is
    // abandoned at once and holds the RUNNING flag for the next prompts.
    let (concrete, reranker) = scripted(|docs| Ok(vec![0.0; docs.len()]));
    let stage = rerank_stage(
        &reranker,
        &HooksConfig::default(),
        "the daemon",
        pool(2),
        None,
    )
    .await;
    assert_eq!(stage.outcome, "no_budget");
    assert_eq!(concrete.calls.load(Ordering::SeqCst), 0);
}
