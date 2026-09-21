//! Document embeddings using local ONNX inference.
//!
//! Uses fastembed with AllMiniLML6V2 (384-dim) for embedding generation.
//! This is the shared embedding backend for both document search and code
//! intelligence semantic search.

use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::error::{Error, Result};

/// Embedding dimension for AllMiniLML6V2.
pub const EMBEDDING_DIM: usize = 384;

/// Batch size for fastembed inference.
///
/// Controls how many texts ONNX Runtime processes per inference call.
/// Smaller = less peak memory (each batch gets its own arena that's never freed).
/// 32 is a good balance: fast enough, but doesn't balloon memory via rayon parallelism.
const EMBED_BATCH_SIZE: usize = 32;

/// Model identifier stored alongside embeddings for migration detection.
pub const MODEL_NAME: &str = "AllMiniLML6V2";

/// Shared embedding service for generating document and query vectors.
///
/// Wraps fastembed's `TextEmbedding` model. Thread-safe for concurrent use
/// via shared reference (fastembed handles internal synchronization).
pub struct EmbeddingService {
    model: TextEmbedding,
}

impl std::fmt::Debug for EmbeddingService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingService")
            .field("model", &MODEL_NAME)
            .field("dimension", &EMBEDDING_DIM)
            .finish()
    }
}

/// Cap rayon's global pool to one worker before the first fastembed call.
///
/// fastembed parallelises batches with `texts.par_chunks(..)` on rayon's GLOBAL
/// pool — one worker per core — while every ONNX session it builds sets
/// `with_intra_threads(available_parallelism())`, a second pool that is also one
/// thread per core and that `InitOptions` exposes no knob for. Nesting the two
/// means N rayon workers issuing concurrent `Session::run()` into a single
/// N-thread ORT pool on N cores. ORT's pool spin-waits, so the contention burns
/// every core instead of blocking: measured at 1250% CPU, ~0 system time, and no
/// forward progress for 20 minutes.
///
/// Keep the parallelism where it pays — inside ORT, which parallelises a single
/// inference — and let the batch loop run serially on top of it.
///
/// `build_global` returns Err if the pool is already initialised. That is benign:
/// it only means the pool exists, which is the state we want to reach anyway.
fn cap_rayon_global_pool() {
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global();
}

/// How much nicer than its caller a dedicated embedding process runs.
///
/// Not a thread cap. fastembed's ONNX sessions set
/// `with_intra_threads(available_parallelism())` and `InitOptions` exposes no
/// knob for it at the pinned version, so the thread count is not ours to
/// choose. Priority is, and it expresses the behaviour that was actually
/// wanted: take every idle core, yield the moment something else wants one.
/// A static cap cannot do the first half and a load sampler is a scheduler
/// nobody should have to maintain.
///
/// Measured 2026-09-21 on 14 cores: `mdkb embed` at the caller's priority held
/// 900–1080% and took the load average from 10 to 27. Reniced to +15 it still
/// held 858% with the machine otherwise idle, while a higher-priority
/// competitor ran unimpeded.
pub const DEFAULT_EMBED_NICE: i32 = 15;

/// Lower this process's scheduling priority, permanently.
///
/// **One-way.** Measured 2026-09-21: an unprivileged process may lower its own
/// priority and may not raise it back — `setpriority` returns `EPERM`. So this
/// cannot be a scoped RAII guard, and the first version of it, which tried to
/// restore on drop, was wrong.
///
/// That is why the policy lives at the CLI boundary and not in this library:
/// call it only from a process whose whole job is to embed and then exit. A
/// daemon or MCP server that called it would answer every later hook at the
/// lowered priority, for as long as it lived, with no way back.
///
/// Process-wide rather than per-thread on purpose: the CPU is burned by ORT's
/// own worker threads, and a thread-scoped change would leave exactly the
/// threads that matter running at full priority.
///
/// Returns the new priority when it changed something. `nice <= 0` is a no-op
/// so a caller can honour a config of 0 without branching.
///
/// Unix only. Windows has no `getpriority`/`PRIO_PROCESS`; lowering a process
/// there means `SetPriorityClass`, a `windows-sys` dependency this crate does
/// not carry for one batch-job nicety. The Windows build returns `None`, which
/// the contract above already means: nothing changed.
#[cfg(unix)]
pub fn lower_process_priority(nice: i32) -> Option<i32> {
    if nice <= 0 {
        return None;
    }
    // SAFETY: getpriority/setpriority on the current process. The errno dance
    // is why `getpriority` needs it: -1 is a legal priority AND the error
    // return, so errno must be cleared first to tell the two apart.
    unsafe {
        *errno_slot() = 0;
        let current = libc::getpriority(libc::PRIO_PROCESS, 0);
        if current == -1 && *errno_slot() != 0 {
            tracing::debug!("embedding: could not read process priority; leaving it alone");
            return None;
        }
        let target = current + nice;
        if libc::setpriority(libc::PRIO_PROCESS, 0, target) != 0 {
            tracing::debug!("embedding: could not lower process priority");
            return None;
        }
        Some(target)
    }
}

