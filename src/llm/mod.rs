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

/// Get or initialize the cached embedding service.
///
/// Returns a shared reference via `Arc`. Thread-safe.
pub fn get_cached_service() -> crate::error::Result<Arc<EmbeddingService>> {
    let guard = CACHED_SERVICE.lock().map_err(|_| {
        crate::error::Error::other("Embedding service cache lock poisoned")
    })?;
    if let Some(service) = guard.as_ref() {
        return Ok(Arc::clone(service));
    }
    drop(guard);

    // Initialize outside the lock to avoid holding it during model download
    let service = Arc::new(EmbeddingService::new()?);

    let mut guard = CACHED_SERVICE.lock().map_err(|_| {
        crate::error::Error::other("Embedding service cache lock poisoned")
    })?;
    // Double-check after re-acquiring lock
    if let Some(existing) = guard.as_ref() {
        return Ok(Arc::clone(existing));
    }
    *guard = Some(Arc::clone(&service));
    Ok(service)
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
