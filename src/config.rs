//! Configuration management for .mdkb/config.toml.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{ErrorKind, Result};

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

    /// MCP server configuration.
    pub mcp: McpConfig,

    /// Convention-based auto-collection detection.
    pub conventions: ConventionsConfig,

    /// Code intelligence configuration.
    pub code: CodeConfig,

    /// Knowledge-graph edge extraction.
    pub graph: GraphConfig,

    /// Claude Code / Codex lifecycle hooks.
    pub hooks: HooksConfig,

    /// Database maintenance.
    pub db: DbConfig,

    /// AI-distilled behavioral-prior mining.
    pub priors: PriorsConfig,

    /// Usage telemetry (opt-in, privacy-safe).
    pub telemetry: TelemetryConfig,
}

/// Usage telemetry settings. Hook-call counts are always recorded (counts, not
/// content); richer per-query events are opt-in and never store query text.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Record a `query_events` row per recall search (hash + latency + result
    /// count — NEVER the query text). Off by default: it is the input for the
    /// self-evaluation roadmap, opt-in until that ships.
    pub query_events: bool,
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

    /// When true, the document/collection walker honors `.gitignore`,
    /// `.git/info/exclude` and the global gitignore. When false (default),
    /// gitignore is ignored and `.mdkbignore` is read instead — preserving
    /// the historical behavior where gitignored directories like `stories/`
    /// and `plans/` remain indexed.
    pub respect_gitignore: bool,
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

    /// Auto-embed changed documents during `mdkb update` so hybrid search never
    /// silently degrades to BM25. Killable if the ONNX cost is unwanted.
    pub auto_embed_docs: bool,

    /// Include the `claude_sessions` collection in auto-embed / `mdkb embed`.
    /// Off by default: transcripts are large, high-churn, and excluded from
    /// default search — embed them explicitly with `mdkb embed --collection`.
    pub auto_embed_sessions: bool,

    /// Embed memory entries on write (`memory add`, `memory import`) so they are
    /// vector-searchable immediately, like the MCP path. On by default. Set false
    /// to make writes never touch the ONNX model — the entry is left pending and
    /// `mdkb update` backfills it. Also the hermetic switch for tests that write
    /// memory but don't exercise embeddings.
    pub auto_embed_memory: bool,

    /// Memory-scope search tuning.
    pub memory: SearchMemoryConfig,
}

/// Memory-scope search tuning.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchMemoryConfig {
    /// RRF weight for the access-count × recency signal.
    ///
    /// Ranks memories higher when they have been `get`'d frequently and
    /// recently. `0.0` disables the signal. The get path is the only writer:
    /// `search` must NOT bump `access_count`, preserving SELECT idempotency.
    pub access_recency_weight: f64,

    /// Half-life for recency decay in seconds (default ~30 days).
    pub recency_half_life_secs: i64,
}

impl Default for SearchMemoryConfig {
    fn default() -> Self {
        Self {
            access_recency_weight: 0.2,
            recency_half_life_secs: 30 * 24 * 60 * 60,
        }
    }
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
    /// Inactivity timeout before unloading models (seconds).
    pub inactivity_timeout_secs: u64,
}

/// Dead `[models]` keys that once selected the embedding model. The embedder is
/// now fixed to all-MiniLM-L6-v2 (384-dim), so these are ignored if present —
/// [`detect_dead_model_keys`] surfaces a warning rather than silently accepting
/// them (they were never functional).
pub const DEAD_MODEL_KEYS: &[&str] = &["embedding_repo", "embedding_file"];

/// Return any dead `[models]` embedding keys present in raw config TOML so the
/// caller can warn the user they are ignored (the embedder is fixed).
pub fn detect_dead_model_keys(raw_toml: &str) -> Vec<&'static str> {
    let Ok(value) = raw_toml.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(models) = value.get("models").and_then(|m| m.as_table()) else {
        return Vec::new();
    };
    DEAD_MODEL_KEYS
        .iter()
        .filter(|k| models.contains_key(**k))
        .copied()
        .collect()
}

/// Convention-based auto-collection detection settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConventionsConfig {
    /// Enable auto-detection of convention-based collections.
    pub enabled: bool,
}

/// Knowledge-graph edge extraction settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GraphConfig {
    /// Enable knowledge-graph edge extraction during indexing.
    pub enabled: bool,

    /// Frontmatter keys treated as typed edges (allowlist). Evolution's own keys
    /// (supersedes/updates/corrects/retracts/extends) are owned by the evolution
    /// subsystem and should not be listed here.
    pub frontmatter_relations: Vec<String>,

    /// Extract body wikilinks (`[[target]]`) as soft edges.
    pub include_wikilinks: bool,
}

