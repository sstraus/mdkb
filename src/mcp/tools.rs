//! MCP tool definitions and parameters.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Parameters for the search tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchParams {
    /// Search query text.
    pub query: String,

    /// Maximum number of results (default: 10).
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Optional collection filter.
    #[serde(default)]
    pub collection: Option<String>,

    /// Include superseded/retracted documents (default: false).
    #[serde(default)]
    pub include_superseded: bool,
}

fn default_limit() -> usize {
    10
}

/// Parameters for the get tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GetParams {
    /// Document ID or path.
    pub id: String,

    /// Optional line range (e.g., "10:50").
    #[serde(default)]
    pub lines: Option<String>,
}

/// Parameters for the multi_get tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MultiGetParams {
    /// Pattern to match (glob).
    pub pattern: String,

    /// Optional collection filter.
    #[serde(default)]
    pub collection: Option<String>,
}

/// Parameters for the memory index tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryIndexParams {
    /// Maximum entries to return (default: 50).
    #[serde(default = "default_memory_limit")]
    pub limit: usize,
}

fn default_memory_limit() -> usize {
    50
}

/// Parameters for the memory get tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryGetParams {
    /// Memory entry ID.
    pub id: String,
}

/// Parameters for the memory write tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryWriteParams {
    /// Entry ID (slug, e.g., "auth-oauth2-flow").
    pub id: String,

    /// Concise title (max 50 chars).
    pub title: String,

    /// Full content.
    pub content: String,

    /// Entry type: topic, problem, or decision.
    #[serde(default = "default_entry_type")]
    pub entry_type: String,

    /// Tags for categorization.
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_entry_type() -> String {
    "topic".to_string()
}

/// Parameters for the memory search tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemorySearchParams {
    /// Search query.
    pub query: String,

    /// Maximum results (default: 10).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_params_deserialize() {
        let json = r#"{"query": "rust programming", "limit": 5}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "rust programming");
        assert_eq!(params.limit, 5);
        assert!(params.collection.is_none());
    }

    #[test]
    fn test_search_params_default_limit() {
        let json = r#"{"query": "test"}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 10);
    }

    #[test]
    fn test_get_params_deserialize() {
        let json = r#"{"id": "readme.md", "lines": "1:50"}"#;
        let params: GetParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.id, "readme.md");
        assert_eq!(params.lines, Some("1:50".to_string()));
    }

    #[test]
    fn test_get_params_no_lines() {
        let json = r#"{"id": "123"}"#;
        let params: GetParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.id, "123");
        assert!(params.lines.is_none());
    }
}
