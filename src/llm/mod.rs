//! LLM integration for semantic search.
//!
//! This module provides local LLM inference for:
//! - Generating document embeddings
//! - Reranking search results
//!
//! ## Model Caching
//!
//! Loading LLM models from disk is expensive (2-5 seconds). For long-running
//! processes like the MCP server, use `get_cached_model()` to reuse the same
//! model instance across requests.

pub mod embeddings;

#[doc(inline)]
pub use embeddings::EmbeddingModel;

use std::sync::Mutex;

/// Global cached embedding model instance.
///
/// Uses Mutex<Option<Box<EmbeddingModel>>> to support fallible initialization.
/// The Box allows us to return a stable pointer that won't move.
static CACHED_MODEL: Mutex<Option<Box<EmbeddingModel>>> = Mutex::new(None);

/// Get or initialize the cached embedding model.
///
/// This function returns a reference to a globally cached model instance,
/// avoiding the 2-5 second load time on subsequent calls. Thread-safe.
///
/// # Safety
///
/// The returned reference is valid for 'static because:
/// 1. The model is stored in a static Mutex
/// 2. Once initialized, the Box is never replaced or dropped
/// 3. The Box provides a stable heap address
///
/// # Example
///
/// ```ignore
/// let model = mdkb::llm::get_cached_model()?;
/// let embedding = model.embed("some text")?;
/// ```
pub fn get_cached_model() -> crate::error::Result<&'static EmbeddingModel> {
    // First, try to get existing model without holding lock long
    {
        let guard = CACHED_MODEL.lock().map_err(|_| {
            crate::error::Error::other("Model cache lock poisoned")
        })?;
        if let Some(model) = guard.as_ref() {
            // Safety: The Box lives in a static Mutex and is never moved or dropped.
            // The pointer remains valid for the lifetime of the program.
            let ptr: *const EmbeddingModel = model.as_ref();
            return Ok(unsafe { &*ptr });
        }
    }

    // Model not loaded, need to initialize
    let mut guard = CACHED_MODEL.lock().map_err(|_| {
        crate::error::Error::other("Model cache lock poisoned")
    })?;

    // Double-check after acquiring write lock (another thread may have initialized)
    if guard.is_none() {
        let model = EmbeddingModel::load(None, None)?;
        *guard = Some(Box::new(model));
    }

    // Return reference to the now-initialized model
    let model = guard.as_ref().expect("model should be initialized");
    // Safety: Same as above - Box in static Mutex, never moved or dropped
    let ptr: *const EmbeddingModel = model.as_ref();
    Ok(unsafe { &*ptr })
}
