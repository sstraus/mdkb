//! Error types for mdkb.
//!
//! Uses a canonical error struct pattern (M-ERRORS-CANONICAL-STRUCTS) with
//! backtrace capture for improved debugging. The error captures a backtrace
//! at creation time, which can be displayed when `RUST_BACKTRACE=1` is set.

use std::backtrace::Backtrace;
use std::fmt;
use std::path::PathBuf;

use thiserror::Error;

/// Main error type for mdkb operations.
///
/// Wraps an `ErrorKind` with a captured backtrace for debugging.
/// The backtrace is automatically captured when the error is created.
pub struct Error {
    /// The specific error kind.
    kind: ErrorKind,
    /// Backtrace captured at error creation time.
    backtrace: Backtrace,
}

impl Error {
    /// Create a new error from an error kind.
    fn new(kind: ErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Get the error kind.
    #[must_use]
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }

    /// Get the backtrace captured when this error was created.
    #[must_use]
    pub fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }

    /// Create a new configuration error.
    pub fn config(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config(msg.into()))
    }

    /// Create a new MCP error.
    pub fn mcp(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Mcp(msg.into()))
    }

    /// Create a new generic error.
    pub fn other(msg: impl Into<String>) -> Self {
        Self::new(ErrorKind::Other(msg.into()))
    }

    /// Returns true if this is a "not found" type error.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.kind.is_not_found()
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.kind)
            .field("backtrace", &self.backtrace)
            .finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.kind)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.kind.source()
    }
}

/// The specific kind of error that occurred.
#[derive(Error, Debug)]
pub enum ErrorKind {
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

    // Watcher errors
    #[error("file watcher error: {0}")]
    Watcher(String),

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

impl ErrorKind {
    /// Returns true if this is a "not found" type error.
    #[must_use]
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

// Implement From for ErrorKind to Error
impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self::new(kind)
    }
}

// Re-implement From traits for Error to maintain ergonomic error handling
impl From<rusqlite::Error> for Error {
    fn from(err: rusqlite::Error) -> Self {
        Self::new(ErrorKind::Database(err))
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Self::new(ErrorKind::Io(err))
    }
}

impl From<globset::Error> for Error {
    fn from(err: globset::Error) -> Self {
        Self::new(ErrorKind::GlobPattern(err))
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Self::new(ErrorKind::Json(err))
    }
}

impl From<toml::de::Error> for Error {
    fn from(err: toml::de::Error) -> Self {
        Self::new(ErrorKind::TomlParse(err))
    }
}

impl From<toml::ser::Error> for Error {
    fn from(err: toml::ser::Error) -> Self {
        Self::new(ErrorKind::TomlSerialize(err))
    }
}

/// Result type alias for mdkb operations.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = Error::from(ErrorKind::CollectionNotFound {
            name: "docs".to_string(),
        });
        assert_eq!(err.to_string(), "collection not found: docs");
    }

    #[test]
    fn test_is_not_found() {
        let err = Error::from(ErrorKind::DocumentNotFound {
            id: "123".to_string(),
        });
        assert!(err.is_not_found());

        let err = Error::config("bad config");
        assert!(!err.is_not_found());
    }

    #[test]
    fn test_error_kind_access() {
        let err = Error::from(ErrorKind::CollectionNotFound {
            name: "test".to_string(),
        });
        assert!(matches!(err.kind(), ErrorKind::CollectionNotFound { .. }));
    }

    #[test]
    fn test_backtrace_captured() {
        let err = Error::config("test error");
        // Backtrace should be captured (may be empty if RUST_BACKTRACE not set)
        let _bt = err.backtrace();
    }

    #[test]
    fn test_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err = Error::from(io_err);
        assert!(matches!(err.kind(), ErrorKind::Io(_)));
    }
}
