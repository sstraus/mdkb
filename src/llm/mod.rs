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
