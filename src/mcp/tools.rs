//! MCP tool definitions and parameters.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Where the `root` grammar is documented. Every error this module produces
/// points here, and nothing else does: the grammar is not in the tool schemas
/// and not in the server instructions, because both are charged on every
/// request of every session while the cheatsheet costs nothing until an
/// operator asks for it.
const GRAMMAR_HINT: &str = "Run `mdkb cheatsheet` for the root grammar.";

/// One item of a `root` selector.
///
/// The two are told apart syntactically, by [`Path::is_absolute`] alone, before
/// anything is looked up or touched on disk. That is what keeps an absolute
/// path behaving exactly as it always did: a path to a repo the map has never
/// heard of is still a path, and a repo name can never shadow one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootTerm {
    /// An absolute path, used as written.
    Path(PathBuf),
    /// A repo name, resolved against the known roots by last path component.
    Name(String),
}

/// A parsed `root` selector — the ONE place the `root` string is interpreted.
///
/// Every tool goes through this instead of matching on the string, which is
/// what `resolve_handle` used to do: an absolute path or nothing, with `"*"`
/// special-cased and rejected everywhere but `search`.
///
/// # The comma rule
///
/// A comma is the list separator, always. A path that contains one is therefore
/// ambiguous — `/srv/my,repo` is both a real directory and a two-item list —
/// and [`parse`](Self::parse) REFUSES it by name rather than splitting it into
/// two selectors that resolve to nothing. An input that cannot mean one thing
/// is not answered as if it did; that is the same rule criterion 3 applies to
/// an ambiguous name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootSelector {
    /// No `root` given: whatever the caller's default repo is.
    Default,
    /// `*` — every known root.
    All,
    /// One or more explicit terms, in the order the caller wrote them.
    List(Vec<RootTerm>),
}

impl RootSelector {
    /// Parse a raw `root` value. Fails only on an input that cannot mean one
    /// thing; an unknown or ambiguous NAME is a resolution failure, not a
    /// parse failure, because parsing does not know the map.
    pub fn parse(raw: Option<&str>) -> Result<Self, String> {
        let Some(raw) = raw else {
            return Ok(Self::Default);
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Self::Default);
        }
        if trimmed == "*" {
            return Ok(Self::All);
        }

        if trimmed.contains(',') && Path::new(trimmed).exists() {
            return Err(format!(
                "root=\"{trimmed}\" is both an existing path and a comma-separated list, \
                 so it cannot be resolved either way. A comma separates repos; a path \
                 containing one cannot be selected by path — select it by name instead. \
                 {GRAMMAR_HINT}"
            ));
        }

        let mut terms = Vec::new();
        for segment in trimmed.split(',') {
            let segment = segment.trim();
            if segment.is_empty() {
                return Err(format!(
                    "root=\"{trimmed}\" has an empty item. {GRAMMAR_HINT}"
                ));
            }
            if segment == "*" {
                return Err(format!(
                    "root=\"*\" means every known repo and cannot be combined with other \
                     items. {GRAMMAR_HINT}"
                ));
            }
            terms.push(if Path::new(segment).is_absolute() {
                RootTerm::Path(PathBuf::from(segment))
            } else {
                RootTerm::Name(segment.to_string())
            });
        }
        Ok(Self::List(terms))
    }

    /// The roots this selector names.
    ///
    /// `known` is every root the daemon knows; `open` is the subset with a live
    /// handle, which is what [`Self::Default`] has always meant. Neither is read
    /// for a path term, so an absolute path resolves with an empty map.
    ///
    /// Duplicates are collapsed, keeping first position: a list that names one
    /// repo twice is one repo, not a doubled fan-out.
    pub fn resolve(&self, known: &[PathBuf], open: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
        let resolved = match self {
            Self::Default => open.to_vec(),
            Self::All => known.to_vec(),
            Self::List(terms) => terms
                .iter()
                .map(|term| resolve_term(term, known))
                .collect::<Result<Vec<_>, _>>()?,
        };

        let mut seen = std::collections::HashSet::new();
        Ok(resolved
            .into_iter()
            .filter(|root| seen.insert(root.clone()))
            .collect())
    }

    /// Why a tool that reads one repo refuses `*`.
    ///
    /// Distinct from [`Self::multi_root_rejection`]: `*` is refused for what it
    /// means, not for how many repos happen to be known. Refusing it only when
    /// it resolves to several would accept it on a daemon with one repo open
    /// and answer as if the caller had named that repo.
    pub fn wildcard_rejection() -> String {
        "root=\"*\" is supported only by search. Pass the exact repository root for get and \
         other tools."
            .to_string()
    }

    /// Does resolving this selector need the filesystem walk for nested
    /// stores, or only the roots already on the map?
    ///
    /// `Default` and `*` mean "every store under here", which is a question
    /// only the walk can answer. An explicit PATH is itself and reads no map
    /// at all; a NAME is looked up among known roots, and a store nobody has
    /// recorded can still be named, so that one needs discovery too.
    ///
    /// The walk visits every directory beneath every known root — on this
    /// machine 9355 of them across 13 roots — and it was being paid on every
    /// `get`, `memory_write` and `graph` call whose `root` was an explicit
    /// path, where it could not change the answer.
    pub fn needs_discovery(&self) -> bool {
        match self {
            Self::Default | Self::All => true,
            Self::List(terms) => terms.iter().any(|t| matches!(t, RootTerm::Name(_))),
        }
    }

    /// What a tool that cannot fan out says about a selector naming `count`
    /// repos. Names the tool that CAN, the way the old `root="*"` message did.
    ///
    /// The count is in the message because the caller's next move depends on
    /// it: a selector that named two repos is a typo to correct, one that
    /// named thirty is a workspace to narrow.
    pub fn multi_root_rejection(count: usize) -> String {
        format!(
            "This root selector names {count} repos, and only `search` fans out across \
             repos. Pass one repo — a name or an absolute path. {GRAMMAR_HINT}"
        )
    }
}

