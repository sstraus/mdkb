//! Configuration management for .mdkb/config.toml.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Main configuration structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Indexing configuration.
    pub indexing: IndexingConfig,

    /// Chunking configuration (for Phase 3).
    pub chunking: ChunkingConfig,

    /// Search configuration.
    pub search: SearchConfig,

    /// Memory index configuration (Phase 6).
    pub memory: MemoryConfig,

    /// LLM models configuration (Phase 3+).
    pub models: ModelsConfig,
}

/// Indexing settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexingConfig {
    /// Default glob pattern for markdown files.
    pub default_pattern: String,

    /// Debounce interval for file watcher (ms).
    pub debounce_ms: u64,

    /// Parse YAML frontmatter.
    pub parse_frontmatter: bool,

    /// Parse [[wiki-links]].
    pub parse_wikilinks: bool,

    /// Index heading structure.
    pub index_headings: bool,
}

/// Chunking settings (Phase 3).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ChunkingConfig {
    /// Chunking strategy: fixed, markdown, semantic.
    pub strategy: String,

    /// Maximum tokens per chunk.
    pub max_tokens: usize,

    /// Overlap tokens between chunks.
    pub overlap_tokens: usize,

    /// Include header path in chunk context.
    pub include_header_path: bool,
}

/// Search settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// Default result limit.
    pub default_limit: usize,

    /// Minimum score threshold.
    pub min_score: f64,

    /// RRF fusion constant.
    pub rrf_k: u32,

    /// BM25 weight in hybrid search.
    pub bm25_weight: f64,

    /// Vector weight in hybrid search.
    pub vector_weight: f64,

    /// Top-k for reranking.
    pub rerank_top_k: usize,
}

/// Memory index settings (Phase 6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Enable memory index.
    pub enabled: bool,

    /// Memory directory relative to .mdkb/.
    pub directory: String,

    /// Maximum entries in warmup index.
    pub warmup_limit: usize,

    /// Maximum title length.
    pub title_max_chars: usize,

    /// Ordering field: access_count, updated_at, created_at.
    pub order_by: String,

    /// Track access counts.
    pub track_access: bool,
}

/// LLM models settings (Phase 3+).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// HuggingFace repo for embedding model.
    pub embedding_repo: String,

    /// Embedding model filename.
    pub embedding_file: String,

    /// HuggingFace repo for reranker model.
    pub reranker_repo: String,

    /// Reranker model filename.
    pub reranker_file: String,

    /// HuggingFace repo for condensation model.
    pub condense_repo: String,

    /// Condensation model filename.
    pub condense_file: String,

    /// Inactivity timeout before unloading models (seconds).
    pub inactivity_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            indexing: IndexingConfig::default(),
            chunking: ChunkingConfig::default(),
            search: SearchConfig::default(),
            memory: MemoryConfig::default(),
            models: ModelsConfig::default(),
        }
    }
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            default_pattern: "**/*.md".to_string(),
            debounce_ms: 100,
            parse_frontmatter: true,
            parse_wikilinks: true,
            index_headings: true,
        }
    }
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            strategy: "markdown".to_string(),
            max_tokens: 512,
            overlap_tokens: 64,
            include_header_path: true,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_limit: 10,
            min_score: 0.0,
            rrf_k: 60,
            bm25_weight: 1.0,
            vector_weight: 0.7,
            rerank_top_k: 50,
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "memory".to_string(),
            warmup_limit: 50,
            title_max_chars: 50,
            order_by: "access_count".to_string(),
            track_access: true,
        }
    }
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            embedding_repo: "nomic-ai/nomic-embed-text-v1.5-GGUF".to_string(),
            embedding_file: "nomic-embed-text-v1.5.Q4_K_M.gguf".to_string(),
            reranker_repo: "BAAI/bge-reranker-base-GGUF".to_string(),
            reranker_file: "bge-reranker-base.Q4_K_M.gguf".to_string(),
            condense_repo: "bartowski/Llama-3.2-3B-Instruct-GGUF".to_string(),
            condense_file: "Llama-3.2-3B-Instruct-Q4_K_M.gguf".to_string(),
            inactivity_timeout_secs: 120,
        }
    }
}

/// Valid chunking strategies.
const VALID_CHUNKING_STRATEGIES: &[&str] = &["fixed", "markdown", "semantic"];

