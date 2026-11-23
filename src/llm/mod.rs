//! Embedding service for semantic search.
//!
//! Provides local ONNX-based embedding generation using fastembed (AllMiniLML6V2).
//! Used by both document hybrid search and code intelligence semantic search.

pub mod embeddings;
pub mod onnx_threads;

#[doc(inline)]
pub use onnx_threads::{DEFAULT_ORT_THREADS, ORT_THREADS_ENV, bound_onnx_threads};

#[doc(inline)]
pub use embeddings::{
    DEFAULT_EMBED_NICE, EmbeddingService, cosine_similarity, lower_process_priority,
};

use std::sync::{Arc, Mutex};

/// Global cached embedding service instance.
///
/// Embedding model init is expensive (~1-2s). This singleton avoids
/// reloading on every request for long-running processes like the MCP server.
static CACHED_SERVICE: Mutex<Option<Arc<EmbeddingService>>> = Mutex::new(None);

/// Held while one caller builds the service, so concurrent cold callers wait
/// for that build instead of starting their own.
static INIT_LOCK: Mutex<()> = Mutex::new(());
static GRANITE_SERVICE: Mutex<Option<Arc<EmbeddingService>>> = Mutex::new(None);
static GRANITE_INIT: Mutex<()> = Mutex::new(());

pub fn service_for(
    model: embeddings::Model,
    download: bool,
) -> crate::error::Result<Arc<EmbeddingService>> {
    let (cache, init) = match model {
        embeddings::Model::MiniLm => (&CACHED_SERVICE, &INIT_LOCK),
        embeddings::Model::Granite107M => (&GRANITE_SERVICE, &GRANITE_INIT),
    };
    get_or_init(cache, init, || EmbeddingService::for_model(model, download))
}

/// The cached embedding service, or an error when the weights are not on disk.
///
/// **Never downloads.** Every caller here is a query or a write that embeds as
/// a side effect, and all of them already treat an error as "carry on without a
/// vector" — hybrid search falls back to BM25, `embed_entry_by_rowid` leaves the
/// row pending. A download in any of them instead blocks the caller on a 90 MB
/// fetch with no way to say no.
///
/// Measured 2026-09-16: `mdkb search --scope memory` gained the vector leg
/// (story 084), and six integration tests that spawn the binary under an
/// isolated `HOME` stopped failing and started hanging indefinitely inside
/// `TextEmbedding::try_new`, waiting on the network. The same command on a
/// developer machine without the model would have done the same thing, inside a
/// hook, unasked.
///
/// Commands whose *purpose* is to build embeddings call
/// [`get_or_download_service`] instead.
///
/// Returns a shared reference via `Arc`. Thread-safe.
pub fn get_cached_service() -> crate::error::Result<Arc<EmbeddingService>> {
    service_for(embeddings::Model::MiniLm, false)
}

/// Get or initialize the cached embedding service, downloading the weights when
/// they are absent.
///
/// Only for commands the user ran *to* build embeddings — `mdkb update`, and an
/// eval invoked with `--download`. Everything else uses [`get_cached_service`].
///
/// Returns a shared reference via `Arc`. Thread-safe.
pub fn get_or_download_service() -> crate::error::Result<Arc<EmbeddingService>> {
    service_for(embeddings::Model::MiniLm, true)
}

/// The value in `cache`, built by `build` when empty. Single flight: `init`
/// lets one caller build while the others wait and then share its result.
///
/// Measured 2026-11-15 (049 benchmark): without `init`, 8 concurrent cold
/// callers built 8 embedding models and kept one, ~1640 MiB at peak.
///
/// `cache` itself is locked only to read or store, never during the build, so
/// [`release_cached_service`] does not wait for a download. A failed build
/// (error or panic) stores nothing and releases `init`; the next caller builds
/// again.
fn get_or_init<T>(
    cache: &Mutex<Option<Arc<T>>>,
    init: &Mutex<()>,
    build: impl FnOnce() -> crate::error::Result<T>,
) -> crate::error::Result<Arc<T>> {
    let cached = || {
        cache
            .lock()
            .map(|g| g.clone())
            .map_err(|_| crate::error::Error::other("Embedding service cache lock poisoned"))
    };
    if let Some(service) = cached()? {
        return Ok(service);
    }
    // `init` guards no data; a panicked build only means the next caller retries.
    let _building = init
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(service) = cached()? {
        return Ok(service);
    }
    let service = Arc::new(build()?);
    *cache
        .lock()
        .map_err(|_| crate::error::Error::other("Embedding service cache lock poisoned"))? =
        Some(Arc::clone(&service));
    Ok(service)
}

/// Reset the cached service (test-only). Returns the old value if any.
#[cfg(test)]
fn reset_cached_service() -> Option<Arc<EmbeddingService>> {
    CACHED_SERVICE.lock().ok().and_then(|mut g| g.take())
}

/// Release the cached embedding service to free ONNX Runtime memory.
///
/// The ONNX Runtime arena allocator never returns memory to the OS while the
/// session is alive. Calling this after bulk embedding generation frees
/// gigabytes of arena memory. The service will be re-initialized on next use.
///
/// Also calls `mi_collect(true)` to force mimalloc to return freed pages
/// to the OS (mimalloc retains freed memory by default for reuse).
pub fn release_cached_service() {
    if let Ok(mut guard) = CACHED_SERVICE.lock() {
        *guard = None;
    }
    if let Ok(mut guard) = GRANITE_SERVICE.lock() {
        *guard = None;
    }
    // Force mimalloc to return freed pages to the OS.
    // Without this, the ~1GB+ of ONNX Runtime arena memory stays mapped.
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: every concurrent cold caller building its own model and keeping
    /// one. The 049 benchmark measured 8 callers -> 8 models, ~1640 MiB peak.
    #[test]
    #[ignore = "requires ONNX model download"]
    fn concurrent_cold_callers_build_one_model() {
        const CALLERS: usize = 8;
        reset_cached_service();
        let before = embeddings::instance_counts();
        let barrier = Arc::new(std::sync::Barrier::new(CALLERS));
        let handles: Vec<_> = (0..CALLERS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    get_cached_service().unwrap()
                })
            })
            .collect();
        let services: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let started = embeddings::instance_counts().started - before.started;
        assert_eq!(started, 1, "{CALLERS} cold callers built {started} models");
        assert!(
            services.iter().all(|s| Arc::ptr_eq(s, &services[0])),
            "every caller must get the same Arc instance"
        );
        drop(services);
        reset_cached_service();
    }

    /// Catches: a failed build that leaves the cache unusable (an error
    /// remembered, or a held init lock), so no later caller gets a service.
    #[test]
    fn failed_init_lets_the_next_caller_retry() {
        let cache = Mutex::new(None);
        let init = Mutex::new(());
        let err = get_or_init(&cache, &init, || -> crate::error::Result<u32> {
            Err(crate::error::Error::other("download failed"))
        });
        assert!(err.is_err());
        let panicked = std::thread::scope(|s| {
            s.spawn(|| get_or_init(&cache, &init, || -> crate::error::Result<u32> { panic!() }))
                .join()
        });
        assert!(panicked.is_err());
        assert_eq!(*get_or_init(&cache, &init, || Ok(7)).unwrap(), 7);
        assert_eq!(
            *get_or_init(&cache, &init, || Ok(8)).unwrap(),
            7,
            "cached value reused"
        );
    }
}
