//! The rerank step of automatic recall: which language a prompt is in, what
//! score that language needs, and what the hook does with the reranker's
//! answer, its silence, or its failure.
//!
//! The retrieval is unchanged. MiniLM's hybrid search hands over its top five
//! with the cosine gate off; the cross-encoder (`llm::rerank`) scores the five
//! against the prompt; the entries at or above the language's floor are
//! injected. Anything short of a score — loading, timeout, error — leaves the
//! caller with the result MiniLM's own gate produced, which is the behavior
//! before the reranker existed.

use std::sync::Arc;
use std::time::Duration;

use crate::config::HooksConfig;
use crate::llm::rerank::Reranker;
use crate::store::memory::MemoryEntry;

/// How many candidates the reranker scores. The eval measured five: ten cost
/// twice the latency for a recall gain that did not reach significance.
pub const RERANK_POOL_SIZE: usize = 5;

/// What the hook keeps back from the reranker's budget for the work after it:
/// recall ledger write, enrichment, doc neighbors, prior block. Measured on
/// the Mac in `docs/recall-options-eval.md`.
const RESERVE_AFTER_RERANK_MS: u64 = 150;

/// Below this the reranker cannot finish a five-candidate call, so starting one
/// only burns cores the rest of the hook wants.
const MIN_RERANK_BUDGET_MS: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    It,
    En,
}

/// Function words of each language. A word that sits in both lists (`in`, `me`,
/// `i`: the Italian plural article and the English pronoun) votes for neither; words that are a function word in one language and a
/// content word in the other (`come`, `so`) are listed only where they are
/// function words.
const IT_WORDS: &[&str] = &[
    "il", "lo", "gli", "un", "uno", "una", "di", "del", "della", "dei", "delle", "che", "per",
    "con", "non", "sono", "come", "perche", "perché", "cosa", "dove", "quando", "anche", "ma",
    "mi", "ti", "ci", "si", "è", "e", "nel", "nella", "sul", "sulla", "alla", "alle", "questo",
    "questa", "quello", "puoi", "posso", "devo", "dobbiamo", "vorrei", "fare", "hai", "ho", "ha",
    "abbiamo", "tutto", "tutti", "più", "già", "dimmi", "ricordi", "adesso", "ora", "qui",
    "sempre", "cioè", "però", "allora", "quindi", "poi", "se", "mio", "tuo", "nostro", "suo", "le",
    "la", "in", "me", "i",
];

const EN_WORDS: &[&str] = &[
    "the", "is", "are", "was", "were", "of", "to", "and", "for", "with", "what", "how", "why",
    "when", "where", "does", "do", "did", "we", "you", "can", "could", "should", "would", "this",
    "that", "it", "in", "on", "not", "have", "has", "had", "be", "been", "my", "our", "your",
    "which", "who", "an", "or", "but", "if", "then", "from", "about", "there", "here", "just",
    "also", "will", "i", "me", "us",
];

/// The language of `prompt`, by a vote of function words.
///
/// A tie — including a prompt with no function words at all, such as a bare
/// identifier — is Italian, which carries the higher floor: when the language
/// is unknown, the gate that admits fewer prompts is the safe one.
pub fn prompt_language(prompt: &str) -> Language {
    let (mut it, mut en) = (0usize, 0usize);
    for word in prompt
        .split(|c: char| !c.is_alphabetic() && c != '\'')
        .filter(|w| !w.is_empty())
    {
        let lower = word.to_lowercase();
        let lower = lower.as_str();
        if IT_WORDS.contains(&lower) && !EN_WORDS.contains(&lower) {
            it += 1;
        } else if EN_WORDS.contains(&lower) && !IT_WORDS.contains(&lower) {
            en += 1;
        }
    }
    if en > it { Language::En } else { Language::It }
}

/// Whether `prompt`'s language has the reranker switched on.
pub fn enabled_for(cfg: &HooksConfig, prompt: &str) -> bool {
    match prompt_language(prompt) {
        Language::It => cfg.recall_rerank_it,
        Language::En => cfg.recall_rerank_en,
    }
}

/// The top score `prompt` must reach to inject recall.
pub fn min_score_for(cfg: &HooksConfig, prompt: &str) -> f32 {
    match prompt_language(prompt) {
        Language::It => cfg.recall_rerank_min_score_it,
        Language::En => cfg.recall_rerank_min_score_en,
    }
}

/// How long the reranker may run, or `None` when what is left of the hook
/// cannot hold a call.
///
/// `elapsed_ms` is the hook's own clock. When the hook deadline fires the host
/// gets nothing, which is worse than the MiniLM fallback. The hook client waits
/// that deadline plus a margin (`HooksConfig::user_prompt_submit_effective_deadline_ms`),
/// so this one value is the ceiling, also when the deadline is `0`. The
/// reranker's budget is what is left of it, after the work behind it.
pub fn rerank_budget(cfg: &HooksConfig, elapsed_ms: u64) -> Option<Duration> {
    let ceiling = cfg.user_prompt_submit_effective_deadline_ms();
    let budget = cfg.recall_rerank_deadline_ms.min(
        ceiling
            .saturating_sub(elapsed_ms)
            .saturating_sub(RESERVE_AFTER_RERANK_MS),
    );
    (budget >= MIN_RERANK_BUDGET_MS).then(|| Duration::from_millis(budget))
}

/// What the rerank step decided.
#[derive(Debug)]
pub struct RerankStage {
    /// The `rerank_outcome` for `hook-events.jsonl`.
    pub outcome: &'static str,
    /// The entries to inject in place of the MiniLM result, best first. `None`
    /// keeps the MiniLM result: the reranker gave no answer.
    pub entries: Option<Vec<MemoryEntry>>,
}

