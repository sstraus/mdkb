//! Cross-encoder reranker for automatic recall.
//!
//! jina-reranker-v2-base-multilingual, the int8 ONNX file, through fastembed's
//! `UserDefinedRerankingModel`. It scores (prompt, memory) pairs, which is what
//! MiniLM's bi-encoder cosine cannot do for an Italian prompt over an English
//! store: measured 2026-09-30 (`docs/recall-options-eval.md`, "Lighter
//! rerankers"), MiniLM alone admits no Italian positive at its absolute gate,
//! and this reranker on MiniLM's top five recovers a third of them.
//!
//! One instance per process, loaded on a background thread the first time a
//! hook asks for it. **The hook never waits for the load and never downloads**
//! (same rule as [`super::get_cached_service`]): until the weights are in memory
//! a call answers [`RerankError::Loading`], and the caller falls back to the
//! bi-encoder result. The weights are fetched by `mdkb embed`
//! ([`download`]), the command whose job is to fetch models.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use fastembed::{
    RerankInitOptionsUserDefined, TextRerank, TokenizerFiles, UserDefinedRerankingModel,
};

use crate::error::{Error, Result};

/// HF repository of the reranker.
pub const MODEL_REPO: &str = "jinaai/jina-reranker-v2-base-multilingual";

/// The revision the eval measured. Pinned: a thresholds table fitted on one set
/// of weights says nothing about another.
pub const MODEL_REVISION: &str = "9cfeff2df7d40d1b78e75e5e9cebec92a99813c9";

/// The int8 file: 280 MB on disk, ~0.95 GB resident, 2.5x faster than fp32 for
/// two fewer hits out of 40.
const MODEL_FILE: &str = "onnx/model_int8.onnx";

const TOKENIZER_FILES: [&str; 4] = [
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];

/// Pairs per ONNX call. The pool is five, so one call.
const RERANK_BATCH: usize = 16;

/// Why a rerank produced no scores. Each variant is one `rerank_outcome` in
/// `hook-events.jsonl` ([`RerankError::outcome`]).
#[derive(Debug, thiserror::Error)]
pub enum RerankError {
    /// The weights are being read into memory; try again on a later prompt.
    #[error("reranker is loading")]
    Loading,
    /// The weights are not on disk. `mdkb embed` fetches them.
    #[error("reranker weights not cached at {}", .0.display())]
    NotCached(PathBuf),
    /// The weights are on disk and could not be loaded. Permanent for the life
    /// of the process: a file that failed once is not retried on every prompt.
    #[error("reranker failed to load: {0}")]
    LoadFailed(String),
    /// A previous rerank is still running, typically one the hook's deadline
    /// abandoned. A blocking task cannot be cancelled, so a second one would
    /// only compete with it for every core.
    #[error("a rerank is already running")]
    Busy,
    /// The model ran and errored or panicked.
    #[error("rerank failed: {0}")]
    Failed(String),
}

impl RerankError {
    /// The name the hook log uses for this failure.
    pub fn outcome(&self) -> &'static str {
        match self {
            RerankError::Loading => "loading",
            RerankError::NotCached(_) => "not_cached",
            RerankError::LoadFailed(_) => "load_failed",
            RerankError::Busy => "busy",
            RerankError::Failed(_) => "failed",
        }
    }
}

/// Scores (query, document) pairs. A trait so the hook's budget handling can be
/// exercised without a 280 MB model.
pub trait Reranker: Send + Sync + std::fmt::Debug {
    /// One relevance logit per document, in document order. Higher is more
    /// relevant; the scale is the model's own, not a probability.
    fn score(&self, query: &str, docs: &[String]) -> std::result::Result<Vec<f32>, RerankError>;
}

/// The process-wide reranker every repo shares: one copy of the weights per
/// daemon, however many stores it serves.
pub fn shared() -> Arc<dyn Reranker> {
    Arc::new(SharedReranker)
}

#[derive(Debug)]
struct SharedReranker;

enum Load {
    Idle,
    Loading,
    Ready(Arc<TextRerank>),
    Failed(String),
}

static LOAD: Mutex<Load> = Mutex::new(Load::Idle);

/// Set while a rerank runs. See [`RerankError::Busy`].
static RUNNING: AtomicBool = AtomicBool::new(false);

struct RunGuard;