/// Valid memory order_by values.
const VALID_ORDER_BY: &[&str] = &["access_count", "updated_at", "created_at"];

/// Minimum allowed max_tokens for chunking.
const MIN_MAX_TOKENS: usize = 64;

impl Config {
    /// Load configuration from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(Error::ConfigNotFound {
                path: path.to_path_buf(),
            });
        }
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }

    /// Load configuration or return defaults if file doesn't exist.
    pub fn load_or_default(path: impl AsRef<Path>) -> Self {
        Self::load(path).unwrap_or_default()
    }

    /// Save configuration to a TOML file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let content = toml::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Generate default configuration content as TOML string.
    pub fn default_toml() -> Result<String> {
        let config = Config::default();
        let content = toml::to_string_pretty(&config)?;
        Ok(content)
    }

    /// Validate configuration values.
    pub fn validate(&self) -> Result<()> {
        // Search validation
        if self.search.default_limit == 0 {
            return Err(Error::ConfigInvalid {
                field: "search.default_limit".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        // Chunking validation
        if !VALID_CHUNKING_STRATEGIES.contains(&self.chunking.strategy.as_str()) {
            return Err(Error::ConfigInvalid {
                field: "chunking.strategy".to_string(),
                message: format!(
                    "must be one of: {}",
                    VALID_CHUNKING_STRATEGIES.join(", ")
                ),
            });
        }

        if self.chunking.max_tokens < MIN_MAX_TOKENS {
            return Err(Error::ConfigInvalid {
                field: "chunking.max_tokens".to_string(),
                message: format!("must be at least {MIN_MAX_TOKENS}"),
            });
        }

        if self.chunking.overlap_tokens >= self.chunking.max_tokens {
            return Err(Error::ConfigInvalid {
                field: "chunking.overlap_tokens".to_string(),
                message: "must be less than max_tokens".to_string(),
            });
        }

        // Memory validation
        if !VALID_ORDER_BY.contains(&self.memory.order_by.as_str()) {
            return Err(Error::ConfigInvalid {
                field: "memory.order_by".to_string(),
                message: format!("must be one of: {}", VALID_ORDER_BY.join(", ")),
            });
        }

        Ok(())
    }

    /// Create config from environment variables with defaults.
    ///
    /// Environment variables follow the pattern: MDKB_SECTION_FIELD
    /// e.g., MDKB_SEARCH_DEFAULT_LIMIT, MDKB_INDEXING_DEBOUNCE_MS
    pub fn from_env_with_defaults() -> Self {
        let mut config = Config::default();

        // Search overrides
        if let Ok(val) = std::env::var("MDKB_SEARCH_DEFAULT_LIMIT") {
            if let Ok(limit) = val.parse() {
                config.search.default_limit = limit;
            }
        }

        // Indexing overrides
        if let Ok(val) = std::env::var("MDKB_INDEXING_DEBOUNCE_MS") {
            if let Ok(debounce) = val.parse() {
                config.indexing.debounce_ms = debounce;
            }
        }

        // Memory overrides
        if let Ok(val) = std::env::var("MDKB_MEMORY_WARMUP_LIMIT") {
            if let Ok(limit) = val.parse() {
                config.memory.warmup_limit = limit;
            }
        }

        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.indexing.default_pattern, "**/*.md");
        assert_eq!(config.search.default_limit, 10);
        assert_eq!(config.memory.warmup_limit, 50);
    }

    #[test]
    fn test_config_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.search.default_limit, parsed.search.default_limit);
    }

    // ==================== Models Config Tests ====================

    #[test]
    fn test_models_config_defaults() {
        let config = Config::default();
        assert_eq!(
            config.models.embedding_repo,
            "nomic-ai/nomic-embed-text-v1.5-GGUF"
        );
        assert_eq!(
            config.models.embedding_file,
            "nomic-embed-text-v1.5.Q4_K_M.gguf"
        );
        assert_eq!(
            config.models.condense_repo,
            "bartowski/Llama-3.2-3B-Instruct-GGUF"
        );
    }

    #[test]
    fn test_models_config_serialization() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("[models]"));
        assert!(toml_str.contains("embedding_repo"));
        assert!(toml_str.contains("condense_repo"));
    }

    // ==================== Validation Tests ====================

    #[test]
    fn test_validate_valid_config() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_invalid_search_limit() {
        let mut config = Config::default();
        config.search.default_limit = 0;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("default_limit"));
    }

    #[test]
    fn test_validate_invalid_chunking_strategy() {
        let mut config = Config::default();
        config.chunking.strategy = "invalid_strategy".to_string();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("strategy"));
    }

    #[test]
    fn test_validate_invalid_memory_order_by() {
        let mut config = Config::default();
        config.memory.order_by = "invalid_order".to_string();
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("order_by"));
    }

    #[test]
    fn test_validate_max_tokens_too_small() {
        let mut config = Config::default();
        config.chunking.max_tokens = 10; // Too small
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max_tokens"));
    }

    #[test]
    fn test_validate_overlap_exceeds_max() {
        let mut config = Config::default();
        config.chunking.max_tokens = 100;
        config.chunking.overlap_tokens = 150; // More than max
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overlap"));
    }

    // ==================== File Loading Tests ====================

    #[test]
    fn test_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let toml_content = r#"
