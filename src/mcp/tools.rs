//! MCP tool definitions and parameters.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Parameters for the search tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchParams {
    /// Search query text.
    pub query: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo. Use "*" for cross-repo search.
    #[serde(default)]
    pub root: Option<String>,

    /// Maximum number of results (default: 10).
    #[serde(default = "default_limit")]
    pub limit: usize,

    /// Optional collection filter.
    #[serde(default)]
    pub collection: Option<String>,

    /// Include superseded/retracted documents (default: false).
    #[serde(default)]
    pub include_superseded: bool,

    /// Search scope: "docs", "memory", "code", or "symbols". Omit to search docs+memory.
    #[serde(default)]
    pub scope: Option<String>,

    /// Filter by symbol kind (scope="code"/"symbols", e.g., "function", "struct") or entry type (scope="memory", e.g., "problem", "decision").
    #[serde(default)]
    pub kind: Option<String>,

    /// Minimum similarity score 0.0-1.0 when scope is "code" (default: 0.5).
    #[serde(default = "default_threshold")]
    pub threshold: f32,

    /// Filter by file path (substring match) when scope is "symbols".
    #[serde(default)]
    pub file: Option<String>,
}

fn default_limit() -> usize {
    10
}

/// Parameters for the get tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GetParams {
    /// Document ID, path, or memory slug.
    pub id: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,

    /// Optional line range (e.g., "10:50").
    #[serde(default)]
    pub lines: Option<String>,

    /// Output format: "full" (default), "summary" (title + first paragraph), or "history" (revision diffs, memory only).
    #[serde(default)]
    pub format: Option<String>,
}

/// Parameters for the memory write tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryWriteParams {
    /// Entry ID (slug, e.g., "auth-oauth2-flow").
    pub id: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,

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

    /// Source type: official_docs, user_statement (default), auto_extracted, or inference.
    #[serde(default = "default_source_type")]
    pub source_type: String,

    /// TTL in seconds. Entry expires after this duration. Omit for permanent.
    #[serde(default)]
    pub ttl: Option<u64>,
}

fn default_entry_type() -> String {
    "topic".to_string()
}

fn default_source_type() -> String {
    "user_statement".to_string()
}

/// A single memory entry within a batch write.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryWriteBatchEntry {
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

    /// Source type: official_docs, user_statement (default), auto_extracted, or inference.
    #[serde(default = "default_source_type")]
    pub source_type: String,

    /// TTL in seconds. Entry expires after this duration. Omit for permanent.
    #[serde(default)]
    pub ttl: Option<u64>,
}

/// Parameters for batch memory write.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryWriteBatchParams {
    /// Memory entries to write (max 20).
    pub entries: Vec<MemoryWriteBatchEntry>,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,
}

/// Parameters for the memory_delete tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryDeleteParams {
    /// Memory entry ID.
    pub id: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,
}

/// Parameters for the memory_list tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryListParams {
    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,

    /// Maximum entries to return (default: 20).
    #[serde(default = "default_memory_list_limit")]
    pub limit: usize,

    /// Sort order: "recent" (last accessed), "popular" (access count), "newest" (created). Default: "recent".
    #[serde(default = "default_memory_list_sort")]
    pub sort: String,
}

fn default_memory_list_limit() -> usize {
    20
}

fn default_memory_list_sort() -> String {
    "recent".to_string()
}

// ---------------------------------------------------------------------------
// Code intelligence tool parameters
// ---------------------------------------------------------------------------

/// Parameters for code_graph: call graph queries with direction.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CodeGraphParams {
    /// Symbol name to look up.
    pub name: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,

    /// Graph direction: "calls" (default, outgoing), "callers" (incoming), or "impact" (transitive).
    #[serde(default = "default_direction")]
    pub direction: String,

    /// Exact symbol ID (use to disambiguate when multiple symbols share a name).
    #[serde(default)]
    pub symbol_id: Option<u32>,

    /// Maximum traversal depth when direction is "impact" (default: 3).
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
}

fn default_direction() -> String {
    "calls".to_string()
}

fn default_max_depth() -> usize {
    3
}

