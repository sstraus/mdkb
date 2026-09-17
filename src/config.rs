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

    /// Usage telemetry (opt-in and privacy-minimized, not anonymous).
    pub telemetry: TelemetryConfig,
}

/// Usage telemetry settings. Hook-call counts are always recorded (counts, not
/// content); richer per-query events are opt-in and never store query text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    /// Record a `query_events` row per recall search (hash + latency + result
    /// count — NEVER the query text). Off by default: it is the input for the
    /// self-evaluation roadmap, opt-in until that ships.
    pub query_events: bool,

    /// Maximum age of query events in days. Applied on every telemetry write,
    /// so retention does not depend on a background job being alive.
    pub retention_days: u32,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            query_events: false,
            retention_days: 30,
        }
    }
}

/// Indexing settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexingConfig {
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

    /// Absolute cosine floor a memory entry must clear to be returned at all.
    ///
    /// The relevance gate. Fusion scores are max-normalized, so the best match
    /// to any prompt scores 1.0 and no relative floor can reject it; this one
    /// is measured against the query embedding itself. An entry passes on
    /// cosine ≥ this value, or on a strong lexical match (an identifier, a
    /// phrase, or several rare terms — `store::hybrid::strong_lexical_match`).
    /// Confidence is not part of the decision.
    ///
    /// Calibrated on `assets/eval/memory-recall.json`: see
    /// `eval::fixture::tests::print_the_precision_recall_curve_over_tau`. `0.0`
    /// keeps every semantically scored candidate, restoring the pre-gate
    /// behavior.
    pub min_recall_cosine: f32,
}

impl Default for SearchMemoryConfig {
    fn default() -> Self {
        Self {
            access_recency_weight: 0.2,
            recency_half_life_secs: 30 * 24 * 60 * 60,
            min_recall_cosine: MIN_RECALL_COSINE_DEFAULT,
        }
    }
}

/// Default cosine floor for memory recall, read off the precision-recall curve
/// in `docs/retrieval-eval.md` rather than picked by hand.
///
/// The rule: the lowest floor at which no labelled in-domain negative is
/// admitted. Measured over 36 held-out queries and 40 negatives in hybrid
/// mode, that is 0.40 — precision 1.000 at recall@5 0.583. 0.35 buys 5 more
/// hits and costs 5 false positives, a one-for-one trade this path cannot
/// take: recall is injected into a prompt nobody asked to enrich, so a wrong
/// entry is charged on every turn while a missing one costs one search.
/// `eval::fixture::tests::print_the_precision_recall_curve_over_tau` asserts
/// the choice still follows the rule, so a fixture change reopens it.
pub const MIN_RECALL_COSINE_DEFAULT: f32 = 0.40;

/// Memory index settings (Phase 6).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Maximum entries in warmup index.
    pub warmup_limit: usize,
}

/// Dotted paths of the keys in `raw_toml` that no field of [`Config`] reads.
///
/// `Config` is `#[serde(default)]` without `deny_unknown_fields`, so a key the
/// schema does not know is accepted and ignored on load: a typo, or a knob a
/// release removed, changes nothing and says nothing. The caller warns per key
/// instead of failing the load. A file that does not parse reports nothing;
/// [`Config::load`] is where that error is raised.
pub fn unknown_keys(raw_toml: &str) -> Vec<String> {
    let Ok(raw) = raw_toml.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut schema = Config::default();
    // The serializer writes no key for `None`, so every `Option` field has to
    // be `Some` here or a user who sets it is told the key is unknown.
    schema.priors.distiller_program = Some(String::new());
    let Ok(toml::Value::Table(schema)) = toml::Value::try_from(schema) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_unknown_keys(&raw, &schema, "", &mut out);
    out
}

fn collect_unknown_keys(
    raw: &toml::Table,
    schema: &toml::Table,
    prefix: &str,
    out: &mut Vec<String>,
) {
    for (key, value) in raw {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match (schema.get(key), value) {
            (None, toml::Value::Table(unknown)) if !unknown.is_empty() => {
                collect_unknown_table(unknown, &path, out);
            }
            (None, _) => out.push(path),
            (Some(toml::Value::Table(known)), toml::Value::Table(given)) => {
                collect_unknown_keys(given, known, &path, out);
            }
            _ => {}
        }
    }
}