impl RerankStage {
    fn fallback(outcome: &'static str) -> Self {
        Self {
            outcome,
            entries: None,
        }
    }
}

/// Score `pool` against `prompt` within `budget` and keep the entries at or
/// above the prompt's language floor.
///
/// The gate is the top score. Every entry that clears the floor is injected, in
/// score order; the ones that do not are left out even when the top one
/// passes. Measured on the 2026-09-30 eval pool, admitting all five once the
/// top passes injects 75 entries for 14 Italian hits, and per-entry only 20 for
/// 12.
pub async fn rerank_stage(
    reranker: &Arc<dyn Reranker>,
    cfg: &HooksConfig,
    prompt: &str,
    pool: Vec<MemoryEntry>,
    budget: Option<Duration>,
) -> RerankStage {
    // Before anything else: an empty pool must not wake the model, whose first
    // call starts a 1 GB load.
    if pool.is_empty() {
        return RerankStage::fallback("no_candidates");
    }
    let Some(budget) = budget else {
        return RerankStage::fallback("no_budget");
    };
    let docs: Vec<String> = pool
        .iter()
        .map(|e| format!("{} {}", e.title, e.content))
        .collect();
    let (reranker, query) = (Arc::clone(reranker), prompt.to_string());
    let run = tokio::task::spawn_blocking(move || reranker.score(&query, &docs));
    let scores = match tokio::time::timeout(budget, run).await {
        Err(_) => return RerankStage::fallback("timeout"),
        Ok(Err(_)) => return RerankStage::fallback("failed"),
        Ok(Ok(Err(error))) => return RerankStage::fallback(error.outcome()),
        Ok(Ok(Ok(scores))) => scores,
    };
    // A wrong-length or NaN answer is a broken model, not a verdict: `below_gate`
    // would replace MiniLM's result with nothing for every prompt.
    if scores.len() != pool.len() || scores.iter().any(|score| !score.is_finite()) {
        return RerankStage::fallback("failed");
    }
    let floor = min_score_for(cfg, prompt);
    let mut scored: Vec<(f32, MemoryEntry)> = scores.into_iter().zip(pool).collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let admitted: Vec<MemoryEntry> = scored
        .into_iter()
        .filter(|(score, _)| *score >= floor)
        .map(|(_, entry)| entry)
        .collect();
    RerankStage {
        outcome: if admitted.is_empty() {
            "below_gate"
        } else {
            "ok"
        },
        entries: Some(admitted),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_follows_the_function_words() {
        assert_eq!(
            prompt_language("come faccio a far parlare in italiano il daemon?"),
            Language::It
        );
        assert_eq!(
            prompt_language("how does the daemon reload its config when it changes"),
            Language::En
        );
    }

    #[test]
    fn unknown_language_takes_the_stricter_floor() {
        // Catches: a bare identifier or a path reading as English and getting
        // the floor 0.9 lower, which admits the negatives the Italian fit set
        // measured between the two.
        let cfg = HooksConfig::default();
        assert!(cfg.recall_rerank_min_score_it > cfg.recall_rerank_min_score_en);
        assert_eq!(prompt_language("src/mcp/dispatch.rs"), Language::It);
        assert_eq!(
            min_score_for(&cfg, "src/mcp/dispatch.rs").to_bits(),
            cfg.recall_rerank_min_score_it.to_bits()
        );
        assert_eq!(prompt_language(""), Language::It);
    }

    #[test]
    fn an_english_sentence_with_one_italian_word_is_still_english() {
        // `ma` is Italian but outvoted.
        assert_eq!(
            prompt_language("what do we know about the oauth flow ma only for mobile"),
            Language::En
        );
    }

    #[test]
    fn budget_is_clamped_to_what_the_hook_has_left() {
        // Catches: the reranker holding its own 700 ms after the context phase
        // already spent most of the hook, so the hook deadline or the client's
        // socket timeout fires and the prompt gets no recall at all instead of
        // MiniLM's.
        let cfg = HooksConfig::default();
        assert_eq!(rerank_budget(&cfg, 0), Some(Duration::from_millis(700)));
        let deadline = cfg.user_prompt_submit_deadline_ms;
        let left = deadline - 200 - RESERVE_AFTER_RERANK_MS;
        assert_eq!(rerank_budget(&cfg, 200), Some(Duration::from_millis(left)));
        assert_eq!(rerank_budget(&cfg, deadline - 100), None);
        assert_eq!(rerank_budget(&cfg, 10_000), None);
    }

    #[test]
    fn a_short_hook_deadline_clamps_the_budget() {
        let cfg = HooksConfig {
            user_prompt_submit_deadline_ms: 500,
            ..HooksConfig::default()
        };
        assert_eq!(
            rerank_budget(&cfg, 100),
            Some(Duration::from_millis(500 - 100 - RESERVE_AFTER_RERANK_MS))
        );
    }

    #[test]
    fn hook_deadline_zero_still_stops_at_the_default_deadline() {
        // Catches: `deadline = 0` read as "no limit", so a slow reranker runs
        // past the wait the hook client keeps and the host gets nothing.
        let cfg = HooksConfig {
            user_prompt_submit_deadline_ms: 0,
            ..HooksConfig::default()
        };
        assert_eq!(rerank_budget(&cfg, 0), Some(Duration::from_millis(700)));
        assert_eq!(rerank_budget(&cfg, 60_000), None);
    }
}
