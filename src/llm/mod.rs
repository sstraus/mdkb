//! LLM integration for semantic search.
//!
//! This module provides local LLM inference for:
//! - Generating document embeddings
//! - Reranking search results
//!
//! All LLM features are gated behind the `llm` feature flag.

#[cfg(feature = "llm")]
pub mod embeddings;

#[cfg(feature = "llm")]
pub use embeddings::EmbeddingModel;

/// Placeholder for when LLM feature is disabled.
#[cfg(not(feature = "llm"))]
pub fn llm_not_available() -> crate::error::Error {
    crate::error::Error::Other("LLM features require --features llm".to_string())
}
