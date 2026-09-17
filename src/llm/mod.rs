//! Embedding service for semantic search.
//!
//! Provides local ONNX-based embedding generation using fastembed (AllMiniLML6V2).
//! Used by both document hybrid search and code intelligence semantic search.

pub mod embeddings;

#[doc(inline)]
pub use embeddings::{EmbeddingService, cosine_similarity};

use std::sync::{Arc, Mutex};

/// Global cached embedding service instance.
///
/// Embedding model init is expensive (~1-2s). This singleton avoids
/// reloading on every request for long-running processes like the MCP server.
static CACHED_SERVICE: Mutex<Option<Arc<EmbeddingService>>> = Mutex::new(None);

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
    if CACHED_SERVICE.lock().is_ok_and(|g| g.is_none()) && !embeddings::model_is_cached() {
        return Err(crate::error::Error::other(format!(
            "embedding model not cached at {} — run `mdkb embed` to fetch it",
            embeddings::model_cache_path().display()
        )));
    }
    get_or_download_service()
}

/// Get or initialize the cached embedding service, downloading the weights when
/// they are absent.
///
/// Only for commands the user ran *to* build embeddings — `mdkb embed`, and an
/// eval invoked with `--download`. Everything else uses [`get_cached_service`].
///
/// Returns a shared reference via `Arc`. Thread-safe.
pub fn get_or_download_service() -> crate::error::Result<Arc<EmbeddingService>> {
    let guard = CACHED_SERVICE
        .lock()
        .map_err(|_| crate::error::Error::other("Embedding service cache lock poisoned"))?;
    if let Some(service) = guard.as_ref() {
        return Ok(Arc::clone(service));
    }
    drop(guard);

    // Initialize outside the lock to avoid holding it during model download
    let service = Arc::new(EmbeddingService::new()?);

    let mut guard = CACHED_SERVICE
        .lock()
        .map_err(|_| crate::error::Error::other("Embedding service cache lock poisoned"))?;
    // Double-check after re-acquiring lock
    if let Some(existing) = guard.as_ref() {
        return Ok(Arc::clone(existing));
    }
    *guard = Some(Arc::clone(&service));
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
    // Force mimalloc to return freed pages to the OS.
    // Without this, the ~1GB+ of ONNX Runtime arena memory stays mapped.
    unsafe {
        libmimalloc_sys::mi_collect(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires ONNX model download"]
    fn test_cached_service_returns_same_arc() {
        reset_cached_service();
        let a = get_cached_service().unwrap();
        let b = get_cached_service().unwrap();
        assert!(
            Arc::ptr_eq(&a, &b),
            "get_cached_service() must return the same Arc instance"
        );
        reset_cached_service();
    }
}