fn collect_unknown_table(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
    for (key, value) in table {
        let path = format!("{prefix}.{key}");
        match value {
            toml::Value::Table(nested) if !nested.is_empty() => {
                collect_unknown_table(nested, &path, out);
            }
            _ => out.push(path),
        }
    }
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

    /// Recall expansion: number of top recall seeds whose memory-graph neighbors
    /// are surfaced during UserPromptSubmit injection.
    pub expand_seeds: usize,

    /// Recall expansion: maximum total memory-graph neighbors surfaced across all
    /// seeds (hard cap on the recall hot path).
    pub expand_neighbors: usize,

    /// Doc-graph expansion: max frontmatter-neighbor lines surfaced when a prompt
    /// names a document (the expansion is always 1-hop; this caps the count).
    pub doc_neighbor_cap: usize,
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
}

/// Code intelligence configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeConfig {
    /// Enable code intelligence features.
    pub enabled: bool,

    /// Code indexing pipeline settings.
    pub indexing: CodeIndexingConfig,

    /// Semantic code search settings.
    pub semantic_search: CodeSemanticSearchConfig,

    /// Duplication detection settings.
    pub duplication: CodeDuplicationConfig,
}

/// Duplication detection settings.
///
/// Its own model, deliberately. `semantic_search.model` backs `vec_documents`
/// and `vec_memory`; repointing it would invalidate both and force a full
/// re-embed of everything indexed. Duplication asks a different question and
/// measurably needs a different model — see `src/eval/embedding_gap.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeDuplicationConfig {
    /// Enable the semantic pass — the embedding half of `mdkb dup`, which
    /// loads a model and costs roughly 2.4 CPU-seconds per body. The
    /// structural pass runs either way and needs no weights; measured on this
    /// repository the semantic half took 817 s of an 818 s run and found 69 of
    /// 767 clusters. Off by default: `mdkb dup --semantic`, or a
    /// `--threshold` override, turns it on for a single run, and this key is
    /// the standing opt-in. Never runs during `mdkb index` either way.
    pub semantic: bool,

    /// Embedding model for bodies. Must be one of the models the duplication
    /// pass supports; an unknown name is rejected at load rather than mid-run.
    pub model: String,

    /// Minimum cosine similarity for a pair the structural pass did not
    /// already cluster.
    pub similarity_threshold: f32,

    /// Bits two structural fingerprints may differ by and still cluster.
    pub hamming_threshold: u32,

    /// Fewest named AST nodes a body may hold and still be reported.
    pub min_nodes: u32,
}

/// Code indexing pipeline settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeIndexingConfig {
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

    /// Minimum cosine similarity threshold for results.
    pub threshold: f64,
}

impl Default for CodeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            indexing: CodeIndexingConfig::default(),
            semantic_search: CodeSemanticSearchConfig::default(),
            duplication: CodeDuplicationConfig::default(),
        }
    }
}

impl Default for CodeDuplicationConfig {
    fn default() -> Self {
        Self {
            semantic: false,
            model: crate::code::duplication::embed::DEFAULT_DUP_MODEL.to_string(),
            similarity_threshold: DEFAULT_DUP_SIMILARITY_THRESHOLD,
            hamming_threshold: crate::code::duplication::body::SIMHASH_HAMMING_THRESHOLD,
            min_nodes: crate::code::duplication::scan::MIN_BODY_NODES,
        }
    }
}