/// MCP server settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Maximum tokens per response (0 = unlimited).
    pub max_response_tokens: usize,

    /// Maximum tokens per document in multi_get (0 = unlimited).
    pub max_document_tokens: usize,

    /// Truncate content with ellipsis when exceeding limits.
    pub truncate_with_ellipsis: bool,

    /// Include token count in response metadata.
    pub include_token_count: bool,
}

/// Code intelligence configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeConfig {
    /// Enable code intelligence features.
    pub enabled: bool,

    /// Index path relative to .mdkb/.
    pub index_path: String,

    /// Code indexing pipeline settings.
    pub indexing: CodeIndexingConfig,

    /// Semantic code search settings.
    pub semantic_search: CodeSemanticSearchConfig,
}

/// Code indexing pipeline settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeIndexingConfig {
    /// Worker threads for parsing (0 = auto-detect from CPU count).
    pub parallelism: usize,

    /// Glob patterns to ignore during indexing.
    pub ignore_patterns: Vec<String>,

    /// Batch size for pipeline commits.
    pub batch_size: usize,

    /// When true (default), the code walker honors `.gitignore`,
    /// `.git/info/exclude` and the global gitignore. The `# mdkb:index`
    /// annotation remains active in this mode. When false, gitignore is
    /// ignored and `.mdkbignore` is read instead.
    pub respect_gitignore: bool,

    /// File-watcher debounce interval (ms). Rapid filesystem events within this
    /// window collapse into one. Raised from the historical 100ms to a gentler
    /// 300ms default so editor save-storms don't wake the reindexer repeatedly.
    pub debounce_ms: u64,

    /// Idle window (ms) the watcher waits after the last change before flushing
    /// an incremental reindex, coalescing an editing session into a single pass.
    /// Kept long (30s) by default because each flush re-embeds changed code
    /// symbols; lower it for a fresher index at the cost of more ONNX passes.
    pub batch_idle_ms: u64,
}

/// Semantic code search settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeSemanticSearchConfig {
    /// Enable semantic (embedding-based) code search.
    pub enabled: bool,

    /// Embedding model identifier (e.g., "AllMiniLML6V2").
    pub model: String,

    /// Minimum cosine similarity threshold for results.
    pub threshold: f64,
}

impl Default for CodeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            index_path: "code.sqlite".to_string(),
            indexing: CodeIndexingConfig::default(),
            semantic_search: CodeSemanticSearchConfig::default(),
        }
    }
}

impl Default for CodeIndexingConfig {
    fn default() -> Self {
        Self {
            parallelism: DEFAULT_CODE_PARALLELISM,
            ignore_patterns: DEFAULT_CODE_IGNORE_PATTERNS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            batch_size: DEFAULT_CODE_BATCH_SIZE,
            respect_gitignore: true,
            debounce_ms: DEFAULT_CODE_DEBOUNCE_MS,
            batch_idle_ms: DEFAULT_CODE_BATCH_IDLE_MS,
        }
    }
}

impl Default for CodeSemanticSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: DEFAULT_CODE_SEMANTIC_MODEL.to_string(),
            threshold: DEFAULT_CODE_SEMANTIC_THRESHOLD,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            indexing: IndexingConfig::default(),
            chunking: ChunkingConfig::default(),
            search: SearchConfig::default(),
            memory: MemoryConfig::default(),
            models: ModelsConfig::default(),
            mcp: McpConfig::default(),
            conventions: ConventionsConfig::default(),
            code: CodeConfig::default(),
            graph: GraphConfig::default(),
            hooks: HooksConfig::default(),
            db: DbConfig::default(),
            priors: PriorsConfig::default(),
            telemetry: TelemetryConfig::default(),
        }
    }
}

/// Database maintenance settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DbConfig {
    /// Trigger `PRAGMA optimize` every N persistent tool calls. `0` disables runtime optimize.
    pub optimize_interval_calls: u64,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            optimize_interval_calls: 200,
        }
    }
}

