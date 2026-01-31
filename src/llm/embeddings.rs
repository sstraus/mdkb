//! Document embeddings using local LLM inference.
//!
//! Uses nomic-embed-text for high-quality embeddings.

use std::path::PathBuf;
use std::sync::Arc;

use hf_hub::api::sync::Api;
use indicatif::{ProgressBar, ProgressStyle};
use llama_cpp_rs::{LlamaModel, LlamaParams};

use crate::error::{Error, Result};

/// Default embedding model from HuggingFace.
pub const DEFAULT_EMBEDDING_REPO: &str = "nomic-ai/nomic-embed-text-v1.5-GGUF";
pub const DEFAULT_EMBEDDING_FILE: &str = "nomic-embed-text-v1.5.Q4_K_M.gguf";

/// Embedding dimension for nomic-embed-text.
pub const EMBEDDING_DIM: usize = 768;

/// Embedding model for generating document vectors.
pub struct EmbeddingModel {
    model: LlamaModel,
}

impl EmbeddingModel {
    /// Load the embedding model, downloading if necessary.
    pub fn load(repo: Option<&str>, file: Option<&str>) -> Result<Self> {
        let repo = repo.unwrap_or(DEFAULT_EMBEDDING_REPO);
        let file = file.unwrap_or(DEFAULT_EMBEDDING_FILE);

        // Download model from HuggingFace
        let model_path = download_model(repo, file)?;

        // Load model
        let params = LlamaParams::default();
        let model = LlamaModel::load_from_file(&model_path, params)
            .map_err(|e| Error::Other(format!("Failed to load embedding model: {}", e)))?;

        Ok(Self { model })
    }

    /// Generate embedding for text.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Prepend search prefix for nomic-embed
        let prefixed = format!("search_document: {}", text);

        let embedding = self
            .model
            .embed(&prefixed)
            .map_err(|e| Error::Other(format!("Embedding failed: {}", e)))?;

        Ok(embedding)
    }

    /// Generate embeddings for multiple texts in batch.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Generate query embedding (different prefix for queries).
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let prefixed = format!("search_query: {}", query);

        let embedding = self
            .model
            .embed(&prefixed)
            .map_err(|e| Error::Other(format!("Query embedding failed: {}", e)))?;

        Ok(embedding)
    }
}

/// Download model from HuggingFace Hub.
fn download_model(repo: &str, filename: &str) -> Result<PathBuf> {
    let api = Api::new().map_err(|e| Error::Other(format!("HuggingFace API error: {}", e)))?;

    // Show progress bar
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap(),
    );
    pb.set_message(format!("Downloading {} from {}...", filename, repo));

    let model_path = api
        .model(repo.to_string())
        .get(filename)
        .map_err(|e| Error::Other(format!("Failed to download model: {}", e)))?;

    pb.finish_with_message(format!("Model ready: {}", model_path.display()));

    Ok(model_path)
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
}
