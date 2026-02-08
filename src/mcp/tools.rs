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
    /// Document ID, path, or memory slug.
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

/// Parameters for the metrics tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MetricsParams {
    /// Number of days to analyze (default: 7).
    #[serde(default = "default_period")]
    pub period_days: u32,

    /// Include latency percentiles (default: true).
    #[serde(default = "default_true")]
    pub include_latency: bool,

    /// Include quality metrics (default: true).
    #[serde(default = "default_true")]
    pub include_quality: bool,
}

fn default_period() -> u32 {
    7
}

fn default_true() -> bool {
    true
}

/// Parameters for the collection_add tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CollectionAddParams {
    /// Collection name.
    pub name: String,

    /// Path to directory containing documents.
    pub path: String,

    /// Glob pattern for files (default: **/*.md).
    #[serde(default = "default_collection_pattern")]
    pub pattern: String,
}

fn default_collection_pattern() -> String {
    "**/*.md".to_string()
}

/// Parameters for the collection_remove tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CollectionRemoveParams {
    /// Name of the collection to remove.
    pub name: String,
}

/// Parameters for the memory_delete tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryDeleteParams {
    /// Memory entry ID.
    pub id: String,
}

/// Direction for evolution queries.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema, Default)]
#[serde(rename_all = "lowercase")]
pub enum EvolutionDirection {
    /// Show documents this one supersedes/updates.
    Ancestors,
    /// Show documents that supersede/update this one.
    Descendants,
    /// Show both ancestors and descendants.
    #[default]
    Both,
}

/// Parameters for the evolution tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct EvolutionParams {
    /// Document ID or path to query.
    pub path: String,

    /// Direction: ancestors (what this supersedes), descendants (what supersedes this), or both.
    #[serde(default)]
    pub direction: EvolutionDirection,
}

// ---------------------------------------------------------------------------
// Code intelligence tool parameters (requires `code-intel` feature)
// ---------------------------------------------------------------------------

/// Parameters for find_symbol: exact name lookup with optional filters.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FindSymbolParams {
    /// Exact symbol name to find.
    pub name: String,

    /// Filter by symbol kind (e.g., "function", "struct", "method").
    #[serde(default)]
    pub kind: Option<String>,

    /// Filter by file path (substring match).
    #[serde(default)]
    pub file: Option<String>,
}

/// Parameters for search_symbols: fuzzy text search across symbol names/signatures.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchSymbolsParams {
    /// Search query text (matched against names, signatures, doc comments).
    pub query: String,

    /// Filter by symbol kind (e.g., "function", "struct").
    #[serde(default)]
    pub kind: Option<String>,

    /// Maximum results (default: 10).
    #[serde(default = "default_limit")]
    pub limit: usize,
}

/// Parameters for get_calls: what functions does a symbol call?
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GetCallsParams {
    /// Symbol name to look up.
    pub name: String,

    /// Exact symbol ID (use to disambiguate when multiple symbols share a name).
    #[serde(default)]
    pub symbol_id: Option<u32>,
}

/// Parameters for find_callers: what calls a given symbol?
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct FindCallersParams {
    /// Symbol name to look up.
    pub name: String,

    /// Exact symbol ID (use to disambiguate when multiple symbols share a name).
    #[serde(default)]
    pub symbol_id: Option<u32>,
}

/// Parameters for analyze_impact: dependency graph from a symbol.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct AnalyzeImpactParams {
    /// Symbol name to start from.
    pub name: String,

    /// Exact symbol ID (use to disambiguate when multiple symbols share a name).
    #[serde(default)]
    pub symbol_id: Option<u32>,

    /// Maximum traversal depth (default: 3).
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
}

