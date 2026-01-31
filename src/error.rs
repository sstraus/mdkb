//! Error types for mdkb.

use std::path::PathBuf;
use thiserror::Error;

/// Main error type for mdkb operations.
#[derive(Error, Debug)]
pub enum Error {
    // Configuration errors
    #[error("configuration error: {0}")]
    Config(String),

    #[error("configuration file not found: {path}")]
    ConfigNotFound { path: PathBuf },

    #[error("invalid configuration: {field}: {message}")]
    ConfigInvalid { field: String, message: String },

    // Storage errors
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("database not initialized at {path}")]
    DatabaseNotFound { path: PathBuf },

    #[error("database migration failed: {0}")]
    Migration(String),

    // Collection errors
    #[error("collection not found: {name}")]
    CollectionNotFound { name: String },

    #[error("collection already exists: {name}")]
    CollectionExists { name: String },

    #[error("invalid collection path: {path}")]
    CollectionInvalidPath { path: PathBuf },

    // Document errors
    #[error("document not found: {id}")]
    DocumentNotFound { id: String },

    #[error("document parse error in {path}: {message}")]
    DocumentParse { path: PathBuf, message: String },

    // Search errors
    #[error("invalid search query: {0}")]
    InvalidQuery(String),

    #[error("search failed: {0}")]
    SearchFailed(String),

    // File system errors
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("path not found: {path}")]
    PathNotFound { path: PathBuf },

    #[error("not a directory: {path}")]
    NotADirectory { path: PathBuf },

    #[error("glob pattern error: {0}")]
    GlobPattern(#[from] globset::Error),

    // MCP errors
    #[error("MCP server error: {0}")]
    Mcp(String),

    // Serialization errors
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("TOML serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    // Generic errors
    #[error("{0}")]
    Other(String),
}

/// Result type alias for mdkb operations.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Create a new configuration error.
    pub fn config(msg: impl Into<String>) -> Self {
        Self::Config(msg.into())
    }

    /// Create a new MCP error.
    pub fn mcp(msg: impl Into<String>) -> Self {
        Self::Mcp(msg.into())
    }

    /// Create a new generic error.
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// Returns true if this is a "not found" type error.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::ConfigNotFound { .. }
                | Self::DatabaseNotFound { .. }
                | Self::CollectionNotFound { .. }
                | Self::DocumentNotFound { .. }
                | Self::PathNotFound { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = Error::CollectionNotFound {
            name: "docs".to_string(),
        };
        assert_eq!(err.to_string(), "collection not found: docs");
    }

    #[test]
    fn test_is_not_found() {
        let err = Error::DocumentNotFound {
            id: "123".to_string(),
        };
        assert!(err.is_not_found());

        let err = Error::Config("bad config".to_string());
        assert!(!err.is_not_found());
    }
}
