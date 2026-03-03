//! Domain layer - core business logic (hexagonal architecture).
//!
//! This layer contains pure business logic independent of storage or UI.
//! All domain types are storage-agnostic and can be tested with mocks.

pub mod conventions;
pub mod frontmatter;
pub mod links;
pub mod sessions;
pub mod traits;

use serde::{Deserialize, Serialize};

/// How a collection was created.
pub const COLLECTION_SOURCE_MANUAL: &str = "manual";
/// Collection was auto-detected via directory conventions.
pub const COLLECTION_SOURCE_CONVENTION: &str = "convention";
/// Collection was created for Claude Code session indexing.
pub const COLLECTION_SOURCE_SESSIONS: &str = "sessions";
/// Collection name for Claude Code sessions (excluded from default search).
pub const COLLECTION_CLAUDE_SESSIONS: &str = "claude_sessions";

/// A collection of documents from a directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    /// Unique name for this collection.
    pub name: String,

    /// Base path for the collection.
    pub path: String,

    /// Glob pattern for matching files.
    pub pattern: String,

    /// How this collection was created: "manual" or "convention".
    #[serde(default = "default_source")]
    pub source: String,

    /// Creation timestamp (Unix seconds).
    pub created_at: i64,

    /// Last update timestamp (Unix seconds).
    pub updated_at: i64,
}

fn default_source() -> String {
    COLLECTION_SOURCE_MANUAL.to_string()
}

/// A document in the knowledge base.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    /// Unique document ID.
    pub id: i64,

    /// Collection this document belongs to.
    pub collection: String,

    /// Relative path within the collection.
    pub relative_path: String,

    /// Content hash (SHA256).
    pub hash: String,

    /// Document title (from H1 or frontmatter).
    pub title: Option<String>,

    /// Parsed frontmatter metadata.
    pub metadata: Option<serde_json::Value>,

    /// File modification time (Unix seconds).
    pub file_modified_at: i64,

    /// Indexing time (Unix seconds).
    pub indexed_at: i64,
}

/// Search query parameters.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    /// The search text.
    pub text: String,

    /// Maximum results to return.
    pub limit: usize,

    /// Filter by collection name.
    pub collection: Option<String>,

    /// Filter by tags.
    pub tags: Vec<String>,

    /// Include superseded/retracted documents (default: false).
    pub include_superseded: bool,
}

/// A search result with score and snippets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Document ID.
    pub id: i64,

    /// Collection name.
    pub collection: String,

    /// Relative path.
    pub path: String,

    /// Document title.
    pub title: Option<String>,

    /// BM25 relevance score.
    pub score: f64,

    /// Matching snippets with highlights.
    pub snippets: Vec<String>,

    /// Document status (current, superseded, retracted).
    #[serde(default)]
    pub status: Option<String>,

    /// Path of document that superseded this one, if any.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

/// Index status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexStatus {
    /// Total number of collections.
    pub collections: usize,

    /// Total number of indexed documents.
    pub documents: usize,

    /// Number of stale documents needing reindex.
    pub stale_documents: usize,

    /// Database size in bytes.
    pub db_size_bytes: u64,

    /// Last update timestamp.
    pub last_updated: Option<i64>,
}

/// Result of an update operation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateResult {
    /// Number of new documents indexed.
    pub added: usize,

    /// Number of documents updated (content changed).
    pub updated: usize,

    /// Number of documents removed (file deleted).
    pub removed: usize,

    /// Number of documents unchanged (skipped).
    pub unchanged: usize,

    /// Errors encountered during indexing.
    pub errors: Vec<String>,
}