[indexing]
default_pattern = "*.md"
debounce_ms = 200

[search]
default_limit = 20
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.indexing.default_pattern, "*.md");
        assert_eq!(config.indexing.debounce_ms, 200);
        assert_eq!(config.search.default_limit, 20);
        // Other fields should have defaults
        assert!(config.indexing.parse_frontmatter);
    }

    #[test]
    fn test_load_nonexistent_file() {
        let result = Config::load("/nonexistent/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_or_default_nonexistent() {
        let config = Config::load_or_default("/nonexistent/config.toml");
        assert_eq!(config.indexing.default_pattern, "**/*.md");
    }

    #[test]
    fn test_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let mut config = Config::default();
        config.search.default_limit = 42;
        config.save(&config_path).unwrap();

        let loaded = Config::load(&config_path).unwrap();
        assert_eq!(loaded.search.default_limit, 42);
    }

    // ==================== Environment Override Tests ====================

    #[test]
    fn test_env_override_search_limit() {
        // SAFETY: Test environment, single-threaded execution
        unsafe {
            std::env::set_var("MDKB_SEARCH_DEFAULT_LIMIT", "25");
        }
        let config = Config::from_env_with_defaults();
        unsafe {
            std::env::remove_var("MDKB_SEARCH_DEFAULT_LIMIT");
        }

        assert_eq!(config.search.default_limit, 25);
    }

    #[test]
    fn test_env_override_indexing_debounce() {
        // SAFETY: Test environment, single-threaded execution
        unsafe {
            std::env::set_var("MDKB_INDEXING_DEBOUNCE_MS", "500");
        }
        let config = Config::from_env_with_defaults();
        unsafe {
            std::env::remove_var("MDKB_INDEXING_DEBOUNCE_MS");
        }

        assert_eq!(config.indexing.debounce_ms, 500);
    }

    #[test]
    fn test_env_override_memory_warmup_limit() {
        // SAFETY: Test environment, single-threaded execution
        unsafe {
            std::env::set_var("MDKB_MEMORY_WARMUP_LIMIT", "100");
        }
        let config = Config::from_env_with_defaults();
        unsafe {
            std::env::remove_var("MDKB_MEMORY_WARMUP_LIMIT");
        }

        assert_eq!(config.memory.warmup_limit, 100);
    }

    #[test]
    fn test_env_override_invalid_value_uses_default() {
        // SAFETY: Test environment, single-threaded execution
        unsafe {
            std::env::set_var("MDKB_SEARCH_DEFAULT_LIMIT", "not_a_number");
        }
        let config = Config::from_env_with_defaults();
        unsafe {
            std::env::remove_var("MDKB_SEARCH_DEFAULT_LIMIT");
        }

        // Should fall back to default when parse fails
        assert_eq!(config.search.default_limit, 10);
    }

    // ==================== Default TOML Generation ====================

    #[test]
    fn test_default_toml_generation() {
        let toml_str = Config::default_toml().unwrap();
        assert!(toml_str.contains("[indexing]"));
        assert!(toml_str.contains("[chunking]"));
        assert!(toml_str.contains("[search]"));
        assert!(toml_str.contains("[memory]"));
        assert!(toml_str.contains("[models]"));
    }
}