/// AI-distilled behavioral-prior mining settings.
///
/// The whole subsystem is a kill-switched opt-in: `mining_enabled` gates the
/// Stop-hook episode→candidate→distill→promote pipeline, and it stays off until
/// a `distiller_program` is configured (mdkb ships ONNX embeddings only, no chat
/// model, so distillation requires an external agent CLI). Injection of already
/// promoted priors is a separate, cheaper toggle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PriorsConfig {
    /// Master kill switch for the mining pipeline (Stop-hook distillation). Off
    /// by default: no episode is distilled until a human opts in.
    pub mining_enabled: bool,

    /// External agent CLI that distills a candidate episode into a prior (e.g.
    /// `claude`). The prompt is piped on stdin. `None` disables mining even when
    /// `mining_enabled` is true — there is no built-in chat model to fall back to.
    pub distiller_program: Option<String>,

    /// Arguments passed to `distiller_program` (e.g. `["-p"]` for headless mode).
    pub distiller_args: Vec<String>,

    /// Surface promoted, trigger-matched priors at PreToolUse / UserPromptSubmit.
    /// Independent of `mining_enabled` so already-mined priors keep helping even
    /// if further mining is paused.
    pub injection_enabled: bool,

    /// Hard cap on promoted priors injected into a single hook invocation.
    pub max_injected_per_hook: usize,
}

impl Default for PriorsConfig {
    fn default() -> Self {
        Self {
            mining_enabled: false,
            distiller_program: None,
            distiller_args: Vec::new(),
            injection_enabled: true,
            max_injected_per_hook: 1,
        }
    }
}

/// The raw `[priors]` table declared in a TOML config file, or `None` when the
/// file is absent, unparseable, or omits the section. Only keys the user set
/// explicitly are returned, so a layered merge can distinguish "unset" from
/// "set to the default value".
pub fn raw_priors_layer(path: impl AsRef<Path>) -> Option<toml::Table> {
    let content = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = toml::from_str(&content).ok()?;
    match table.get("priors") {
        Some(toml::Value::Table(t)) => Some(t.clone()),
        _ => None,
    }
}

/// Merge a global `[priors]` base with an optional per-repo override, the repo's
/// keys winning field-by-field, then deserialize into a typed [`PriorsConfig`]
/// (any key set in neither layer falls back to its default). This is the single
/// definition of priors layering: `default < global daemon.toml < per-repo
/// config.toml`. A type-invalid merged value degrades to defaults with a warning
/// rather than aborting a repo open.
pub fn merge_priors(global: &toml::Table, repo: Option<&toml::Table>) -> PriorsConfig {
    let mut merged = global.clone();
    if let Some(r) = repo {
        for (k, v) in r {
            merged.insert(k.clone(), v.clone());
        }
    }
    match toml::Value::Table(merged).try_into() {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("invalid [priors] config, using defaults: {e}");
            PriorsConfig::default()
        }
    }
}

/// Lifecycle hook settings for Claude Code / Codex integration.
///
/// Hooks are fire-and-forget: any internal error must still return exit code 0
/// so the host CLI is never blocked. These toggles let users disable individual
/// events per-project via `.mdkb/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HooksConfig {
    pub session_start_enabled: bool,
    pub user_prompt_submit_enabled: bool,
    pub post_tool_use_enabled: bool,
    pub pre_tool_use_enabled: bool,

    /// Maximum number of entries injected on SessionStart (warmup). Secondary
    /// bound — `warmup_token_budget` is the primary cap.
    pub warmup_limit: usize,

    /// Approximate token budget for the warmup block (≈4 chars/token). Emission
    /// stops before a line that would exceed it — lines are never truncated
    /// mid-way, so every injected line keeps its id+type+title+tags.
    pub warmup_token_budget: usize,

    /// Maximum number of recall results injected on UserPromptSubmit.
    pub recall_limit: usize,

    /// Latency budget in milliseconds; hook truncates output if exceeded.
    pub latency_budget_ms: u64,

    /// Minimum hybrid score for a recall result to be injected.
    pub min_recall_score: f64,

    /// Minimum confidence for a warmup entry to be injected. `0.0` (default)
    /// disables the floor — every access-ranked entry is eligible.
    pub warmup_min_confidence: f64,

    /// Half-life for access-recency re-ranking in seconds (default 7 days).
    ///
    /// Controls how quickly the recency signal decays. Entries accessed more
    /// recently get a larger boost. Set to 0 to disable re-ranking.
    pub recall_half_life_secs: i64,

    /// When true, hooks require a running daemon and skip the in-process
    /// fallback (MDKB_NO_DAEMON=1). The generated shell command will NOT
    /// include the `if ! ...; then MDKB_NO_DAEMON=1 ...; fi` wrapper.
    pub daemon_required: bool,

    /// On a definition Grep/Bash search (`fn X`, `struct X`, …), inject the real
    /// `file:line` hits from the code index instead of a "use mdkb" suggestion.
    /// Falls back to the suggestion when the symbol is not indexed.
    pub code_hits_in_pretooluse: bool,

    /// When the prompt names a document (path or `.md`), inject up to 3 one-hop
    /// frontmatter graph neighbors (paths + relation labels) in UserPromptSubmit.
    pub doc_graph_in_recall: bool,

    /// Opt-in gate for UserPromptSubmit injection. When true, context (recall,
    /// related docs, priors, call-graph hint) is injected ONLY for prompts that
    /// begin with `*`; every other prompt is left untouched. The leading `*` is
    /// stripped before recall so it never reaches FTS or the model. Default
    /// `false` keeps the always-on behavior.
    pub user_prompt_submit_require_sigil: bool,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            session_start_enabled: true,
            user_prompt_submit_enabled: true,
            post_tool_use_enabled: true,
            pre_tool_use_enabled: true,
            warmup_limit: 10,
            warmup_token_budget: 300,
            recall_limit: 5,
            latency_budget_ms: 200,
            min_recall_score: 0.3,
            warmup_min_confidence: 0.25,
            recall_half_life_secs: 7 * 24 * 60 * 60, // 7 days
            daemon_required: false,
            code_hits_in_pretooluse: true,
            doc_graph_in_recall: true,
            user_prompt_submit_require_sigil: false,
        }
    }
}