fn default_max_depth() -> usize {
    3
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

    #[test]
    fn test_evolution_params_default_direction() {
        let json = r#"{"path": "docs/api.md"}"#;
        let params: EvolutionParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.path, "docs/api.md");
        assert!(matches!(params.direction, EvolutionDirection::Both));
    }

    #[test]
    fn test_evolution_params_with_direction() {
        let json = r#"{"path": "docs/api.md", "direction": "ancestors"}"#;
        let params: EvolutionParams = serde_json::from_str(json).unwrap();
        assert!(matches!(params.direction, EvolutionDirection::Ancestors));

        let json = r#"{"path": "docs/api.md", "direction": "descendants"}"#;
        let params: EvolutionParams = serde_json::from_str(json).unwrap();
        assert!(matches!(params.direction, EvolutionDirection::Descendants));
    }

    #[test]
    fn test_metrics_params_defaults() {
        let json = r#"{}"#;
        let params: MetricsParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.period_days, 7);
        assert!(params.include_latency);
        assert!(params.include_quality);
    }

    #[test]
    fn test_metrics_params_custom() {
        let json = r#"{"period_days": 30, "include_latency": false, "include_quality": true}"#;
        let params: MetricsParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.period_days, 30);
        assert!(!params.include_latency);
        assert!(params.include_quality);
    }

    #[test]
    fn test_collection_add_params_deserialize() {
        let json = r#"{"name": "docs", "path": "docs/", "pattern": "**/*.txt"}"#;
        let params: CollectionAddParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "docs");
        assert_eq!(params.path, "docs/");
        assert_eq!(params.pattern, "**/*.txt");
    }

    #[test]
    fn test_collection_add_params_default_pattern() {
        let json = r#"{"name": "notes", "path": "notes/"}"#;
        let params: CollectionAddParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "notes");
        assert_eq!(params.path, "notes/");
        assert_eq!(params.pattern, "**/*.md");
    }

    #[test]
    fn test_collection_remove_params_deserialize() {
        let json = r#"{"name": "docs"}"#;
        let params: CollectionRemoveParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "docs");
    }

    #[test]
    fn test_memory_delete_params_deserialize() {
        let json = r#"{"id": "auth-oauth2-pkce"}"#;
        let params: MemoryDeleteParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.id, "auth-oauth2-pkce");
    }

    // --- Code intelligence param tests ---

    #[test]
    fn test_find_symbol_params_minimal() {
        let json = r#"{"name": "process_data"}"#;
        let params: FindSymbolParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "process_data");
        assert!(params.kind.is_none());
        assert!(params.file.is_none());
    }

    #[test]
    fn test_find_symbol_params_with_filters() {
        let json = r#"{"name": "process_data", "kind": "function", "file": "src/main.rs"}"#;
        let params: FindSymbolParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "process_data");
        assert_eq!(params.kind.as_deref(), Some("function"));
        assert_eq!(params.file.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn test_search_symbols_params_defaults() {
        let json = r#"{"query": "handler"}"#;
        let params: SearchSymbolsParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "handler");
        assert!(params.kind.is_none());
        assert_eq!(params.limit, 10);
    }

    #[test]
    fn test_search_symbols_params_custom() {
        let json = r#"{"query": "handler", "kind": "method", "limit": 25}"#;
        let params: SearchSymbolsParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "handler");
        assert_eq!(params.kind.as_deref(), Some("method"));
        assert_eq!(params.limit, 25);
    }

    #[test]
    fn test_get_calls_params_name_only() {
        let json = r#"{"name": "main"}"#;
        let params: GetCallsParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "main");
        assert!(params.symbol_id.is_none());
    }

    #[test]
    fn test_get_calls_params_with_id() {
        let json = r#"{"name": "main", "symbol_id": 42}"#;
        let params: GetCallsParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "main");
        assert_eq!(params.symbol_id, Some(42));
    }

    #[test]
    fn test_find_callers_params() {
        let json = r#"{"name": "process", "symbol_id": 7}"#;
        let params: FindCallersParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "process");
        assert_eq!(params.symbol_id, Some(7));
    }

    #[test]
    fn test_analyze_impact_params_defaults() {
        let json = r#"{"name": "Database.connect"}"#;
        let params: AnalyzeImpactParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "Database.connect");
        assert!(params.symbol_id.is_none());
        assert_eq!(params.max_depth, 3);
    }

    #[test]
    fn test_analyze_impact_params_custom() {
        let json = r#"{"name": "init", "symbol_id": 1, "max_depth": 5}"#;
        let params: AnalyzeImpactParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "init");
        assert_eq!(params.symbol_id, Some(1));
        assert_eq!(params.max_depth, 5);
    }
}