#[cfg(not(unix))]
pub fn lower_process_priority(_nice: i32) -> Option<i32> {
    None
}

/// The address of `errno`, which every libc spells differently.
///
/// Apple and the BSDs export `__error`, glibc and musl `__errno_location`,
/// Android and the NetBSD family `__errno`. Calling the Darwin name
/// unconditionally is what broke the Linux and Windows builds; keep the
/// spelling in exactly one place, and refuse to compile on a unix this has
/// never been checked against rather than guess at its libc.
///
/// # Safety
/// The returned pointer is valid for the calling thread only.
#[cfg(unix)]
unsafe fn errno_slot() -> *mut libc::c_int {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    unsafe {
        libc::__error()
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::__errno_location()
    }
    #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
    unsafe {
        libc::__errno()
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "linux",
        target_os = "android",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    compile_error!("errno_slot: unhandled unix target; name this libc's errno symbol");
}

impl EmbeddingService {
    /// Create a new embedding service, downloading the model if needed.
    ///
    /// Models are cached in `~/.cache/fastembed/` (shared across all projects)
    /// instead of per-project `.fastembed_cache/` directories.
    pub fn new() -> Result<Self> {
        cap_rayon_global_pool();
        let cache_dir = shared_cache_dir();
        let model = TextEmbedding::try_new(
            InitOptions::new(EmbeddingModel::AllMiniLML6V2)
                .with_cache_dir(cache_dir)
                .with_show_download_progress(true),
        )
        .map_err(|e| Error::other(format!("Failed to initialize embedding model: {e}")))?;

        Ok(Self { model })
    }

    /// Generate embeddings for multiple document texts in batch.
    ///
    /// Uses a small batch size (32) to limit ONNX Runtime memory arena allocation.
    /// Larger batches cause rayon to run multiple ONNX inferences in parallel,
    /// each allocating its own memory arena that is never freed.
    pub fn embed_documents(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        self.model
            .embed(texts.to_vec(), Some(EMBED_BATCH_SIZE))
            .map_err(|e| Error::other(format!("Failed to embed documents: {e}")))
    }

    /// Generate embedding for a single query text.
    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let mut results = self
            .model
            .embed(vec![text], Some(EMBED_BATCH_SIZE))
            .map_err(|e| Error::other(format!("Failed to embed query: {e}")))?;
        results
            .pop()
            .ok_or_else(|| Error::other("Embedding model returned no results"))
    }

    /// Embedding dimension (384 for AllMiniLML6V2).
    pub const fn dimension() -> usize {
        EMBEDDING_DIM
    }

    /// Model name for storage and migration detection.
    pub const fn model_name() -> &'static str {
        MODEL_NAME
    }
}

/// Compute cosine similarity between two vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a * norm_b)
}

/// Shared cache directory for fastembed models.
///
/// Uses `~/.cache/fastembed/` so all mdkb instances (and other projects)
/// share a single model copy instead of downloading one per project.
/// Respects `FASTEMBED_CACHE_DIR` env var if set.
fn shared_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FASTEMBED_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache/fastembed");
    }
    // Fallback: use CWD-relative (original fastembed behavior)
    PathBuf::from(".fastembed_cache")
}

