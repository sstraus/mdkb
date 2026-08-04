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

    /// Minimum confidence threshold 0.0-1.0 when scope is "memory". Entries below this are excluded. Omit or 0.0 = no filter.
    #[serde(default)]
    pub min_confidence: Option<f64>,
}

fn default_limit() -> usize {
    10
}

/// Parameters for the get tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GetParams {
    /// Document ID, path, or memory slug.
    pub id: String,

    /// Exact repository root path (daemon mode). Omit for default/standalone repo. "*" is supported only by search.
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

    /// Full content. Omit when using source_file.
    #[serde(default)]
    pub content: String,

    /// Read content from this file path instead of content field. Mutually exclusive with content.
    #[serde(default)]
    pub source_file: Option<String>,

    /// Entry type: topic, problem, decision, reminder (time-bound; pair with due_in), prior (behavioral; 30d TTL default), or handoff (session handover).
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

    /// Reminder due time in seconds from now. Use with entry_type="reminder". Omit for non-reminders.
    #[serde(default)]
    pub due_in: Option<u64>,

    /// Typed edges from this entry (max 10): [{relation, target, target_kind}].
    #[serde(default)]
    pub relates: Vec<RelatesInput>,

    /// Authoring agent recorded as provenance (e.g. "claude", "codex").
    #[serde(default)]
    pub agent: Option<String>,

    /// On near-duplicate conflict: omitted rejects (default); "contradicts" writes the entry and links it to the similar one with a contradicts edge.
    #[serde(default)]
    pub on_conflict: Option<String>,

    /// When true, validate and report the action without persisting.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_entry_type() -> String {
    "topic".to_string()
}

fn default_source_type() -> String {
    "user_statement".to_string()
}

fn default_target_kind() -> String {
    "memory".to_string()
}

/// A typed relation to attach to a memory entry at write time.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct RelatesInput {
    /// Relation type.
    #[schemars(with = "crate::store::memory_graph::MemoryRelation")]
    pub relation: String,

    /// Target: a memory entry slug, or a document relative path when target_kind is "doc".
    pub target: String,

    /// Target kind (default "memory").
    #[serde(default = "default_target_kind")]
    #[schemars(with = "crate::store::memory_graph::TargetKind")]
    pub target_kind: String,
}

/// A single memory entry within a batch write.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryWriteBatchEntry {
    /// Entry ID (slug, e.g., "auth-oauth2-flow").
    pub id: String,

    /// Concise title (max 50 chars).
    pub title: String,

    /// Full content. Omit when using source_file.
    #[serde(default)]
    pub content: String,

    /// Read content from this file path instead of content field. Mutually exclusive with content.
    #[serde(default)]
    pub source_file: Option<String>,

    /// Entry type: topic, problem, decision, reminder (time-bound; pair with due_in), prior (behavioral; 30d TTL default), or handoff (session handover).
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

    /// Reminder due time in seconds from now. Use with entry_type="reminder". Omit for non-reminders.
    #[serde(default)]
    pub due_in: Option<u64>,

    /// Typed edges from this entry (max 10): [{relation, target, target_kind}].
    #[serde(default)]
    pub relates: Vec<RelatesInput>,

    /// Authoring agent recorded as provenance (e.g. "claude", "codex").
    #[serde(default)]
    pub agent: Option<String>,

    /// On near-duplicate conflict: omitted rejects (default); "contradicts" writes the entry and links it to the similar one with a contradicts edge.
    #[serde(default)]
    pub on_conflict: Option<String>,
}

/// Parameters for batch memory write.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryWriteBatchParams {
    /// Memory entries to write (max 20).
    pub entries: Vec<MemoryWriteBatchEntry>,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,

    /// When true, validate and report the actions without persisting any entry.
    #[serde(default)]
    pub dry_run: bool,
}

/// Parameters for the memory_delete tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryDeleteParams {
    /// Memory entry ID.
    pub id: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,

    /// When true, report whether the entry would be deleted without removing it.
    #[serde(default)]
    pub dry_run: bool,
}

/// Parameters for the memory_confirm tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryConfirmParams {
    /// Memory entry ID.
    pub id: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,

    /// Outcome signal: "confirmed" increments confirmations; "refuted" decrements (floor 0).
    pub outcome: String,
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

