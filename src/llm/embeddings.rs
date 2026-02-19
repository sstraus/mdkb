//! Document embeddings using local ONNX inference.
//!
//! Uses fastembed with AllMiniLML6V2 (384-dim) for embedding generation.
//! This is the shared embedding backend for both document search and code
//! intelligence semantic search.

use std::path::PathBuf;

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

impl EmbeddingService {
    /// Create a new embedding service, downloading the model if needed.
    ///
    /// Models are cached in `~/.cache/fastembed/` (shared across all projects)
    /// instead of per-project `.fastembed_cache/` directories.
    pub fn new() -> Result<Self> {
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
            .embed(vec![text], None)
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
    fn test_embedding_dim_constant() {
        assert_eq!(EmbeddingService::dimension(), 384);
    }

    #[test]
    fn test_model_name_constant() {
        assert_eq!(EmbeddingService::model_name(), "AllMiniLML6V2");
    }

    // Integration tests that require model download
    #[test]
    #[ignore]
    fn test_embed_query_returns_correct_dimension() {
        let service = EmbeddingService::new().unwrap();
        let embedding = service.embed_query("test query").unwrap();
        assert_eq!(embedding.len(), EMBEDDING_DIM);
    }

    #[test]
    #[ignore]
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
    #[ignore]
    fn test_embed_documents_empty() {
        let service = EmbeddingService::new().unwrap();
        let embeddings = service.embed_documents(&[]).unwrap();
        assert!(embeddings.is_empty());
    }

    #[test]
    #[ignore]
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