impl Default for CodeIndexingConfig {
    fn default() -> Self {
        Self {
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
/// The `[priors]` the daemon actually mines with for the repo whose config
/// lives at `config_path`: the global `daemon.toml` layer under the per-repo
/// override, the same merge `RepoHandle::open` performs.
///
/// Reading only the repo config reports "disabled" for every repo whose mining
/// was turned on globally, which is the normal setup — so anything that reports
/// or checks mining status has to come through here.
pub fn effective_priors(config_path: impl AsRef<Path>) -> PriorsConfig {
    let global = match crate::daemon::config::DaemonConfig::load_or_default(
        &crate::daemon::config::DaemonConfig::config_path(),
    ) {
        Ok(c) => c.priors,
        Err(e) => {
            // A corrupt daemon.toml would otherwise read as "mining disabled"
            // and send the operator looking in the wrong place.
            tracing::warn!("daemon.toml failed to load, priors defaulted: {e}");
            toml::Table::new()
        }
    };
    merge_priors(&global, raw_priors_layer(config_path).as_ref())
}

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

    /// Maximum number of matching documents injected on UserPromptSubmit,
    /// alongside the memory recall. Same hybrid engine as `mdkb search
    /// --scope docs`, reusing the recall query and embedding. Set to 0 to
    /// inject memory only.
    pub recall_docs_limit: usize,

    /// Overrun threshold in milliseconds. A hook that runs longer than this has
    /// its telemetry row copied to `.mdkb/hook-slow.jsonl`, alongside the
    /// `hook-events.jsonl` row every run writes.
    ///
    /// It does NOT cap anything. The doc here used to say the hook truncates
    /// its output when the budget is exceeded, and no code ever did that — a
    /// reader who trusted it would set the value expecting shorter injections
    /// and get the same output, silently. Truncating on a stopwatch would also
    /// be the wrong knob: the output size is already bounded by `warmup_limit`
    /// and `warmup_token_budget`, and cutting a block mid-way once a machine
    /// happens to be busy makes the injection non-deterministic.
    ///
    /// Use `warmup_limit` / `warmup_token_budget` to bound the output, and this
    /// to decide what counts as slow enough to look at.
    pub latency_budget_ms: u64,

    /// Minimum confidence for a warmup entry to be injected. `0.0` (default)
    /// disables the floor — every access-ranked entry is eligible.
    pub warmup_min_confidence: f64,

    /// When true, hooks require a running daemon and skip every in-process
    /// fallback, including an explicit `MDKB_NO_DAEMON=1` request.
    pub daemon_required: bool,

    /// On a definition Grep/Bash search (`fn X`, `struct X`, …), inject the real
    /// `file:line` hits from the code index instead of a "use mdkb" suggestion.
    /// Falls back to the suggestion when the symbol is not indexed.
    pub code_hits_in_pretooluse: bool,

    /// When the prompt names a document (path or `.md`), inject up to 3 one-hop
    /// frontmatter graph neighbors (paths + relation labels) in UserPromptSubmit.
    pub doc_graph_in_recall: bool,

    /// Gate for UserPromptSubmit injection. When true (default), context (recall,
    /// related docs, priors, call-graph hint) is injected ONLY for prompts that
    /// begin with `*`; every other prompt is left untouched — mdkb stays quiet
    /// unless you explicitly ask. The leading `*` is stripped before recall so it
    /// never reaches FTS or the model (and stopwords are already dropped from the
    /// recall query). Set `false` for the always-on behavior.
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
            recall_docs_limit: 3,
            latency_budget_ms: 200,
            warmup_min_confidence: 0.25,
            daemon_required: false,
            code_hits_in_pretooluse: true,
            doc_graph_in_recall: true,
            user_prompt_submit_require_sigil: true,
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
            expand_seeds: 2,
            expand_neighbors: 3,
            doc_neighbor_cap: 3,
        }
    }
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
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
            warmup_limit: DEFAULT_WARMUP_LIMIT,
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            max_response_tokens: DEFAULT_MAX_RESPONSE_TOKENS,
            max_document_tokens: DEFAULT_MAX_DOCUMENT_TOKENS,
            truncate_with_ellipsis: true,
        }
    }
}

/// Valid chunking strategies.
const VALID_CHUNKING_STRATEGIES: &[&str] = &["fixed", "markdown", "semantic"];

/// Minimum allowed max_tokens for chunking.
/// 64 tokens is the minimum practical size for semantic coherence.
const MIN_MAX_TOKENS: usize = 64;

// =============================================================================
// Default value constants with documentation
// =============================================================================

/// Maximum tokens per chunk for embedding models.
/// 512 is the typical context limit for embedding models like nomic-embed-text.
/// Larger values may truncate; smaller values reduce semantic coherence.
const DEFAULT_MAX_TOKENS: usize = 512;

/// Token overlap between consecutive chunks.
/// 64 tokens (~12.5% of 512) maintains context continuity across chunk boundaries
/// without excessive redundancy.
const DEFAULT_OVERLAP_TOKENS: usize = 64;

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