impl Default for ConventionsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            frontmatter_relations: ["owner", "stakeholders", "themes", "related"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            include_wikilinks: true,
        }
    }
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            default_pattern: "**/*.md".to_string(),
            debounce_ms: DEFAULT_DEBOUNCE_MS,
            parse_frontmatter: true,
            parse_wikilinks: true,
            index_headings: true,
            respect_gitignore: false,
        }
    }
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            strategy: "markdown".to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            overlap_tokens: DEFAULT_OVERLAP_TOKENS,
            include_header_path: true,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_limit: 10,
            min_score: 0.0,
            rrf_k: DEFAULT_RRF_K,
            bm25_weight: DEFAULT_BM25_WEIGHT,
            vector_weight: DEFAULT_VECTOR_WEIGHT,
            auto_embed_docs: true,
            auto_embed_sessions: false,
            auto_embed_memory: true,
            memory: SearchMemoryConfig::default(),
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            directory: "memory".to_string(),
            warmup_limit: DEFAULT_WARMUP_LIMIT,
            title_max_chars: 50,
            order_by: "access_count".to_string(),
            track_access: true,
        }
    }
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            inactivity_timeout_secs: DEFAULT_INACTIVITY_TIMEOUT_SECS,
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            max_response_tokens: DEFAULT_MAX_RESPONSE_TOKENS,
            max_document_tokens: DEFAULT_MAX_DOCUMENT_TOKENS,
            truncate_with_ellipsis: true,
            include_token_count: false,
        }
    }
}

/// Valid chunking strategies.
const VALID_CHUNKING_STRATEGIES: &[&str] = &["fixed", "markdown", "semantic"];

/// Valid memory order_by values.
const VALID_ORDER_BY: &[&str] = &["access_count", "updated_at", "created_at"];

/// Minimum allowed max_tokens for chunking.
/// 64 tokens is the minimum practical size for semantic coherence.
const MIN_MAX_TOKENS: usize = 64;

// =============================================================================
// Default value constants with documentation
// =============================================================================

/// Default debounce interval for file watcher (milliseconds).
/// 100ms provides responsive updates while batching rapid file saves together.
/// This is fast enough for interactive use but avoids redundant reindexing
/// when editors save files multiple times in quick succession.
const DEFAULT_DEBOUNCE_MS: u64 = 100;

/// Maximum tokens per chunk for embedding models.
/// 512 is the typical context limit for embedding models like nomic-embed-text.
/// Larger values may truncate; smaller values reduce semantic coherence.
const DEFAULT_MAX_TOKENS: usize = 512;

/// Token overlap between consecutive chunks.
/// 64 tokens (~12.5% of 512) maintains context continuity across chunk boundaries
/// without excessive redundancy.
const DEFAULT_OVERLAP_TOKENS: usize = 64;

/// RRF (Reciprocal Rank Fusion) constant k.
/// Standard value from Cormack et al. (2009) "Reciprocal Rank Fusion outperforms
/// Condorcet and individual Rank Learning Methods". k=60 balances contributions
/// from different ranking sources; lower values favor top-ranked results more.
const DEFAULT_RRF_K: u32 = 60;

/// BM25 weight in hybrid search.
/// 1.0 gives full weight to keyword/lexical matching.
const DEFAULT_BM25_WEIGHT: f64 = 1.0;