/// Parameters for graph: knowledge-graph queries with direction.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GraphParams {
    /// Entity to query: a document path, numeric ID, or raw slug.
    pub entity: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,

    /// Direction: "links" (default, outgoing edges), "backlinks" (incoming edges),
    /// "neighbors" (adjacent entities, undirected), or "path" (shortest path to `to`).
    #[serde(default = "default_graph_direction")]
    pub direction: String,

    /// Target entity, required when direction is "path".
    #[serde(default)]
    pub to: Option<String>,

    /// Filter to a single relation type (e.g. "owner", "themes", "links_to").
    #[serde(default)]
    pub relation: Option<String>,

    /// Traversal depth when direction is "neighbors" (default: 1).
    #[serde(default = "default_graph_depth")]
    pub depth: u32,

    /// Scope: "doc" (default, the document graph) or "memory" (the memory-entry graph).
    #[serde(default)]
    pub scope: Option<String>,
}

fn default_graph_direction() -> String {
    "links".to_string()
}

fn default_graph_depth() -> u32 {
    1
}

fn default_max_depth() -> usize {
    3
}

fn default_threshold() -> f32 {
    0.5
}

/// Parameters for the usage tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct UsageParams {
    /// Limit totals to current session (default: true). Set false to include lifetime aggregates across all sessions.
    #[serde(default = "default_session_only")]
    pub session_only: bool,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,
}

fn default_session_only() -> bool {
    true
}

/// Parameters for symbols_in_file: list all symbols in a specific file.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SymbolsInFileParams {
    /// Relative file path from repo root.
    pub file: String,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,
}

/// Parameters for code_find: exact symbol name lookup with optional filters.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct CodeFindParams {
    /// Exact symbol name to search for.
    pub name: String,

    /// Filter by symbol kind (e.g. "Function", "Struct", "Constant").
    #[serde(default)]
    pub kind: Option<String>,

    /// Filter results to files matching this substring.
    #[serde(default)]
    pub file: Option<String>,

    /// Max results to return (default 50).
    #[serde(default)]
    pub limit: Option<u32>,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,
}

/// Parameters for symbol_at_position: find the symbol at a given location.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SymbolAtPositionParams {
    /// Relative file path from repo root.
    pub file: String,

    /// 1-based line number.
    pub line: u32,

    /// 0-based column number (optional).
    #[serde(default)]
    pub col: Option<u32>,

    /// Repository root path (daemon mode). Omit for default/standalone repo.
    #[serde(default)]
    pub root: Option<String>,
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

    /// Resolve a property subschema of `RelatesInput`, following a `$ref` into
    /// the root `$defs` when schemars factors the enum out.
    fn relates_property_schema(property: &str) -> serde_json::Value {
        let root = serde_json::to_value(schemars::schema_for!(MemoryWriteParams)).unwrap();
        let defs = root.get("$defs").expect("schema has $defs");
        let sub = defs["RelatesInput"]["properties"][property].clone();
        match sub.get("$ref").and_then(|r| r.as_str()) {
            Some(r) => {
                let name = r.rsplit('/').next().expect("ref has a name");
                defs[name].clone()
            }
            None => sub,
        }
    }

    fn enum_values(schema: &serde_json::Value) -> Vec<String> {
        schema["enum"]
            .as_array()
            .expect("subschema declares enum")
            .iter()
            .map(|v| v.as_str().expect("enum value is a string").to_string())
            .collect()
    }

    /// The wire type is `String`, so nothing but the schema stops a client from
    /// inventing a relation (agents have guessed "implements"). Assert the closed
    /// set reaches the JSON Schema, straight from the domain enum.
    #[test]
    fn test_relates_relation_schema_advertises_closed_set() {
        let expected: Vec<String> = crate::store::memory_graph::MemoryRelation::ALL
            .iter()
            .map(|r| r.as_str().to_string())
            .collect();
        assert_eq!(
            enum_values(&relates_property_schema("relation")),
            expected,
            "relation must advertise exactly MemoryRelation::ALL"
        );
    }

    #[test]
    fn test_relates_target_kind_schema_advertises_closed_set() {
        assert_eq!(
            enum_values(&relates_property_schema("target_kind")),
            vec!["memory".to_string(), "doc".to_string()],
        );
    }
}