/// The repos a `root`-less call means, given the workspace the client declared.
///
/// `Default` used to mean `open`: whatever happened to have a live handle. That
/// is an accident of LRU order, not a decision — a call from a workspace whose
/// store was never opened answered about five unrelated repos and named none of
/// them. `scope` is what the client said it is working on (its MCP roots), so a
/// `root`-less call means that workspace **and every store nested beneath it**:
/// a hierarchy is not re-indexed into one store, it is fanned out over.
///
/// Falls back to `open` when the scope names nothing known — a workspace with
/// no store under it must not silently become an empty answer.
pub fn default_roots(scope: &[PathBuf], known: &[PathBuf], open: &[PathBuf]) -> Vec<PathBuf> {
    let within: Vec<PathBuf> = known
        .iter()
        .filter(|root| scope.iter().any(|s| root.starts_with(s)))
        .cloned()
        .collect();
    if within.is_empty() {
        open.to_vec()
    } else {
        within
    }
}

/// The one store a `root`-less call means for a tool that cannot fan out.
///
/// [`default_roots`] answers "every store in this workspace", which is what a
/// fan-out wants and what a single-target tool cannot use. The workspace the
/// client declared is its own statement of what it is working on, so when that
/// path is itself a store, that store is the answer — and the stores nested
/// beneath it stay reachable only through `search` or an explicit root. A
/// `memory_write` must never land in a sub-store nobody named.
///
/// `None` is "not a choice this function may make": a scope that anchors no
/// store (a container of repositories, where nothing is more the caller's repo
/// than anything else), or several declared paths that are each a store.
pub fn workspace_anchor(scope: &[PathBuf], roots: &[PathBuf]) -> Option<PathBuf> {
    let mut anchors = scope.iter().filter(|s| roots.contains(s));
    let first = anchors.next()?;
    anchors.next().is_none().then(|| first.clone())
}

