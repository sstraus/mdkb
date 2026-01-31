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
    /// Pattern to match (glob or regex).
    pub pattern: String,
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
