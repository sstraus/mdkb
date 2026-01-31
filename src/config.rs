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

impl Default for Config {
    fn default() -> Self {
        Self {
            indexing: IndexingConfig::default(),
            chunking: ChunkingConfig::default(),
            search: SearchConfig::default(),
            memory: MemoryConfig::default(),
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
}