impl RunGuard {
    fn acquire() -> Option<Self> {
        RUNNING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RunGuard)
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

/// `<cache>/models--<org>--<name>/snapshots/<revision>`, where hf-hub links the
/// files of [`MODEL_REVISION`].
fn snapshot_dir() -> PathBuf {
    super::embeddings::cache_base_dir()
        .join(format!("models--{}", MODEL_REPO.replace('/', "--")))
        .join("snapshots")
        .join(MODEL_REVISION)
}

/// True when every file the reranker needs is linked into `dir`. hf-hub links a
/// file only once its blob is complete, so an interrupted download is not
/// mistaken for a cached model.
fn weights_cached(dir: &Path) -> bool {
    dir.join(MODEL_FILE).exists() && TOKENIZER_FILES.iter().all(|f| dir.join(f).exists())
}

/// True when the reranker weights are on disk.
pub fn is_cached() -> bool {
    weights_cached(&snapshot_dir())
}

fn load_model(dir: &Path) -> Result<TextRerank> {
    let read = |name: &str| {
        std::fs::read(dir.join(name))
            .map_err(|e| Error::other(format!("reading {name} of the reranker: {e}")))
    };
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    TextRerank::try_new_from_user_defined(
        UserDefinedRerankingModel::new(dir.join(MODEL_FILE), tokenizer_files),
        RerankInitOptionsUserDefined::default(),
    )
    .map_err(|e| Error::other(format!("loading the reranker: {e}")))
}

fn set_load(state: Load) {
    let mut guard = LOAD.lock().unwrap_or_else(|e| e.into_inner());
    *guard = state;
}

/// The loaded model, or the reason there is none. Starts the background load on
/// the first call that finds the weights on disk.
fn ready_model() -> std::result::Result<Arc<TextRerank>, RerankError> {
    let mut guard = LOAD.lock().unwrap_or_else(|e| e.into_inner());
    match &*guard {
        Load::Ready(model) => return Ok(Arc::clone(model)),
        Load::Loading => return Err(RerankError::Loading),
        Load::Failed(why) => return Err(RerankError::LoadFailed(why.clone())),
        Load::Idle => {}
    }
    let dir = snapshot_dir();
    if !weights_cached(&dir) {
        // Not a failure state: `mdkb embed` may fetch them while we run.
        return Err(RerankError::NotCached(dir));
    }
    *guard = Load::Loading;
    drop(guard);

    let spawned = std::thread::Builder::new()
        .name("mdkb-rerank-load".into())
        .spawn(move || {
            let loaded = std::panic::catch_unwind(|| load_model(&dir))
                .unwrap_or_else(|_| Err(Error::other("the reranker load panicked")));
            match loaded {
                Ok(model) => set_load(Load::Ready(Arc::new(model))),
                Err(e) => {
                    tracing::warn!("recall reranker unavailable: {e}");
                    set_load(Load::Failed(e.to_string()));
                }
            }
        });
    if let Err(e) = spawned {
        set_load(Load::Failed(e.to_string()));
        return Err(RerankError::LoadFailed(e.to_string()));
    }
    Err(RerankError::Loading)
}

impl Reranker for SharedReranker {
    fn score(&self, query: &str, docs: &[String]) -> std::result::Result<Vec<f32>, RerankError> {
        let model = ready_model()?;
        let _running = RunGuard::acquire().ok_or(RerankError::Busy)?;
        let documents: Vec<&str> = docs.iter().map(String::as_str).collect();
        let ranked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            model.rerank(query, documents, false, Some(RERANK_BATCH))
        }))
        .map_err(|_| RerankError::Failed("the model panicked".into()))?
        .map_err(|e| RerankError::Failed(e.to_string()))?;
        // fastembed sorts by score; callers want the input order.
        let mut scores = vec![f32::NEG_INFINITY; docs.len()];
        for result in ranked {
            if let Some(slot) = scores.get_mut(result.index) {
                *slot = result.score;
            }
        }
        Ok(scores)
    }
}

/// Fetch the reranker weights into the hf-hub cache. Only for `mdkb embed`; a
/// hook or a daemon never calls it (see the module docs).
pub fn download() -> Result<()> {
    use hf_hub::{Cache, Repo, RepoType, api::sync::ApiBuilder};

    if is_cached() {
        return Ok(());
    }
    let api = ApiBuilder::from_cache(Cache::new(super::embeddings::cache_base_dir()))
        .with_progress(true)
        .build()
        .map_err(|e| Error::other(format!("reranker download: {e}")))?;
    let repo = api.repo(Repo::with_revision(
        MODEL_REPO.to_string(),
        RepoType::Model,
        MODEL_REVISION.to_string(),
    ));
    for file in std::iter::once(MODEL_FILE).chain(TOKENIZER_FILES) {
        repo.get(file)
            .map_err(|e| Error::other(format!("reranker download of {file}: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_cached_needs_every_file_not_just_the_model() {
        // Catches: a download interrupted after the model but before the
        // tokenizer reads as cached, and the load thread then fails on the
        // first prompt instead of the hook reporting `not_cached`.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("onnx")).unwrap();
        std::fs::write(tmp.path().join(MODEL_FILE), b"x").unwrap();
        assert!(!weights_cached(tmp.path()));
        for file in TOKENIZER_FILES {
            std::fs::write(tmp.path().join(file), b"x").unwrap();
        }
        assert!(weights_cached(tmp.path()));
    }

    #[test]
    fn run_guard_admits_one_rerank_at_a_time() {
        // Catches: an abandoned rerank and the next prompt's running together,
        // each at full thread count, which is what doubles the latency of both.
        let first = RunGuard::acquire().expect("idle");
        assert!(RunGuard::acquire().is_none());
        drop(first);
        assert!(RunGuard::acquire().is_some());
    }
}