/// Resolve one term. A path is itself; a name is looked up by last component.
fn resolve_term(term: &RootTerm, known: &[PathBuf]) -> Result<PathBuf, String> {
    let name = match term {
        RootTerm::Path(path) => return Ok(path.clone()),
        RootTerm::Name(name) => name,
    };

    let hits: Vec<&PathBuf> = known
        .iter()
        .filter(|root| root.file_name().and_then(|n| n.to_str()) == Some(name.as_str()))
        .collect();

    match hits.len() {
        1 => Ok(hits[0].clone()),
        // Neither arm may look like an empty result: "no such repo" and "which
        // of these two" are facts the caller can act on, and a search that
        // quietly returned nothing would hide both.
        0 => Err(format!(
            "No known repo is named \"{name}\". Known: {}. {GRAMMAR_HINT}",
            known_names(known)
        )),
        _ => Err(format!(
            "\"{name}\" names {} repos: {}. Pass one of those paths. {GRAMMAR_HINT}",
            hits.len(),
            hits.iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The names a caller could have written, for an error that is actionable.
fn known_names(known: &[PathBuf]) -> String {
    if known.is_empty() {
        return "no repos are registered".to_string();
    }
    known
        .iter()
        .filter_map(|root| root.file_name().and_then(|n| n.to_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Closed set of scopes accepted by [`SearchParams`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Docs,
    Memory,
    Code,
    Symbols,
    Duplicates,
}

impl SearchScope {
    pub const ALL: [Self; 5] = [
        Self::Docs,
        Self::Memory,
        Self::Code,
        Self::Symbols,
        Self::Duplicates,
    ];

    /// Can this scope be answered across several repos at once?
    ///
    /// Code, symbol and duplication indexes are per-repo: there is no merged
    /// answer to give, which is why the fan-out refuses them. Documents and
    /// memory merge on score and have one.
    ///
    /// Asked BEFORE the fan-out is chosen, not inside it. A rootless call from
    /// a workspace holding nested stores resolves to several repos, and
    /// choosing the fan-out on that count alone turned `scope="code"` into
    /// "Specify a root" for a caller who had named no root and whose declared
    /// workspace was itself a store — while `get` from the same place
    /// anchored to that workspace and answered.
    pub const fn fans_out(self) -> bool {
        matches!(self, Self::Docs | Self::Memory)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Docs => "docs",
            Self::Memory => "memory",
            Self::Code => "code",
            Self::Symbols => "symbols",
            Self::Duplicates => "duplicates",
        }
    }
}

impl TryFrom<&str> for SearchScope {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::ALL
            .into_iter()
            .find(|scope| scope.as_str() == value)
            .ok_or(())
    }
}

/// Parameters for the search tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SearchParams {
    /// Search query text.
    pub query: String,

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

    /// Search scope: "docs", "memory", "code", "symbols", or "duplicates". Omit to search docs+memory. A field's [tag] lists the scopes it applies under; other scopes ignore it.
    #[serde(default)]
    pub scope: Option<String>,

    /// [code|symbols] symbol kind, e.g. "function", "struct". [memory] entry type, e.g. "problem", "decision".
    #[serde(default)]
    pub kind: Option<String>,

    /// [code|duplicates] Minimum similarity 0.0-1.0. Omit to use the configured threshold. Under "duplicates" it also enables the semantic pass, which loads a model and takes minutes on a large repository — omit it for the fast structural sweep.
    #[serde(default)]
    pub threshold: Option<f32>,

    /// [symbols] file path substring. [duplicates] file path prefix; omit it to sweep the repository, which ignores query.
    #[serde(default)]
    pub file: Option<String>,

    /// [memory] Minimum confidence 0.0-1.0; entries below it are excluded, in the default docs+memory search too. Omit or 0.0 = no filter.
    #[serde(default)]
    pub min_confidence: Option<f64>,

    /// [duplicates] Git ref: report only clusters touching what changed since it, still scored against the whole index — what did this change duplicate.
    #[serde(default)]
    pub since: Option<String>,
}

fn default_limit() -> usize {
    10
}

/// Parameters for the get tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GetParams {
    /// Document ID, path, or memory slug.
    pub id: String,

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
    #[serde(default)]
    pub source_type: Option<String>,

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
    #[serde(default)]
    pub source_type: Option<String>,

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

    #[serde(default)]
    pub root: Option<String>,

    /// Outcome signal: "confirmed" increments confirmations; "refuted" decrements (floor 0).
    pub outcome: String,
}

/// Parameters for the memory_list tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct MemoryListParams {
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

/// Parameters for the usage tool.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct UsageParams {
    /// Limit totals to current session (default: true). Set false to include lifetime aggregates across all sessions.
    #[serde(default = "default_session_only")]
    pub session_only: bool,

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

    #[serde(default)]
    pub root: Option<String>,
}

/// Parameters for symbol_at_position: find the symbol at a given location.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct SymbolAtPositionParams {
    /// Relative file path from repo root.
    pub file: String,

    /// 1-based line number, as shown in search results and editors. The
    /// `line_start`/`line_end` in the response are 0-based tree-sitter rows.
    pub line: u32,

    /// 0-based column number (optional).
    #[serde(default)]
    pub col: Option<u32>,

    #[serde(default)]
    pub root: Option<String>,
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_rootless_call_means_the_workspace_and_every_store_beneath_it() {
        let scope = [PathBuf::from("/a")];
        let known = [
            PathBuf::from("/a"),
            PathBuf::from("/a/work"),
            PathBuf::from("/a/work/people/x"),
            PathBuf::from("/b"),
        ];
        let open = [PathBuf::from("/b")];

        let got = default_roots(&scope, &known, &open);

        assert_eq!(
            got,
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/a/work"),
                PathBuf::from("/a/work/people/x")
            ],
            "the hierarchy is fanned out over, and an unrelated open repo is not in it"
        );
    }

    #[test]
    fn without_a_declared_workspace_the_open_repos_still_answer() {
        let known = [PathBuf::from("/a"), PathBuf::from("/b")];
        let open = [PathBuf::from("/b")];

        assert_eq!(default_roots(&[], &known, &open), vec![PathBuf::from("/b")]);
    }

    #[test]
    fn a_workspace_with_no_store_beneath_it_does_not_become_an_empty_answer() {
        let scope = [PathBuf::from("/nowhere")];
        let known = [PathBuf::from("/a")];
        let open = [PathBuf::from("/a")];

        assert_eq!(
            default_roots(&scope, &known, &open),
            vec![PathBuf::from("/a")],
            "falling back to open is better than answering about nothing"
        );
    }

    #[test]
    fn a_sibling_that_merely_shares_a_name_prefix_is_not_beneath_the_workspace() {
        let scope = [PathBuf::from("/a")];
        let known = [PathBuf::from("/a"), PathBuf::from("/ab")];
        let open = [PathBuf::from("/ab")];

        assert_eq!(
            default_roots(&scope, &known, &open),
            vec![PathBuf::from("/a")],
            "/ab is not under /a"
        );
    }
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
        // Omitted threshold must stay absent, not pin a hardcoded default: the
        // configured code.semantic_search.threshold is what fills it in, and a
        // literal default here would silently override the user's setting.
        assert!(params.threshold.is_none());
        assert!(params.file.is_none());
    }

    #[test]
    fn test_search_params_code_scope_custom() {
        let json = r#"{"query": "pool", "scope": "code", "kind": "struct", "threshold": 0.5}"#;
        let params: SearchParams = serde_json::from_str(json).unwrap();
        assert_eq!(params.query, "pool");
        assert_eq!(params.scope.as_deref(), Some("code"));
        assert_eq!(params.kind.as_deref(), Some("struct"));
        assert!((params.threshold.unwrap() - 0.5).abs() < f32::EPSILON);
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

    /// A path a term is told from a name by: `Path::is_absolute`, which on
    /// Windows wants a drive — a rooted `/a/b` there is a name, not a path.
    #[cfg(windows)]
    const ABS_A: &str = r"C:\a\b";
    #[cfg(windows)]
    const ABS_B: &str = r"C:\c\d";
    #[cfg(not(windows))]
    const ABS_A: &str = "/a/b";
    #[cfg(not(windows))]
    const ABS_B: &str = "/c/d";

    #[test]
    fn only_the_selectors_that_can_change_answer_pay_for_discovery() {
        // An explicit path is itself: `resolve_term` returns it without ever
        // reading the known set, so the walk cannot change the result.
        assert!(!RootSelector::parse(Some(ABS_A)).unwrap().needs_discovery());
        assert!(
            !RootSelector::parse(Some(&format!("{ABS_A},{ABS_B}")))
                .unwrap()
                .needs_discovery()
        );
        // A name is looked up among known roots, and a nested store nobody
        // recorded is findable only by the walk.
        assert!(RootSelector::parse(Some("mdkb")).unwrap().needs_discovery());
        assert!(
            RootSelector::parse(Some(&format!("{ABS_A},mdkb")))
                .unwrap()
                .needs_discovery(),
            "one name in the list is enough"
        );
        // Both of these mean "every store under here".
        assert!(RootSelector::parse(None).unwrap().needs_discovery());
        assert!(RootSelector::parse(Some("*")).unwrap().needs_discovery());
    }
}