/// Vector similarity weight in hybrid search.
/// 0.7 gives semantic search slightly less influence than BM25, reflecting that
/// exact keyword matches are often more reliable than semantic similarity.
const DEFAULT_VECTOR_WEIGHT: f64 = 0.7;

/// Maximum tokens per MCP response.
/// 50,000 tokens is a reasonable limit that fits within most LLM context windows
/// while providing substantial content. Set to 0 for unlimited.
const DEFAULT_MAX_RESPONSE_TOKENS: usize = 50_000;

/// Maximum tokens per document in multi_get.
/// 10,000 tokens per document prevents single large files from consuming
/// the entire response budget. Set to 0 for unlimited.
const DEFAULT_MAX_DOCUMENT_TOKENS: usize = 10_000;

/// Maximum documents in memory warmup index.
/// 50 entries is enough for common documents without excessive memory use.
const DEFAULT_WARMUP_LIMIT: usize = 50;

/// Model inactivity timeout before unloading (seconds).
/// 2 minutes allows for interactive use patterns while freeing memory
/// when the user has moved on to other tasks.
const DEFAULT_INACTIVITY_TIMEOUT_SECS: u64 = 120;

/// Code indexing worker threads. 0 = auto-detect from CPU count.
/// Auto-detect uses crossbeam's built-in thread pool sizing.
const DEFAULT_CODE_PARALLELISM: usize = 0;

/// Glob patterns to ignore during code indexing.
/// Covers common build output, dependencies, and generated files.
const DEFAULT_CODE_IGNORE_PATTERNS: &[&str] = &[
    "**/target/**",
    "**/node_modules/**",
    "**/.git/**",
    "**/vendor/**",
    "**/dist/**",
    "**/build/**",
    "**/__pycache__/**",
    "**/.venv/**",
];

/// Batch size for pipeline commits during code indexing.
/// 500 balances memory usage with commit overhead.
const DEFAULT_CODE_BATCH_SIZE: usize = 500;

/// Default file-watcher debounce (ms) for code indexing. 300ms coalesces an
/// editor's rapid save/temp-file churn without a perceptible freshness lag.
const DEFAULT_CODE_DEBOUNCE_MS: u64 = 300;

/// Default idle window (ms) before flushing a coalesced incremental reindex.
/// 30s lets a whole editing session accumulate into one batch: each flush of the
/// code index re-embeds the changed symbols (an ONNX pass), so a short window
/// would re-run inference on every stop-start pause. The live PostToolUse path
/// still injects edited files directly, so the index isn't actually 30s stale in
/// practice. Now config-driven via `[code.indexing] batch_idle_ms`.
const DEFAULT_CODE_BATCH_IDLE_MS: u64 = 30_000;

/// Default embedding model for semantic code search.
/// AllMiniLML6V2 is a fast, lightweight model (384 dimensions).
const DEFAULT_CODE_SEMANTIC_MODEL: &str = "AllMiniLML6V2";

