//! Document embeddings using local LLM inference.
//!
//! Uses nomic-embed-text for high-quality embeddings.

use std::path::PathBuf;

use hf_hub::api::sync::Api;
use indicatif::{ProgressBar, ProgressStyle};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};

use crate::error::{Error, Result};

/// Default embedding model from HuggingFace.
pub const DEFAULT_EMBEDDING_REPO: &str = "nomic-ai/nomic-embed-text-v1.5-GGUF";
pub const DEFAULT_EMBEDDING_FILE: &str = "nomic-embed-text-v1.5.Q4_K_M.gguf";

/// Embedding dimension for nomic-embed-text.
pub const EMBEDDING_DIM: usize = 768;

/// Embedding model for generating document vectors.
pub struct EmbeddingModel {
    backend: LlamaBackend,
    model: LlamaModel,
}

impl EmbeddingModel {
    /// Load the embedding model, downloading if necessary.
    pub fn load(repo: Option<&str>, file: Option<&str>) -> Result<Self> {
        let repo = repo.unwrap_or(DEFAULT_EMBEDDING_REPO);
        let file = file.unwrap_or(DEFAULT_EMBEDDING_FILE);

        // Download model from HuggingFace
        let model_path = download_model(repo, file)?;

        // Initialize llama backend
        let backend = LlamaBackend::init()
            .map_err(|e| Error::other(format!("Failed to init llama backend: {}", e)))?;

        // Load model
        let model_params = LlamaModelParams::default();
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)
            .map_err(|e| Error::other(format!("Failed to load embedding model: {}", e)))?;

        Ok(Self { backend, model })
    }

    /// Generate embedding for text.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Prepend search prefix for nomic-embed
        let prefixed = format!("search_document: {}", text);
        self.generate_embedding(&prefixed)
    }

    /// Generate embeddings for multiple texts in batch.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Generate query embedding (different prefix for queries).
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let prefixed = format!("search_query: {}", query);
        self.generate_embedding(&prefixed)
    }

    /// Internal method to generate embedding from text.
    fn generate_embedding(&self, text: &str) -> Result<Vec<f32>> {
        // Create context with embeddings enabled
        let n_threads = std::thread::available_parallelism()
            .map(|p| p.get() as i32)
            .unwrap_or(4);

        let ctx_params = LlamaContextParams::default()
            .with_n_threads_batch(n_threads)
            .with_embeddings(true);

        let mut ctx = self
            .model
            .new_context(&self.backend, ctx_params)
            .map_err(|e| Error::other(format!("Failed to create context: {}", e)))?;

        // Tokenize the text
        let tokens = self
            .model
            .str_to_token(text, AddBos::Always)
            .map_err(|e| Error::other(format!("Failed to tokenize: {}", e)))?;

        let n_ctx = ctx.n_ctx() as usize;
        if tokens.len() > n_ctx {
            return Err(Error::other(format!(
                "Text too long: {} tokens exceeds context size {}",
                tokens.len(),
                n_ctx
            )));
        }

        // Create batch and add tokens
        let mut batch = LlamaBatch::new(n_ctx, 1);
        batch
            .add_sequence(&tokens, 0, false)
            .map_err(|e| Error::other(format!("Failed to add sequence to batch: {}", e)))?;

        // Clear KV cache and decode
        ctx.clear_kv_cache();
        ctx.decode(&mut batch)
            .map_err(|e| Error::other(format!("Failed to decode: {}", e)))?;

        // Get embeddings for sequence 0
        let embedding = ctx
            .embeddings_seq_ith(0)
            .map_err(|e| Error::other(format!("Failed to get embeddings: {}", e)))?;

        // Normalize the embedding
        Ok(normalize(embedding))
    }
}

/// Download model from HuggingFace Hub.
fn download_model(repo: &str, filename: &str) -> Result<PathBuf> {
    let api = Api::new().map_err(|e| Error::other(format!("HuggingFace API error: {}", e)))?;

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
        .map_err(|e| Error::other(format!("Failed to download model: {}", e)))?;

    pb.finish_with_message(format!("Model ready: {}", model_path.display()));

    Ok(model_path)
}

/// Normalize a vector using L2 normalization.
fn normalize(input: &[f32]) -> Vec<f32> {
    let magnitude = input
        .iter()
        .fold(0.0, |acc, &val| val.mul_add(val, acc))
        .sqrt();

    if magnitude == 0.0 {
        return input.to_vec();
    }

    input.iter().map(|&val| val / magnitude).collect()
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
    fn test_normalize() {
        let v = vec![3.0, 4.0];
        let normalized = normalize(&v);
        // 3/5 = 0.6, 4/5 = 0.8
        assert!((normalized[0] - 0.6).abs() < 0.001);
        assert!((normalized[1] - 0.8).abs() < 0.001);
    }

    #[test]
    fn test_normalize_zero_vector() {
        let v = vec![0.0, 0.0, 0.0];
        let normalized = normalize(&v);
        assert_eq!(normalized, vec![0.0, 0.0, 0.0]);
    }

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