/// Default cosine similarity threshold for semantic code search.
/// 0.3 is a permissive default; higher values improve precision at cost of recall.
const DEFAULT_CODE_SEMANTIC_THRESHOLD: f64 = 0.3;

/// Cosine floor for a duplication pair, from the measured gate.
///
/// `JinaEmbeddingsV2BaseCode` truncated to 256 dims scored a mean of 0.7453 on
/// the case set built to be duplication and 0.5949 on the adversarial set built
/// to look like it and not be. 0.70 sits between the two, near the upper one:
/// the report is read by a human, so a finding that is not one costs more than
/// a borderline one that goes unreported.
const DEFAULT_DUP_SIMILARITY_THRESHOLD: f32 = 0.70;

impl Config {
    /// Load configuration from a TOML file.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use mdkb::Config;
    ///
    /// let config = Config::load(".mdkb/config.toml")?;
    /// println!("Recall limit: {}", config.hooks.recall_limit);
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

    /// The same defaults, every line commented out.
    ///
    /// This is what `mdkb init` writes, and writing the live version instead is
    /// a measured bug: `Config` and all 20 of its sections are
    /// `#[serde(default)]`, so a key absent from the file takes the value in
    /// the code — but a key *present* takes the file's. Materialising every
    /// default froze them at whatever they were the day a store was created.
    /// Lowering `code.duplication.hamming_threshold` from 12 to 6 reached no
    /// existing store for exactly this reason: the 12 was already on disk.
    ///
    /// Commented out, the file still shows every option and its shipped value —
    /// which is why `init` writes one at all — while the code stays the single
    /// place a default lives. Uncommenting a line is then what it looks like:
    /// a deliberate override, not an accident of creation date.
    pub fn commented_default_toml() -> Result<String> {
        let mut out = String::from(
            "# mdkb configuration.\n\
             #\n\
             # Every line below is the shipped default, commented out. Leave it\n\
             # commented and the value follows the code as mdkb is upgraded;\n\
             # uncomment a line to pin that one setting to a value of your own.\n\n",
        );
        for line in Self::default_toml()?.lines() {
            if line.trim().is_empty() {
                out.push('\n');
            } else {
                out.push_str("# ");
                out.push_str(line);
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Validate configuration values.
    pub fn validate(&self) -> Result<()> {
        if self.telemetry.retention_days == 0 || self.telemetry.retention_days > 365 {
            return Err(ErrorKind::ConfigInvalid {
                field: "telemetry.retention_days".to_string(),
                message: "must be between 1 and 365".to_string(),
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

        // Duplication validation. Rejected at load, not mid-run: a scan that
        // parses a repository and then fails on a threshold has wasted the
        // expensive part before reading the cheap mistake.
        let dup = &self.code.duplication;
        if !(0.0..=1.0).contains(&dup.similarity_threshold) {
            return Err(ErrorKind::ConfigInvalid {
                field: "code.duplication.similarity_threshold".to_string(),
                message: "must be between 0.0 and 1.0".to_string(),
            }
            .into());
        }

        // A simhash is 64 bits, so a threshold at or above 64 clusters every
        // body with every other one — the report would be one group holding the
        // whole repository.
        if dup.hamming_threshold >= 64 {
            return Err(ErrorKind::ConfigInvalid {
                field: "code.duplication.hamming_threshold".to_string(),
                message: "must be less than 64, the width of a simhash".to_string(),
            }
            .into());
        }

        if dup.min_nodes == 0 {
            return Err(ErrorKind::ConfigInvalid {
                field: "code.duplication.min_nodes".to_string(),
                message: "must be greater than 0".to_string(),
            }
            .into());
        }

        if !crate::code::duplication::embed::SUPPORTED_DUP_MODELS.contains(&dup.model.as_str()) {
            return Err(ErrorKind::ConfigInvalid {
                field: "code.duplication.model".to_string(),
                message: format!(
                    "must be one of: {}",
                    crate::code::duplication::embed::SUPPORTED_DUP_MODELS.join(", ")
                ),
            }
            .into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(!config.indexing.respect_gitignore);
        assert!(config.search.auto_embed_docs);
        assert_eq!(config.memory.warmup_limit, 50);
        assert!(!config.telemetry.query_events);
        assert_eq!(config.telemetry.retention_days, 30);
    }

    #[test]
    fn telemetry_retention_must_be_bounded() {
        let mut config = Config::default();
        config.telemetry.retention_days = 0;
        assert!(config.validate().is_err());
        config.telemetry.retention_days = 366;
        assert!(config.validate().is_err());
        config.telemetry.retention_days = 30;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_config_roundtrip() {
        let config = Config::default();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.memory.warmup_limit, parsed.memory.warmup_limit);
    }

    #[test]
    fn test_hooks_config_defaults() {
        let cfg = HooksConfig::default();
        // warmup_limit is the secondary bound; warmup_token_budget is primary.
        assert_eq!(cfg.warmup_limit, 10);
        assert_eq!(cfg.warmup_token_budget, 300);
        assert_eq!(cfg.recall_limit, 5);
        // Documents leg on by default; 0 would make recall memory-only.
        assert_eq!(cfg.recall_docs_limit, 3);
        // Confidence floor on by default so low-signal entries stay out of warmup.
        assert!((cfg.warmup_min_confidence - 0.25).abs() < f64::EPSILON);
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

    // ==================== Unknown Key Tests ====================

    /// A removed knob, or a typo, must be named by its dotted path and must
    /// not fail the load: the keys beside it still apply.
    #[test]
    fn unknown_keys_names_a_removed_knob_by_its_dotted_path() {
        let raw = "[code]\nindex_path = \"my-code-idx\"\nenabled = false\n\n\
                   [models]\nembedding_repo = \"x\"\n";
        assert_eq!(
            unknown_keys(raw),
            vec!["code.index_path", "models.embedding_repo"]
        );

        let cfg: Config = toml::from_str(raw).expect("an unknown key never fails the load");
        assert!(!cfg.code.enabled, "the known key beside it still applies");
    }

    #[test]
    fn unknown_keys_is_empty_for_every_shipped_key() {
        assert!(unknown_keys(&Config::default_toml().unwrap()).is_empty());
        // `None` is never serialised, so this key is absent from a serialised
        // default and has to be known by other means.
        assert!(unknown_keys("[priors]\ndistiller_program = \"claude\"\n").is_empty());
        assert!(unknown_keys("").is_empty());
        assert!(
            unknown_keys("not toml [").is_empty(),
            "a file that does not parse is Config::load's error, not a key report"
        );
    }

    // ==================== Validation Tests ====================

    #[test]
    fn test_validate_valid_config() {
        let config = Config::default();
        assert!(config.validate().is_ok());
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
respect_gitignore = true

[hooks]
recall_limit = 20
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let config = Config::load(&config_path).unwrap();
        assert!(config.indexing.respect_gitignore);
        assert_eq!(config.hooks.recall_limit, 20);
        // Other fields should have defaults
        assert!(config.search.auto_embed_docs);
    }

    #[test]
    fn test_load_rejects_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        // chunking.strategy outside the allowlist violates validate()
        let toml_content = r#"
[chunking]
strategy = "invalid_strategy"
"#;
        std::fs::write(&config_path, toml_content).unwrap();

        let result = Config::load(&config_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("strategy"));
    }

    #[test]
    fn test_load_nonexistent_file() {
        let result = Config::load("/nonexistent/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_or_default_nonexistent() {
        let config = Config::load_or_default("/nonexistent/config.toml");
        assert_eq!(config.memory.warmup_limit, DEFAULT_WARMUP_LIMIT);
    }

    #[test]
    fn test_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");

        let mut config = Config::default();
        config.memory.warmup_limit = 42;
        config.save(&config_path).unwrap();

        let loaded = Config::load(&config_path).unwrap();
        assert_eq!(loaded.memory.warmup_limit, 42);
    }

    // ==================== Default TOML Generation ====================

    #[test]
    fn test_default_toml_generation() {
        let toml_str = Config::default_toml().unwrap();
        assert!(toml_str.contains("[indexing]"));
        assert!(toml_str.contains("[chunking]"));
        assert!(toml_str.contains("[search]"));
        assert!(toml_str.contains("[memory]"));
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
        assert_eq!(
            parsed.code.indexing.batch_size,
            config.code.indexing.batch_size
        );
        assert!(
            (parsed.code.semantic_search.threshold - config.code.semantic_search.threshold).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn test_code_config_partial_override() {
        let toml_content = r#"
[code]
enabled = false

[code.indexing]
batch_size = 1000

[code.semantic_search]
enabled = true
threshold = 0.5
"#;
        let config: Config = toml::from_str(toml_content).unwrap();
        assert!(!config.code.enabled);
        assert_eq!(config.code.indexing.batch_size, 1000);
        assert!(config.code.indexing.respect_gitignore);
        assert!(config.code.semantic_search.enabled);
        assert!((config.code.semantic_search.threshold - 0.5).abs() < f64::EPSILON);
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
    fn test_validate_duplication_similarity_threshold_out_of_range() {
        let mut config = Config::default();
        config.code.duplication.similarity_threshold = 1.5;
        let err = config.validate().unwrap_err().to_string();
        assert!(
            err.contains("code.duplication.similarity_threshold"),
            "{err}"
        );

        config.code.duplication.similarity_threshold = -0.1;
        assert!(config.validate().is_err());
    }

    /// A simhash is 64 bits wide. A threshold at 64 puts every body in one
    /// cluster, so the report becomes a single group holding the repository —
    /// not an error anyone would notice at read time.
    #[test]
    fn test_validate_duplication_hamming_threshold_cannot_reach_the_simhash_width() {
        let mut config = Config::default();
        config.code.duplication.hamming_threshold = 64;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("code.duplication.hamming_threshold"), "{err}");

        config.code.duplication.hamming_threshold = 63;
        assert!(config.validate().is_ok(), "63 bits is legal, if useless");
    }

    #[test]
    fn test_validate_duplication_min_nodes_zero() {
        let mut config = Config::default();
        config.code.duplication.min_nodes = 0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("code.duplication.min_nodes"), "{err}");
    }

    /// An unmeasured model silently reports the wrong pairs instead of failing.
    #[test]
    fn test_validate_duplication_model_must_be_one_that_was_measured() {
        let mut config = Config::default();
        config.code.duplication.model = "BgeSmallEnV15".to_string();
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("code.duplication.model"), "{err}");
    }

    /// The file `init` writes must not pin anything.
    ///
    /// Writing the live defaults instead is how lowering
    /// `hamming_threshold` from 12 to 6 reached no store that already existed:
    /// the old value was sitting in every `config.toml` ever created, and a
    /// present key beats the code.
    #[test]
    fn the_config_init_writes_pins_no_value() {
        let written = Config::commented_default_toml().unwrap();

        for (n, line) in written.lines().enumerate() {
            assert!(
                line.trim().is_empty() || line.starts_with('#'),
                "line {} would pin a value: {line:?}",
                n + 1
            );
        }

        // Commented out is not the same as absent: the options still have to be
        // readable, or there is no reason to write the file at all.
        assert!(written.contains("hamming_threshold"), "{written}");
        assert!(written.contains("[code.duplication]"), "{written}");

        // And what it parses to is the defaults, not an empty config.
        let parsed: Config = toml::from_str(&written).expect("a fully commented file still parses");
        assert_eq!(
            parsed.code.duplication.hamming_threshold,
            Config::default().code.duplication.hamming_threshold,
            "a commented file must take the value from the code"
        );
    }

    #[test]
    fn test_duplication_defaults_match_the_measured_gate() {
        let config = Config::default();
        let dup = &config.code.duplication;

        assert!(!dup.semantic, "the semantic pass is opt-in");
        assert_eq!(dup.model, "JinaEmbeddingsV2BaseCode");
        assert!((dup.similarity_threshold - 0.70).abs() < f32::EPSILON);
        assert_eq!(dup.hamming_threshold, 6);
        assert_eq!(dup.min_nodes, 30);
        assert!(config.validate().is_ok(), "the defaults must be valid");
    }

    /// The section is `#[serde(default)]`: an existing config file with no
    /// `[code.duplication]` table still loads, with the measured defaults.
    #[test]
    fn test_a_config_without_a_duplication_section_still_parses() {
        let parsed: Config = toml::from_str("[code]\n").unwrap();

        assert_eq!(
            parsed.code.duplication.model,
            crate::code::duplication::embed::DEFAULT_DUP_MODEL
        );
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