/// Default cosine similarity threshold for semantic code search.
/// 0.3 is a permissive default; higher values improve precision at cost of recall.
const DEFAULT_CODE_SEMANTIC_THRESHOLD: f64 = 0.3;

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mdkb::Config;
    ///
    /// let config = Config::load(".mdkb/config.toml")?;
    /// println!("Search limit: {}", config.search.default_limit);
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::ConfigNotFound`] if the file doesn't exist.
    /// Returns [`ErrorKind::Io`] if the file can't be read.
    /// Returns [`ErrorKind::TomlParse`] if the TOML is malformed.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(ErrorKind::ConfigNotFound {
                path: path.to_path_buf(),
            }
            .into());
        }
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        config.validate()?;
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
            return Err(ErrorKind::ConfigInvalid {
                field: "search.default_limit".to_string(),
                message: "must be greater than 0".to_string(),
            }
            .into());
        }

        // Chunking validation
        if !VALID_CHUNKING_STRATEGIES.contains(&self.chunking.strategy.as_str()) {
            return Err(ErrorKind::ConfigInvalid {
                field: "chunking.strategy".to_string(),
                message: format!("must be one of: {}", VALID_CHUNKING_STRATEGIES.join(", ")),
            }
            .into());
        }

        if self.chunking.max_tokens < MIN_MAX_TOKENS {
            return Err(ErrorKind::ConfigInvalid {
                field: "chunking.max_tokens".to_string(),
                message: format!("must be at least {MIN_MAX_TOKENS}"),
            }
            .into());
        }

        if self.chunking.overlap_tokens >= self.chunking.max_tokens {
            return Err(ErrorKind::ConfigInvalid {
                field: "chunking.overlap_tokens".to_string(),
                message: "must be less than max_tokens".to_string(),
            }
            .into());
        }

        // Code indexing validation
        if self.code.indexing.batch_size == 0 {
            return Err(ErrorKind::ConfigInvalid {
                field: "code.indexing.batch_size".to_string(),
                message: "must be greater than 0".to_string(),
            }
            .into());
        }

        if self.code.semantic_search.threshold < 0.0 || self.code.semantic_search.threshold > 1.0 {
            return Err(ErrorKind::ConfigInvalid {
                field: "code.semantic_search.threshold".to_string(),
                message: "must be between 0.0 and 1.0".to_string(),
            }
            .into());
        }

        // Memory validation
        if !VALID_ORDER_BY.contains(&self.memory.order_by.as_str()) {
            return Err(ErrorKind::ConfigInvalid {
                field: "memory.order_by".to_string(),
                message: format!("must be one of: {}", VALID_ORDER_BY.join(", ")),
            }
            .into());
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
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::*;

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

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

    #[test]
    fn test_hooks_config_defaults() {
        let cfg = HooksConfig::default();
        // warmup_limit is the secondary bound; warmup_token_budget is primary.
        assert_eq!(cfg.warmup_limit, 10);
        assert_eq!(cfg.warmup_token_budget, 300);
        assert_eq!(cfg.recall_limit, 5);
        // Confidence floor on by default so low-signal entries stay out of warmup.
        assert_eq!(cfg.warmup_min_confidence, 0.25);
    }

    #[test]
    fn test_hooks_config_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.hooks.warmup_limit, parsed.hooks.warmup_limit);
        assert_eq!(config.hooks.recall_limit, parsed.hooks.recall_limit);
    }

    // ==================== Code Indexing Watcher Tests ====================

    #[test]
    fn test_code_indexing_watcher_gentle_defaults() {
        let cfg = Config::default();
        assert_eq!(
            cfg.code.indexing.debounce_ms, 300,
            "gentle debounce default"
        );
        assert_eq!(
            cfg.code.indexing.batch_idle_ms, 30_000,
            "batch-idle stays long: each flush re-embeds changed code (ONNX cost)"
        );
    }

    #[test]
    fn test_code_indexing_watcher_overrides_parse() {
        let toml_str = "[code.indexing]\ndebounce_ms = 1000\nbatch_idle_ms = 5000\n";
        let cfg: Config = toml::from_str(toml_str).expect("parse [code.indexing] overrides");
        assert_eq!(cfg.code.indexing.debounce_ms, 1000);
        assert_eq!(cfg.code.indexing.batch_idle_ms, 5000);
        // Unspecified sibling fields keep their defaults (serde(default)).
        assert!(cfg.code.indexing.respect_gitignore);
    }

    #[test]
    fn test_code_indexing_watcher_partial_override_keeps_other_default() {
        // Only debounce set → batch_idle stays at the gentle default.
        let cfg: Config =
            toml::from_str("[code.indexing]\ndebounce_ms = 200\n").expect("partial parse");
        assert_eq!(cfg.code.indexing.debounce_ms, 200);
        assert_eq!(cfg.code.indexing.batch_idle_ms, 30_000);
    }

    #[test]
    fn test_code_indexing_watcher_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(
            config.code.indexing.debounce_ms,
            parsed.code.indexing.debounce_ms
        );
        assert_eq!(
            config.code.indexing.batch_idle_ms,
            parsed.code.indexing.batch_idle_ms
        );
    }

    // ==================== Models Config Tests ====================

    #[test]
    fn test_models_config_defaults() {
        let config = Config::default();
        assert_eq!(
            config.models.inactivity_timeout_secs,
            DEFAULT_INACTIVITY_TIMEOUT_SECS
        );
    }

    #[test]
    fn test_dead_model_keys_detected() {
        let raw = "[models]\nembedding_repo = \"x\"\nembedding_file = \"y\"\n";
        let dead = detect_dead_model_keys(raw);
        assert!(dead.contains(&"embedding_repo"));
        assert!(dead.contains(&"embedding_file"));
    }

    #[test]
    fn test_no_dead_model_keys_when_absent() {
        assert!(detect_dead_model_keys("[models]\ninactivity_timeout_secs = 60\n").is_empty());
        assert!(detect_dead_model_keys("").is_empty());
        // Legacy config with dead keys still parses (keys ignored, not an error).
        let cfg: Config =
            toml::from_str("[models]\nembedding_repo = \"x\"\n").expect("legacy config parses");
        assert_eq!(
            cfg.models.inactivity_timeout_secs,
            DEFAULT_INACTIVITY_TIMEOUT_SECS
        );
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
    fn test_load_rejects_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        // search.default_limit = 0 violates validate()
        let toml_content = r#"
[search]
default_limit = 0
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = Config::load(&config_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("default_limit"));
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
        let _guard = env_lock();
        unsafe { std::env::set_var("MDKB_SEARCH_DEFAULT_LIMIT", "25") };
        let config = Config::from_env_with_defaults();
        unsafe { std::env::remove_var("MDKB_SEARCH_DEFAULT_LIMIT") };
        assert_eq!(config.search.default_limit, 25);
    }

    #[test]
    fn test_env_override_indexing_debounce() {
        let _guard = env_lock();
        unsafe { std::env::set_var("MDKB_INDEXING_DEBOUNCE_MS", "500") };
        let config = Config::from_env_with_defaults();
        unsafe { std::env::remove_var("MDKB_INDEXING_DEBOUNCE_MS") };
        assert_eq!(config.indexing.debounce_ms, 500);
    }

    #[test]
    fn test_env_override_memory_warmup_limit() {
        let _guard = env_lock();
        unsafe { std::env::set_var("MDKB_MEMORY_WARMUP_LIMIT", "100") };
        let config = Config::from_env_with_defaults();
        unsafe { std::env::remove_var("MDKB_MEMORY_WARMUP_LIMIT") };
        assert_eq!(config.memory.warmup_limit, 100);
    }

    #[test]
    fn test_env_override_invalid_value_uses_default() {
        let _guard = env_lock();
        unsafe { std::env::set_var("MDKB_SEARCH_DEFAULT_LIMIT", "not_a_number") };
        let config = Config::from_env_with_defaults();
        unsafe { std::env::remove_var("MDKB_SEARCH_DEFAULT_LIMIT") };
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
        assert!(toml_str.contains("[code]"));
        assert!(toml_str.contains("[graph]"));
    }

    // ==================== Graph Config Tests ====================

    #[test]
    fn test_graph_config_defaults() {
        let config = Config::default();
        assert!(config.graph.enabled);
        assert!(config.graph.include_wikilinks);
        assert!(
            config
                .graph
                .frontmatter_relations
                .contains(&"owner".to_string()),
            "default allowlist should include 'owner'"
        );
        // Evolution's reserved keys must not leak into the graph allowlist.
        assert!(
            !config
                .graph
                .frontmatter_relations
                .contains(&"supersedes".to_string()),
            "evolution keys must not be in the graph allowlist"
        );
    }

    #[test]
    fn test_graph_config_serialization_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("[graph]"));

        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.graph.enabled, config.graph.enabled);
        assert_eq!(
            parsed.graph.include_wikilinks,
            config.graph.include_wikilinks
        );
        assert_eq!(
            parsed.graph.frontmatter_relations,
            config.graph.frontmatter_relations
        );
    }

    // ==================== Code Config Tests ====================

    #[test]
    fn test_code_config_defaults() {
        let config = Config::default();
        assert!(config.code.enabled);
        assert_eq!(config.code.index_path, "code.sqlite");
        assert_eq!(config.code.indexing.parallelism, 0);
        assert_eq!(config.code.indexing.batch_size, 500);
        assert!(!config.code.indexing.ignore_patterns.is_empty());
        assert!(
            config
                .code
                .indexing
                .ignore_patterns
                .contains(&"**/target/**".to_string())
        );
        assert!(config.code.semantic_search.enabled);
        assert_eq!(config.code.semantic_search.model, "AllMiniLML6V2");
    }

    #[test]
    fn test_code_config_serialization_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(toml_str.contains("[code]"));
        assert!(toml_str.contains("[code.indexing]"));
        assert!(toml_str.contains("[code.semantic_search]"));

        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.code.enabled, config.code.enabled);
        assert_eq!(parsed.code.index_path, config.code.index_path);
        assert_eq!(
            parsed.code.indexing.batch_size,
            config.code.indexing.batch_size
        );
        assert_eq!(
            parsed.code.semantic_search.threshold,
            config.code.semantic_search.threshold
        );
    }

    #[test]
    fn test_code_config_partial_override() {
        let toml_content = r#"
[code]
enabled = false
index_path = "my-code-idx"

[code.indexing]
batch_size = 1000

[code.semantic_search]
enabled = true
threshold = 0.5
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(!config.code.enabled);
        assert_eq!(config.code.index_path, "my-code-idx");
        assert_eq!(config.code.indexing.batch_size, 1000);
        assert_eq!(config.code.indexing.parallelism, 0);
        assert!(config.code.semantic_search.enabled);
        assert_eq!(config.code.semantic_search.threshold, 0.5);
    }

    #[test]
    fn test_validate_code_batch_size_zero() {
        let mut config = Config::default();
        config.code.indexing.batch_size = 0;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("batch_size"));
    }

    #[test]
    fn test_validate_code_threshold_out_of_range() {
        let mut config = Config::default();
        config.code.semantic_search.threshold = 1.5;
        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("threshold"));

        config.code.semantic_search.threshold = -0.1;
        let result = config.validate();
        assert!(result.is_err());
    }

    #[test]
    fn test_priors_config_defaults_are_off_and_safe() {
        let config = Config::default();
        // Mining is a kill-switched opt-in: off, and no distiller wired.
        assert!(!config.priors.mining_enabled);
        assert!(config.priors.distiller_program.is_none());
        assert!(config.priors.distiller_args.is_empty());
        // Injecting already-promoted priors is safe/cheap and on by default,
        // hard-capped so a hook can never flood context.
        assert!(config.priors.injection_enabled);
        assert_eq!(config.priors.max_injected_per_hook, 1);
    }

    #[test]
    fn test_priors_config_roundtrips_through_toml() {
        let toml = r#"
[priors]
mining_enabled = true
distiller_program = "claude"
distiller_args = ["-p"]
max_injected_per_hook = 3
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.priors.mining_enabled);
        assert_eq!(config.priors.distiller_program.as_deref(), Some("claude"));
        assert_eq!(config.priors.distiller_args, vec!["-p".to_string()]);
        assert_eq!(config.priors.max_injected_per_hook, 3);
        // Unset field falls back to its default.
        assert!(config.priors.injection_enabled);
    }

    #[test]
    fn merge_priors_repo_overrides_global_field_by_field() {
        // Global sets the machine-wide distiller and turns mining on.
        let global: toml::Table = toml::from_str(
            r#"mining_enabled = true
distiller_program = "codex"
distiller_args = ["exec", "-m", "gpt-5-mini"]"#,
        )
        .unwrap();
        // Repo overrides only mining_enabled; everything else must be inherited.
        let repo: toml::Table = toml::from_str("mining_enabled = false").unwrap();

        let merged = merge_priors(&global, Some(&repo));
        assert!(!merged.mining_enabled, "repo key wins");
        assert_eq!(
            merged.distiller_program.as_deref(),
            Some("codex"),
            "distiller inherited from global"
        );
        assert_eq!(merged.distiller_args, vec!["exec", "-m", "gpt-5-mini"]);
        // A key set in neither layer keeps its default.
        assert!(merged.injection_enabled);
    }

    #[test]
    fn merge_priors_no_repo_layer_yields_global() {
        let global: toml::Table = toml::from_str(r#"distiller_program = "codex""#).unwrap();
        let merged = merge_priors(&global, None);
        assert_eq!(merged.distiller_program.as_deref(), Some("codex"));
        assert!(!merged.mining_enabled, "unset stays default-off");
    }

    #[test]
    fn merge_priors_empty_global_is_all_defaults() {
        let merged = merge_priors(&toml::Table::new(), None);
        assert_eq!(merged, PriorsConfig::default());
    }

    #[test]
    fn raw_priors_layer_returns_only_present_sections() {
        let dir = tempfile::tempdir().unwrap();

        let with = dir.path().join("with.toml");
        std::fs::write(
            &with,
            "[priors]\nmining_enabled = true\n[hooks]\nstop = false\n",
        )
        .unwrap();
        let layer = raw_priors_layer(&with).expect("priors section present");
        assert_eq!(
            layer.get("mining_enabled"),
            Some(&toml::Value::Boolean(true))
        );
        assert!(
            !layer.contains_key("stop"),
            "only the priors table is lifted"
        );

        let without = dir.path().join("without.toml");
        std::fs::write(&without, "[hooks]\nstop = false\n").unwrap();
        assert!(raw_priors_layer(&without).is_none());

        assert!(raw_priors_layer(dir.path().join("missing.toml")).is_none());
    }

    #[test]
    fn test_code_config_ignore_patterns_default() {
        let config = CodeIndexingConfig::default();
        assert!(
            config
                .ignore_patterns
                .contains(&"**/node_modules/**".to_string())
        );
        assert!(config.ignore_patterns.contains(&"**/.git/**".to_string()));
        assert!(config.ignore_patterns.contains(&"**/vendor/**".to_string()));
    }
}