/// The directory fastembed reads the AllMiniLML6V2 weights from.
///
/// fastembed resolves `HF_HOME` before the cache dir we pass it, so the same
/// order applies here — otherwise a presence check could look in one place
/// while `EmbeddingService::new` downloads into another.
pub fn model_cache_path() -> PathBuf {
    let base = std::env::var("HF_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| shared_cache_dir());
    let code = TextEmbedding::get_model_info(&EmbeddingModel::AllMiniLML6V2)
        .map(|info| info.model_code.clone())
        .unwrap_or_else(|_| "Qdrant/all-MiniLM-L6-v2-onnx".to_string());
    base.join(format!("models--{}", code.replace('/', "--")))
}

/// True when a complete download of the model sits in `model_dir`, so
/// `EmbeddingService::new` will load it from disk and touch no network.
///
/// hf-hub links each file into `snapshots/<revision>/` only after its blob is
/// complete, so a snapshot holding `model.onnx` is the signal; a bare cache
/// directory left by an interrupted download is not.
pub fn model_cached_at(model_dir: &Path) -> bool {
    let Ok(snapshots) = std::fs::read_dir(model_dir.join("snapshots")) else {
        return false;
    };
    snapshots
        .flatten()
        .any(|snapshot| snapshot.path().join("model.onnx").exists())
}

/// True when the AllMiniLML6V2 weights are cached (see [`model_cached_at`]).
pub fn model_is_cached() -> bool {
    model_cached_at(&model_cache_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![0.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 0.001);
    }

    #[test]
    fn cap_rayon_global_pool_limits_the_batch_loop_to_one_worker() {
        // The nested-pool blowup needs a real ONNX session to reproduce, so
        // assert the lever itself: once capped, fastembed's `par_chunks` batch
        // loop can no longer fan out to one worker per core on top of ORT's own
        // per-core pool.
        cap_rayon_global_pool();
        assert_eq!(rayon::current_num_threads(), 1);
        // Idempotent: the second call hits the already-initialised pool and must
        // neither panic nor change it.
        cap_rayon_global_pool();
        assert_eq!(rayon::current_num_threads(), 1);
    }

    #[test]
    fn model_cached_at_needs_a_complete_snapshot_not_just_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let model_dir = dir.path().join("models--Qdrant--all-MiniLM-L6-v2-onnx");
        // No directory at all.
        assert!(!model_cached_at(&model_dir));
        // The layout an interrupted download leaves: blobs, no snapshot file.
        std::fs::create_dir_all(model_dir.join("blobs")).unwrap();
        std::fs::create_dir_all(model_dir.join("snapshots/abc")).unwrap();
        assert!(!model_cached_at(&model_dir));
        // A snapshot holding the weights is a complete download.
        std::fs::write(model_dir.join("snapshots/abc/model.onnx"), b"onnx").unwrap();
        assert!(model_cached_at(&model_dir));
    }

    #[test]
    fn model_cache_path_ends_in_the_hf_hub_folder_for_the_production_model() {
        let path = model_cache_path();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("models--Qdrant--all-MiniLM-L6-v2-onnx")
        );
    }

    /// Priority is per PROCESS and lowering it is IRREVERSIBLE without
    /// privilege, so these tests share one piece of global state that nothing
    /// can put back. Serialised, and each one only ever moves it further down
    /// by a small amount, so the test binary ends a little nicer than it
    /// started and nothing else is affected.
    #[cfg(unix)]
    static PRIORITY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    fn current_nice() -> i32 {
        unsafe {
            *errno_slot() = 0;
            libc::getpriority(libc::PRIO_PROCESS, 0)
        }
    }

    #[test]
    #[cfg(unix)]
    fn lowering_moves_the_process_down_by_the_requested_amount() {
        let _serial = PRIORITY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = current_nice();
        let applied = lower_process_priority(1);
        assert_eq!(applied, Some(before + 1));
        assert_eq!(current_nice(), before + 1);
    }

    #[test]
    #[cfg(unix)]
    fn lowering_is_one_way_and_the_api_does_not_pretend_otherwise() {
        // The measurement that killed the first design: an unprivileged
        // process cannot raise its own priority back. A scoped guard would
        // have silently failed to restore, leaving a daemon demoted forever.
        let _serial = PRIORITY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = current_nice();
        lower_process_priority(1);
        let raised = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, before) };
        assert_eq!(raised, -1, "raising priority must fail without privilege");
        assert_eq!(current_nice(), before + 1, "and leave it where it was put");
    }

    #[test]
    #[cfg(unix)]
    fn zero_is_a_no_op() {
        let _serial = PRIORITY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = current_nice();
        assert_eq!(lower_process_priority(0), None);
        assert_eq!(current_nice(), before, "a config of 0 changes nothing");
    }

    #[test]
    fn test_embedding_dim_constant() {
        assert_eq!(EmbeddingService::dimension(), 384);
    }

    #[test]
    fn test_model_name_constant() {
        assert_eq!(EmbeddingService::model_name(), "AllMiniLML6V2");
    }

    // Integration tests that require model download
    #[test]
    #[ignore = "requires ONNX model download"]
    fn test_embed_query_returns_correct_dimension() {
        let service = EmbeddingService::new().unwrap();
        let embedding = service.embed_query("test query").unwrap();
        assert_eq!(embedding.len(), EMBEDDING_DIM);
    }

    #[test]
    #[ignore = "requires ONNX model download"]
    fn test_embed_documents_batch() {
        let service = EmbeddingService::new().unwrap();
        let texts = vec!["first document", "second document", "third document"];
        let embeddings = service.embed_documents(&texts).unwrap();
        assert_eq!(embeddings.len(), 3);
        for emb in &embeddings {
            assert_eq!(emb.len(), EMBEDDING_DIM);
        }
    }

    #[test]
    #[ignore = "requires ONNX model download"]
    fn test_embed_documents_empty() {
        let service = EmbeddingService::new().unwrap();
        let embeddings = service.embed_documents(&[]).unwrap();
        assert!(embeddings.is_empty());
    }

    #[test]
    #[ignore = "requires ONNX model download"]
    fn test_similar_texts_have_higher_similarity() {
        let service = EmbeddingService::new().unwrap();
        let cat = service.embed_query("cat").unwrap();
        let kitten = service.embed_query("kitten").unwrap();
        let airplane = service.embed_query("airplane").unwrap();

        let sim_related = cosine_similarity(&cat, &kitten);
        let sim_unrelated = cosine_similarity(&cat, &airplane);
        assert!(
            sim_related > sim_unrelated,
            "cat-kitten ({:.3}) should be more similar than cat-airplane ({:.3})",
            sim_related,
            sim_unrelated
        );
    }
}
