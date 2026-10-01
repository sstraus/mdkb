//! Round 2 adversarial cases for the recall rerank stage (story 202-4c67):
//! per-language switches, neutral words, budget boundaries, outcome names.
//! Public `mcp::recall_rerank` surface only; no model.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mdkb::cli::hook_client::HOOK_TIMEOUT_USER_PROMPT_SUBMIT;
use mdkb::config::HooksConfig;
use mdkb::llm::rerank::{RerankError, Reranker};
use mdkb::mcp::recall_rerank::{
    Language, enabled_for, prompt_language, rerank_budget, rerank_stage,
};
use mdkb::store::memory::MemoryEntry;

#[derive(Debug)]
struct Fixed(Result<Vec<f32>, fn() -> RerankError>);

impl Reranker for Fixed {
    fn score(&self, _query: &str, _docs: &[String]) -> Result<Vec<f32>, RerankError> {
        match &self.0 {
            Ok(scores) => Ok(scores.clone()),
            Err(make) => Err(make()),
        }
    }
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

const ITALIAN: &str = "perché il daemon non risponde quando il disco è pieno";
const ENGLISH: &str = "why does the daemon not respond when the disk is full";
const UNKNOWN: &str = "src/mcp/dispatch.rs";

#[test]
fn each_language_reads_only_its_own_switch() {
    // Catches: the two flags swapped, or a bare identifier (language unknown,
    // priced as Italian) following the English switch, so `en = true, it = false`
    // reranks paths and `it = true, en = false` skips them.
    let cases = [
        // (it, en, italian, english, unknown)
        (true, true, true, true, true),
        (true, false, true, false, true),
        (false, true, false, true, false),
        (false, false, false, false, false),
    ];
    for (it, en, want_it, want_en, want_unknown) in cases {
        let mut cfg = HooksConfig::default();
        cfg.recall_rerank_it = it;
        cfg.recall_rerank_en = en;
        let label = format!("it={it} en={en}");
        assert_eq!(
            enabled_for(&cfg, ITALIAN),
            want_it,
            "italian prompt, {label}"
        );
        assert_eq!(
            enabled_for(&cfg, ENGLISH),
            want_en,
            "english prompt, {label}"
        );
        assert_eq!(
            enabled_for(&cfg, UNKNOWN),
            want_unknown,
            "identifier, {label}"
        );
        assert_eq!(cfg.recall_rerank_any(), it || en, "{label}");
    }
}

#[test]
fn words_shared_by_both_languages_never_decide() {
    // Catches: `in` / `me` / `i` counted for one language again. Each prompt has
    // two shared words against one clear word; counting the shared ones flips it.
    assert_eq!(prompt_language("me in the loop"), Language::En);
    assert_eq!(prompt_language("me lo dici in"), Language::It);
    assert_eq!(
        prompt_language("in me i in me i"),
        Language::It,
        "no vote is a tie"
    );
    assert_eq!(prompt_language("i think the cache is stale"), Language::En);
}

#[test]
fn budget_boundary_is_inclusive_at_the_minimum_and_never_underflows() {
    // Catches: `>` for `>=` at the 100 ms minimum, and an elapsed time past the
    // hook's own clock panicking on underflow instead of answering `None`.
    let cfg = HooksConfig::default();
    let client = HOOK_TIMEOUT_USER_PROMPT_SUBMIT.as_millis() as u64;
    let reserve = 150; // docs/hooks.md: what the hook keeps for the work after the rerank
    let at_minimum = client - reserve - 100;
    assert_eq!(
        rerank_budget(&cfg, at_minimum),
        Some(Duration::from_millis(100))
    );
    assert_eq!(rerank_budget(&cfg, at_minimum + 1), None);
    assert_eq!(rerank_budget(&cfg, u64::MAX), None);

    let mut tiny = HooksConfig::default();
    tiny.user_prompt_submit_deadline_ms = 1;
    assert_eq!(rerank_budget(&tiny, 0), None);

    let mut zero = HooksConfig::default();
    zero.recall_rerank_deadline_ms = 0;
    assert_eq!(
        rerank_budget(&zero, 0),
        None,
        "a zero rerank deadline is no budget"
    );
}

#[tokio::test]
async fn every_failure_has_its_own_logged_outcome_and_keeps_minilm() {
    // Catches: two failure kinds sharing one `rerank_outcome` (the log can no
    // longer say whether the weights are missing, loading, busy or broken), or a
    // new variant (`one_shot`) falling into a catch-all.
    let cases: [(fn() -> RerankError, &str); 6] = [
        (|| RerankError::Loading, "loading"),
        (|| RerankError::NotCached(PathBuf::from("x")), "not_cached"),
        (|| RerankError::LoadFailed("x".into()), "load_failed"),
        (|| RerankError::Busy, "busy"),
        (|| RerankError::Failed("x".into()), "failed"),
        (|| RerankError::OneShot, "one_shot"),
    ];
    for (make, outcome) in cases {
        let reranker: Arc<dyn Reranker> = Arc::new(Fixed(Err(make)));
        let stage = rerank_stage(
            &reranker,
            &HooksConfig::default(),
            ITALIAN,
            vec![entry("a")],
            Some(Duration::from_secs(2)),
        )
        .await;
        assert_eq!(stage.outcome, outcome);
        assert!(
            stage.entries.is_none(),
            "{outcome} must keep the MiniLM result"
        );
    }
}

#[tokio::test]
async fn equal_scores_keep_minilm_order() {
    // Catches: an unstable or reversed sort reshuffling entries the reranker
    // could not tell apart, so MiniLM's ranking (the tiebreak) is lost.
    let reranker: Arc<dyn Reranker> = Arc::new(Fixed(Ok(vec![2.0, 2.0, 2.0, 3.0])));
    let pool = ["a", "b", "c", "d"].map(entry).to_vec();
    let stage = rerank_stage(
        &reranker,
        &HooksConfig::default(),
        ITALIAN,
        pool,
        Some(Duration::from_secs(2)),
    )
    .await;
    let ids: Vec<String> = stage.entries.unwrap().into_iter().map(|e| e.id).collect();
    assert_eq!(ids, ["d", "a", "b", "c"]);
}