fn default_threshold() -> f32 {
    0.5
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
        assert!(params.scope.is_none());
    }

    #[test]
    fn test_search_params_with_scope() {
        let json = r#"{"query": "auth", "scope": "memory"}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "auth");
        assert_eq!(params.scope.as_deref(), Some("memory"));
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
    fn test_memory_delete_params_deserialize() {
        let json = r#"{"id": "auth-oauth2-pkce"}"#;
        let params: MemoryDeleteParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.id, "auth-oauth2-pkce");
    }

    #[test]
    fn test_memory_list_params_defaults() {
        let json = r#"{}"#;
        let params: MemoryListParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 20);
        assert_eq!(params.sort, "recent");
    }

    #[test]
    fn test_memory_list_params_custom() {
        let json = r#"{"limit": 5, "sort": "popular"}"#;
        let params: MemoryListParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.limit, 5);
        assert_eq!(params.sort, "popular");
    }

    // --- Code intelligence param tests (via SearchParams scopes) ---

    #[test]
    fn test_search_params_code_scope_defaults() {
        let json = r#"{"query": "auth handler", "scope": "code"}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "auth handler");
        assert_eq!(params.scope.as_deref(), Some("code"));
        assert!(params.kind.is_none());
        assert!((params.threshold - 0.5).abs() < f32::EPSILON);
        assert!(params.file.is_none());
    }

    #[test]
    fn test_search_params_code_scope_custom() {
        let json = r#"{"query": "pool", "scope": "code", "kind": "struct", "threshold": 0.5}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "pool");
        assert_eq!(params.scope.as_deref(), Some("code"));
        assert_eq!(params.kind.as_deref(), Some("struct"));
        assert!((params.threshold - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_search_params_symbols_scope_with_file() {
        let json = r#"{"query": "handler", "scope": "symbols", "kind": "function", "file": "src/main.rs"}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "handler");
        assert_eq!(params.scope.as_deref(), Some("symbols"));
        assert_eq!(params.kind.as_deref(), Some("function"));
        assert_eq!(params.file.as_deref(), Some("src/main.rs"));
    }

    #[test]
    fn test_code_graph_params_defaults() {
        let json = r#"{"name": "main"}"#;
        let params: CodeGraphParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "main");
        assert_eq!(params.direction, "calls");
        assert!(params.symbol_id.is_none());
        assert_eq!(params.max_depth, 3);
    }

    #[test]
    fn test_code_graph_params_callers() {
        let json = r#"{"name": "process", "direction": "callers", "symbol_id": 7}"#;
        let params: CodeGraphParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "process");
        assert_eq!(params.direction, "callers");
        assert_eq!(params.symbol_id, Some(7));
    }

    #[test]
    fn test_code_graph_params_impact() {
        let json = r#"{"name": "init", "direction": "impact", "symbol_id": 1, "max_depth": 5}"#;
        let params: CodeGraphParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.name, "init");
        assert_eq!(params.direction, "impact");
        assert_eq!(params.symbol_id, Some(1));
        assert_eq!(params.max_depth, 5);
    }

    // --- Root parameter backward compatibility tests ---

    #[test]
    fn test_search_params_root_absent() {
        let json = r#"{"query": "test"}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert!(params.root.is_none());
    }

    #[test]
    fn test_search_params_root_present() {
        let json = r#"{"query": "test", "root": "/repos/projectA"}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.root.as_deref(), Some("/repos/projectA"));
    }

    #[test]
    fn test_search_params_root_cross_repo() {
        let json = r#"{"query": "test", "root": "*"}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.root.as_deref(), Some("*"));
    }

    #[test]
    fn test_get_params_root_absent() {
        let json = r#"{"id": "readme.md"}"#;
        let params: GetParams = serde_json::from_str(json).unwrap();
        assert!(params.root.is_none());
    }

    #[test]
    fn test_get_params_root_present() {
        let json = r#"{"id": "readme.md", "root": "/repos/projectA"}"#;
        let params: GetParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.root.as_deref(), Some("/repos/projectA"));
    }

    #[test]
    fn test_memory_write_params_root() {
        let json = r#"{"id": "test", "title": "t", "content": "c", "root": "/foo"}"#;
        let params: MemoryWriteParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.root.as_deref(), Some("/foo"));
    }

    #[test]
    fn test_memory_list_params_root() {
        let json = r#"{"root": "/bar"}"#;
        let params: MemoryListParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.root.as_deref(), Some("/bar"));
    }

    #[test]
    fn test_code_graph_params_root() {
        let json = r#"{"name": "main", "root": "/project"}"#;
        let params: CodeGraphParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.root.as_deref(), Some("/project"));
        assert!(params.symbol_id.is_none());
    }
}
