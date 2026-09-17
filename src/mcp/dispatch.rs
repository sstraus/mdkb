//! Transport-agnostic MCP tool dispatch.
//!
//! Each tool body lives here as a free function taking a `&RepoHandle` plus
//! a `DispatchContext`. `dispatch_call` routes a JSON-RPC-style call by
//! method name and returns a JSON `Value`. The rmcp `#[tool]` wrappers in
//! `server.rs` call these impls and wrap the result into a `CallToolResult`;
//! the hook socket in `daemon::ipc_server` calls `dispatch_call` directly.
//!
//! Story 3 of `plans/daemon-ipc-socket.md` — initial slice wires `status`
//! only. Remaining 10 tools land in follow-up commits under #007-16f7.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use futures::future::join_all;
use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};

use crate::cli::hook_logic::{
    REINDEX_TOOLS, canonicalize_under_cwd, classify_bash_search, classify_definition_search,
    classify_grep_pattern, is_mdkb_invocation, prompt_is_wrapup, tool_input_path,
};
use crate::code::indexing::IndexFacade;
use crate::core::Context;
use crate::core::cli_mutation::{CliMutation, CliMutationResult};
use crate::core::indexing::{UpdateOutcome, UpdateRequest, update_documents};
use crate::core::search::{handle_hybrid_search, handle_mget};
use crate::daemon::registry::RepoHandle;
use crate::domain::{SearchResult, UpdateResult};
use crate::error::ErrorKind;
use crate::metrics::{
    UsageMetrics, count_tokens, truncate_with_continuation, truncate_with_ellipsis,
};
use crate::store::memory::get_warmup_entries;
use crate::store::memory_graph::{self, MemoryRelation, TargetKind};
use crate::store::{collections, documents, evolution, memory, search, stats};

use super::mcp_error;
/// Pick the JSON-RPC code a store error must travel under.
///
/// `INTERNAL_ERROR` is the daemon's post-dispatch code: it tells the CLI that a
/// method got as far as running, so the outcome is unknown and the CLI must NOT
/// retry the write itself. A validation refusal never ran anything — see
/// [`crate::error::ErrorKind::is_validation_refusal`] — so wearing that code
/// made a rejected entry id report "the daemon may still be writing" and blocked
/// the fallback that would have printed the real cause.
fn store_error_code(error: &crate::Error) -> ErrorCode {
    if error.is_validation_refusal() {
        ErrorCode::INVALID_PARAMS
    } else {
        ErrorCode::INTERNAL_ERROR
    }
}

fn mcp_store_error(context: &str, error: impl Into<crate::Error>) -> McpError {
    let error = error.into();
    McpError {
        code: store_error_code(&error),
        message: format!("{context}: {error}").into(),
        data: error
            .is_index_corrupt()
            .then(|| json!({ "index_corrupt": true })),
    }
}

/// A refusal that speaks for itself: the error text is already the whole story,
/// so it travels unwrapped, under the code its kind earns.
fn mcp_refusal(error: crate::Error) -> McpError {
    McpError {
        code: store_error_code(&error),
        message: error.to_string().into(),
        data: None,
    }
}

fn mcp_error_reports_corruption(error: &McpError) -> bool {
    error
        .data
        .as_ref()
        .and_then(|data| data.get("index_corrupt"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn close_context_on_reported_corruption<T>(
    slot: &mut Option<Context>,
    operation: &str,
    result: Result<T, McpError>,
) -> Result<T, McpError> {
    if result.as_ref().is_err_and(mcp_error_reports_corruption) {
        tracing::error!(
            operation,
            "database statement reported index corruption — closing the connection for automatic recovery"
        );
        *slot = None;
    }
    result
}
#[cfg(test)]
use super::tools::RelatesInput;
use super::tools::{
    CodeFindParams, CodeGraphParams, GetParams, GraphParams, MemoryConfirmParams,
    MemoryDeleteParams, MemoryListParams, MemoryWriteBatchEntry, SearchParams,
    SymbolAtPositionParams, SymbolsInFileParams, UsageParams,
};

const MAX_HOOK_PROMPT_FINGERPRINTS: usize = 32;

fn format_symbol(sym: &crate::code::symbol::Symbol) -> String {
    format_symbol_with_file_tokens(sym, None)
}

fn format_symbol_with_file_tokens(
    sym: &crate::code::symbol::Symbol,
    file_tokens: Option<u32>,
) -> String {
    let suffix = file_tokens
        .map(|count| format!(" (file: ~{count}tok)"))
        .unwrap_or_default();
    let mut output = format!(
        "  sym#{} {:?} {} in {}:{}{}\n",
        sym.id.value(),
        sym.kind,
        sym.name,
        sym.file_path,
        sym.range.start_line,
        suffix,
    );
    if let Some(signature) = &sym.signature {
        output.push_str(&format!("    Signature: {signature}\n"));
    }
    if let Some(doc) = &sym.doc_comment {
        output.push_str(&format!("    Doc: {}\n", truncate_text(doc, 120)));
    }
    output
}

fn resolve_document(
    conn: &rusqlite::Connection,
    path_or_id: &str,
) -> crate::error::Result<crate::domain::Document> {
    if let Ok(id) = path_or_id.parse::<i64>() {
        if let Some(document) = documents::get_document(conn, id)? {
            return Ok(document);
        }
    }
    for collection in collections::list_collections(conn)? {
        if let Some(document) = documents::get_document_by_path(conn, &collection.name, path_or_id)?
        {
            return Ok(document);
        }
    }
    Err(ErrorKind::DocumentNotFound {
        id: path_or_id.to_string(),
    }
    .into())
}

pub(super) fn relative_time_ago(unix_ts: i64) -> String {
    let secs = (chrono::Utc::now().timestamp() - unix_ts).max(0);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 7 * 86400 {
        format!("{}d ago", secs / 86400)
    } else if secs < 30 * 86400 {
        format!("{}w ago", secs / (7 * 86400))
    } else if secs < 365 * 86400 {
        format!("{}mo ago", secs / (30 * 86400))
    } else {
        format!("{}y ago", secs / (365 * 86400))
    }
}

pub(super) fn truncate_text(text: &str, max_len: usize) -> String {
    let text = text.replace('\n', " ");
    if text.len() <= max_len {
        return text;
    }
    let mut cut = max_len.saturating_sub(3);
    while !text.is_char_boundary(cut) && cut > 0 {
        cut -= 1;
    }
    format!("{}...", &text[..cut])
}

const OOD_SCORE_THRESHOLD: f64 = 0.3;

pub(super) fn ood_hint(
    query: &str,
    result_count: usize,
    top_score: Option<f64>,
) -> Option<&'static str> {
    if query.trim().is_empty() {
        return Some("\n> The query is empty. Pass search terms — an empty query matches nothing.");
    }
    if result_count == 0 {
        return Some(
            "\n> No results. mdkb is semantic search — it won't match literal strings. \
             Use Grep for exact string/regex matching in source files.",
        );
    }
    if top_score.is_some_and(|score| score < OOD_SCORE_THRESHOLD) {
        return Some(
            "\n> Low-confidence results. If searching for a literal string or pattern, \
             use Grep instead — mdkb only does semantic/fuzzy matching.",
        );
    }
    None
}

pub(super) fn format_search_results(results: &[SearchResult], limit: usize) -> String {
    use crate::store::hybrid::lost_in_middle_reorder;

    let mut ordered: Vec<_> = results
        .iter()
        .filter(|result| result.score != 0.0)
        .collect();
    if ordered.is_empty() {
        return "No matching documents found.".to_string();
    }
    lost_in_middle_reorder(&mut ordered);

    let mut output = if ordered.len() >= limit {
        format!(
            "Showing {} results (limit reached, refine query for more precise results):\n",
            ordered.len()
        )
    } else {
        String::new()
    };
    for result in &ordered {
        let title = result.title.as_deref().unwrap_or("(untitled)");
        if let Some(root) = &result.repo_root {
            output.push_str(&format!(
                "[{}] {} - {} (score: {:.2}, repo: {})\n",
                result.id, result.path, title, result.score, root
            ));
        } else {
            output.push_str(&format!(
                "[{}] {} - {} (score: {:.2})\n",
                result.id, result.path, title, result.score
            ));
        }
        for snippet in &result.snippets {
            output.push_str(&format!("  {snippet}\n"));
        }
    }

    let retrieval_ids: Vec<_> = ordered
        .iter()
        .map(|result| {
            if result.collection == "memory" && !result.path.is_empty() {
                result.path.clone()
            } else {
                result.id.to_string()
            }
        })
        .collect();
    let repo_roots: Vec<_> = ordered
        .iter()
        .filter_map(|result| result.repo_root.as_deref())
        .collect();
    if let Some(root) = repo_roots.first() {
        let id = serde_json::to_string(&retrieval_ids[0]).expect("string serialization");
        let root = serde_json::to_string(root).expect("string serialization");
        output.push_str(&format!("\nUse get({id}, root={root}) to read one."));
        if retrieval_ids.len() > 1 {
            output.push_str(" For another result, pass its listed repo as root.");
        }
        output.push_str(" root=\"*\" is search-only.");
    } else if retrieval_ids.len() == 1 {
        output.push_str(&format!("\nUse get(\"{}\") to read.", retrieval_ids[0]));
    } else {
        output.push_str(&format!(
            "\nUse get(\"{}\") to read one, or get(\"{}\") for all.",
            retrieval_ids[0],
            retrieval_ids.join(",")
        ));
    }
    output
}

fn format_memory_search_results(entries: &[memory::MemoryEntry]) -> String {
    use crate::store::hybrid::lost_in_middle_reorder;

    if entries.is_empty() {
        return "No matching memory entries found.".to_string();
    }
    let mut ordered: Vec<_> = entries.iter().collect();
    lost_in_middle_reorder(&mut ordered);
    let mut output = format!("Found {} memory entries:\n\n", entries.len());
    for entry in ordered {
        let confirmed = entry
            .last_confirmed_at
            .map(|timestamp| format!(", confirmed:{}", relative_time_ago(timestamp)))
            .unwrap_or_default();
        output.push_str(&format!(
            "- [{}] {} ({}, conf:{:.2}, confirms:{}, access:{}{}, {}{}): {}\n",
            entry.id,
            entry.title,
            entry.entry_type,
            entry.confidence(),
            entry.confirmations,
            entry.access_count,
            confirmed,
            relative_time_ago(entry.updated_at),
            format_ttl_info(entry.expires_at),
            truncate_text(&entry.content, 100)
        ));
    }
    output
}

fn apply_min_confidence(
    entries: Vec<memory::MemoryEntry>,
    min: Option<f64>,
) -> Vec<memory::MemoryEntry> {
    match min {
        Some(threshold) if threshold > 0.0 => entries
            .into_iter()
            .filter(|entry| entry.confidence() >= threshold)
            .collect(),
        _ => entries,
    }
}

fn format_ttl_info(expires_at: Option<i64>) -> String {
    match expires_at {
        Some(timestamp) if timestamp <= chrono::Utc::now().timestamp() => ", EXPIRED".to_string(),
        Some(timestamp) => {
            let date = chrono::DateTime::from_timestamp(timestamp, 0)
                .map(|value| value.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| timestamp.to_string());
            format!(", expires:{date}")
        }
        None => String::new(),
    }
}

/// Drop a session's dedup state after this long untouched. Sessions are only
/// explicitly reset on a same-session Stop/wrapup; one that ends abnormally
/// (client crash, kill, no Stop hook) would otherwise leak forever. An hour is
/// far longer than any real session's inter-hook gap.
const HOOK_SESSION_TTL: std::time::Duration = std::time::Duration::from_hours(1);

/// Hard cap on live sessions as a safety net against a burst of distinct keys
/// within the TTL window. When exceeded, the least-recently-touched session is
/// evicted (its only cost is re-showing already-injected context once).
const MAX_HOOK_SESSIONS: usize = 256;

#[derive(Debug, Default)]
pub struct HookDedupState {
    sessions: HashMap<String, HookSessionState>,
}

#[derive(Debug)]
struct HookSessionState {
    memory_ids: HashSet<String>,
    prior_ids: HashSet<String>,
    related_lines: HashSet<String>,
    prompt_fingerprints: VecDeque<String>,
    /// Last time this session's dedup state was accessed; drives TTL/LRU eviction.
    last_touched: std::time::Instant,
}

impl HookSessionState {
    fn new(now: std::time::Instant) -> Self {
        Self {
            memory_ids: HashSet::new(),
            prior_ids: HashSet::new(),
            related_lines: HashSet::new(),
            prompt_fingerprints: VecDeque::new(),
            last_touched: now,
        }
    }
}

/// Daemon-global state shared across all dispatched tool calls.
#[derive(Clone)]
pub struct DispatchContext {
    pub metrics: Arc<UsageMetrics>,
    pub session_id: Arc<AtomicI64>,
    pub persistent_call_count: Arc<AtomicU64>,
    pub optimize_interval_calls: u64,
    pub hook_dedup: Arc<StdMutex<HookDedupState>>,
    /// Where a hook parks background work so the caller can wait for it.
    ///
    /// `None` is the daemon: it outlives every hook by hours, so detaching is
    /// correct and collecting handles would only leak them. `Some` is the
    /// `MDKB_NO_DAEMON` in-process route, where the process exits the moment the
    /// hook returns — there, a detached task is dropped before it runs, which is
    /// why Stop-hook mining produced nothing at all on that route.
    pub background: Option<Arc<StdMutex<Vec<tokio::task::JoinHandle<()>>>>>,
}

impl DispatchContext {
    /// Run `fut` in the background, keeping it awaitable when the caller asked
    /// for that (see [`DispatchContext::background`]).
    fn spawn_background(&self, fut: impl std::future::Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(fut);
        if let Some(slot) = &self.background
            && let Ok(mut pending) = slot.lock()
        {
            pending.push(handle);
        }
    }

    /// Wait for everything [`DispatchContext::spawn_background`] collected.
    /// A no-op on the daemon, which never collects.
    pub async fn join_background(&self) {
        let Some(slot) = &self.background else {
            return;
        };
        loop {
            let Some(handle) = slot.lock().ok().and_then(|mut p| p.pop()) else {
                return;
            };
            let _ = handle.await;
        }
    }
}

impl std::fmt::Debug for DispatchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchContext")
            .field("session_id", &self.session_id.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// The session a hook event belongs to. Every injection is filed under this
/// string and the Stop hook settles by it, so the two MUST read the same field:
/// a prior injected under one key and looked for under another is never settled
/// at all, and its belief stays frozen — the failure this loop exists to end.
fn event_session(event: &Value) -> String {
    event
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(UNKNOWN_SESSION)
        .to_string()
}

/// Filed against events that carry no `session_id`. Such events still settle
/// against each other, which is the best available answer and no worse than
/// dropping them.
const UNKNOWN_SESSION: &str = "unknown";

fn hook_session_key(handle: &RepoHandle, params: &Value) -> String {
    if let Some(session_id) = params
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("{}|session:{session_id}", handle.root.display());
    }

    if let Some(transcript_path) = params
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return format!("{}|transcript:{transcript_path}", handle.root.display());
    }

    format!("{}|repo", handle.root.display())
}

/// The session's real working directory, as reported by the hook host, trusted
/// only when it sits inside the store.
///
/// `params.root` has already been collapsed to the store anchor by
/// `resolve_hook_root`, so it says nothing about WHICH project a session is in
/// when one store anchors many sibling projects. `params.cwd` is the raw host
/// event field that does — but it is client-supplied over the hook socket, so
/// it is accepted only when absolute and, after canonicalization, under `root`.
/// Anything else (missing, relative, unreadable, escaping via `..`, or another
/// repo entirely) yields `None`, which every caller must read as "unscoped" and
/// handle exactly as before this existed.
fn hook_session_cwd(params: &Value, root: &std::path::Path) -> Option<std::path::PathBuf> {
    let cwd = std::path::PathBuf::from(
        params
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?,
    );
    if !cwd.is_absolute() {
        return None;
    }
    let cwd = cwd.canonicalize().ok()?;
    let root = root.canonicalize().ok()?;
    cwd.starts_with(&root).then_some(cwd)
}

fn prompt_fingerprint(prompt: &str) -> String {
    prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The current session id as a provenance string, or `None` before a session is
/// established (session_id == 0).
fn session_provenance(dctx: &DispatchContext) -> Option<String> {
    let id = dctx.session_id.load(Ordering::Relaxed);
    (id > 0).then(|| id.to_string())
}

impl DispatchContext {
    #[cfg(feature = "http-server")]
    pub(crate) fn new(
        metrics: Arc<UsageMetrics>,
        session_id: Arc<AtomicI64>,
        persistent_call_count: Arc<AtomicU64>,
        optimize_interval_calls: u64,
    ) -> Self {
        Self {
            metrics,
            session_id,
            persistent_call_count,
            optimize_interval_calls,
            hook_dedup: Arc::new(StdMutex::new(HookDedupState::default())),
            // The HTTP server has no caller to hand background work back to:
            // it outlives every request, so a task parked here would never be
            // awaited. Spawning detached is the correct behaviour there.
            background: None,
        }
    }

    fn with_hook_session<R>(&self, key: &str, f: impl FnOnce(&mut HookSessionState) -> R) -> R {
        let mut state = self
            .hook_dedup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = std::time::Instant::now();

        // TTL sweep: drop sessions untouched past the TTL so abnormally-ended
        // sessions can't leak. Cheap — the map is bounded by MAX_HOOK_SESSIONS.
        state
            .sessions
            .retain(|_, s| now.duration_since(s.last_touched) < HOOK_SESSION_TTL);

        state
            .sessions
            .entry(key.to_string())
            .or_insert_with(|| HookSessionState::new(now))
            .last_touched = now;

        // LRU safety net: if a burst of distinct keys exceeds the cap within the
        // TTL, evict the oldest session other than the one we just touched.
        if state.sessions.len() > MAX_HOOK_SESSIONS {
            if let Some(oldest) = state
                .sessions
                .iter()
                .filter(|(k, _)| k.as_str() != key)
                .min_by_key(|(_, s)| s.last_touched)
                .map(|(k, _)| k.clone())
            {
                state.sessions.remove(&oldest);
            }
        }

        let session = state
            .sessions
            .get_mut(key)
            .expect("session was just inserted");
        f(session)
    }

    fn reset_hook_session(&self, key: &str) {
        let mut state = self
            .hook_dedup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.sessions.remove(key);
    }

    fn remember_hook_prompt(&self, key: &str, fingerprint: &str) -> bool {
        if fingerprint.is_empty() {
            return false;
        }
        self.with_hook_session(key, |session| {
            let repeated = session
                .prompt_fingerprints
                .iter()
                .any(|seen| seen == fingerprint);
            if !repeated {
                session
                    .prompt_fingerprints
                    .push_back(fingerprint.to_string());
                while session.prompt_fingerprints.len() > MAX_HOOK_PROMPT_FINGERPRINTS {
                    session.prompt_fingerprints.pop_front();
                }
            }
            repeated
        })
    }

    fn retain_new_hook_memories(&self, key: &str, results: &mut Vec<memory::MemoryEntry>) {
        self.with_hook_session(key, |session| {
            results.retain(|entry| !session.memory_ids.contains(&entry.id));
            for entry in results {
                session.memory_ids.insert(entry.id.clone());
            }
        });
    }

    fn hook_prior_seen(&self, key: &str, prior_id: &str) -> bool {
        self.with_hook_session(key, |session| session.prior_ids.contains(prior_id))
    }

    fn record_hook_prior(&self, key: &str, prior_id: &str) {
        self.with_hook_session(key, |session| {
            session.prior_ids.insert(prior_id.to_string());
        });
    }

    fn retain_new_hook_related_lines(&self, key: &str, related: &mut Vec<String>) {
        self.with_hook_session(key, |session| {
            related.retain(|line| !session.related_lines.contains(line));
            for line in related {
                session.related_lines.insert(line.clone());
            }
        });
    }

    /// Record a tool call against per-repo stats. No-op when session not yet
    /// established (session_id == 0). Uses `handle.ctx` so it works in both
    /// standalone and global daemon mode.
    pub async fn record_persistent_call(
        &self,
        handle: &RepoHandle,
        tool_name: &str,
        tokens: usize,
        results: usize,
        truncated: bool,
    ) {
        let session_id = self.session_id.load(Ordering::Relaxed);
        if session_id == 0 {
            return;
        }

        let mut ctx_guard = handle.ctx.lock().await;
        if ctx_guard.is_none() {
            return;
        }

        let call_count = self.persistent_call_count.fetch_add(1, Ordering::Relaxed) + 1;
        let outcome =
            crate::core::run_guarded_write(&mut ctx_guard, "persistent call telemetry", |ctx| {
                stats::record_call(&ctx.conn, session_id, tool_name, tokens, results, truncated)?;
                if crate::store::maintenance::should_optimize(
                    call_count,
                    self.optimize_interval_calls,
                ) {
                    crate::store::maintenance::run_optimize(&ctx.conn)?;
                }
                Ok(())
            });
        if let Some(Err(error)) = outcome {
            tracing::warn!("Failed to record call stats: {error}");
        }
    }
}

/// Ensure the repo's database context is initialized.
pub async fn ensure_handle_context(handle: &RepoHandle) -> Result<(), McpError> {
    let mut ctx_guard = handle.ctx.lock().await;
    if ctx_guard.is_none() {
        if handle.doc_reindex_active.load(Ordering::Relaxed) {
            return Err(mcp_error("Repo initializing, retry shortly"));
        }
        let ctx = match Context::open(&handle.root) {
            Ok(ctx) => ctx,
            Err(e) if e.is_not_found() => {
                tracing::info!("Auto-initializing mdkb at {}", handle.root.display());
                Context::init(&handle.root)
                    .map_err(|e| mcp_error(format!("Failed to auto-initialize mdkb: {e}")))?
            }
            Err(e) => return Err(mcp_error(format!("Failed to open database: {e}"))),
        };
        // Autoheal rebuilt an empty index — schedule a full rebuild (docs +
        // sessions + code) to repopulate it from source. Sending the repo root
        // (a directory) is the watcher's full-rebuild signal, distinct from the
        // file paths post_tool_use injects. Best-effort: a full channel means a
        // rebuild is already queued, which is exactly what we want.
        if ctx.rebuilt_from_corruption {
            if let Err(e) = handle.reindex_tx.try_send(handle.root.clone()) {
                tracing::warn!("failed to schedule post-heal reindex: {e}");
            }
        }
        *ctx_guard = Some(ctx);
    }
    Ok(())
}

/// Run one daemon-backed memory mutation under the cross-process mutation lock,
/// verify the resulting database through a fresh connection, and release the
/// long-lived context immediately if the index is corrupt.
///
/// Memory tools used to write directly through `RepoHandle::ctx`. That bypassed
/// both the project lock and [`crate::core::run_mutation`], so a daemon
/// could retain the live lock after detecting corruption and block its own
/// quarantine indefinitely. The fresh-connection probe is intentional: the
/// working connection's pager can report a file torn underneath it as healthy.
fn run_handle_memory_mutation<T>(
    slot: &mut Option<Context>,
    what: &str,
    f: impl FnOnce(&Context) -> Result<T, McpError>,
) -> Result<T, McpError> {
    let (result, verification) = {
        let ctx = slot
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;
        let _writer_guard = crate::store::mutation_lock::acquire_writer(&ctx.db_path, what)
            .map_err(|e| mcp_error(format!("Failed to acquire writer lock: {e}")))?;
        let _mutation_guard = crate::store::mutation_lock::acquire(&ctx.db_path, what)
            .map_err(|e| mcp_error(format!("Failed to acquire mutation lock: {e}")))?;

        crate::store::heal::invalidate_marker(&ctx.db_path);
        let result = f(ctx);
        let verification = crate::store::heal::verify_and_mark_throttled(&ctx.db_path);
        (result, verification)
    };

    let result = close_context_on_reported_corruption(slot, what, result);

    if let Err(error) = verification {
        tracing::error!(
            operation = what,
            error = %error,
            "index is corrupt after memory mutation — closing this connection so the next open can quarantine, salvage memory and rebuild"
        );
        *slot = None;
        return Err(mcp_error(format!(
            "Index is corrupt after {what}; the connection was closed for automatic recovery: {error}"
        )));
    }

    result
}

/// Drain pending memory embeddings in the background. Single-flight per handle
/// (a second call while one is in flight is a no-op), gated on a cheap `COUNT(*)`,
/// with ONNX inference pushed off the async runtime via `spawn_blocking`.
///
/// Best-effort: every failure degrades to a debug log — a background task must
/// never surface errors. Returns the spawned task handle when this call won the
/// single-flight guard, or `None` when a drain was already running. Hook callers
/// ignore the handle; tests await it to observe the drain deterministically.
pub fn spawn_embedding_backfill(handle: Arc<RepoHandle>) -> Option<tokio::task::JoinHandle<usize>> {
    if handle.backfill_in_flight.swap(true, Ordering::AcqRel) {
        return None; // another drain already in flight
    }
    Some(tokio::spawn(run_embedding_backfill(handle)))
}

/// Awaitable core of [`spawn_embedding_backfill`]: reset the single-flight guard
/// on exit (even on panic), open the context, and — only when something is
/// pending — drain it off the runtime. Returns the number of entries embedded.
/// Standalone (not inlined into the spawn) so tests can await it directly.
async fn run_embedding_backfill(handle: Arc<RepoHandle>) -> usize {
    // RAII reset so a panicking drain can't wedge the guard permanently.
    struct FlightGuard(Arc<RepoHandle>);
    impl Drop for FlightGuard {
        fn drop(&mut self) {
            self.0.backfill_in_flight.store(false, Ordering::Release);
        }
    }
    let _guard = FlightGuard(Arc::clone(&handle));

    if let Err(e) = ensure_handle_context(&handle).await {
        tracing::debug!("embedding backfill: context open failed: {e}");
        return 0;
    }
    let ctx = Arc::clone(&handle.ctx);
    let drained = tokio::task::spawn_blocking(move || {
        let mut guard = ctx.blocking_lock();
        // Cheap indexed COUNT(*): only a positive count is worth loading the model.
        match crate::core::run_guarded_read(&mut guard, "embedding backlog count", |ctx| {
            crate::store::memory::count_pending_embeddings(&ctx.conn)
        }) {
            Some(Ok(0)) | None => Some(0),
            Some(Ok(_)) => {
                match crate::core::run_mutation(&mut guard, "memory embedding backfill", |ctx| {
                    crate::store::memory::backfill_memory_embeddings(&ctx.conn)
                }) {
                    Some(Ok(n)) => Some(n),
                    Some(Err(e)) => {
                        tracing::debug!("embedding backfill: backfill failed: {e}");
                        Some(0)
                    }
                    None => Some(0),
                }
            }
            Some(Err(error)) => {
                tracing::debug!("embedding backfill: pending count failed: {error}");
                Some(0)
            }
        }
    })
    .await
    .ok()
    .flatten()
    .unwrap_or(0);
    if drained > 0 {
        tracing::debug!("embedding backfill: drained {drained} pending memory embeddings");
    }
    drained
}

/// Acquire (and lazily initialize) the code index on a repo handle.
pub async fn acquire_handle_code_index(
    handle: &RepoHandle,
) -> Result<tokio::sync::MutexGuard<'_, Option<IndexFacade>>, McpError> {
    if handle.code_reindex_active.load(Ordering::Relaxed) {
        return Ok(handle.code_index.lock().await);
    }
    let mut idx_guard = handle.code_index.lock().await;
    if idx_guard.is_none() {
        let index_path = handle.root.join(".mdkb/code.sqlite");
        let mut facade = IndexFacade::open_or_create(&index_path)
            .map_err(|e| mcp_error(format!("Failed to open code index: {e}")))?;
        let pipeline_config = crate::code::indexing::pipeline::PipelineConfig {
            ignore_patterns: handle.code_ignore_patterns.clone(),
            respect_gitignore: handle.config.code.indexing.respect_gitignore,
            ..Default::default()
        };
        facade = facade.with_config(pipeline_config);
        *idx_guard = Some(facade);
    }
    Ok(idx_guard)
}

// ── Tool impls ──────────────────────────────────────────────────────────────

/// `status` — returns the human-readable index status string. Callers wrap
/// this in the transport-appropriate envelope (CallToolResult or JSON-RPC).
pub async fn status_impl(handle: &RepoHandle) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    let mut ctx_guard = handle.ctx.lock().await;
    let mut output = crate::core::run_guarded_read(&mut ctx_guard, "status", |ctx| {
        let index_status = search::get_status(&ctx.conn)?;

        let mut output = format!(
            "## Index Status\n\nDocuments: {}\nStale: {}\nDB Size: {} bytes\n",
            index_status.documents, index_status.stale_documents, index_status.db_size_bytes
        );

        let coll_list = collections::list_collections(&ctx.conn)?;

        output.push_str(&format!("\n## Collections ({})\n\n", coll_list.len()));
        if coll_list.is_empty() {
            output.push_str("No collections configured. Markdown files are indexed via collections (use CLI: `mdkb collection add <name> <path>`).\n");
        } else {
            for coll in &coll_list {
                let doc_count =
                    collections::get_collection_document_count(&ctx.conn, &coll.name)?;
                let source_tag = if coll.source == "convention" {
                    "[convention]"
                } else {
                    "[manual]"
                };
                output.push_str(&format!(
                    "- {} {} ({}): {} docs, pattern: {}\n",
                    coll.name, source_tag, coll.path, doc_count, coll.pattern
                ));
            }
        }

        Ok(output)
    })
    .ok_or_else(|| mcp_error("Database not initialized"))?
    .map_err(|e| mcp_error(format!("Failed to get status: {e}")))?;
    drop(ctx_guard);

    if let Ok(idx_guard) = acquire_handle_code_index(handle).await {
        if let Some(facade) = idx_guard.as_ref() {
            let symbols = facade.symbol_count();
            let files = facade.file_count();
            let relationships = facade.relationship_count();
            output.push_str(&format!(
                "\n## Code Index\n\nSymbols: {}\nFiles: {}\nRelationships: {}\n",
                symbols, files, relationships
            ));
            if symbols == 0 {
                output.push_str("\nNo symbols indexed yet. Run `update` to index source code.\n");
            }
        }
    }

    Ok(output)
}

/// `memory_delete` — delete a memory entry by id. Returns the human-readable
/// result string; callers wrap it for the transport.
pub async fn memory_delete_impl(
    handle: &RepoHandle,
    id: &str,
    dry_run: bool,
) -> Result<String, McpError> {
    memory::validate_entry_id(id).map_err(mcp_refusal)?;
    ensure_handle_context(handle).await?;

    if dry_run {
        let mut ctx_guard = handle.ctx.lock().await;
        let exists =
            crate::core::run_guarded_read(&mut ctx_guard, "memory delete dry run", |ctx| {
                memory::get_entry_without_tracking(&ctx.conn, id)
            })
            .ok_or_else(|| mcp_error("Database not initialized"))?
            .map_err(|e| mcp_store_error("Failed to check existing entry", e))?
            .is_some();
        return Ok(if exists {
            format!("dry-run: would delete memory entry '{id}'")
        } else {
            format!("dry-run: memory entry '{id}' not found")
        });
    }

    let mut ctx_guard = handle.ctx.lock().await;
    let deleted = run_handle_memory_mutation(&mut ctx_guard, "memory delete", |ctx| {
        // Literally the same door as `mdkb memory rm`, not a copy of it: the
        // archive-then-delete order that keeps a retired entry from being
        // re-imported must not exist twice.
        crate::core::memory::handle_memory_rm(ctx, id)
            .map_err(|e| mcp_store_error("Failed to delete memory entry", e))
    })?;

    Ok(if deleted {
        format!("Deleted memory entry '{id}'.")
    } else {
        format!("Memory entry '{id}' not found.")
    })
}

/// `memory_confirm` — record a Bayesian confirmation signal against an entry.
/// `outcome` must be "confirmed" (raises `confirmations`) or "refuted" (raises
/// `corrections` and suppresses automatic injection).
pub async fn memory_confirm_impl(
    handle: &RepoHandle,
    id: &str,
    outcome: &str,
) -> Result<String, McpError> {
    let delta = memory::outcome_to_delta(outcome).map_err(|e| mcp_error(e.to_string()))?;

    ensure_handle_context(handle).await?;

    let mut ctx_guard = handle.ctx.lock().await;
    run_handle_memory_mutation(&mut ctx_guard, "memory confirm", |ctx| {
        memory::confirm_entry(&ctx.conn, id, delta)
            .map_err(|e| mcp_store_error("Failed to confirm memory entry", e))
    })
}

/// Generic error returned for any `source_file` rejection (missing,
/// out-of-root, oversized, or permission-denied). Kept identical across all
/// causes so the response never leaks OS file-existence/permission info.
const SOURCE_FILE_ERROR: &str = "source_file is invalid or inaccessible";

/// Maximum size (in bytes) of a `source_file` that `memory_write` will read.
/// Enforced against file metadata length before any content is read, so an
/// oversized file is rejected without ever being loaded into memory.
const MAX_SOURCE_FILE_BYTES: u64 = 1024 * 1024;

/// Core logic for writing a single memory entry. Used by both
/// Resolve `source_file` → content. Returns `(content, source_path)`.
/// Errors if both content and source_file are provided.
///
/// `root` is the repo root the caller is scoped to; `source_file` must
/// canonicalize to a path under it (symlink escapes included) or the read is
/// rejected with a generic error.
fn resolve_source_file(
    root: &std::path::Path,
    content: &str,
    source_file: Option<&str>,
) -> Result<(String, Option<String>), McpError> {
    match source_file {
        Some(_) if !content.is_empty() => Err(mcp_error(
            "Cannot specify both content and source_file — use one or the other",
        )),
        Some(path) => {
            let canonical_root = root
                .canonicalize()
                .map_err(|_| mcp_error(SOURCE_FILE_ERROR))?;
            let abs = std::path::Path::new(path)
                .canonicalize()
                .map_err(|_| mcp_error(SOURCE_FILE_ERROR))?;
            if !abs.starts_with(&canonical_root) {
                return Err(mcp_error(SOURCE_FILE_ERROR));
            }
            let metadata = std::fs::metadata(&abs).map_err(|_| mcp_error(SOURCE_FILE_ERROR))?;
            if metadata.len() > MAX_SOURCE_FILE_BYTES {
                return Err(mcp_error(SOURCE_FILE_ERROR));
            }
            let text = std::fs::read_to_string(&abs).map_err(|_| mcp_error(SOURCE_FILE_ERROR))?;
            Ok((text, Some(abs.to_string_lossy().to_string())))
        }
        None if content.is_empty() => {
            Err(mcp_error("Either content or source_file must be provided"))
        }
        None => Ok((content.to_string(), None)),
    }
}

/// Compute a query embedding off the async runtime. ONNX inference is
/// CPU-bound (10-100ms); running it while the per-repo `ctx` mutex is held
/// stalls a tokio worker and serializes every call on that repo. This mirrors
/// the `spawn_blocking`-before-lock pattern in `memory_write_impl`. On failure
/// returns `None` (with a warn) so callers transparently fall back to BM25.
async fn embed_query_off_lock(query: &str) -> Option<Vec<f32>> {
    let query = query.to_string();
    let result = tokio::task::spawn_blocking(move || {
        crate::llm::get_cached_service().and_then(|s| s.embed_query(&query))
    })
    .await;
    match result {
        Ok(Ok(emb)) => Some(emb),
        Ok(Err(e)) => {
            tracing::warn!("query embedding failed, falling back to BM25-only: {e}");
            None
        }
        Err(e) => {
            tracing::warn!("query embedding task panicked, falling back to BM25-only: {e}");
            None
        }
    }
}

/// `memory_write` — create or update a single memory entry. Wraps
/// [`crate::core::memory::write_memory`] with `RepoHandle` ctx acquisition.
///
/// Embedding is generated **before** the ctx lock is acquired so that
/// CPU-bound ONNX inference (10–100 ms) never blocks the tokio executor
/// while holding the Mutex guard.
pub async fn memory_write_impl(
    handle: &RepoHandle,
    entry: &MemoryWriteBatchEntry,
    session: Option<&str>,
    dry_run: bool,
) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    // Resolve source_file off the runtime — it does blocking fs canonicalize +
    // read (up to MAX_SOURCE_FILE_BYTES), which shouldn't stall a tokio worker.
    let (content, source_path) = {
        let root = handle.root.clone();
        let content_in = entry.content.clone();
        let source_file_in = entry.source_file.clone();
        tokio::task::spawn_blocking(move || {
            resolve_source_file(&root, &content_in, source_file_in.as_deref())
        })
        .await
        .map_err(|e| mcp_error(format!("source_file resolution task panicked: {e}")))??
    };

    // Pre-compute embedding outside the lock — ONNX is CPU-bound. Skipped for
    // dry-run, which returns before any embedding or write happens.
    let embedding = if dry_run {
        None
    } else {
        let embed_text = format!("{} {}", entry.title, content);
        tokio::task::spawn_blocking(move || {
            crate::llm::get_cached_service()
                .ok()
                .and_then(|svc| svc.embed_query(&embed_text).ok())
        })
        .await
        .unwrap_or(None)
    };

    let relations = entry
        .relates
        .iter()
        .map(|edge| crate::core::memory::WriteRelation {
            relation: edge.relation.clone(),
            target: edge.target.clone(),
            target_kind: edge.target_kind.clone(),
        })
        .collect::<Vec<_>>();
    let input = crate::core::memory::WriteMemoryInput {
        id: &entry.id,
        title: &entry.title,
        content: &content,
        entry_type: &entry.entry_type,
        source_type: entry.source_type.as_deref(),
        tags: &entry.tags,
        ttl: entry.ttl,
        due_in: entry.due_in,
        embedding: embedding.as_deref(),
        // Already embedded off the lock above: letting `write_memory` do it
        // would run ONNX on the runtime thread holding the context.
        embed_when_missing: false,
        source_path: source_path.as_deref(),
        relates: &relations,
        session,
        agent: entry.agent.as_deref(),
        on_conflict: entry.on_conflict.as_deref(),
        dry_run,
    };

    let mut ctx_guard = handle.ctx.lock().await;
    if dry_run {
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;
        let result = crate::core::memory::write_memory(&ctx.conn, input)
            .map_err(|error| mcp_store_error("Memory write failed", error));
        close_context_on_reported_corruption(&mut ctx_guard, "memory write dry run", result)
    } else {
        let id = input.id;
        run_handle_memory_mutation(&mut ctx_guard, "memory write", |ctx| {
            let output = crate::core::memory::write_memory(&ctx.conn, input)
                .map_err(|error| mcp_store_error("Memory write failed", error))?;
            // Same door as `mdkb memory add`: the file exists the moment the
            // row does, instead of at the next sync.
            crate::core::memory_sync::project_after_write(ctx, id, chrono::Utc::now().timestamp());
            Ok(output)
        })
    }
}

/// `memory_write_batch` — create or update up to 20 entries in one call.
/// Returns `(joined_output, count)`. Enforces empty/limit guards before
/// touching the DB.
///
/// All embeddings are generated **before** the ctx lock is acquired so that
/// CPU-bound ONNX inference never blocks the tokio executor while holding
/// the Mutex guard.
pub async fn memory_write_batch_impl(
    handle: &RepoHandle,
    entries: &[MemoryWriteBatchEntry],
    session: Option<&str>,
    dry_run: bool,
) -> Result<(String, usize), McpError> {
    if entries.is_empty() {
        return Err(mcp_error("entries array must not be empty"));
    }
    if entries.len() > 20 {
        return Err(mcp_error("max 20 entries per batch"));
    }

    ensure_handle_context(handle).await?;

    // Resolve source_file → content for all entries before computing embeddings,
    // off the runtime (blocking fs canonicalize + read per entry).
    let resolved: Vec<(String, Option<String>)> = {
        let root = handle.root.clone();
        let inputs: Vec<(String, Option<String>)> = entries
            .iter()
            .map(|e| (e.content.clone(), e.source_file.clone()))
            .collect();
        tokio::task::spawn_blocking(move || {
            inputs
                .iter()
                .map(|(content, sf)| resolve_source_file(&root, content, sf.as_deref()))
                .collect::<Result<_, _>>()
        })
        .await
        .map_err(|e| mcp_error(format!("source_file resolution task panicked: {e}")))??
    };

    // Pre-compute embeddings for all entries outside the lock — ONNX is CPU-bound.
    // Skipped for dry-run, which returns before any embedding or write happens.
    let embeddings: Vec<Option<Vec<f32>>> = if dry_run {
        vec![None; entries.len()]
    } else {
        let embed_texts: Vec<String> = entries
            .iter()
            .zip(resolved.iter())
            .map(|(e, (content, _))| format!("{} {}", e.title, content))
            .collect();
        tokio::task::spawn_blocking(move || match crate::llm::get_cached_service() {
            Ok(svc) => embed_texts
                .iter()
                .map(|text| svc.embed_query(text).ok())
                .collect(),
            Err(_) => vec![None; embed_texts.len()],
        })
        .await
        .unwrap_or_else(|_| vec![None; entries.len()])
    };

    let run = |ctx: &Context| {
        let mut results = Vec::with_capacity(entries.len());
        for ((entry, (content, source_path)), embedding) in
            entries.iter().zip(resolved.iter()).zip(embeddings)
        {
            let relations = entry
                .relates
                .iter()
                .map(|edge| crate::core::memory::WriteRelation {
                    relation: edge.relation.clone(),
                    target: edge.target.clone(),
                    target_kind: edge.target_kind.clone(),
                })
                .collect::<Vec<_>>();
            let result = crate::core::memory::write_memory(
                &ctx.conn,
                crate::core::memory::WriteMemoryInput {
                    id: &entry.id,
                    title: &entry.title,
                    content,
                    entry_type: &entry.entry_type,
                    source_type: entry.source_type.as_deref(),
                    tags: &entry.tags,
                    ttl: entry.ttl,
                    due_in: entry.due_in,
                    embedding: embedding.as_deref(),
                    // Batch-embedded off the lock above, for the same reason.
                    embed_when_missing: false,
                    source_path: source_path.as_deref(),
                    relates: &relations,
                    session,
                    agent: entry.agent.as_deref(),
                    on_conflict: entry.on_conflict.as_deref(),
                    dry_run,
                },
            )
            .map_err(|error| mcp_store_error("Memory write failed", error))?;
            if !dry_run {
                crate::core::memory_sync::project_after_write(
                    ctx,
                    &entry.id,
                    chrono::Utc::now().timestamp(),
                );
            }
            results.push(result);
        }

        let count = results.len();
        Ok((results.join("\n"), count))
    };

    let mut ctx_guard = handle.ctx.lock().await;
    if dry_run {
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;
        let result = run(ctx);
        close_context_on_reported_corruption(&mut ctx_guard, "memory write batch dry run", result)
    } else {
        run_handle_memory_mutation(&mut ctx_guard, "memory write batch", run)
    }
}

/// The most entries one `memory_list` call returns, whatever `limit` asks.
pub const MEMORY_LIST_MAX_LIMIT: usize = 200;

/// `memory_list` — list active memory entries sorted by `sort` ("recent" |
/// "popular" | "newest"). Returns `(rendered_text, entry_count)` so callers
/// can record search metrics.
pub async fn memory_list_impl(
    handle: &RepoHandle,
    limit: usize,
    sort: &str,
) -> Result<(String, usize), McpError> {
    let sort_order: memory::MemorySortOrder = sort.parse().map_err(mcp_error)?;
    let limit = limit.min(MEMORY_LIST_MAX_LIMIT);

    ensure_handle_context(handle).await?;

    let mut ctx_guard = handle.ctx.lock().await;
    let entries = crate::core::run_guarded_read(&mut ctx_guard, "memory list", |ctx| {
        memory::list_entries_sorted(
            &ctx.conn,
            limit,
            sort_order,
            Some(memory::EntryStatus::Active),
        )
    })
    .ok_or_else(|| mcp_error("Database not initialized"))?
    .map_err(|e| mcp_error(format!("Failed to list memory entries: {e}")))?;

    if entries.is_empty() {
        return Ok(("No memory entries.".to_string(), 0));
    }

    let mut out = format!("Found {} memory entries:\n\n", entries.len());
    for e in &entries {
        let tags = e
            .tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ttl_info = format_ttl_info(e.expires_at);
        out.push_str(&format!(
            "- [{}] {} ({}, {}{}): {} {}\n",
            e.entry_type,
            e.id,
            e.title,
            relative_time_ago(e.updated_at),
            ttl_info,
            truncate_text(&e.content, 80),
            tags,
        ));
    }
    let count = entries.len();
    Ok((out, count))
}

/// `search` — single-repo hybrid search. Routes by `params.scope`:
/// "docs", "memory", "code", "symbols", or `None` (docs+memory). Returns
/// `(rendered_text, result_count)`. Cross-repo (`root="*"`) lives in
/// `cross_repo_search_impl`.
/// Append the index-empty hint when a search returned nothing AND the store
/// really is empty (0 docs, 0 memory) — so an empty result after autoheal
/// quarantine reads as "run `mdkb update`", not "nothing matched".
fn append_empty_index_hint(
    output: &mut String,
    count: usize,
    conn: &rusqlite::Connection,
) -> crate::Result<()> {
    if count == 0 && crate::store::search::index_is_empty(conn)? {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(crate::store::search::INDEX_EMPTY_HINT);
    }
    Ok(())
}

pub async fn search_impl(
    handle: &RepoHandle,
    params: &SearchParams,
) -> Result<(String, usize), McpError> {
    ensure_handle_context(handle).await?;

    let scope = params
        .scope
        .as_deref()
        .map(crate::mcp::tools::SearchScope::try_from)
        .transpose()
        .map_err(|()| {
            let invalid = params.scope.as_deref().unwrap_or_default();
            let valid = crate::mcp::tools::SearchScope::ALL
                .map(crate::mcp::tools::SearchScope::as_str)
                .join(", ");
            mcp_error(format!("Invalid scope: '{invalid}'. Valid: {valid}."))
        })?;
    let limit = params.limit.min(100);

    match scope {
        Some(crate::mcp::tools::SearchScope::Docs) => {
            let mut ctx_guard = handle.ctx.lock().await;
            let results = crate::core::run_guarded_read(&mut ctx_guard, "document search", |ctx| {
                handle_hybrid_search(
                    ctx,
                    &params.query,
                    limit,
                    params.collection.as_deref(),
                    params.include_superseded,
                )
            })
            .ok_or_else(|| mcp_error("Database not initialized"))?
            .map_err(|e| mcp_error(format!("Search failed: {e}")))?;

            let top_score = results.first().map(|r| r.score);
            let mut output = format_search_results(&results, limit);
            if let Some(hint) = ood_hint(&params.query, results.len(), top_score) {
                output.push_str(hint);
            }
            crate::core::run_guarded_read(&mut ctx_guard, "empty-index hint", |ctx| {
                append_empty_index_hint(&mut output, results.len(), &ctx.conn)
            })
            .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?
            .map_err(|e| mcp_store_error("Failed to inspect index state", e))?;
            Ok((output, results.len()))
        }
        Some(crate::mcp::tools::SearchScope::Memory) => {
            let query_embedding = embed_query_off_lock(&params.query).await;
            let mut ctx_guard = handle.ctx.lock().await;
            let entries = crate::core::run_guarded_read(&mut ctx_guard, "memory search", |ctx| {
                memory::search_entries_recall(
                    &ctx.conn,
                    &params.query,
                    query_embedding.as_deref(),
                    limit,
                    None,
                    &handle.config.search.memory,
                )
            })
            .ok_or_else(|| mcp_error("Database not initialized"))?
            .map_err(|e| mcp_error(format!("Memory search failed: {e}")))?;
            let entries: Vec<memory::MemoryEntry> =
                entries.into_iter().map(|result| result.entry).collect();
            let entries = apply_min_confidence(entries, params.min_confidence);

            let mut output = format_memory_search_results(&entries);
            if let Some(hint) = ood_hint(&params.query, entries.len(), None) {
                output.push_str(hint);
            }
            crate::core::run_guarded_read(&mut ctx_guard, "empty-index hint", |ctx| {
                append_empty_index_hint(&mut output, entries.len(), &ctx.conn)
            })
            .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?
            .map_err(|e| mcp_store_error("Failed to inspect index state", e))?;
            Ok((output, entries.len()))
        }
        None => {
            let query_embedding = embed_query_off_lock(&params.query).await;
            let mut ctx_guard = handle.ctx.lock().await;
            let (doc_results, mem_entries) =
                crate::core::run_guarded_read(&mut ctx_guard, "combined search", |ctx| {
                    let docs = handle_hybrid_search(
                        ctx,
                        &params.query,
                        limit,
                        params.collection.as_deref(),
                        params.include_superseded,
                    )?;
                    let memories = memory::search_entries_recall(
                        &ctx.conn,
                        &params.query,
                        query_embedding.as_deref(),
                        limit,
                        None,
                        &handle.config.search.memory,
                    )?;
                    Ok((docs, memories))
                })
                .ok_or_else(|| mcp_error("Database not initialized"))?
                .map_err(|e| mcp_error(format!("Search failed: {e}")))?;
            let mem_entries: Vec<memory::MemoryEntry> =
                mem_entries.into_iter().map(|result| result.entry).collect();
            let mem_entries = apply_min_confidence(mem_entries, params.min_confidence);

            let total = doc_results.len() + mem_entries.len();
            let top_score = doc_results.first().map(|r| r.score);

            let mut output = if total == 0 {
                String::new()
            } else {
                let mut s = String::new();
                if !doc_results.is_empty() {
                    s.push_str(&format_search_results(&doc_results, limit));
                }
                if !mem_entries.is_empty() {
                    if !doc_results.is_empty() {
                        s.push_str("\n## Memory\n\n");
                    }
                    s.push_str(&format_memory_search_results(&mem_entries));
                }
                s
            };
            if let Some(hint) = ood_hint(&params.query, total, top_score) {
                output.push_str(hint);
            }
            crate::core::run_guarded_read(&mut ctx_guard, "empty-index hint", |ctx| {
                append_empty_index_hint(&mut output, total, &ctx.conn)
            })
            .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?
            .map_err(|e| mcp_store_error("Failed to inspect index state", e))?;
            Ok((output, total))
        }
        Some(crate::mcp::tools::SearchScope::Code | crate::mcp::tools::SearchScope::Symbols) => {
            let mut idx_guard = acquire_handle_code_index(handle).await?;
            let Some(facade) = idx_guard.as_mut() else {
                return Ok(("Code index is being rebuilt, retry shortly.".to_string(), 0));
            };

            if scope == Some(crate::mcp::tools::SearchScope::Code) {
                let code_limit = params.limit.min(5);
                let results = crate::core::code::semantic_search_scoped(
                    facade,
                    &handle.config,
                    &params.query,
                    params.kind.as_deref(),
                    code_limit,
                    params.threshold,
                )
                .map_err(|e| mcp_error(e.to_string()))?;

                if results.is_empty() {
                    return Ok(("No semantic matches found.".to_string(), 0));
                }
                let mut out = format!("Found {} semantic match(es):\n\n", results.len());
                for (sym, score) in &results {
                    out.push_str(&format_symbol(sym));
                    out.push_str(&format!("    Similarity: {score:.3}\n"));
                    out.push('\n');
                }
                let count = results.len();
                Ok((out, count))
            } else {
                let found = crate::core::code::search_symbols_scoped(
                    facade,
                    &params.query,
                    params.kind.as_deref(),
                    params.file.as_deref(),
                    limit,
                )
                .map_err(|e| mcp_error(e.to_string()))?;
                let symbols = found.symbols;

                if symbols.is_empty() {
                    let total = facade.symbol_count();
                    return Ok((
                        format!("0 matches ({total} symbols indexed). Try a shorter name."),
                        0,
                    ));
                }
                let rel_paths: Vec<String> = symbols
                    .iter()
                    .map(|s| s.file_path.to_string())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let token_map = facade.get_file_token_estimates(&rel_paths);
                let mut out = if found.total > symbols.len() {
                    format!(
                        "Showing {} of {} symbol(s) — narrow with kind/file, or raise limit:\n\n",
                        symbols.len(),
                        found.total,
                    )
                } else {
                    format!("Found {} symbol(s):\n\n", symbols.len())
                };
                for sym in &symbols {
                    out.push_str(&format_symbol_with_file_tokens(
                        sym,
                        token_map.get(sym.file_path.as_ref()).copied(),
                    ));
                    out.push('\n');
                }
                let count = symbols.len();
                Ok((out, count))
            }
        }
        Some(crate::mcp::tools::SearchScope::Duplicates) => {
            // The audit reads the code index off its own read-only connection,
            // so it does not take the code-index guard the other code scopes
            // need. It does take the memory connection, for the ignore-list.
            let mut ctx_guard = handle.ctx.lock().await;
            let report =
                crate::core::run_guarded_read(&mut ctx_guard, "duplication audit", |ctx| {
                    crate::core::dup::handle_dup(
                        &handle.root,
                        Some(&ctx.conn),
                        &handle.config,
                        &crate::core::dup::DupOverrides {
                            // No `semantic` field on the MCP schema: it would be
                            // charged on every turn for an audit run rarely.
                            // `threshold` is the opt-in, and it is the only
                            // knob the pass has.
                            semantic: false,
                            threshold: params.threshold,
                            min_nodes: None,
                            // An empty query sweeps the repository; `file` is what
                            // narrows it, matching `mdkb dup --file`.
                            file: params.file.clone(),
                            since: params.since.clone(),
                        },
                    )
                })
                .ok_or_else(|| mcp_error("Database not initialized"))?
                .map_err(|e| mcp_error(format!("Duplication audit failed: {e}")))?;

            Ok((report.markdown.clone(), report.clusters()))
        }
    }
}

/// `search` (cross-repo) — fan out across the registered handles, merge with
/// score-descending sort, and truncate to `params.limit`. Memory results from
/// each repo are formatted as a single pseudo-result for compatibility with
/// `SearchResult`-based aggregation.
///
/// Code/symbols scopes are rejected — those indexes are per-repo only.
pub async fn cross_repo_search_impl(
    handles: &[Arc<RepoHandle>],
    params: &SearchParams,
) -> Result<(String, usize), McpError> {
    if handles.is_empty() {
        return Err(mcp_error(
            "No repos registered. Pass root=\"/abs/path\" to open one, or provide MCP roots/list.",
        ));
    }

    let scope = params
        .scope
        .as_deref()
        .map(crate::mcp::tools::SearchScope::try_from)
        .transpose()
        .map_err(|()| mcp_error("Invalid search scope"))?;
    let limit = params.limit.min(100);

    if matches!(
        scope,
        Some(
            crate::mcp::tools::SearchScope::Code
                | crate::mcp::tools::SearchScope::Symbols
                | crate::mcp::tools::SearchScope::Duplicates
        )
    ) {
        return Err(mcp_error(
            "Cross-repo search is not supported for code/symbols/duplicates scope. Specify a root.",
        ));
    }

    // Fan-out concurrently across all repos, then merge. Per-repo errors are
    // logged and treated as empty results so a single failing repo does not
    // abort the whole cross-repo search.
    let per_repo_futures = handles.iter().map(|handle| {
        let handle = Arc::clone(handle);
        let query = params.query.clone();
        let collection = params.collection.clone();
        let include_superseded = params.include_superseded;
        let min_confidence = params.min_confidence;
        let memory_cfg = handle.config.search.memory.clone();

        async move {
            if let Err(e) = ensure_handle_context(&handle).await {
                tracing::warn!(
                    "cross_repo_search: skipping {}: {}",
                    handle.root.display(),
                    e.message
                );
                return Vec::<SearchResult>::new();
            }
            // Embed off the runtime before locking — memory scope only needs it.
            let query_embedding = if scope == Some(crate::mcp::tools::SearchScope::Memory) {
                embed_query_off_lock(&query).await
            } else {
                None
            };

            let mut ctx_guard = handle.ctx.lock().await;

            let repo_tag = handle.root.display().to_string();
            let mut repo_results: Vec<SearchResult> = Vec::new();

            match scope {
                Some(crate::mcp::tools::SearchScope::Docs) | None => {
                    let result = crate::core::run_guarded_read(
                        &mut ctx_guard,
                        "cross-repo document search",
                        |ctx| {
                            handle_hybrid_search(
                                ctx,
                                &query,
                                limit,
                                collection.as_deref(),
                                include_superseded,
                            )
                        },
                    );
                    match result {
                        Some(Ok(mut results)) => {
                            for r in &mut results {
                                r.repo_root = Some(repo_tag.clone());
                            }
                            repo_results.extend(results);
                        }
                        Some(Err(e)) => {
                            tracing::warn!(
                                "cross_repo_search: docs search failed on {}: {e}",
                                repo_tag
                            );
                        }
                        None => {}
                    }
                }
                Some(crate::mcp::tools::SearchScope::Memory) => {
                    let result = crate::core::run_guarded_read(
                        &mut ctx_guard,
                        "cross-repo memory search",
                        |ctx| {
                            memory::search_entries_recall(
                                &ctx.conn,
                                &query,
                                query_embedding.as_deref(),
                                limit,
                                None,
                                &memory_cfg,
                            )
                        },
                    );
                    match result {
                        Some(Ok(entries)) => {
                            let entries: Vec<memory::MemoryEntry> =
                                entries.into_iter().map(|result| result.entry).collect();
                            let entries = apply_min_confidence(entries, min_confidence);
                            if !entries.is_empty() {
                                let text = format_memory_search_results(&entries);
                                let mut pseudo = SearchResult {
                                    id: 0,
                                    collection: "memory".to_string(),
                                    path: String::new(),
                                    title: None,
                                    score: 1.0,
                                    snippets: vec![text],
                                    status: None,
                                    superseded_by: None,
                                    repo_root: Some(repo_tag),
                                };
                                if let Some(e) = entries.first() {
                                    pseudo.path.clone_from(&e.id);
                                    pseudo.title = Some(e.title.clone());
                                }
                                repo_results.push(pseudo);
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!(
                                "cross_repo_search: memory search failed on {}: {e}",
                                repo_tag
                            );
                        }
                        None => {}
                    }
                }
                _ => {}
            }

            repo_results
        }
    });

    let mut all_results: Vec<SearchResult> = join_all(per_repo_futures)
        .await
        .into_iter()
        .flatten()
        .collect();

    all_results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    all_results.truncate(limit);

    let output = if all_results.is_empty() {
        "No results across repos. Try broader terms.".to_string()
    } else {
        format_search_results(&all_results, limit)
    };

    let count = all_results.len();
    Ok((output, count))
}

/// Render a single document's content with optional line range and evolution
/// metadata. Fetches through [`crate::core::ops::get_document_content`], the
/// one place CLI and MCP both go for a document's blob and line range.
/// Truncation uses `handle.config.mcp.max_response_tokens` for parity.
fn render_document_content(
    handle: &RepoHandle,
    ctx: &Context,
    doc: &crate::domain::Document,
    lines: Option<&str>,
) -> Result<String, McpError> {
    let mut output =
        crate::core::ops::get_document_content(ctx, doc, lines).map_err(|e| match e.kind() {
            ErrorKind::DocumentNotFound { .. } => {
                mcp_error("Content missing for document. Try `update` to reindex.")
            }
            _ => mcp_store_error("Failed to get document content", e),
        })?;

    let document_status = match evolution::get_document_status(&ctx.conn, doc.id) {
        Ok(status) => status,
        Err(error) if error.is_index_corrupt() => {
            return Err(mcp_store_error("Failed to read document status", error));
        }
        Err(_) => None,
    };
    if let Some((status, reason)) = document_status {
        let status_str = format!("{status:?}");
        if status_str != "Current" {
            output.push_str(&format!("\n\n---\n**Status:** {status_str}"));
            if let Some(r) = reason {
                output.push_str(&format!(" ({r})"));
            }
            match evolution::get_superseded_by(&ctx.conn, doc.id) {
                Ok(descendants) => {
                    for evo in &descendants {
                        let source = documents::get_document(&ctx.conn, evo.source_doc_id)
                            .map_err(|e| {
                                mcp_store_error("Failed to read superseding document", e)
                            })?;
                        if let Some(source) = source {
                            output.push_str(&format!(
                                "\n**Superseded by:** {} ({})",
                                source.relative_path, evo.relationship
                            ));
                        }
                    }
                }
                Err(error) if error.is_index_corrupt() => {
                    return Err(mcp_store_error("Failed to read document evolution", error));
                }
                Err(_) => {}
            }
        }
    }

    let max_tokens = handle.config.mcp.max_response_tokens;
    let output = if max_tokens > 0 {
        truncate_with_continuation(&output, max_tokens, doc.id).content
    } else {
        output
    };
    Ok(output)
}

/// `get` — comma-separated batch retrieval. Returns aggregated text and the
/// number of items found. Errors when no items resolved.
const GET_BATCH_MAX_IDS: usize = 50;

async fn get_batch_impl(
    handle: &RepoHandle,
    ids: &str,
    lines: Option<&str>,
) -> Result<(String, usize), McpError> {
    let id_count = ids.split(',').filter(|s| !s.trim().is_empty()).count();
    if id_count > GET_BATCH_MAX_IDS {
        return Err(mcp_error(format!(
            "get_batch: too many IDs ({id_count}); limit is {GET_BATCH_MAX_IDS}"
        )));
    }

    let mut ctx_guard = handle.ctx.lock().await;

    let mut output = String::new();
    let mut found = 0usize;

    for raw_id in ids.split(',') {
        let id = raw_id.trim();
        if id.is_empty() {
            continue;
        }

        if let Ok(numeric_id) = id.parse::<i64>() {
            let doc = crate::core::run_guarded_read(&mut ctx_guard, "batch document get", |ctx| {
                documents::get_document(&ctx.conn, numeric_id)
            })
            .ok_or_else(|| mcp_error("Database not initialized"))?
            .map_err(|e| mcp_error(format!("Failed to get document: {e}")))?;
            if let Some(doc) = doc {
                let ctx = ctx_guard
                    .as_ref()
                    .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?;
                let rendered = render_document_content(handle, ctx, &doc, lines);
                let rendered = close_context_on_reported_corruption(
                    &mut ctx_guard,
                    "batch document render",
                    rendered,
                );
                match rendered {
                    Ok(content) => {
                        let title = doc.title.as_deref().unwrap_or("(untitled)");
                        output.push_str(&format!(
                            "=== [{}] {} - {} ===\n{}\n\n",
                            doc.id, doc.relative_path, title, content
                        ));
                        found += 1;
                        continue;
                    }
                    Err(e) => {
                        output.push_str(&format!(
                            "=== [{}] {} ===\nContent error: {}\n\n",
                            doc.id, doc.relative_path, e
                        ));
                        found += 1;
                        continue;
                    }
                }
            }
        }

        let resolved =
            crate::core::run_guarded_read(&mut ctx_guard, "batch document resolve", |ctx| {
                resolve_document(&ctx.conn, id)
            })
            .ok_or_else(|| mcp_error("Database not initialized"))?;
        if let Err(error) = &resolved {
            if error.is_index_corrupt() {
                return Err(mcp_error(format!("Document resolution failed: {error}")));
            }
        }
        if let Ok(doc) = resolved {
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?;
            let rendered = render_document_content(handle, ctx, &doc, lines);
            let rendered = close_context_on_reported_corruption(
                &mut ctx_guard,
                "batch document render",
                rendered,
            );
            match rendered {
                Ok(content) => {
                    let title = doc.title.as_deref().unwrap_or("(untitled)");
                    output.push_str(&format!(
                        "=== [{}] {} - {} ===\n{}\n\n",
                        doc.id, doc.relative_path, title, content
                    ));
                    found += 1;
                    continue;
                }
                Err(e) => {
                    output.push_str(&format!(
                        "=== [{}] {} ===\nContent error: {}\n\n",
                        doc.id, doc.relative_path, e
                    ));
                    found += 1;
                    continue;
                }
            }
        }

        let entry = crate::core::run_guarded_write(&mut ctx_guard, "batch memory get", |ctx| {
            memory::get_entry(&ctx.conn, id)
        })
        .ok_or_else(|| mcp_error("Database not initialized"))?
        .map_err(|e| mcp_store_error("Failed to get memory", e))?;
        if let Some(entry) = entry {
            let ttl = format_ttl_info(entry.expires_at);
            output.push_str(&format!(
                "=== [MEM] {} - {}{} ===\n{}\n\n",
                entry.id, entry.title, ttl, entry.content
            ));
            found += 1;
            continue;
        }

        output.push_str(&format!("=== {id} ===\nNot found\n\n"));
    }

    if found == 0 {
        return Err(mcp_error("None of the requested items were found."));
    }
    Ok((output, found))
}

/// `get` — glob retrieval. Returns aggregated text, number of docs matched,
/// and a `truncated` flag indicating whether `max_response_tokens` clipped
/// the output.
async fn get_glob_impl(
    handle: &RepoHandle,
    pattern: &str,
) -> Result<(String, usize, bool), McpError> {
    let doc_limit = handle.config.mcp.max_document_tokens;
    let truncate_ellipsis = handle.config.mcp.truncate_with_ellipsis;
    let max_response_tokens = handle.config.mcp.max_response_tokens;

    let (output, result_count) = {
        let mut ctx_guard = handle.ctx.lock().await;
        let results = crate::core::run_guarded_read(&mut ctx_guard, "glob retrieval", |ctx| {
            handle_mget(ctx, pattern, None)
        })
        .ok_or_else(|| mcp_error("Database not initialized"))?
        .map_err(|e| mcp_error(format!("Glob retrieval failed: {e}")))?;

        if results.is_empty() {
            let mut msg = "No documents matched pattern.".to_string();
            crate::core::run_guarded_read(&mut ctx_guard, "empty-index hint", |ctx| {
                append_empty_index_hint(&mut msg, 0, &ctx.conn)
            })
            .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?
            .map_err(|e| mcp_store_error("Failed to inspect index state", e))?;
            return Ok((msg, 0, false));
        }

        let mut output = format!("Found {} documents:\n\n", results.len());
        for (doc, content) in &results {
            let title = doc.title.as_deref().unwrap_or("(untitled)");

            let truncated_content = if doc_limit > 0 {
                let content_tokens = count_tokens(content);
                if content_tokens > doc_limit {
                    if truncate_ellipsis {
                        truncate_with_ellipsis(content, doc_limit)
                    } else {
                        crate::metrics::tokens::truncate_to_tokens(content, doc_limit).0
                    }
                } else {
                    content.clone()
                }
            } else {
                content.clone()
            };

            output.push_str(&format!(
                "=== [{}] {} - {} ===\n{}\n\n",
                doc.id, doc.relative_path, title, truncated_content
            ));
        }
        let result_count = results.len();
        (output, result_count)
    };

    let original_len = output.len();
    let output = if max_response_tokens > 0 {
        crate::metrics::tokens::truncate_to_tokens(&output, max_response_tokens).0
    } else {
        output
    };
    let truncated = output.len() < original_len;
    Ok((output, result_count, truncated))
}

/// `get` — full implementation. Returns `(text, count, truncated)` so callers
/// can record metrics. Single-doc and memory-slug paths report `count=1` and
/// `truncated=false`. Batch and glob paths report their own counts and (for
/// glob) actual truncation status.
pub async fn get_impl(
    handle: &RepoHandle,
    params: &GetParams,
) -> Result<(String, usize, bool), McpError> {
    ensure_handle_context(handle).await?;

    let id = &params.id;

    if id.contains(',') {
        let (text, count) = get_batch_impl(handle, id, params.lines.as_deref()).await?;
        return Ok((text, count, false));
    }

    if id.contains('*') || id.contains('?') {
        return get_glob_impl(handle, id).await;
    }

    let mut ctx_guard = handle.ctx.lock().await;

    if let Ok(numeric_id) = id.parse::<i64>() {
        let doc = crate::core::run_guarded_read(&mut ctx_guard, "document get", |ctx| {
            documents::get_document(&ctx.conn, numeric_id)
        })
        .ok_or_else(|| mcp_error("Database not initialized"))?
        .map_err(|e| mcp_error(format!("Failed to get document: {e}")))?;
        if let Some(doc) = doc {
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?;
            let rendered = render_document_content(handle, ctx, &doc, params.lines.as_deref());
            let output =
                close_context_on_reported_corruption(&mut ctx_guard, "document render", rendered)?;
            return Ok((output, 1, false));
        }
    }

    if id.contains('/') || id.contains('.') {
        let resolved = crate::core::run_guarded_read(&mut ctx_guard, "document resolve", |ctx| {
            resolve_document(&ctx.conn, id)
        })
        .ok_or_else(|| mcp_error("Database not initialized"))?;
        if let Ok(doc) = resolved {
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?;
            let rendered = render_document_content(handle, ctx, &doc, params.lines.as_deref());
            let output =
                close_context_on_reported_corruption(&mut ctx_guard, "document render", rendered)?;
            return Ok((output, 1, false));
        }
    }

    let memory_entry = crate::core::run_guarded_write(&mut ctx_guard, "memory get", |ctx| {
        memory::get_entry(&ctx.conn, id)
    })
    .ok_or_else(|| mcp_error("Database not initialized"))?
    .map_err(|e| mcp_store_error("Failed to get memory", e))?;
    if let Some(entry) = memory_entry {
        if params.format.as_deref() == Some("history") {
            let revisions =
                crate::core::run_guarded_read(&mut ctx_guard, "memory revision history", |ctx| {
                    memory::get_revisions(&ctx.conn, &entry.id)
                })
                .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?
                .map_err(|e| mcp_store_error("Failed to read memory revisions", e))?;
            let output = if revisions.is_empty() {
                format!("No revision history for '{}'", entry.id)
            } else {
                let mut parts = vec![format!(
                    "# Revision history for '{}' ({} revision{})\n",
                    entry.id,
                    revisions.len(),
                    if revisions.len() == 1 { "" } else { "s" }
                )];
                for (i, rev) in revisions.iter().enumerate() {
                    let date = chrono::DateTime::from_timestamp(rev.created_at, 0)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| "?".to_string());
                    parts.push(format!(
                        "## Revision {} ({})\n```diff\n{}\n```",
                        i + 1,
                        date,
                        rev.diff
                    ));
                }
                parts.join("\n\n")
            };
            return Ok((output, 1, false));
        }

        let is_summary = params.format.as_deref() == Some("summary");
        let body = if is_summary {
            entry
                .content
                .split("\n\n")
                .next()
                .unwrap_or(&entry.content)
                .to_string()
        } else {
            entry.content.clone()
        };
        let conf = entry.confidence();
        let last_conf = entry
            .last_confirmed_at
            .map(|ts| {
                let days = (chrono::Utc::now().timestamp() - ts) as f64 / 86400.0;
                if days < 1.0 {
                    "today".to_string()
                } else {
                    format!("{}d ago", days as u64)
                }
            })
            .unwrap_or_else(|| "never".to_string());
        let conf_line = format!(
            "Confidence: {:.2} ({}↑, confirmed {}, source: {})",
            conf, entry.confirmations, last_conf, entry.source_type
        );

        let (revision_summary, provenance, edges) =
            crate::core::run_guarded_read(&mut ctx_guard, "memory metadata", |ctx| {
                Ok((
                    memory::get_revision_summary(&ctx.conn, &entry.id)?,
                    memory::get_provenance(&ctx.conn, &entry.id)?,
                    memory_graph::outgoing(&ctx.conn, &entry.id, None)?,
                ))
            })
            .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?
            .map_err(|e| mcp_store_error("Failed to read memory metadata", e))?;
        let rev_line = if revision_summary.count == 0 {
            String::new()
        } else {
            let dates: Vec<String> = revision_summary
                .dates
                .iter()
                .map(|&ts| {
                    chrono::DateTime::from_timestamp(ts, 0)
                        .map(|dt| dt.format("%Y-%m-%d").to_string())
                        .unwrap_or_else(|| "?".to_string())
                })
                .collect();
            format!(
                "\nHistory: {} revision{} ({})",
                revision_summary.count,
                if revision_summary.count == 1 { "" } else { "s" },
                dates.join(", ")
            )
        };

        let now = chrono::Utc::now().timestamp();
        let expired_marker = match entry.expires_at {
            Some(ts) if ts <= now => " [EXPIRED]",
            _ => "",
        };
        let ttl_line = match entry.expires_at {
            Some(ts) => {
                let dt = chrono::DateTime::from_timestamp(ts, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| ts.to_string());
                format!("\nExpires: {dt}")
            }
            None => String::new(),
        };
        let prov_line = match provenance {
            (session, agent) if session.is_some() || agent.is_some() => {
                let mut parts = Vec::new();
                if let Some(s) = session {
                    parts.push(format!("session {s}"));
                }
                if let Some(a) = agent {
                    parts.push(format!("agent {a}"));
                }
                format!("\nProvenance: {}", parts.join(", "))
            }
            _ => String::new(),
        };
        let edges_line = if edges.is_empty() {
            String::new()
        } else {
            let rels: Vec<String> = edges
                .iter()
                .map(|e| format!("{} {}", e.relation, e.target_ref))
                .collect();
            format!("\nEdges: {}", rels.join(", "))
        };
        let output = format!(
            "# {}{} ({})\n\nType: {} | Status: {} | Tags: {}\nAccessed: {} times | {}{}{}{}{}\n\n{}",
            entry.title,
            expired_marker,
            entry.id,
            entry.entry_type,
            entry.status,
            if entry.tags.is_empty() {
                "none".to_string()
            } else {
                entry.tags.join(", ")
            },
            entry.access_count,
            conf_line,
            rev_line,
            ttl_line,
            prov_line,
            edges_line,
            body
        );
        return Ok((output, 1, false));
    }

    let resolved = crate::core::run_guarded_read(&mut ctx_guard, "document resolve", |ctx| {
        resolve_document(&ctx.conn, id)
    })
    .ok_or_else(|| mcp_error("Database not initialized"))?;
    if let Ok(doc) = resolved {
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database closed for automatic recovery"))?;
        let rendered = render_document_content(handle, ctx, &doc, params.lines.as_deref());
        let output =
            close_context_on_reported_corruption(&mut ctx_guard, "document render", rendered)?;
        return Ok((output, 1, false));
    }

    Err(mcp_error(format!("Not found: '{}'.", params.id)))
}

/// `update` — incremental refresh of documents, code, and sessions.
///
/// Each phase diffs against what is already indexed (documents by hash, code by
/// content-hash via `IndexFacade::update`, sessions by hash) and only re-processes
/// what changed.
///
/// Returns the numbers, not a rendering of them: a routed `mdkb update` is
/// executed here and printed by the CLI, which cannot honour `--format json` on
/// prose. [`render_update_outcome`] does the rendering for the callers that want
/// text.
pub async fn update_impl(
    handle: &RepoHandle,
    request: &UpdateRequest,
) -> Result<UpdateOutcome, McpError> {
    ensure_handle_context(handle).await?;

    // A targeted run reindexes the named files only. Sessions are deliberately
    // absent from it: they live outside the root and have no path the caller
    // could have named, so including them would make "update this one file"
    // walk every transcript on the machine.
    let targeted = request.is_targeted();

    // Run the synchronous update (SQLite + filesystem + ONNX) on a blocking thread,
    // taking the lock there rather than holding an async guard across it (PERF-1).
    let docs = {
        let ctx = Arc::clone(&handle.ctx);
        let root = handle.root.clone();
        let request = request.clone();
        tokio::task::spawn_blocking(move || {
            let mut ctx_guard = ctx.blocking_lock();
            crate::core::run_mutation(&mut ctx_guard, "document update", |ctx| {
                update_documents(ctx, &root, &request)
            })
            .ok_or_else(|| "Database not initialized".to_string())?
            .map_err(|e| format!("Document update failed: {e}"))
        })
        .await
        .map_err(|e| mcp_error(format!("Document update task panicked: {e}")))?
        .map_err(mcp_error)?
    };

    let (code, code_error) = {
        let mut idx_guard = acquire_handle_code_index(handle).await?;
        let outcome =
            crate::code::indexing::run_code_mutation(&mut idx_guard, "code update", |facade| {
                if targeted {
                    crate::core::code::index_paths(facade, &handle.root, &request.files)
                } else {
                    facade.update(&handle.root)
                }
            });
        match outcome {
            Some(Ok(stats)) => (
                crate::core::indexing::report_code_stats(targeted, stats),
                None,
            ),
            Some(Err(e)) => {
                tracing::error!("Code reindex failed: {e:#}");
                (None, Some(format!("{e:#}")))
            }
            None => (None, None),
        }
    };

    let sessions = if targeted {
        None
    } else {
        index_sessions(handle).await
    };

    Ok(UpdateOutcome {
        docs,
        code,
        code_error,
        sessions,
    })
}

/// The session leg of `update`, or `None` when it did nothing worth reporting.
///
/// Every failure here is a warning, not an error: sessions are a convenience
/// index over transcripts the user never asked mdkb to own, and losing them
/// must not fail an update of the documents they asked it to index.
async fn index_sessions(handle: &RepoHandle) -> Option<UpdateResult> {
    let home = match crate::daemon::config::home_dir() {
        Ok(home) => home,
        Err(e) => {
            tracing::warn!("Session indexing skipped: cannot resolve home dir: {e}");
            return None;
        }
    };
    let sessions_base = home.join(".claude/projects");
    let project_root = handle.root.to_string_lossy().to_string();

    // Session indexing is likewise synchronous — run it off the async worker
    // with the lock taken on the blocking thread (PERF-1).
    let ctx = Arc::clone(&handle.ctx);
    let indexed = tokio::task::spawn_blocking(move || {
        let mut ctx_guard = ctx.blocking_lock();
        crate::core::run_mutation(&mut ctx_guard, "session index", |ctx| {
            crate::core::sessions::handle_session_index(ctx, &sessions_base, &project_root)
        })
    })
    .await;

    match indexed {
        Ok(Some(Ok(sr))) if sr.added > 0 || sr.updated > 0 || sr.sessions_archived > 0 => Some(sr),
        Ok(Some(Ok(_)) | None) => None,
        Ok(Some(Err(e))) => {
            tracing::warn!("Session indexing failed: {e}");
            None
        }
        Err(e) => {
            tracing::warn!("Session indexing task panicked: {e}");
            None
        }
    }
}

/// Render an [`UpdateOutcome`] as the markdown summary the MCP `update` tool
/// returns.
///
/// The CLI does not use this — it has `--format` and its own renderers. This is
/// the shape Claude reads.
pub fn render_update_outcome(outcome: &UpdateOutcome) -> String {
    let d = &outcome.docs;
    let mut out = format!(
        "## Documents\n\nAdded: {}\nUpdated: {}\nRemoved: {}\nUnchanged: {}",
        d.added, d.updated, d.removed, d.unchanged
    );
    if d.memory_embeddings_backfilled > 0 {
        out.push_str(&format!(
            "\nMemory embeddings backfilled: {}",
            d.memory_embeddings_backfilled
        ));
    }
    if d.doc_embeddings_generated > 0 {
        out.push_str(&format!(
            "\nDoc embeddings generated: {}",
            d.doc_embeddings_generated
        ));
    }

    if let Some(stats) = &outcome.code {
        out.push_str(&format!(
            "\n\n## Code\n\nFiles: {}\nSymbols: {}\nRelationships: {}",
            stats.files_indexed, stats.symbols_indexed, stats.relationships_collected
        ));
    }
    if let Some(e) = &outcome.code_error {
        out.push_str(&format!("\n\n## Code\n\nReindex failed: {e}"));
    }
    if let Some(sr) = &outcome.sessions {
        out.push_str(&format!(
            "\n\n## Sessions\n\nAdded: {}\nUpdated: {}\nUnchanged: {}\nArchived: {}",
            sr.added, sr.updated, sr.unchanged, sr.sessions_archived
        ));
    }
    out
}

fn symbol_to_json(s: &crate::code::symbol::Symbol) -> serde_json::Value {
    serde_json::json!({
        "name": s.name.as_ref(),
        "kind": s.kind.to_string(),
        "file_path": s.file_path.as_ref(),
        "line_start": s.range.start_line,
        "line_end": s.range.end_line,
        "col_start": s.range.start_column,
        "col_end": s.range.end_column,
        "signature": s.signature.as_deref(),
        "scope_context": s.scope_context.as_ref().map(|sc| format!("{sc:?}")),
    })
}

fn symbols_to_json_string(symbols: &[crate::code::symbol::Symbol]) -> Result<String, McpError> {
    let json_symbols: Vec<serde_json::Value> = symbols.iter().map(symbol_to_json).collect();
    serde_json::to_string(&json_symbols)
        .map_err(|e| mcp_error(format!("failed to serialize symbols: {e}")))
}

/// `symbols_in_file` — list all symbols in a file, ordered by position.
pub async fn symbols_in_file_impl(
    handle: &RepoHandle,
    params: &SymbolsInFileParams,
) -> Result<String, McpError> {
    let idx_guard = acquire_handle_code_index(handle).await?;
    let Some(facade) = idx_guard.as_ref() else {
        return Err(mcp_error("code index not available — run `update` first"));
    };
    let symbols = facade
        .db()
        .symbols_in_file_ordered(&params.file)
        .map_err(|e| mcp_error(format!("symbols_in_file: {e}")))?;

    symbols_to_json_string(&symbols)
}

/// `code_find` — exact symbol lookup by name with optional filters.
pub async fn code_find_impl(
    handle: &RepoHandle,
    params: &CodeFindParams,
) -> Result<String, McpError> {
    let idx_guard = acquire_handle_code_index(handle).await?;
    let Some(facade) = idx_guard.as_ref() else {
        return Err(mcp_error("code index not available — run `update` first"));
    };

    let kind = crate::core::code::parse_kind_filter(params.kind.as_deref())
        .map_err(|e| mcp_error(e.to_string()))?;
    let limit = params.limit.unwrap_or(50) as usize;

    let (results, total) = facade.query_symbols(
        crate::code::storage::NameMatch::Exact(&params.name),
        kind.as_deref(),
        params.file.as_deref(),
        limit,
    );

    // `total` travels with the rows: a boilerplate name like `tests` matches
    // hundreds of definitions, and a capped array alone reads as the whole set.
    let json_symbols: Vec<serde_json::Value> = results.iter().map(symbol_to_json).collect();
    serde_json::to_string(&serde_json::json!({
        "total": total,
        "showing": json_symbols.len(),
        "symbols": json_symbols,
    }))
    .map_err(|e| mcp_error(format!("failed to serialize symbols: {e}")))
}

/// `symbol_at_position` — find the innermost symbol at a given file position.
///
/// `params.line` is 1-based, which is what every line number Claude has already
/// seen is: search results render `start_line + 1`, and so does every editor.
/// The stored column is a 0-based tree-sitter row, so the conversion happens
/// here — passing the parameter straight through named the symbol one line down.
pub async fn symbol_at_position_impl(
    handle: &RepoHandle,
    params: &SymbolAtPositionParams,
) -> Result<String, McpError> {
    let idx_guard = acquire_handle_code_index(handle).await?;
    let Some(facade) = idx_guard.as_ref() else {
        return Err(mcp_error("code index not available — run `update` first"));
    };
    let row = params.line.saturating_sub(1);
    let symbol = facade
        .db()
        .symbol_at_position(&params.file, row, params.col)
        .map_err(|e| mcp_error(format!("symbol_at_position: {e}")))?;

    match symbol {
        Some(s) => Ok(serde_json::json!({
            "name": s.name.as_ref(),
            "kind": format!("{:?}", s.kind),
            "file_path": s.file_path.as_ref(),
            "line_start": s.range.start_line,
            "line_end": s.range.end_line,
            "col_start": s.range.start_column,
            "col_end": s.range.end_column,
            "signature": s.signature.as_deref(),
            "module_path": s.module_path.as_deref(),
        })
        .to_string()),
        None => Ok("null".to_string()),
    }
}

/// Hop limit for `graph` path queries over MCP (the CLI exposes `--max-hops`).
const GRAPH_MCP_MAX_HOPS: u32 = 6;

/// `graph` — knowledge-graph queries. Dispatches by direction (links/backlinks/
/// neighbors/path) into the graph store and returns formatted text.
pub async fn graph_impl(handle: &RepoHandle, params: &GraphParams) -> Result<String, McpError> {
    use crate::store::graph;

    ensure_handle_context(handle).await?;
    let mut ctx_guard = handle.ctx.lock().await;

    let relation = params.relation.as_deref();
    let entity = &params.entity;

    // Memory scope: traverse the memory-entry graph (memory_edges) instead of the
    // document graph. Only links/backlinks are meaningful here.
    if params.scope.as_deref() == Some("memory") {
        let rel = params
            .relation
            .as_deref()
            .map(|r| r.parse::<MemoryRelation>())
            .transpose()
            .map_err(mcp_error)?;
        let direction = params.direction.as_str();
        if !matches!(direction, "links" | "backlinks") {
            return Err(mcp_error(format!(
                "scope=memory supports links and backlinks, not '{direction}'."
            )));
        }
        let output = crate::core::run_guarded_read(&mut ctx_guard, "memory graph query", |ctx| {
            let edges = if direction == "links" {
                memory_graph::outgoing(&ctx.conn, entity, rel)
            } else {
                memory_graph::incoming(&ctx.conn, entity, rel)
            }?;
            Ok(format_memory_graph_edges(entity, direction, &edges))
        })
        .ok_or_else(|| mcp_error("Database not initialized"))?
        .map_err(|e| mcp_store_error("Memory graph query failed", e))?;
        return Ok(output);
    }

    let direction = params.direction.as_str();
    if !matches!(direction, "links" | "backlinks" | "neighbors" | "path") {
        return Err(mcp_error(format!(
            "Unknown direction '{direction}'. Use links, backlinks, neighbors, or path."
        )));
    }
    let to = if direction == "path" {
        Some(
            params
                .to
                .as_deref()
                .ok_or_else(|| mcp_error("direction=path requires 'to'"))?,
        )
    } else {
        None
    };
    let output = crate::core::run_guarded_read(&mut ctx_guard, "document graph query", |ctx| {
        Ok(match direction {
            "links" => {
                let doc = resolve_document(&ctx.conn, entity)?;
                let edges = graph::get_outgoing(&ctx.conn, doc.id, relation)?;
                let views = graph::edge_views(&ctx.conn, &edges)?;
                format_graph_edges(entity, "links", &views)
            }
            "backlinks" => {
                let edges = graph::get_incoming(&ctx.conn, entity, relation)?;
                let views = graph::edge_views(&ctx.conn, &edges)?;
                format_graph_edges(entity, "backlinks", &views)
            }
            "neighbors" => {
                let doc = resolve_document(&ctx.conn, entity)?;
                let nbrs = graph::neighbors(&ctx.conn, doc.id, relation, params.depth)?;
                format_graph_neighbors(entity, &nbrs)
            }
            "path" => {
                let to = to.expect("path destination was validated");
                let doc = resolve_document(&ctx.conn, entity)?;
                match graph::shortest_path(&ctx.conn, doc.id, to, GRAPH_MCP_MAX_HOPS)? {
                    Some(nodes) => format!("{}: {}", entity, nodes.join(" -> ")),
                    None => format!("No path from {entity} to {to}."),
                }
            }
            _ => unreachable!("direction was validated"),
        })
    })
    .ok_or_else(|| mcp_error("Database not initialized"))?
    .map_err(|e| mcp_store_error("Document graph query failed", e))?;
    Ok(output)
}

fn format_memory_graph_edges(
    entity: &str,
    label: &str,
    edges: &[crate::store::memory_graph::MemoryEdge],
) -> String {
    if edges.is_empty() {
        return format!("No {label} for {entity}.");
    }
    let mut out = format!("{label} for {entity}:");
    for e in edges {
        // links: show where this entry points; backlinks: show who points here.
        let other = if label == "links" {
            &e.target_ref
        } else {
            &e.source_id
        };
        out.push_str(&format!(
            "\n- {} (via {}, {})",
            other, e.relation, e.target_kind
        ));
    }
    out
}

fn format_graph_edges(
    entity: &str,
    label: &str,
    edges: &[crate::store::graph::EdgeView],
) -> String {
    if edges.is_empty() {
        return format!("No {label} for {entity}.");
    }
    let mut out = format!("{} {label} for {entity}:\n", edges.len());
    for e in edges {
        out.push_str(&format!(
            "  {} --{}--> {} ({})\n",
            e.source, e.relation, e.target_ref, e.source_kind
        ));
    }
    out
}

fn format_graph_neighbors(entity: &str, neighbors: &[crate::store::graph::Neighbor]) -> String {
    if neighbors.is_empty() {
        return format!("No neighbors for {entity}.");
    }
    let mut out = format!("{} neighbors of {entity}:\n", neighbors.len());
    for n in neighbors {
        out.push_str(&format!(
            "  {} (depth {}, via {})\n",
            n.entity,
            n.depth,
            n.via.join(", ")
        ));
    }
    out
}

/// `code_graph` output: prose for agents, resolved symbols for programmatic
/// callers.
///
/// Both come out of the same traversal. The prose is the agent-facing product
/// and stays exactly as it reads today; `symbols` exists so a client that wants
/// locations (an editor's "find references", say) never has to scrape the
/// prose back apart. Only the hook socket ships both — the MCP tool keeps
/// returning `text` alone.
#[derive(Debug)]
pub struct CodeGraphOutput {
    pub text: String,
    pub symbols: Vec<crate::code::symbol::Symbol>,
}

fn resolve_symbol(
    facade: &IndexFacade,
    name: &str,
    symbol_id: Option<u32>,
) -> Result<crate::code::symbol::Symbol, McpError> {
    if let Some(id) = symbol_id {
        let symbol_id = crate::code::types::SymbolId::new(id)
            .ok_or_else(|| mcp_error("Invalid symbol_id: 0 is reserved."))?;
        return facade
            .get_symbol(symbol_id)
            .ok_or_else(|| mcp_error(format!("Symbol not found: sym#{id}.")));
    }

    let matches = facade.find_symbols_by_name(name);
    match matches.len() {
        0 if name.len() >= 3 => {
            let fuzzy = facade.search_symbols(name, 10);
            match fuzzy.len() {
                0 => Err(mcp_error(format!("No symbol found: '{name}'."))),
                1 => Ok(fuzzy.into_iter().next().expect("one fuzzy symbol")),
                _ => Err(disambiguation_error(name, &fuzzy)),
            }
        }
        0 => Err(mcp_error(format!("No symbol found: '{name}'."))),
        1 => Ok(matches.into_iter().next().expect("one exact symbol")),
        _ => Err(disambiguation_error(name, &matches)),
    }
}

pub(super) fn disambiguation_error(
    name: &str,
    candidates: &[crate::code::symbol::Symbol],
) -> McpError {
    let mut message = format!("Multiple symbols match '{name}'. Pass symbol_id:\n");
    for symbol in candidates {
        let scope = match &symbol.scope_context {
            Some(crate::code::symbol::ScopeContext::ClassMember {
                class_name: Some(class_name),
            }) => format!(" [in {class_name}]"),
            Some(crate::code::symbol::ScopeContext::Local {
                parent_name: Some(parent_name),
                ..
            }) => format!(" [in {parent_name}]"),
            _ => String::new(),
        };
        let signature = symbol
            .signature
            .as_ref()
            .map(|value| format!(" `{}`", truncate_text(value.trim(), 60)))
            .unwrap_or_default();
        message.push_str(&format!(
            "  sym#{} - {:?} {} in {} ({}){}{}\n",
            symbol.id.value(),
            symbol.kind,
            symbol.name,
            symbol.file_path,
            symbol.range,
            scope,
            signature,
        ));
    }
    mcp_error(message)
}

/// `code_graph` — call graph queries. Resolves the symbol then dispatches by
/// direction (calls/callers/impact).
pub async fn code_graph_impl(
    handle: &RepoHandle,
    params: &CodeGraphParams,
) -> Result<CodeGraphOutput, McpError> {
    let idx_guard = acquire_handle_code_index(handle).await?;
    let Some(facade) = idx_guard.as_ref() else {
        return Ok(CodeGraphOutput {
            text: "Code index is being rebuilt, retry shortly.".to_string(),
            symbols: Vec::new(),
        });
    };

    let symbol = resolve_symbol(facade, &params.name, params.symbol_id)?;

    // How many entries got here on the strength of a bare name alone, and — for
    // `impact` — how many of those the walk refused to continue through.
    // Counted here because the tier is what the walk returns; reporting it is
    // the useful statement on this side, where every edge is resolved by
    // construction and a `CallTarget` would say `Resolved` every time.
    let mut unplaced_arrivals = 0usize;
    let mut stopped_arrivals = 0usize;
    let mut call_evidence = Vec::new();
    let hits: Vec<crate::code::symbol::Symbol> = match params.direction.as_str() {
        "calls" => {
            let (calls, _) = crate::core::code::classify_calls(facade, symbol.id);
            calls
                .into_iter()
                .map(|call| {
                    call_evidence.push((call.tier, call.is_unique));
                    call.symbol
                })
                .collect()
        }
        "callers" => facade
            .get_callers_by_tier(symbol.id)
            .into_iter()
            .map(|(s, tier)| {
                unplaced_arrivals += usize::from(tier == crate::code::storage::TIER_UNPLACED);
                s
            })
            .collect(),
        "impact" => {
            let radius = facade.get_impact_by_tier(symbol.id, params.max_depth);
            stopped_arrivals = radius.stopped_arrivals;
            // The ambiguous arrivals go last, under their own heading: the two
            // groups answer different questions and a single list merges them.
            let mut reached = radius.reached;
            reached.sort_by_key(|(_, tier)| *tier == crate::code::storage::TIER_UNPLACED);
            reached
                .into_iter()
                .map(|(s, tier)| {
                    unplaced_arrivals += usize::from(tier == crate::code::storage::TIER_UNPLACED);
                    s
                })
                .collect()
        }
        _ => {
            return Err(mcp_error(format!(
                "Invalid direction: '{}'. Valid: calls, callers, impact.",
                params.direction
            )));
        }
    };

    let symbols = hits.clone();

    // What the call graph could not place, for the `calls` direction only —
    // it is the direction that has targets at all. Without this an answer of
    // "does not call any indexed functions" reads as "calls nothing", which is
    // wrong for the majority of symbols: most of what code calls lives in
    // another crate or on a receiver whose type is not indexed.
    let unplaced = if params.direction == "calls" {
        unplaced_calls(facade, symbol.id)
    } else {
        crate::core::code::UnplacedCalls::default()
    };

    if hits.is_empty() {
        let text = match params.direction.as_str() {
            "calls" => format!(
                "{} ({:?}) calls no indexed function.{}{}",
                symbol.name,
                symbol.kind,
                unplaced_suffix(&unplaced),
                receiver_inference_note(&symbol.file_path, &unplaced, &call_evidence)
            ),
            "callers" => format!(
                "{} ({:?}) has no indexed callers.",
                symbol.name, symbol.kind
            ),
            _ => format!(
                "{} ({:?}) has no reachable symbols within {} hop(s).",
                symbol.name, symbol.kind, params.max_depth
            ),
        };
        return Ok(CodeGraphOutput { text, symbols });
    }

    let mut text = match params.direction.as_str() {
        "calls" => format!(
            "{} ({:?}) calls {} indexed function(s).{}{}\n\n",
            symbol.name,
            symbol.kind,
            hits.len(),
            unplaced_suffix(&unplaced),
            receiver_inference_note(&symbol.file_path, &unplaced, &call_evidence)
        ),
        "callers" => format!(
            "{} ({:?}) is called by {} function(s).{}\n\n",
            symbol.name,
            symbol.kind,
            hits.len(),
            unplaced_suffix_arrivals(unplaced_arrivals, &symbol.name)
        ),
        _ => format!(
            "Impact radius for {} ({:?}): {} symbol(s) within {} hop(s).{}\n\n",
            symbol.name,
            symbol.kind,
            hits.len() - unplaced_arrivals,
            params.max_depth,
            impact_coverage_note(unplaced_arrivals, stopped_arrivals, &symbol.name)
        ),
    };
    // Where the placed arrivals end and the ambiguous frontier begins. Only
    // `impact` sorts its hits this way; the other directions never split.
    let frontier_starts_at = hits.len() - unplaced_arrivals;
    for (index, sym) in hits.iter().enumerate() {
        if params.direction == "impact" && index == frontier_starts_at {
            text.push_str(
                "Ambiguous frontier — reached by an unqualified call, so the walk stopped \
                 here rather than report what they call:\n\n",
            );
        }
        text.push_str(&format_symbol(sym));
        if let Some((tier, is_unique)) = call_evidence.get(index) {
            text.push_str(&format!(
                "    Resolution: tier {tier}, {}\n",
                if *is_unique {
                    "unique"
                } else {
                    "candidate list"
                }
            ));
        }
        text.push('\n');
    }

    Ok(CodeGraphOutput { text, symbols })
}

/// How far to trust a callers or impact list: the entries that arrived through
/// a call no rule could place name this symbol only because they name *a*
/// symbol of this name.
fn unplaced_suffix_arrivals(unplaced: usize, name: &str) -> String {
    if unplaced == 0 {
        return String::new();
    }
    format!(" {unplaced} arrived through an unqualified call and may belong to another `{name}`.")
}

/// How far an impact radius goes, and where it stopped.
///
/// Two separate facts, and the second is the one a list cannot show. How many
/// entries are here on a name alone says how much of the list to distrust; how
/// many of those the walk refused to continue through says the list is short by
/// their subtrees. Without the second, a truncated radius reads as exhaustive
/// and a real caller two hops out is never mentioned at all.
fn impact_coverage_note(ambiguous: usize, stopped: usize, name: &str) -> String {
    if ambiguous == 0 {
        return String::new();
    }
    let mut note = format!(
        " {ambiguous} more arrived through an unqualified call and may belong to another \
         `{name}`; they are listed apart."
    );
    if stopped > 0 {
        note.push_str(&format!(
            " The walk stopped at {stopped} of them, so the radius is short by whatever \
             those call."
        ));
    }
    note
}

/// The calls of `symbol_id` that no rule placed on an indexed symbol.
fn unplaced_calls(
    facade: &crate::code::indexing::IndexFacade,
    symbol_id: crate::code::types::SymbolId,
) -> crate::core::code::UnplacedCalls {
    let (_, unplaced) = crate::core::code::classify_calls(facade, symbol_id);
    unplaced
}

/// One sentence naming what fell outside the index, empty when nothing did.
///
/// Names the external targets rather than only counting them: knowing a symbol
/// calls `std::fs::write` is what tells Claude to stop looking for it here.
fn unplaced_suffix(unplaced: &crate::core::code::UnplacedCalls) -> String {
    let mut parts = Vec::new();
    if !unplaced.external.is_empty() {
        parts.push(format!(
            "{} outside this index ({})",
            unplaced.external.len(),
            unplaced.external.join(", ")
        ));
    }
    if !unplaced.unknown.is_empty() {
        parts.push(format!(
            "{} on receivers of unindexed type",
            unplaced.unknown.len()
        ));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" Also calls {}.", parts.join(", "))
    }
}

/// Why a non-Rust `calls` answer is weaker than a Rust one, empty when the
/// distinction cannot bite.
///
/// Call-site receiver-type inference exists for Rust only:
/// `src/code/parsing/rust/parser.rs` is the sole producer of
/// `Call::receiver_type`, and every other parser leaves it `None`. So for the
/// other 13 indexed languages a method call carries no receiver type, and the
/// cascade has nothing to place it with beyond the written name — which is the
/// unplaced tier, or no candidate at all. Without this sentence a TypeScript
/// answer reads as authoritative as a Rust one.
///
/// Emitted only when the weakness is actually present: a non-Rust symbol whose
/// calls all resolved at a near tier through a written qualifier is as
/// trustworthy as a Rust one, and the note would be noise.
fn receiver_inference_note(
    file_path: &str,
    unplaced: &crate::core::code::UnplacedCalls,
    tiers: &[(i64, bool)],
) -> String {
    use crate::code::parsing::language::Language;

    // Extension only, never `Language::from_path`: that one falls back to
    // reading the file for a shebang, and this runs on an MCP hot path.
    let language = std::path::Path::new(file_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(Language::from_extension);
    if language == Some(Language::Rust) {
        return String::new();
    }

    let by_name_only = !unplaced.unknown.is_empty()
        || tiers
            .iter()
            .any(|(tier, _)| *tier == crate::code::storage::TIER_UNPLACED);
    if !by_name_only {
        return String::new();
    }

    " Receiver-type inference runs for Rust only, so a method call in this file \
     resolves on its written name alone: confirm the receiver's type in the \
     source before trusting a match."
        .to_string()
}

/// `usage` — token economy audit. Reads session + lifetime stats and returns
/// a JSON-formatted string. `session_id` is the daemon-global current session.
pub async fn usage_impl(
    handle: &RepoHandle,
    params: &UsageParams,
    session_id: i64,
) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    let mut ctx_guard = handle.ctx.lock().await;
    let (session, session_tool_usage, lifetime, lifetime_tool_usage) =
        crate::core::run_guarded_read(&mut ctx_guard, "usage report", |ctx| {
            let session = if session_id > 0 {
                stats::get_session(&ctx.conn, session_id)?
            } else {
                None
            };
            let session_tool_usage = if session_id > 0 {
                stats::get_tool_usage(&ctx.conn, session_id)?
            } else {
                Vec::new()
            };
            let lifetime = if params.session_only {
                None
            } else {
                Some(stats::get_aggregate_stats(&ctx.conn)?)
            };
            let lifetime_tool_usage = if params.session_only {
                Vec::new()
            } else {
                stats::get_aggregate_tool_usage(&ctx.conn)?
            };
            Ok((session, session_tool_usage, lifetime, lifetime_tool_usage))
        })
        .ok_or_else(|| mcp_error("Database not initialized"))?
        .map_err(|e| mcp_error(format!("Failed to read usage: {e}")))?;

    let primary_tools = if params.session_only {
        &session_tool_usage
    } else {
        &lifetime_tool_usage
    };
    let mut top_sorted: Vec<&stats::ToolUsageRecord> = primary_tools.iter().collect();
    top_sorted.sort_by_key(|r| std::cmp::Reverse(r.call_count));
    let top_5_most_called: Vec<Value> = top_sorted
        .iter()
        .take(5)
        .map(|r| {
            json!({
                "tool_name": r.tool_name,
                "call_count": r.call_count,
            })
        })
        .collect();

    let per_tool: Vec<Value> = session_tool_usage
        .iter()
        .map(|r| {
            json!({
                "tool_name": r.tool_name,
                "call_count": r.call_count,
                "total_tokens": r.total_tokens,
                "total_results": r.total_results,
            })
        })
        .collect();

    let session_json = session.as_ref().map(|s| {
        json!({
            "id": s.id,
            "total_calls": s.total_calls,
            "total_tokens": s.total_tokens,
            "truncations": s.truncation_count,
        })
    });

    let mut out = json!({
        "session": session_json,
        "per_tool": per_tool,
        "top_5_most_called": top_5_most_called,
    });

    if let Some(l) = lifetime {
        out["lifetime"] = json!({
            "total_sessions": l.total_sessions,
            "total_calls": l.total_calls,
            "total_tokens": l.total_tokens,
            "truncations": l.total_truncations,
            "avg_tokens_per_call": l.avg_tokens_per_call,
        });
        let lifetime_per_tool: Vec<Value> = lifetime_tool_usage
            .iter()
            .map(|r| {
                json!({
                    "tool_name": r.tool_name,
                    "call_count": r.call_count,
                    "total_tokens": r.total_tokens,
                    "total_results": r.total_results,
                })
            })
            .collect();
        out["lifetime_per_tool"] = Value::Array(lifetime_per_tool);
    }

    serde_json::to_string_pretty(&out)
        .map_err(|e| mcp_error(format!("Failed to serialize usage: {e}")))
}

// ── Hook dispatch impls ───────────────────────────────────────────────────────
// These return raw hook envelopes (hookSpecificOutput) rather than {text:...}.
// Called from dispatch_call "hook.*" arms and from hook_client's no-daemon path.

/// Hook log files (`hook-events.jsonl`, `hook-slow.jsonl`) are rotated once they
/// exceed this size: the oldest half of the lines is dropped on the next append,
/// keeping the log bounded without an external logrotate.
pub const HOOK_LOG_CAP_BYTES: u64 = 1024 * 1024; // 1 MiB

/// Append `line` (which must already end in `\n`) to `path`. If the file exceeds
/// [`HOOK_LOG_CAP_BYTES`], the oldest half of its lines is dropped first so the
/// newest history is retained. Best-effort — I/O errors are swallowed.
pub fn append_hook_log(path: &std::path::Path, line: &str) {
    use std::io::Write as _;

    if std::fs::metadata(path)
        .map(|m| m.len() > HOOK_LOG_CAP_BYTES)
        .unwrap_or(false)
    {
        if let Ok(content) = std::fs::read_to_string(path) {
            let lines: Vec<&str> = content.lines().collect();
            // Keep the newest half (drop the older half).
            let kept = lines[lines.len() / 2..].join("\n");
            let rewritten = if kept.is_empty() {
                String::new()
            } else {
                format!("{kept}\n")
            };
            let _ = std::fs::write(path, rewritten);
        }
    }

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Append one line to `hook-events.jsonl` in the store the hook ran against;
/// also `hook-slow.jsonl` when elapsed exceeds the configured budget. The store,
/// not `.mdkb/` — a namespaced process writes nothing outside its namespace,
/// telemetry included. Best-effort — silently drops on I/O failure. Designed to
/// run inside `spawn_blocking`.
fn log_hook_event(
    root: std::path::PathBuf,
    event: &str,
    outcome: &str,
    elapsed_ms: u64,
    slow_threshold_ms: u64,
) {
    log_hook_event_with_reason(root, event, outcome, None, elapsed_ms, slow_threshold_ms);
}

/// [`log_hook_event`] plus the `reason` an outcome carries, omitted when there
/// is none. An outcome without its reason is what made the mining outage
/// unreadable: `failed` alone cannot be told from a distiller that is missing,
/// one that refuses the request, and one that answers with prose.
fn log_hook_event_with_reason(
    root: std::path::PathBuf,
    event: &str,
    outcome: &str,
    reason: Option<&str>,
    elapsed_ms: u64,
    slow_threshold_ms: u64,
) {
    let ts = chrono::Utc::now().timestamp();
    let mut payload = serde_json::json!({
        "ts": ts,
        "event": event,
        "outcome": outcome,
        "elapsed_ms": elapsed_ms,
    });
    if let Some(reason) = reason {
        payload["reason"] = serde_json::json!(reason);
    }
    let mut line = payload.to_string();
    line.push('\n');
    let mdkb_dir = crate::store::namespace::store_dir(&root).unwrap_or_else(|_| root.join(".mdkb"));
    append_hook_log(&mdkb_dir.join("hook-events.jsonl"), &line);
    if elapsed_ms > slow_threshold_ms {
        append_hook_log(&mdkb_dir.join("hook-slow.jsonl"), &line);
    }
}

/// The project a session is working in, as a token the warmup selectors match
/// against entry tags. `None` = unscoped, and every caller must then behave
/// exactly as it did before scoping existed.
///
/// One `.mdkb` store routinely anchors a whole family of sibling projects, so
/// the store root cannot identify the project — but the first path segment
/// below it can. That segment is only trusted when a collection of that name is
/// registered: collections are created one per subproject, which makes them the
/// store's own statement of "these folders are projects" and keeps a stray
/// `scratch/` or `tmp/` from inventing a scope nobody tagged entries with.
///
/// Deliberately NOT derived from `source_path`: it is populated on a small
/// minority of entries and points at the writing tool's directory, not at the
/// project. Tags are populated and do discriminate — see [`entry_in_scope`].
///
/// The returned token is lowercased so tag matching has a single form.
fn project_scope_token(
    root: &std::path::Path,
    cwd: Option<&std::path::Path>,
    collection_names: &[String],
) -> Option<String> {
    let relative = cwd?.strip_prefix(root).ok()?;
    let segment = match relative.components().next()? {
        std::path::Component::Normal(s) => s.to_str()?,
        _ => return None,
    };
    collection_names
        .iter()
        .any(|name| name.eq_ignore_ascii_case(segment))
        .then(|| segment.to_lowercase())
}

/// True when `entry` belongs to the project named by `token` (lowercased by
/// [`project_scope_token`]). An entry with no matching tag is out of scope, not
/// unwanted: cross-cutting knowledge is demoted in ranking, never filtered.
fn entry_in_scope(entry: &crate::store::memory::MemoryEntry, token: &str) -> bool {
    entry.tags.iter().any(|tag| tag.to_lowercase() == token)
}

/// Minimum confidence for a prior to be treated as "curated" — the threshold
/// the recall gate and the warmup reserved-prior slot both key off.
const PRIOR_CONFIDENCE_GATE: f64 = 0.7;

/// Drop the scores now that nothing else decides admission.
///
/// There used to be a `min_recall_score` floor here, over
/// `rrf_norm * 0.7 + confidence * 0.3`. It could not do the job it was named
/// for: `rrf_norm` is max-normalized, so the best candidate for any prompt
/// scores 1.0 and the sum clears any sane floor — and the confidence term let
/// a well-confirmed entry about something else buy its way in. The gate is now
/// absolute and lives in `search_entries_hybrid_fts`
/// (`search.memory.min_recall_cosine`), before normalization. Confidence is
/// back to what it is good for: ordering what was admitted.
///
/// One thing is still decided here: a disputed entry — one whose last signal was
/// a refutation — is never injected, whatever it scored. Confidence alone cannot
/// do that job, because an entry confirmed twenty times and refuted once still
/// scores 0.84. The score measures how well an entry is believed; being told it
/// is wrong is a different fact, and unasked injection is the one surface where
/// it has to win. The entry stays searchable: an explicit `memory search` still
/// returns it, so a refutation hides nothing from someone who asks.
fn injectable(entries: Vec<memory::ScoredMemoryEntry>) -> Vec<memory::MemoryEntry> {
    entries
        .into_iter()
        .map(|result| result.entry)
        .filter(|entry| !entry.is_disputed())
        .collect()
}

/// True when `entry` is a `Prior` whose confidence at `now` clears the gate.
fn is_high_confidence_prior(entry: &crate::store::memory::MemoryEntry, now: i64) -> bool {
    entry.entry_type == crate::store::memory::EntryType::Prior
        && entry.confidence_at(now) >= PRIOR_CONFIDENCE_GATE
}

/// Rank merged warmup entries: drop sub-floor entries (when `min_confidence`
/// > 0.0), order by `access_count DESC` with `confidence_at` as tie-breaker,
/// > reserve at most one slot for the single highest-confidence curated prior
/// > (confidence >= gate) so it can appear without crowding out hot entries, then
/// > truncate to `limit`.
/// > A handoff whose stripped body is shorter than this is treated as "empty" (an
/// > auto-handoff that captured no real state): its body is not injected as the
/// > session-start handoff block.
const HANDOFF_MIN_BODY_CHARS: usize = 80;

fn rank_warmup_entries(
    mut entries: Vec<crate::store::memory::MemoryEntry>,
    limit: usize,
    min_confidence: f64,
    now: i64,
    scope: Option<&str>,
) -> Vec<crate::store::memory::MemoryEntry> {
    // Handoffs never appear here: the pool query admits only durable types and
    // priors, the newest handoff is injected in full as a body block by
    // hook_session_start_impl, and the caller strips all handoffs before
    // ranking. This operates purely on topic/problem/decision/prior entries,
    // so a truncated handoff title-line can never crowd the list.
    if min_confidence > 0.0 {
        entries.retain(|e| e.confidence_at(now) >= min_confidence);
    }

    // Same rule as the recall path (see `injectable`): an entry whose last
    // signal was a refutation is not warmed up. The pool query already drops
    // net-refuted entries in SQL; this catches the ones a high confirmation
    // count still carries, where the score would happily inject them.
    entries.retain(|e| !e.is_disputed());

    // Project affinity is a BIAS, never a filter: an out-of-scope entry is
    // demoted below every in-scope one but still emitted while budget remains,
    // so cross-cutting knowledge (tagged for no project at all) keeps reaching
    // every session. With no scope every entry scores 0 and the comparator
    // collapses to the pre-scoping one: access_count DESC, confidence tie-break.
    let affinity = |e: &crate::store::memory::MemoryEntry| {
        u8::from(scope.is_some_and(|token| entry_in_scope(e, token)))
    };
    entries.sort_by(|a, b| {
        affinity(b)
            .cmp(&affinity(a))
            .then_with(|| b.access_count.cmp(&a.access_count))
            .then_with(|| {
                b.confidence_at(now)
                    .partial_cmp(&a.confidence_at(now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    if entries.len() <= limit {
        return entries;
    }

    // Reserve one tail slot for the single highest-confidence curated prior: if
    // it would be truncated away, swap it into the last kept slot so a curated
    // prior surfaces without displacing more than one hot entry.
    let best_prior = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| is_high_confidence_prior(e, now))
        .max_by(|(_, a), (_, b)| {
            a.confidence_at(now)
                .partial_cmp(&b.confidence_at(now))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(idx, _)| idx);
    if let Some(idx) = best_prior {
        if idx >= limit {
            entries.swap(limit - 1, idx);
        }
    }

    entries.truncate(limit);
    entries
}

/// RAII guard that clears an in-flight `AtomicBool` on drop — including on a
/// panic unwind. A reindex task takes its resource out of a mutex and only
/// restores it (and clears the flag) after the blocking work returns; if that
/// work panics, the naive code leaves the flag stuck `true` and the resource
/// `None` forever, wedging the handle until a daemon restart (ARCH-A1). This
/// generalizes the local `FlightGuard`.
pub(crate) struct ActiveFlagGuard(Arc<AtomicBool>);

impl ActiveFlagGuard {
    /// Mark the flag active, returning the guard. `None` if another holder is
    /// already active (the flag was already `true`).
    pub(crate) fn arm(flag: Arc<AtomicBool>) -> Option<Self> {
        if flag
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }
        Some(Self(flag))
    }

    /// Wrap a flag the caller already set to `true`, guaranteeing it is cleared
    /// on drop (including panic).
    pub(crate) fn from_armed(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }
}

impl Drop for ActiveFlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

fn schedule_code_index_refresh(handle: &RepoHandle) -> bool {
    if !handle.config.code.enabled {
        return false;
    }
    let Some(active_guard) = ActiveFlagGuard::arm(Arc::clone(&handle.code_reindex_active)) else {
        return true;
    };

    let root = handle.root.clone();
    let code_index = Arc::clone(&handle.code_index);
    let ignore_patterns = handle.code_ignore_patterns.clone();
    let respect_gitignore = handle.config.code.indexing.respect_gitignore;

    tokio::spawn(async move {
        // Clears code_reindex_active on ANY exit, including a panic below.
        let _active_guard = active_guard;

        let mut idx_guard = code_index.lock().await;
        if idx_guard.is_none() {
            let index_path = root.join(".mdkb/code.sqlite");
            match IndexFacade::open_or_create(&index_path) {
                Ok(facade) => {
                    let pipeline_config = crate::code::indexing::pipeline::PipelineConfig {
                        ignore_patterns: ignore_patterns.clone(),
                        respect_gitignore,
                        ..Default::default()
                    };
                    *idx_guard = Some(facade.with_config(pipeline_config));
                }
                Err(e) => {
                    tracing::error!("SessionStart code refresh: failed to open code index: {e}");
                    return;
                }
            }
        }

        let Some(mut facade) = idx_guard.take() else {
            return;
        };
        drop(idx_guard);

        // Content-hash incremental refresh (full build only on empty index).
        // Catch a panic so the facade is always restored to the mutex — a lost
        // facade would leave code_index None (recoverable, but the flag+resource
        // guard keeps the handle usable regardless).
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| facade.update(&root)));
        crate::llm::release_cached_service();
        match outcome {
            Ok(Ok(stats)) => {
                tracing::info!(
                    "SessionStart code refresh: {} files, {} symbols",
                    stats.files_indexed,
                    stats.symbols_indexed
                );
            }
            Ok(Err(e)) => tracing::error!("SessionStart code refresh failed: {e}"),
            Err(_) => {
                tracing::error!("SessionStart code refresh panicked; restoring index handle");
            }
        }

        let mut idx_guard = code_index.lock().await;
        *idx_guard = Some(facade);
        // _active_guard drops here (or on the early returns / panic) → flag cleared.
    });

    true
}

/// Split the newest handoff's body out of the warmup entry set.
///
/// Returns the frontmatter-stripped body of the single most-recent handoff (the
/// session-start "where did I leave off" anchor, injected in full) and the
/// remaining non-handoff entries for the compact list. ALL handoffs are dropped
/// from the returned entries so a truncated handoff title-line never surfaces.
/// The body is `None` when there is no handoff, or the newest one is effectively
/// empty (an auto-handoff whose stripped body is shorter than
/// [`HANDOFF_MIN_BODY_CHARS`]).
///
/// With a `scope` token the candidate set narrows to handoffs tagged for that
/// project, and an empty candidate set injects NOTHING: a handoff is verbatim
/// session state, so another project's is actively misleading — worse than
/// starting with no anchor at all. Unscoped, the rule is unchanged: newest wins.
/// Extract `anchor`'s body and drop EVERY handoff from the compact list.
///
/// The anchor is passed in rather than chosen here, because it is now selected
/// by its own query (`memory::newest_handoff_for_scope`) instead of from the
/// `access_count`-ranked pool — a handoff's access_count is 0 or 1 by
/// construction, so it could never win that race (story 009-686d).
///
/// Handoffs are dropped from the list whether or not one was chosen: a 50-char
/// truncated handoff title-line is useless for context restoration and would
/// only crowd out an entry that is not.
fn strip_handoffs(
    entries: Vec<crate::store::memory::MemoryEntry>,
    anchor: Option<&crate::store::memory::MemoryEntry>,
) -> (Option<String>, Vec<crate::store::memory::MemoryEntry>) {
    use crate::store::memory::{EntryType, strip_frontmatter};
    let body = anchor.and_then(|e| {
        let stripped = strip_frontmatter(&e.content).trim().to_string();
        (stripped.chars().count() >= HANDOFF_MIN_BODY_CHARS).then_some(stripped)
    });
    let rest = entries
        .into_iter()
        .filter(|e| e.entry_type != EntryType::Handoff)
        .collect();
    (body, rest)
}

/// Test seam for the stripping stage, so the handoff-injection scenarios can be
/// asserted against the same function production calls.
#[doc(hidden)]
pub fn strip_handoffs_for_test(
    entries: Vec<crate::store::memory::MemoryEntry>,
    anchor: Option<&crate::store::memory::MemoryEntry>,
) -> (Option<String>, Vec<crate::store::memory::MemoryEntry>) {
    strip_handoffs(entries, anchor)
}

/// Test seam for the ranking stage, so a test can assert that widening the
/// candidate pool does not widen the emitted list.
#[doc(hidden)]
pub fn rank_warmup_entries_for_test(
    entries: Vec<crate::store::memory::MemoryEntry>,
    limit: usize,
    min_confidence: f64,
    now: i64,
    scope: Option<&str>,
) -> Vec<crate::store::memory::MemoryEntry> {
    rank_warmup_entries(entries, limit, min_confidence, now, scope)
}

/// Select warmup lines that fit within `budget` tokens (real tiktoken count,
/// not a chars/4 estimate). The first line always emits — a single over-budget
/// line still beats an empty warmup list; thereafter stop before the first line
/// that would exceed the budget (never truncate mid-line).
fn fit_warmup_lines(lines: &[String], budget: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for line in lines {
        let cost = crate::metrics::tokens::count_tokens(line);
        if !out.is_empty() && used + cost > budget {
            break;
        }
        out.push(line.clone());
        used += cost;
    }
    out
}

/// A loud, non-silent SessionStart banner for any outstanding autoheal
/// quarantine — one line per corrupt file still on disk. `None` when the store
/// is healthy (the common case), so it costs nothing on a clean warmup.
///
/// `doc_count` is the CURRENT `documents` row count, checked so the banner
/// doesn't keep telling the operator to run `mdkb update` after the daemon's
/// own post-heal `full_rebuild_from_heal` has already repopulated it —
/// otherwise the instruction reads as stale/wrong the moment recovery
/// actually succeeds automatically.
fn format_quarantine_banner(mdkb_dir: &std::path::Path, doc_count: i64) -> Option<String> {
    let reports = crate::store::heal::quarantine_reports(mdkb_dir);
    if reports.is_empty() {
        return None;
    }
    let docs_status = if doc_count > 0 {
        format!(
            "Docs already re-indexed automatically ({doc_count} currently indexed) — no action needed."
        )
    } else {
        "Docs not yet re-indexed — run `mdkb update`.".to_string()
    };
    let mut out = String::new();
    for r in reports {
        let date = chrono::DateTime::from_timestamp(r.quarantined_at, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "unknown date".to_string());
        out.push_str(&format!(
            "⚠️ mdkb index was CORRUPT and quarantined ({date}): salvaged {} memory entries, {} edges. {docs_status} Remove `.mdkb/{}` once verified to clear this warning.\n",
            r.memory_entries_salvaged, r.memory_edges_salvaged, r.corrupt_file
        ));
    }
    Some(out)
}

/// A one-line warning when the entry projection and the database disagree on
/// how many entries exist.
///
/// Story 015-2dc2: 387 files drifted from the database for weeks, 265 of them
/// carrying unique knowledge, and were found by accident. The only place the
/// number had ever appeared was the output of an `mdkb update` nobody re-read.
/// Session start is where an agent actually looks.
///
/// Deliberately a smoke signal, not a diagnosis: it counts, it does not parse.
/// Classifying each file (unreadable vs importable vs orphaned) means reading
/// all of them, which belongs in `mdkb stats` — a command someone chose to run
/// — not on a hook that fires every session against thousands of files. Equal
/// counts can still hide offsetting drift, which is exactly why this points at
/// the command that checks properly instead of claiming the store is healthy.
fn format_projection_drift_banner(ctx: &crate::core::Context) -> crate::Result<Option<String>> {
    let (files, rows) = crate::core::memory_sync::projection_file_and_row_counts(ctx)?;
    if files == rows {
        return Ok(None);
    }
    Ok(Some(format!(
        "⚠️ mdkb memory projection out of sync: {files} entry file(s) on disk vs {rows} \
         active database row(s). Run `mdkb stats` for the breakdown, then `mdkb memory sync`.\n"
    )))
}

/// `session_cwd` is the validated session working directory (see
/// [`hook_session_cwd`]) — the only signal that says which project inside a
/// multi-project store this session belongs to. `None` means unscoped: every
/// selection below then behaves exactly as it did before scoping existed.
pub async fn hook_session_start_impl(
    handle: &Arc<RepoHandle>,
    session_cwd: Option<&std::path::Path>,
) -> Value {
    let cfg = &handle.config.hooks;
    if !cfg.session_start_enabled {
        return json!({});
    }
    if !handle.root.join(".mdkb").is_dir() {
        return json!({});
    }
    if ensure_handle_context(handle).await.is_err() {
        return json!({});
    }
    let limit = cfg.warmup_limit.max(1);
    let mut ctx_guard = handle.ctx.lock().await;
    let startup_data =
        crate::core::run_guarded_read(&mut ctx_guard, "hook session warmup", |ctx| {
            let (due_lines, entries) = get_warmup_entries(&ctx.conn, limit)?;
            let doc_count = crate::store::documents::count_documents(&ctx.conn)?;
            let collection_names: Vec<String> = collections::list_collections(&ctx.conn)?
                .into_iter()
                .map(|c| c.name)
                .collect();
            let drift_banner = format_projection_drift_banner(ctx)?;
            // The store this session opened — in a namespace, not `.mdkb/`.
            // The quarantine banner must describe it, not the default store.
            let store_dir = ctx
                .db_path
                .parent()
                .map_or_else(|| handle.root.join(".mdkb"), std::path::Path::to_path_buf);
            Ok((
                due_lines,
                entries,
                doc_count,
                collection_names,
                drift_banner,
                store_dir,
            ))
        });
    let (due_lines, entries, doc_count, collection_names, drift_banner, store_dir) =
        match startup_data {
            Some(Ok(data)) => data,
            Some(Err(error)) => {
                tracing::warn!("hook.session_start warmup failed: {error}");
                return json!({});
            }
            None => return json!({}),
        };
    drop(ctx_guard);

    let scope = project_scope_token(&handle.root, session_cwd, &collection_names);

    // Data-loss banner: surface any outstanding autoheal quarantine loudly,
    // computed before the empty-warmup early return so a freshly-rebuilt (empty)
    // store still gets the warning instead of silence.
    let quarantine_banner = format_quarantine_banner(&store_dir, doc_count);

    // mdkb owns handoff injection: pull the newest handoff's full body out for a
    // dedicated block and drop ALL handoffs from the ranked compact list — a
    // 50-char truncated handoff title-line is useless for context restoration.
    // Scoped, the newest handoff FOR THIS PROJECT is the anchor; with none, no
    // handoff block at all rather than another project's session state.
    // The anchor comes from its own query, not from the access_count-ranked pool
    // above: a handoff is written once and read once, so its access_count is 0
    // or 1 and it structurally loses that ordering to every warm topic. Selecting
    // it here is what stops a scoped session from correctly refusing a foreign
    // handoff and then silently getting none (story 009-686d).
    let anchor = {
        let mut ctx_guard = handle.ctx.lock().await;
        match crate::core::run_guarded_read(&mut ctx_guard, "hook handoff lookup", |ctx| {
            crate::store::memory::newest_handoff_for_scope(&ctx.conn, scope.as_deref())
        }) {
            Some(Ok(anchor)) => anchor,
            Some(Err(error)) => {
                tracing::warn!("hook.session_start handoff lookup failed: {error}");
                return json!({});
            }
            None => return json!({}),
        }
    };
    let (handoff_body, entries) = strip_handoffs(entries, anchor.as_ref());

    // Rank: confidence floor (off at 0.0), project affinity first when a scope
    // resolved, then access_count DESC with confidence tie-break, one reserved
    // slot for the top curated prior.
    let now = chrono::Utc::now().timestamp();
    let ranked = rank_warmup_entries(
        entries,
        limit,
        cfg.warmup_min_confidence,
        now,
        scope.as_deref(),
    );

    // Due reminders lead (preserved verbatim); ranked entries follow.
    let mut lines = due_lines;

    // STALE-DEP marker (read-only): flag ranked entries whose derived_from/supports
    // dependency is superseded or net-refuted in the primary store. Never mutates
    // stored confidence.
    let mut stale_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    if ensure_handle_context(handle).await.is_ok() {
        let mut ctx_guard = handle.ctx.lock().await;
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        match crate::core::run_guarded_read(&mut ctx_guard, "hook stale dependencies", |ctx| {
            memory_graph::stale_dependency_ids(&ctx.conn, &ids)
        }) {
            Some(Ok(ids)) => stale_ids = ids,
            Some(Err(error)) => tracing::warn!("hook stale dependency lookup failed: {error}"),
            None => {}
        }
    }
    lines.extend(ranked.iter().map(|e| {
        let prefix = if stale_ids.contains(&e.id) {
            "[STALE-DEP] "
        } else {
            ""
        };
        format!("{}{}", prefix, crate::store::memory::format_warmup_line(e))
    }));

    // Drain any prior-session pending memory embeddings in the background. Placed
    // here — after the function's last await — so it fires independently of
    // whether warmup produced any output (a repo
    // with pending embeddings but an empty/filtered warmup still gets drained).
    // Single-flight + best-effort; the ctx lock is already released.
    spawn_embedding_backfill(Arc::clone(handle));

    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "mdkb".to_string());

    // The newest handoff body is injected in full — it IS the session-restoration
    // anchor — exempt from the compact-list token budget.
    let mut body = String::new();
    if let Some(banner) = &quarantine_banner {
        body.push_str(banner);
        body.push('\n');
    }
    if let Some(banner) = &drift_banner {
        body.push_str(banner);
        body.push('\n');
    }
    if let Some(hb) = &handoff_body {
        body.push_str("## Last session handoff\n\n");
        body.push_str(hb);
        body.push_str("\n\n");
    }

    // Emit compact lines until the token budget (≈4 chars/token) would be
    // exceeded. Never truncate a line mid-way — stop before it — so every emitted
    // line keeps its id+type+title+tags. The first line always emits (a single
    // over-budget line still beats an empty list).
    if !lines.is_empty() {
        body.push_str("## mdkb memory warmup\n\n");
        for line in fit_warmup_lines(&lines, cfg.warmup_token_budget) {
            body.push_str("- ");
            body.push_str(&line);
            body.push('\n');
        }
    }
    body.push_str(&format!(
        "\n**mdkb:** `* query` = recall | `{bin} cheatsheet` = search/code/graph/audit/memory\n"
    ));

    // Check code index staleness. If stale, kick a detached refresh instead of
    // asking the user to run a manual command from a latency-sensitive hook.
    let code_db = handle.root.join(".mdkb/code.sqlite");
    if handle.config.code.enabled && code_db.exists() {
        if let Ok(conn) = rusqlite::Connection::open_with_flags(
            &code_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            let last = crate::code::storage::schema::last_index_scan_at(&conn).unwrap_or(None);
            if let Some(ts) = last {
                let now = chrono::Utc::now().timestamp();
                let age_days = (now - ts) / 86_400;
                if age_days >= 7 {
                    if schedule_code_index_refresh(handle) {
                        body.push_str(&format!(
                            "\n**⚠️ Code index is {age_days} days stale; refreshing in background.** Retry code lookups shortly.\n"
                        ));
                    } else {
                        body.push_str(&format!(
                            "\n**⚠️ Code index is {age_days} days stale.** Run `{bin} code index` to refresh.\n"
                        ));
                    }
                }
            }
        }
    }

    json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": body,
        }
    })
}

/// Post-recall 1-hop expansion: for the top recalled seeds, surface active memory
/// neighbors reachable by an outgoing edge, formatted `- [id] title (via relation)`.
/// Capped at `EXPAND_RECALL_SEEDS` seeds and `EXPAND_RECALL_NEIGHBORS` neighbors
/// total; already-recalled ids are skipped and superseded/expired/dangling targets
/// are excluded via `resolve_active`, which does not count as a read — bounded
/// work on the recall hot path.
/// `seeds` and `max_neighbors` are the configurable caps (`GraphConfig`); edges
/// arrive `created_at DESC` from [`memory_graph::outgoing`], so candidates are
/// ordered by recency before the cap truncates.
fn expand_recall_neighbors(
    conn: &rusqlite::Connection,
    results: &[memory::MemoryEntry],
    seeds: usize,
    max_neighbors: usize,
) -> crate::Result<Vec<String>> {
    let seen: std::collections::HashSet<&str> = results.iter().map(|e| e.id.as_str()).collect();
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for seed in results.iter().take(seeds) {
        if out.len() >= max_neighbors {
            break;
        }
        let edges = memory_graph::outgoing(conn, &seed.id, None)?;
        for edge in edges {
            if out.len() >= max_neighbors {
                break;
            }
            if edge.target_kind != TargetKind::Memory.as_str() {
                continue;
            }
            if seen.contains(edge.target_ref.as_str()) || !emitted.insert(edge.target_ref.clone()) {
                continue;
            }
            if let Some(n) = memory_graph::resolve_active(conn, &edge.target_ref)? {
                out.push(format!("- [{}] {} (via {})", n.id, n.title, edge.relation));
            }
        }
    }
    Ok(out)
}

pub async fn hook_user_prompt_submit_impl(handle: &RepoHandle, prompt: &str) -> Value {
    hook_user_prompt_submit_impl_with_dedup(handle, prompt, UNKNOWN_SESSION, None).await
}

async fn hook_user_prompt_submit_impl_with_dedup(
    handle: &RepoHandle,
    prompt: &str,
    session: &str,
    dedup: Option<(&DispatchContext, String)>,
) -> Value {
    use crate::cli::hook_logic::prompt_wants_call_graph;

    let cfg = &handle.config.hooks;
    if !cfg.user_prompt_submit_enabled {
        return json!({});
    }
    if prompt.trim().is_empty() {
        return json!({});
    }
    if prompt_is_wrapup(prompt) {
        if let Some((dctx, key)) = &dedup {
            dctx.reset_hook_session(key);
        }
        return json!({});
    }

    // Opt-in sigil: when enabled, only prompts starting with `*` get injection.
    // Strip the `*` (and following whitespace) so it never reaches FTS, the
    // embedder, or the model; a prompt without it is left untouched.
    let prompt = if cfg.user_prompt_submit_require_sigil {
        match prompt.trim_start().strip_prefix('*') {
            Some(rest) => rest.trim_start(),
            None => return json!({}),
        }
    } else {
        prompt
    };

    let prompt_repeat = dedup
        .as_ref()
        .map(|(dctx, key)| dctx.remember_hook_prompt(key, &prompt_fingerprint(prompt)))
        .unwrap_or(false);
    let wants_cg = prompt_wants_call_graph(prompt);
    let fts_query = crate::store::search::build_recall_query(prompt);
    // DEFERRED (2026-06-30) — memory→memory 1-hop expansion. Memories aren't in
    // the graph (edges.source_doc_id FKs documents.id; memory ids are TEXT slugs
    // with no documents row), so a memory_edges table + post-recall expansion is
    // needed. Low yield at ~12 entries; revisit as the corpus grows.
    let path_tokens = if cfg.doc_graph_in_recall {
        crate::cli::hook_logic::path_like_tokens(prompt)
    } else {
        Vec::new()
    };

    if fts_query.is_none() && !wants_cg && path_tokens.is_empty() {
        return json!({});
    }

    let mut results: Vec<memory::MemoryEntry> = Vec::new();
    let mut doc_hits: Vec<(String, Option<String>)> = Vec::new();
    if let Some(ref q) = fts_query {
        if ensure_handle_context(handle).await.is_err() {
            return json!({});
        }
        // Embed the raw prompt off the runtime BEFORE locking — this is the
        // per-turn UserPromptSubmit path, so holding the ctx mutex across
        // CPU-bound ONNX inference would stall a worker every turn. `q`
        // (build_recall_query) is a pre-built OR-expression fed to the FTS leg
        // via `_fts`; embedding the FTS operators would be noise.
        let query_embedding = embed_query_off_lock(prompt).await;
        let mut ctx_guard = handle.ctx.lock().await;
        let limit = cfg.recall_limit.max(1);
        let search_t0 = std::time::Instant::now();
        let search = crate::core::run_guarded_read(&mut ctx_guard, "hook memory recall", |ctx| {
            memory::search_entries_hybrid_fts(
                &ctx.conn,
                q,
                // The raw prompt, not `q`: `q` is the OR-expanded FTS
                // expression, and the lexical admission arm needs the words
                // as written. It is also what `query_embedding` embedded.
                prompt,
                query_embedding.as_deref(),
                limit,
                None,
                &handle.config.search.memory,
            )
        });
        let mut scored_results = match search {
            Some(Ok(entries)) => entries,
            Some(Err(error)) => {
                tracing::warn!("hook memory recall failed: {error}");
                return json!({});
            }
            None => return json!({}),
        };

        // Opt-in, privacy-minimized telemetry: record the recall's shape (HMAC +
        // latency + count) but NEVER the prompt text. Off by default.
        if handle.config.telemetry.query_events {
            let telemetry_now = chrono::Utc::now().timestamp();
            let mut eligible_scores = scored_results
                .iter()
                .filter(|entry| {
                    // Admission already happened in the store, so the only
                    // filter left is the prior-confidence gate applied below.
                    entry.entry_type != crate::store::memory::EntryType::Prior
                        || entry.confidence_at(telemetry_now) >= PRIOR_CONFIDENCE_GATE
                })
                .map(|entry| entry.score);
            let top_score = eligible_scores.next();
            let result_count = i64::try_from(1 + eligible_scores.count()).unwrap_or(i64::MAX);
            let result_count = if top_score.is_some() { result_count } else { 0 };
            let query_hash = match crate::metrics::privacy::hash_query(&handle.root, prompt) {
                Ok(hash) => hash,
                Err(error) => {
                    tracing::warn!("query telemetry key unavailable: {error}");
                    String::new()
                }
            };
            let ev = stats::QueryEvent {
                query_hash,
                query_text: String::new(),
                search_type: "recall".to_string(),
                result_count,
                latency_ms: search_t0.elapsed().as_millis() as i64,
                top_score,
                session_id: None,
            };
            if !ev.query_hash.is_empty()
                && let Some(Err(error)) =
                    crate::core::run_guarded_write(&mut ctx_guard, "query event telemetry", |ctx| {
                        stats::record_query_event(
                            &ctx.conn,
                            &ev,
                            handle.config.telemetry.retention_days,
                        )
                    })
            {
                tracing::warn!("record_query_event failed: {error}");
            }
        }

        // Documents leg — same hybrid engine as `search --scope docs`, reusing
        // the OR-expanded recall query and the embedding already computed above
        // (a second embed would double the per-turn CPU cost). No score floor:
        // RRF normalization pins the top hit at 1.0, so a threshold would filter
        // nothing — `recall_docs_limit` is the control (0 = memory only).
        let docs_limit = cfg.recall_docs_limit;
        if docs_limit > 0 && ctx_guard.is_some() {
            match crate::core::run_guarded_read(&mut ctx_guard, "hook document recall", |ctx| {
                crate::core::search::hybrid_search_fts(
                    ctx,
                    q,
                    query_embedding.as_deref(),
                    docs_limit,
                    None,
                    false,
                )
            }) {
                Some(Ok(hits)) => {
                    doc_hits = hits
                        .into_iter()
                        .map(|hit| (hit.path, hit.title.filter(|t| !t.is_empty())))
                        .collect();
                }
                Some(Err(error)) => {
                    // Degrade silently (hooks must not block) but stay observable.
                    tracing::debug!("recall doc search failed: {error}");
                }
                None => {}
            }
        }
        drop(ctx_guard);

        // prior-specific gate: only high-confidence priors surface
        let now = chrono::Utc::now().timestamp();
        scored_results.retain(|e| {
            e.entry_type != crate::store::memory::EntryType::Prior
                || e.confidence_at(now) >= PRIOR_CONFIDENCE_GATE
        });
        results = injectable(scored_results);

        // Global rank: float high-confidence priors to the top WITHOUT
        // scrambling the rest (stable sort on a boolean key preserves the
        // existing relevance order within each group). THEN truncate to limit.
        results.sort_by_key(|e| !is_high_confidence_prior(e, now));
        results.truncate(limit);
        if let Some((dctx, key)) = &dedup {
            dctx.retain_new_hook_memories(key, &mut results);
        }
    }

    // Post-recall enrichment in a single re-lock (both read-only, capped):
    //  · 1-hop memory-edge expansion — surface active neighbors of the top seeds.
    //  · stale-dependency flags — mark entries whose basis is superseded/refuted.
    let mut expanded: Vec<String> = Vec::new();
    let mut stale_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    if !results.is_empty() && ensure_handle_context(handle).await.is_ok() {
        let mut ctx_guard = handle.ctx.lock().await;
        let enrichment =
            crate::core::run_guarded_read(&mut ctx_guard, "hook recall enrichment", |ctx| {
                let expanded = expand_recall_neighbors(
                    &ctx.conn,
                    &results,
                    handle.config.graph.expand_seeds,
                    handle.config.graph.expand_neighbors,
                )?;
                let ids: Vec<&str> = results.iter().map(|e| e.id.as_str()).collect();
                let stale_ids = memory_graph::stale_dependency_ids(&ctx.conn, &ids)?;
                Ok((expanded, stale_ids))
            });
        match enrichment {
            Some(Ok((found_expanded, found_stale_ids))) => {
                expanded = found_expanded;
                stale_ids = found_stale_ids;
            }
            Some(Err(error)) => tracing::debug!("hook recall enrichment failed: {error}"),
            None => {}
        }
    }

    // D — doc-graph neighbors: when the prompt names a document, surface its
    // 1-hop frontmatter neighbors. Independent of memory recall, deduped against
    // the (now finalized) memory ids about to be injected.
    let mut neighbors: Vec<(String, String)> = Vec::new();
    if !path_tokens.is_empty() {
        // The FTS leg already initialized the context if it ran; only ensure when
        // it didn't (path-only prompt) so we don't re-acquire on the hot path.
        let ctx_ready = fts_query.is_some() || ensure_handle_context(handle).await.is_ok();
        if ctx_ready {
            let mut ctx_guard = handle.ctx.lock().await;
            let seen: std::collections::HashSet<String> =
                results.iter().map(|e| e.id.clone()).collect();
            match crate::core::run_guarded_read(
                &mut ctx_guard,
                "hook document graph neighbors",
                |ctx| {
                    doc_graph_neighbors(
                        &ctx.conn,
                        &path_tokens,
                        &seen,
                        handle.config.graph.doc_neighbor_cap,
                    )
                },
            ) {
                Some(Ok(found)) => neighbors = found,
                Some(Err(error)) => tracing::debug!("hook document neighbors failed: {error}"),
                None => {}
            }
        }
    }

    // A doc reachable both ways is emitted once, as a graph neighbor: that block
    // carries the relation label, which the search hit cannot reconstruct.
    let neighbor_paths: std::collections::HashSet<&str> =
        neighbors.iter().map(|(p, _)| p.as_str()).collect();
    let mut doc_lines: Vec<String> = doc_hits
        .iter()
        .filter(|(path, _)| !neighbor_paths.contains(path.as_str()))
        .map(|(path, title)| match title {
            Some(t) => format!("- {path} — {t}"),
            None => format!("- {path}"),
        })
        .collect();
    let mut related: Vec<String> = neighbors
        .iter()
        .map(|(path, relation)| format!("- {path} ({relation})"))
        .collect();
    if let Some((dctx, key)) = &dedup {
        dctx.retain_new_hook_related_lines(key, &mut doc_lines);
        dctx.retain_new_hook_related_lines(key, &mut related);
    }

    // Trigger-matched behavioral priors whose prompt pattern fires here.
    let prior_block = prompt_prior_block(handle, prompt, session, dedup.as_ref()).await;

    let nothing_found = results.is_empty() && doc_lines.is_empty() && related.is_empty();
    if nothing_found && prior_block.is_none() && !wants_cg {
        return json!({});
    }
    if prompt_repeat && nothing_found && prior_block.is_none() {
        return json!({});
    }

    let mut body = String::new();

    if !results.is_empty() {
        body.push_str("## mdkb: relevant context\n\n");
        for entry in &results {
            let snippet_raw =
                crate::store::memory::strip_frontmatter(&entry.content).replace('\n', " ");
            let snippet: String = snippet_raw.chars().take(160).collect();
            let stale = if stale_ids.contains(&entry.id) {
                "[STALE-DEP] "
            } else {
                ""
            };
            body.push_str(&format!(
                "- {}[{}] {} ({}) — {}\n",
                stale,
                entry.id,
                entry.title,
                relative_time_ago(entry.updated_at),
                snippet
            ));
        }
        // 1-hop edge-expanded neighbors, annotated `(via <relation>)`.
        for line in &expanded {
            body.push_str(line);
            body.push('\n');
        }
        body.push_str("\nIf your work corroborates any entry above, run `mdkb memory confirm <id> --outcome confirmed` instead of writing a new one.\n");
    }

    if !doc_lines.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("## mdkb: matching docs\n\n");
        for line in &doc_lines {
            body.push_str(line);
            body.push('\n');
        }
    }

    if !related.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("## mdkb: related docs\n\n");
        for line in &related {
            body.push_str(line);
            body.push('\n');
        }
    }

    if let Some(prior) = prior_block {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("## mdkb: priors\n\n");
        body.push_str(&prior);
        body.push('\n');
    }

    if wants_cg {
        body.push_str(
            "\n💡 This looks like a call-graph query. Use `code_graph(name)` or `code_graph(name, direction=\"callers\"|\"callees\"|\"impact\")` — one MCP call replaces multi-file Grep.\n",
        );
    }

    if body.is_empty() {
        return json!({});
    }

    json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": body,
        }
    })
}

/// Promoted priors whose `prompt`-kind trigger matches the submitted prompt,
/// formatted as `mdkb prior: <lesson>` lines (and recorded as injected). `None`
/// when injection is disabled, the store is unavailable, or nothing matches.
async fn prompt_prior_block(
    handle: &RepoHandle,
    prompt: &str,
    session: &str,
    dedup: Option<&(&DispatchContext, String)>,
) -> Option<String> {
    use crate::store::priors::{TriggerContext, match_injectable, record_injection};

    if !handle.config.priors.injection_enabled {
        return None;
    }
    let now = chrono::Utc::now().timestamp();
    let max = handle.config.priors.max_injected_per_hook;

    if ensure_handle_context(handle).await.is_err() {
        return None;
    }
    let mut ctx_guard = handle.ctx.lock().await;

    let tctx = TriggerContext::Prompt { text: prompt };
    let hits = crate::core::run_guarded_read(&mut ctx_guard, "prompt prior lookup", |ctx| {
        match_injectable(&ctx.conn, &tctx, now, max)
    })?
    .ok()?;
    if hits.is_empty() {
        return None;
    }
    let mut lines = Vec::with_capacity(hits.len());
    for c in &hits {
        if let Some((dctx, key)) = dedup {
            if dctx.hook_prior_seen(key, &c.id) {
                continue;
            }
        }
        let prior_id = c.id.clone();
        if let Some(Err(error)) =
            crate::core::run_guarded_write(&mut ctx_guard, "prompt prior telemetry", |ctx| {
                record_injection(&ctx.conn, &prior_id, session, now)
            })
        {
            tracing::warn!("record prompt prior injection: {error}");
        }
        if let Some((dctx, key)) = dedup {
            dctx.record_hook_prior(key, &c.id);
        }
        lines.push(format!("mdkb prior: {}", c.lesson));
    }
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n"))
}

/// Resolve doc-path `tokens` to their 1-hop *frontmatter* graph neighbors that
/// point at real documents, formatted as `- <path> (<relation>)` lines, capped
/// at `cap`. Soft wikilink edges are skipped (frontmatter relations are the
/// strong, curated signal) and so are non-document targets (entity tags like
/// `themes`/`owner`). Neighbors whose canonical path is in `seen`
/// (already-injected memory ids) or already emitted are de-duplicated.
fn doc_graph_neighbors(
    conn: &rusqlite::Connection,
    tokens: &[String],
    seen: &std::collections::HashSet<String>,
    cap: usize,
) -> crate::Result<Vec<(String, String)>> {
    use crate::store::graph;
    let mut out: Vec<(String, String)> = Vec::new();
    let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
    for tok in tokens {
        if out.len() >= cap {
            break;
        }
        let Some(doc_id) = graph::resolve_ref_to_doc(conn, tok)? else {
            continue;
        };
        let edges = graph::get_outgoing(conn, doc_id, None)?;
        for edge in edges {
            if out.len() >= cap {
                break;
            }
            if edge.source_kind != graph::KIND_FRONTMATTER {
                continue;
            }
            // Only emit targets that resolve to an actual indexed document, with
            // their canonical path (so `[[b]]`, `b`, and `b.md` collapse to one
            // node). Frontmatter also carries entity relations (owner, themes, …)
            // whose targets are tags, not navigable docs — `resolve_to_path`
            // returns None for those, keeping the "related docs" block honest.
            // One resolution pass per edge.
            let Some(path) = graph::resolve_to_path(conn, &edge.target_ref)? else {
                continue;
            };
            if seen.contains(&path) || !emitted.insert(path.clone()) {
                continue;
            }
            out.push((path, edge.relation));
        }
    }
    Ok(out)
}

/// Number of trailing transcript lines that form the mined episode window.
const STOP_EPISODE_WINDOW_LINES: usize = 800;

/// `hook.stop` — end-of-episode boundary that feeds behavioral-prior mining.
///
/// Returns `{}` immediately. Mining is kill-switched off by default and, when
/// on, spawns an external agent CLI to distill — far too slow for the hook
/// budget — so the actual work is detached into a background task. The hook
/// itself only gates and enqueues.
fn hook_stop_impl(handle: Arc<RepoHandle>, event: &Value, dctx: &DispatchContext) -> Value {
    // Drain mid-session cold-model `memory_write`s in the background. Independent
    // of prior mining — must run even when mining is kill-switched off — so it
    // goes before the mining gate. Single-flight + best-effort.
    spawn_embedding_backfill(Arc::clone(&handle));

    let transcript_path = event
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .map(String::from);
    let session = event_session(event);

    // Settling comes first and is NOT behind the mining gate: priors are
    // injected whenever `injection_enabled` is on, which is a different switch.
    // Gating settlement on mining would leave every prior shown in such a repo
    // permanently unsettled, which is the frozen belief this closes. It re-reads
    // the transcript rather than sharing the mining read — one file read against
    // a task that may never run, or may spend a minute in an LLM call.
    if let Some(path) = transcript_path.clone() {
        dctx.spawn_background(settle_session(Arc::clone(&handle), path, session.clone()));
    }

    let cfg = &handle.config.priors;
    if !cfg.mining_enabled {
        return json!({});
    }
    // No built-in chat model: without a configured distiller there is nothing to
    // mine with, so stay off even when the master flag is on.
    let Some(program) = cfg.distiller_program.clone() else {
        return json!({});
    };
    let args = cfg.distiller_args.clone();
    let Some(transcript_path) = transcript_path else {
        return json!({});
    };

    dctx.spawn_background(mine_episode(
        handle,
        transcript_path,
        session,
        program,
        args,
    ));
    json!({})
}

/// Answer, for every prior injected into this session, whether it held.
///
/// The verdict is the whole point of injecting: `cluster_injection_score`
/// divides by a Beta belief over `confirmed_count`/`refuted_count`, and with
/// nothing ever incrementing either, a freshly promoted prior started at 0.33
/// against a 0.3 threshold and decayed under it in about 20 days. A prior could
/// only ever go dark, however well it worked.
///
/// Best-effort like all Stop-hook work: every failure is a log line.
async fn settle_session(handle: Arc<RepoHandle>, transcript_path: String, session: String) {
    use crate::domain::prior_episode::parse_episode;
    use crate::store::priors::{ObservedError, settle_injections};

    let jsonl = match tokio::task::spawn_blocking(move || std::fs::read_to_string(&transcript_path))
        .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::debug!("prior settling: read transcript failed: {e}");
            return;
        }
        Err(e) => {
            tracing::debug!("prior settling: transcript read task failed: {e}");
            return;
        }
    };
    // The same window the miner reads. A prior injected earlier than this window
    // is settled against what the window shows, which can only ever miss a
    // recurrence — it never invents one.
    let episode = parse_episode(&tail_lines(&jsonl, STOP_EPISODE_WINDOW_LINES));
    let errors: Vec<ObservedError> = episode
        .errors
        .iter()
        .map(|e| ObservedError {
            signature: e.signature.clone(),
            at: e.at,
        })
        .collect();

    if ensure_handle_context(&handle).await.is_err() {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let mut guard = handle.ctx.lock().await;
    match crate::core::run_mutation(&mut guard, "prior settling", |ctx| {
        settle_injections(&ctx.conn, &session, now, &errors)
    }) {
        Some(Ok(report)) if !report.is_empty() => {
            // All three outcomes are logged, not just the one that moves a
            // counter: a run that settles nothing but `unobservable` is the
            // signal that the distiller is not writing error signatures, and
            // reporting only refutations would show it as silence.
            tracing::info!(
                "prior settling: {} refuted, {} unobservable, {} unrefuted \
                 in session {session}",
                report.refuted.len(),
                report.unobservable.len(),
                report.unrefuted.len()
            );
            // A demotion retires a lesson. Named, not counted: this is the one
            // line that explains why a prior stopped appearing.
            if !report.demoted.is_empty() {
                tracing::info!(
                    "prior settling: demoted {} out of injection",
                    report.demoted.join(", ")
                );
            }
        }
        Some(Err(error)) => tracing::debug!("prior settling failed: {error}"),
        _ => {}
    }
}

/// What one prior-mining run did, as recorded in `hook-events.jsonl`.
///
/// The Stop hook returns before the distiller starts, so its own event can only
/// ever say "I detached something". These are the four answers an operator
/// actually needs, and the reason is part of the answer: `failed` alone cannot
/// distinguish a distiller that is missing, one that refuses the request and one
/// that replies with prose.
enum MiningOutcome {
    /// The cheap detector turned the episode down — no LLM call was made. The
    /// common case by far: most sessions teach nothing.
    Gated,
    /// A validated prior was integrated, and its cluster has not yet recurred
    /// across enough distinct sessions to be promoted.
    Distilled,
    /// Integrated, and this observation tipped the cluster over the recurrence
    /// gate. Distinct from `Distilled` because promotion is the only point at
    /// which a mined prior starts being injected: a week of `distilled` with no
    /// `promoted` says the pipeline runs and still teaches the model nothing.
    Promoted,
    /// A well-formed answer the validator turned down. Ordinary, not a fault.
    Rejected(String),
    /// The distiller could not run, or its output could not be used at all.
    Failed(String),
}

impl MiningOutcome {
    fn label(&self) -> &'static str {
        match self {
            Self::Gated => "gated",
            Self::Distilled => "distilled",
            Self::Promoted => "promoted",
            Self::Rejected(_) => "rejected",
            Self::Failed(_) => "failed",
        }
    }

    fn reason(&self) -> Option<&str> {
        match self {
            Self::Gated | Self::Distilled | Self::Promoted => None,
            Self::Rejected(r) | Self::Failed(r) => Some(r),
        }
    }
}

/// Mine one episode and record what happened.
///
/// The outcome is written exactly once, here, rather than at each of the early
/// returns inside — "one event per run" is then a property of the shape, not a
/// rule every future `return` has to remember.
async fn mine_episode(
    handle: Arc<RepoHandle>,
    transcript_path: String,
    session: String,
    program: String,
    args: Vec<String>,
) {
    let root = handle.root.clone();
    let t0 = std::time::Instant::now();
    let outcome = mine_episode_inner(handle, transcript_path, session, program, args).await;
    let elapsed_ms = t0.elapsed().as_millis() as u64;
    let label = outcome.label();
    let reason = outcome.reason().map(str::to_string);

    // `u64::MAX`, not the hook latency budget: this runs detached, after the
    // hook has already answered, and a distiller legitimately takes tens of
    // seconds. Measuring it against the budget would file every successful run
    // in `hook-slow.jsonl` and bury the hooks that really are over budget.
    let _ = tokio::task::spawn_blocking(move || {
        log_hook_event_with_reason(
            root,
            crate::cli::stats_report::MINING_EVENT,
            label,
            reason.as_deref(),
            elapsed_ms,
            u64::MAX,
        );
    })
    .await;
}

/// The awaitable core of prior mining: read the transcript tail → parse the raw
/// episode → gate on the cheap candidate detector → distill via the external CLI
/// → validate → persist as a candidate and promote on recurrence. Best-effort:
/// every failure degrades to a log line and an early return (a background task
/// must never surface errors). Kept as a standalone async fn (not inlined into
/// the detached spawn) so it can be awaited directly in tests.
async fn mine_episode_inner(
    handle: Arc<RepoHandle>,
    transcript_path: String,
    session: String,
    program: String,
    args: Vec<String>,
) -> MiningOutcome {
    use crate::domain::prior_detect::detect_candidate;
    use crate::domain::prior_distill::{
        build_distill_prompt, distiller_failure, parse_distilled, run_distiller_cli,
    };
    use crate::domain::prior_episode::parse_episode;
    use crate::store::priors::integrate_distilled;

    // Read the transcript tail off the async runtime.
    let jsonl = match tokio::task::spawn_blocking(move || std::fs::read_to_string(&transcript_path))
        .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            tracing::debug!("prior mining: read transcript failed: {e}");
            return MiningOutcome::Failed(format!("read transcript failed: {e}"));
        }
        Err(e) => return MiningOutcome::Failed(format!("transcript read task failed: {e}")),
    };
    let window = tail_lines(&jsonl, STOP_EPISODE_WINDOW_LINES);

    let episode = parse_episode(&window);
    let Some(sig) = detect_candidate(&episode) else {
        // The cheap gate: most episodes teach nothing, no LLM call.
        return MiningOutcome::Gated;
    };
    // The failure this lesson will exist to prevent. Recorded on the cluster so
    // a later session can tell "the prior was shown and the error stayed away"
    // from "the prior was shown and it happened anyway".
    let error_signature = sig.error_signature.clone();
    let prompt = build_distill_prompt(&episode, &sig);

    // Spawn the external distiller off the async runtime (blocking process).
    let logged_program = program.clone();
    let run = match tokio::task::spawn_blocking(move || run_distiller_cli(&program, &args, &prompt))
        .await
    {
        Ok(Ok(run)) => run,
        Ok(Err(e)) => {
            // warn, not debug: a distiller that cannot be spawned is a
            // misconfiguration the operator must see. At debug — which the
            // daemon does not log — this stayed invisible from 2026-08-01 to
            // 2026-09-16 while every Stop event reported success.
            tracing::warn!("prior mining: distiller {logged_program:?} could not be spawned: {e}");
            return MiningOutcome::Failed(format!("distiller {logged_program:?}: {e}"));
        }
        Err(e) => return MiningOutcome::Failed(format!("distiller task failed: {e}")),
    };
    let parsed = parse_distilled(&run.stdout);
    if let Some(failure) = distiller_failure(&logged_program, &run, parsed.as_ref().err()) {
        tracing::warn!("prior mining: {failure}");
        return MiningOutcome::Failed(failure);
    }
    let distilled = match parsed {
        Ok(d) => d,
        // A well-formed answer the validator turned down: most episodes teach
        // nothing, so this is ordinary and stays at debug.
        Err(e) => {
            tracing::debug!("prior mining: distiller output rejected: {e}");
            return MiningOutcome::Rejected(e.to_string());
        }
    };

    // Embed the lesson (off the async runtime — ONNX inference is blocking) so
    // integrate_distilled can merge semantically-equivalent clusters. Best-effort:
    // a missing embedder just falls back to exact-trigger-key clustering.
    let lesson = distilled.lesson.clone();
    let lesson_embedding = tokio::task::spawn_blocking(move || {
        crate::llm::get_cached_service()
            .ok()
            .and_then(|s| s.embed_query(&lesson).ok())
    })
    .await
    .ok()
    .flatten();

    if let Err(e) = ensure_handle_context(&handle).await {
        return MiningOutcome::Failed(format!("open store failed: {e}"));
    }
    let now = chrono::Utc::now().timestamp();
    let mut guard = handle.ctx.lock().await;
    match crate::core::run_mutation(&mut guard, "prior mining", |ctx| {
        integrate_distilled(
            &ctx.conn,
            &distilled,
            &session,
            now,
            lesson_embedding.as_deref(),
            error_signature.as_deref(),
        )
    }) {
        Some(Err(error)) => {
            tracing::debug!("prior mining: integrate_distilled failed: {error}");
            MiningOutcome::Failed(format!("integrate failed: {error}"))
        }
        // `None` = the context slot was empty, so the mutation never ran.
        None => MiningOutcome::Failed("store context unavailable".to_string()),
        // The payload is the cluster's promoted memory entry id, present only
        // when this observation tipped it over the recurrence gate.
        Some(Ok(Some(_))) => MiningOutcome::Promoted,
        Some(Ok(None)) => MiningOutcome::Distilled,
    }
}

/// The last `n` lines of `s`, joined with `\n`. The mined episode is the tail of
/// the transcript; older turns are noise for a single end-of-session lesson.
fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

pub async fn hook_post_tool_use_impl(handle: &RepoHandle, event: &Value) -> Value {
    if !handle.config.hooks.post_tool_use_enabled {
        return json!({});
    }
    let Some(tool_name) = event.get("tool_name").and_then(|v| v.as_str()) else {
        return json!({});
    };

    // Priors are matched for EVERY tool, not only the ones that trigger a
    // reindex: "run the generator after editing the template" is a lesson about
    // Bash, which this hook otherwise ignores entirely.
    let prior_block = match event.get("tool_input") {
        Some(input) => posttool_prior_block(handle, tool_name, input, &event_session(event)).await,
        None => None,
    };
    let mut result = match &prior_block {
        Some(text) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PostToolUse",
                "additionalContext": text,
            }
        }),
        None => json!({}),
    };

    if !REINDEX_TOOLS.contains(&tool_name) {
        return result;
    }
    let Some(raw_path) = event.get("tool_input").and_then(tool_input_path) else {
        return result;
    };
    let path = if let Some(p) = canonicalize_under_cwd(&handle.root, &raw_path) {
        std::path::PathBuf::from(p)
    } else {
        tracing::warn!("hook.post_tool_use: rejected path outside root: {raw_path}");
        return result;
    };
    if let Err(e) = handle.reindex_tx.try_send(path) {
        // Bounded logging: warn once per failure episode, not on every edit (the
        // old path logged 571 identical "channel closed" lines). The edit is not
        // lost long-term — the FSEvents watcher and the next `update` still pick
        // it up; only the fast-path injection is skipped this once.
        if !handle
            .reindex_send_warned
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                "hook.post_tool_use: reindex path injection unavailable ({e}); \
                 falling back to watcher/update. Further failures are suppressed \
                 until the channel recovers."
            );
        }
        return result;
    }
    // A prior failure episode (if any) has recovered; re-arm the one-shot warning.
    handle
        .reindex_send_warned
        .store(false, std::sync::atomic::Ordering::Relaxed);
    result["queued"] = json!(true);
    result
}

pub async fn hook_pre_tool_use_impl(handle: &RepoHandle, event: &Value) -> Value {
    if !handle.config.hooks.pre_tool_use_enabled {
        return json!({});
    }
    let Some(tool_name) = event.get("tool_name").and_then(|v| v.as_str()) else {
        return json!({});
    };
    let Some(tool_input) = event.get("tool_input") else {
        return json!({});
    };
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "mdkb".to_string());

    // Grep tool calls arrive with a clean pattern; Bash calls carry a raw shell
    // command we parse for a `grep`/`rg` filesystem search. Both feed the same
    // redirection classifiers. Claude searches code via Bash far more than the
    // Grep tool, so matching Bash is where the redirect actually reaches it. Other
    // tools (Edit/Write/…) get no search suggestion, but STILL flow through to the
    // trigger-matched prior injection below — a path-scoped prior on Edit is the
    // headline case, so this must not early-return.
    let suggestion = match tool_name {
        "Grep" => tool_input
            .get("pattern")
            .and_then(|v| v.as_str())
            .and_then(|pattern| {
                let path = tool_input.get("path").and_then(|v| v.as_str());
                classify_definition_search(pattern, &bin)
                    .or_else(|| classify_grep_pattern(pattern, path, &bin))
            }),
        "Bash" => tool_input
            .get("command")
            .and_then(|v| v.as_str())
            .and_then(|command| classify_bash_search(command, &bin)),
        _ => None,
    };

    // "Act, not suggest": on a definition-classified search, inject the real
    // code-index hits (file:line) and fall back to the suggestion only when the
    // symbol is not indexed. Gated behind a flag and a cheap existence check so
    // the hot path never opens/creates an index for non-definition searches.
    let hits = if handle.config.hooks.code_hits_in_pretooluse {
        match crate::cli::hook_logic::extract_definition_symbol(tool_name, tool_input) {
            Some(sym) => code_index_hits(handle, &sym, 5).await,
            None => None,
        }
    } else {
        None
    };

    let search_block = match (hits, suggestion) {
        (Some(block), _) => Some(block), // act
        (None, Some(s)) => Some(s),      // fall back to suggest
        (None, None) => None,
    };

    // Trigger-matched behavioral priors are complementary to the search
    // redirect: surface any promoted prior whose trigger matches this tool call,
    // appended after the search block.
    let prior_block =
        pretool_prior_block(handle, tool_name, tool_input, &event_session(event)).await;

    let text = match (search_block, prior_block) {
        (Some(s), Some(p)) => Some(format!("{s}\n\n{p}")),
        (Some(s), None) => Some(s),
        (None, Some(p)) => Some(p),
        (None, None) => None,
    };

    match text {
        Some(text) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "additionalContext": text,
            }
        }),
        None => json!({}),
    }
}

/// Promoted priors whose trigger matches this PreToolUse call.
async fn pretool_prior_block(
    handle: &RepoHandle,
    tool: &str,
    tool_input: &Value,
    session: &str,
) -> Option<String> {
    tool_prior_block(handle, tool, tool_input, false, session).await
}

/// Promoted priors whose trigger matches this PostToolUse call.
async fn posttool_prior_block(
    handle: &RepoHandle,
    tool: &str,
    tool_input: &Value,
    session: &str,
) -> Option<String> {
    tool_prior_block(handle, tool, tool_input, true, session).await
}

/// Promoted priors whose trigger matches a tool call, formatted as a context
/// block (and recorded as injected). `None` when injection is disabled, the
/// memory store is unavailable, or nothing matches.
///
/// `after` selects which half of the tool call is being answered: a `pre_tool`
/// lesson warns before the call, a `post_tool` lesson tells the model what to do
/// now that it has happened. Identifying the call is the same work either way,
/// so both hooks come through here.
async fn tool_prior_block(
    handle: &RepoHandle,
    tool: &str,
    tool_input: &Value,
    after: bool,
    session: &str,
) -> Option<String> {
    use crate::store::priors::{TriggerContext, match_injectable, record_injection};

    if !handle.config.priors.injection_enabled {
        return None;
    }
    // Repo-relative path gives clean `src/generated/**`-style glob matching.
    let path = tool_input
        .get("file_path")
        .and_then(|v| v.as_str())
        .map(|p| {
            // The distilled trigger pattern is a `/`-separated glob, so the
            // path it is matched against must be one too.
            std::path::Path::new(p)
                .strip_prefix(&handle.root)
                .map(crate::domain::rel_key)
                .unwrap_or_else(|_| p.to_string())
        });
    let command = tool_input.get("command").and_then(|v| v.as_str());
    let now = chrono::Utc::now().timestamp();
    let max = handle.config.priors.max_injected_per_hook;
    let label = if after { "post-tool" } else { "pre-tool" };

    // Read from the ALREADY-open context only — the tool hot path must never
    // force a DB open (the same reason `code_index_hits` guards on `.exists()`).
    // In the daemon the context is warm after SessionStart, so priors fire; a
    // cold one-shot invocation skips them (best-effort).
    let mut ctx_guard = handle.ctx.lock().await;

    let tctx = if after {
        TriggerContext::PostTool {
            tool,
            path: path.as_deref(),
            command,
        }
    } else {
        TriggerContext::PreTool {
            tool,
            path: path.as_deref(),
            command,
        }
    };
    let hits = crate::core::run_guarded_read(&mut ctx_guard, "tool prior lookup", |ctx| {
        match_injectable(&ctx.conn, &tctx, now, max)
    })?
    .ok()?;
    if hits.is_empty() {
        return None;
    }
    let mut lines = Vec::with_capacity(hits.len());
    for c in &hits {
        let prior_id = c.id.clone();
        if let Some(Err(error)) =
            crate::core::run_guarded_write(&mut ctx_guard, "tool prior telemetry", |ctx| {
                record_injection(&ctx.conn, &prior_id, session, now)
            })
        {
            tracing::warn!("record {label} prior injection: {error}");
        }
        lines.push(format!("mdkb prior: {}", c.lesson));
    }
    Some(lines.join("\n"))
}

/// Up to `limit` indexed definitions of `symbol`, formatted as a PreToolUse
/// context block of `file:line` hits. Returns `None` when the code index is
/// absent or the symbol is unknown — the caller then falls back to the
/// suggestion. The `.exists()` guard keeps the common "no code index" path free
/// of DB initialization, so a project that never indexed code pays nothing here.
/// (`acquire_handle_code_index` can still create the DB if it loses a race with a
/// concurrent delete between the check and the open; that empty DB is benign —
/// `find_symbols_by_name` returns nothing and we fall back to the suggestion.)
async fn code_index_hits(handle: &RepoHandle, symbol: &str, limit: usize) -> Option<String> {
    if !handle.root.join(".mdkb/code.sqlite").exists() {
        return None;
    }
    let idx_guard = match acquire_handle_code_index(handle).await {
        Ok(g) => g,
        Err(e) => {
            // Existing-but-unreadable index (corrupt/IO). Degrade to the
            // suggestion, but stay observable rather than silently dead.
            tracing::debug!("code_index_hits: failed to open code index for `{symbol}`: {e}");
            return None;
        }
    };
    let facade = idx_guard.as_ref()?;
    let symbols = facade.find_symbols_by_name(symbol);
    if symbols.is_empty() {
        return None;
    }
    Some(render_code_index_hits(symbol, &symbols, limit))
}

/// Render the PreToolUse code-index block for `symbols`, showing at most
/// `limit` of them.
///
/// `find_symbols_by_name` returns rows ordered by `(file_path, line_start)`, so
/// the prefix shown here is the first definitions in the repository rather than
/// whichever rows SQLite happened to return. When the list is cut, the block
/// says so: a hook that silently showed 5 of 12 definitions taught the agent
/// that there were 5.
fn render_code_index_hits(
    symbol: &str,
    symbols: &[crate::code::symbol::Symbol],
    limit: usize,
) -> String {
    let mut block = format!("mdkb code index — `{symbol}` defined at:\n");
    for s in symbols.iter().take(limit) {
        // Stored ranges are 0-based (tree-sitter rows); display 1-based lines.
        block.push_str(&format!(
            "- {}:{} ({})\n",
            s.file_path,
            s.range.start_line + 1,
            s.kind
        ));
    }
    if let Some(hidden) = symbols.len().checked_sub(limit).filter(|n| *n > 0) {
        block.push_str(&format!(
            "… and {hidden} more definition(s) of this name.\n"
        ));
    }
    block.push_str("Read the definition directly instead of grepping.\n");
    block
}

/// Attribute a completed hook invocation to the reserved `hooks` pseudo-session
/// in `call_log`. Records counts only — never prompt or tool content.
///
/// Best-effort from the ALREADY-open context: it must NEVER force a DB open on
/// the hook hot path (the same principle as `code_index_hits`/priors — forcing
/// an open here also gives the file watcher a wall-clock window to bootstrap the
/// code index in one-shot CLI invocations). Called AFTER the hook impl, so
/// session_start (which warms the ctx) is counted; a cold pre_tool_use one-shot
/// skips. In the daemon the ctx stays warm, so all hook traffic is counted.
/// `record_call` is three tiny local-SQLite writes (sub-millisecond).
async fn record_hook_call(handle: &RepoHandle, method: &str) {
    let event = method.strip_prefix("hook.").unwrap_or(method);
    let mut ctx_guard = handle.ctx.lock().await;
    if ctx_guard.is_none() {
        return;
    }
    let outcome = crate::core::run_guarded_write(&mut ctx_guard, "hook telemetry", |ctx| {
        let sid = stats::find_or_create_agent_session(&ctx.conn, "hooks")?;
        stats::record_call(&ctx.conn, sid, event, 0, 0, false)
    });
    if let Some(Err(error)) = outcome {
        tracing::warn!("record hook call: {error}");
    }
}

/// Execute the internal CLI mutation protocol against daemon-owned resources.
async fn cli_mutate_impl(
    handle: &RepoHandle,
    mutation: CliMutation,
) -> Result<CliMutationResult, McpError> {
    use CliMutation::{CodeIndex, CodeInit, Compact, Update};

    match mutation {
        Update { request } => Ok(CliMutationResult::Update {
            outcome: update_impl(handle, &request).await?,
        }),
        CodeInit => {
            let index = acquire_handle_code_index(handle).await?;
            if index.is_none() {
                return Err(mcp_error(
                    "Code index is currently rebuilding; retry shortly",
                ));
            }
            Ok(CliMutationResult::CodeInitialized)
        }
        CodeIndex { paths, force } => {
            let mut index = acquire_handle_code_index(handle).await?;
            let stats =
                crate::code::indexing::run_code_mutation(&mut index, "CLI code index", |facade| {
                    if force {
                        crate::core::code::reindex_paths(facade, &handle.root, &paths)
                    } else if paths.is_empty() {
                        facade.update(&handle.root)
                    } else {
                        crate::core::code::index_paths(facade, &handle.root, &paths)
                    }
                })
                .ok_or_else(|| mcp_error("Code index not initialized"))?
                .map_err(|e| mcp_error(format!("Code indexing failed: {e:#}")))?;
            crate::llm::release_cached_service();
            Ok(CliMutationResult::CodeIndexed { stats })
        }
        Compact {
            prune_sessions,
            older_than,
            export,
        } => {
            ensure_handle_context(handle).await?;
            let (prune, index_bytes) = {
                let mut slot = handle.ctx.lock().await;
                run_handle_memory_mutation(&mut slot, "compact", |ctx| {
                    let prune = if prune_sessions {
                        let raw = older_than.as_deref().ok_or_else(|| mcp_error(
                            "--prune-sessions requires --older-than <e.g. 90d> to avoid deleting recent archives",
                        ))?;
                        let secs = crate::core::ops::parse_retention_secs(raw)
                            .map_err(|e| mcp_error(e.to_string()))?;
                        let cutoff = chrono::Utc::now().timestamp().checked_sub(secs).ok_or_else(|| {
                            mcp_error(format!("--older-than '{raw}' is too large to compute a cutoff"))
                        })?;
                        Some(crate::core::ops::handle_prune_sessions(ctx, cutoff, export.as_deref())
                            .map_err(|e| mcp_store_error("Failed to prune sessions", e))?)
                    } else {
                        None
                    };
                    ctx.conn.execute_batch("VACUUM;")
                        .map_err(|e| mcp_store_error("Failed to vacuum index.sqlite", e))?;
                    Ok((prune, ctx.db_path.metadata().map(|m| m.len()).unwrap_or(0)))
                })
                .map_err(|e| mcp_error(format!("compact failed: {e}")))?
            };

            let code_path = handle.root.join(".mdkb/code.sqlite");
            let code_bytes = if code_path.exists() {
                let mut index = handle.code_index.lock().await;
                *index = None;
                let _live = crate::store::mutation_lock::acquire_live_shared(&code_path)
                    .map_err(|e| mcp_error(format!("compact code lock: {e}")))?;
                let conn = rusqlite::Connection::open(&code_path)
                    .map_err(|e| mcp_error(format!("compact code open: {e}")))?;
                conn.execute_batch("VACUUM;")
                    .map_err(|e| mcp_error(format!("compact code vacuum: {e}")))?;
                Some(code_path.metadata().map(|m| m.len()).unwrap_or(0))
            } else {
                None
            };
            Ok(CliMutationResult::Compact {
                prune,
                index_bytes,
                code_bytes,
            })
        }
        mutation => {
            ensure_handle_context(handle).await?;
            let mut slot = handle.ctx.lock().await;
            // No outer wrap: `run_handle_memory_mutation` already returns an
            // `McpError`, so re-wrapping stringified one error inside another and
            // repeated both the code and the phrase, pushing the one fact the
            // operator needs to the end of the line. It also flattened the code
            // the inner error had earned, which is what tells the CLI whether the
            // write started.
            run_handle_memory_mutation(&mut slot, "cli mutation", |ctx| {
                crate::core::cli_mutation::execute_context_mutation(ctx, mutation)
                    .map_err(|e| mcp_store_error("CLI mutation failed", e))
            })
        }
    }
}

/// Dispatch a tool call by method name. Returns a JSON value — callers are
/// responsible for the transport envelope.
///
/// Unknown methods return an `McpError`; the JSON-RPC caller maps that into
/// a `-32601 Method not found` response.
pub async fn dispatch_call(
    tool_name: &str,
    params: Value,
    handle: Arc<RepoHandle>,
    dctx: &DispatchContext,
) -> Result<Value, McpError> {
    match tool_name {
        "cli.mutate" => {
            let mutation: CliMutation = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("cli.mutate: invalid params: {e}")))?;
            let result = cli_mutate_impl(&handle, mutation).await?;
            serde_json::to_value(result)
                .map_err(|e| mcp_error(format!("cli.mutate: encode result: {e}")))
        }
        "status" => {
            let text = status_impl(&handle).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_status(tokens);
            dctx.record_persistent_call(&handle, "status", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_delete" => {
            let request: MemoryDeleteParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("memory_delete: invalid params: {e}")))?;
            let text = memory_delete_impl(&handle, &request.id, request.dry_run).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_delete", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_confirm" => {
            let request: MemoryConfirmParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("memory_confirm: invalid params: {e}")))?;
            let text = memory_confirm_impl(&handle, &request.id, &request.outcome).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_confirm", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_write" => {
            let dry_run = params
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let entry: MemoryWriteBatchEntry = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("memory_write: invalid params: {e}")))?;
            let session = session_provenance(dctx);
            let text = memory_write_impl(&handle, &entry, session.as_deref(), dry_run).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_write", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_write_batch" => {
            let dry_run = params
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let entries_value = params
                .get("entries")
                .cloned()
                .ok_or_else(|| mcp_error("memory_write_batch: missing 'entries'"))?;
            let entries: Vec<MemoryWriteBatchEntry> = serde_json::from_value(entries_value)
                .map_err(|e| mcp_error(format!("memory_write_batch: invalid 'entries': {e}")))?;
            let session = session_provenance(dctx);
            let (text, count) =
                memory_write_batch_impl(&handle, &entries, session.as_deref(), dry_run).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_write_batch", tokens, count, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens, "count": count }))
        }
        "search" => {
            let sp: SearchParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("search: invalid params: {e}")))?;
            if sp.root.as_deref() == Some("*") {
                return Err(mcp_error(
                    "Cross-repo search via dispatch_call is not supported (no registry handle).",
                ));
            }
            let (text, count) = search_impl(&handle, &sp).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_search(tokens, count);
            dctx.record_persistent_call(&handle, "search", tokens, count, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens, "count": count }))
        }
        "get" => {
            let gp: GetParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("get: invalid params: {e}")))?;
            let (text, count, truncated) = get_impl(&handle, &gp).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_get(tokens);
            dctx.record_persistent_call(&handle, "get", tokens, count, truncated)
                .await;
            Ok(json!({ "text": text, "tokens": tokens, "count": count, "truncated": truncated }))
        }
        "memory_list" => {
            let request: MemoryListParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("memory_list: invalid params: {e}")))?;
            let (text, count) = memory_list_impl(&handle, request.limit, &request.sort).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_search(tokens, count);
            dctx.record_persistent_call(&handle, "memory_list", tokens, count, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens, "count": count }))
        }
        "update" => {
            // No params at all is the common case (`update` with no arguments),
            // and `null` does not deserialize into a struct even when every
            // field has a default.
            let request: UpdateRequest = if params.is_null() {
                UpdateRequest::default()
            } else {
                serde_json::from_value(params)
                    .map_err(|e| mcp_error(format!("update: invalid params: {e}")))?
            };
            let outcome = update_impl(&handle, &request).await?;
            let text = render_update_outcome(&outcome);
            let tokens = count_tokens(&text);
            dctx.metrics.record_update(tokens);
            dctx.record_persistent_call(&handle, "update", tokens, 1, false)
                .await;
            // `text` for the callers that print a summary, `outcome` for the
            // routed CLI, which has `--format` and renders the numbers itself.
            Ok(json!({ "text": text, "tokens": tokens, "outcome": outcome }))
        }
        "code_graph" => {
            let cp: CodeGraphParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("code_graph: invalid params: {e}")))?;
            let out = code_graph_impl(&handle, &cp).await?;
            let tokens = count_tokens(&out.text);
            dctx.record_persistent_call(&handle, "code_graph", tokens, 1, false)
                .await;
            // `text` for the agents that read the prose, `symbols` for the
            // programmatic clients that need locations — same shape as
            // `symbols_in_file`, so neither side parses the other's output.
            let symbols: Vec<Value> = out.symbols.iter().map(symbol_to_json).collect();
            Ok(json!({ "text": out.text, "tokens": tokens, "symbols": symbols }))
        }
        "graph" => {
            let gp: GraphParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("graph: invalid params: {e}")))?;
            let text = graph_impl(&handle, &gp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "graph", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "symbols_in_file" => {
            let sp: SymbolsInFileParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("symbols_in_file: invalid params: {e}")))?;
            let text = symbols_in_file_impl(&handle, &sp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "symbols_in_file", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "symbol_at_position" => {
            let sp: SymbolAtPositionParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("symbol_at_position: invalid params: {e}")))?;
            let text = symbol_at_position_impl(&handle, &sp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "symbol_at_position", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "code_find" => {
            let cp: CodeFindParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("code_find: invalid params: {e}")))?;
            let text = code_find_impl(&handle, &cp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "code_find", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "usage" => {
            let up: UsageParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("usage: invalid params: {e}")))?;
            let session_id = dctx.session_id.load(Ordering::Relaxed);
            let text = usage_impl(&handle, &up, session_id).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "usage", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "hook.session_start" => {
            let key = hook_session_key(&handle, &params);
            dctx.reset_hook_session(&key);
            let t0 = std::time::Instant::now();
            let session_cwd = hook_session_cwd(&params, &handle.root);
            let result = hook_session_start_impl(&handle, session_cwd.as_deref()).await;
            let ms = t0.elapsed().as_millis() as u64;
            let outcome = if result == json!({}) {
                "skipped"
            } else {
                "fired"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || {
                log_hook_event(root, "session_start", outcome, ms, budget);
            });
            record_hook_call(&handle, tool_name).await;
            Ok(result)
        }
        "hook.user_prompt_submit" => {
            let prompt = params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let key = hook_session_key(&handle, &params);
            let session = event_session(&params);
            let t0 = std::time::Instant::now();
            let result = hook_user_prompt_submit_impl_with_dedup(
                &handle,
                prompt,
                &session,
                Some((dctx, key)),
            )
            .await;
            let ms = t0.elapsed().as_millis() as u64;
            let outcome = if result == json!({}) {
                "skipped"
            } else {
                "fired"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || {
                log_hook_event(root, "user_prompt_submit", outcome, ms, budget);
            });
            record_hook_call(&handle, tool_name).await;
            Ok(result)
        }
        "hook.post_tool_use" => {
            let t0 = std::time::Instant::now();
            let result = hook_post_tool_use_impl(&handle, &params).await;
            let ms = t0.elapsed().as_millis() as u64;
            let outcome = if result == json!({}) {
                "skipped"
            } else {
                "fired"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || {
                log_hook_event(root, "post_tool_use", outcome, ms, budget);
            });
            record_hook_call(&handle, tool_name).await;
            Ok(json!({}))
        }
        "hook.pre_tool_use" => {
            let t0 = std::time::Instant::now();
            let result = hook_pre_tool_use_impl(&handle, &params).await;
            let ms = t0.elapsed().as_millis() as u64;
            // "mdkb_invocation" is the conversion signal: a Bash command that
            // actually runs mdkb. Tracking it against "fired" measures whether
            // the redirect suggestions land. (A fire produces a non-empty result;
            // an mdkb call produces none, so the checks don't overlap.)
            let outcome = if result != json!({}) {
                "fired"
            } else if params.get("tool_name").and_then(|v| v.as_str()) == Some("Bash")
                && params
                    .get("tool_input")
                    .and_then(|t| t.get("command"))
                    .and_then(|v| v.as_str())
                    .is_some_and(is_mdkb_invocation)
            {
                "mdkb_invocation"
            } else {
                "skipped"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || {
                log_hook_event(root, "pre_tool_use", outcome, ms, budget);
            });
            record_hook_call(&handle, tool_name).await;
            Ok(result)
        }
        "hook.stop" => {
            let key = hook_session_key(&handle, &params);
            // Returns immediately; distillation is detached inside hook_stop_impl.
            let result = hook_stop_impl(Arc::clone(&handle), &params, dctx);
            dctx.reset_hook_session(&key);
            let outcome = if result == json!({}) {
                "skipped"
            } else {
                "fired"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || log_hook_event(root, "stop", outcome, 0, budget));
            record_hook_call(&handle, tool_name).await;
            Ok(result)
        }
        other => Err(McpError {
            code: ErrorCode::METHOD_NOT_FOUND,
            message: format!("unknown tool: {other}").into(),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    #[test]
    fn fit_warmup_lines_respects_token_budget() {
        let lines = vec![
            "alpha beta gamma".to_string(),
            "delta epsilon zeta".to_string(),
            "eta theta iota".to_string(),
        ];
        // Budget 0: the first line still emits (over-budget single line beats empty).
        assert_eq!(fit_warmup_lines(&lines, 0), vec![lines[0].clone()]);
        // Ample budget: every line emits.
        assert_eq!(fit_warmup_lines(&lines, 10_000), lines);
        // Empty input: empty output.
        assert!(fit_warmup_lines(&[], 100).is_empty());
        // Budget = exactly the first line's tokens: the second would exceed → stop at one.
        let t0 = crate::metrics::tokens::count_tokens(&lines[0]);
        assert_eq!(fit_warmup_lines(&lines, t0), vec![lines[0].clone()]);
    }

    // ── Session cwd: the only signal of WHICH project a session is in ────────

    #[test]
    fn hook_session_cwd_accepts_a_directory_under_the_store_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("lattice");
        std::fs::create_dir_all(&project).unwrap();

        let params = json!({"cwd": project.display().to_string()});
        assert_eq!(hook_session_cwd(&params, &root), Some(project));
    }

    #[test]
    fn hook_session_cwd_accepts_the_store_root_itself() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let params = json!({"cwd": root.display().to_string()});
        assert_eq!(hook_session_cwd(&params, &root), Some(root));
    }

    #[test]
    fn hook_session_cwd_rejects_a_path_outside_the_store_root() {
        let tmp = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // Client-supplied: a cwd pointing anywhere else must not be trusted.
        let params = json!({"cwd": other.path().display().to_string()});
        assert_eq!(hook_session_cwd(&params, &root), None);
    }

    #[test]
    fn hook_session_cwd_rejects_relative_and_missing_values() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        assert_eq!(hook_session_cwd(&json!({"cwd": "lattice"}), &root), None);
        assert_eq!(hook_session_cwd(&json!({"cwd": ""}), &root), None);
        assert_eq!(hook_session_cwd(&json!({"cwd": 7}), &root), None);
        // No cwd at all — an older hook client — degrades to unscoped.
        assert_eq!(hook_session_cwd(&json!({}), &root), None);
    }

    #[test]
    fn hook_session_cwd_rejects_a_traversal_that_escapes_the_root() {
        let tmp = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let escape = format!("{}/../{}", root.display(), {
            let o = other.path().canonicalize().unwrap();
            o.file_name().unwrap().to_string_lossy().to_string()
        });
        // Both TempDirs live in the same parent, so `root/../<other>` resolves
        // outside the store: canonicalization must catch it, not the raw prefix.
        assert_eq!(hook_session_cwd(&json!({"cwd": escape}), &root), None);
    }

    // ── Project scope token: which of a store's many projects is in play ─────

    /// Registered collection names, as `project_scope_token` receives them.
    fn collections(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn scope_token_resolves_the_segment_below_root_when_a_collection_matches() {
        let root = std::path::Path::new("/store");
        let known = collections(&["lattice", "riscosity"]);

        // Directly below the root, and arbitrarily deep inside it: both resolve
        // to the project segment, never to a deeper directory name.
        assert_eq!(
            project_scope_token(root, Some(std::path::Path::new("/store/lattice")), &known),
            Some("lattice".to_string())
        );
        assert_eq!(
            project_scope_token(
                root,
                Some(std::path::Path::new("/store/lattice/src/otr")),
                &known
            ),
            Some("lattice".to_string())
        );
    }

    #[test]
    fn scope_token_matches_a_collection_name_case_insensitively() {
        let root = std::path::Path::new("/store");
        let known = collections(&["Lattice"]);

        // The token is normalized to lowercase so tag matching has one form.
        assert_eq!(
            project_scope_token(root, Some(std::path::Path::new("/store/LATTICE")), &known),
            Some("lattice".to_string())
        );
    }

    #[test]
    fn scope_token_is_none_when_there_is_no_project_to_scope_to() {
        let root = std::path::Path::new("/store");
        let known = collections(&["lattice"]);

        // At the store root there is no segment below it — the session is
        // working on the store itself, so warmup stays global.
        assert_eq!(
            project_scope_token(root, Some(std::path::Path::new("/store")), &known),
            None
        );
        // A folder with no registered collection is not a project.
        assert_eq!(
            project_scope_token(root, Some(std::path::Path::new("/store/scratch")), &known),
            None
        );
        // Outside the root entirely (defence in depth — the caller already
        // validated this) and no cwd at all: both unscoped.
        assert_eq!(
            project_scope_token(
                root,
                Some(std::path::Path::new("/elsewhere/lattice")),
                &known
            ),
            None
        );
        assert_eq!(project_scope_token(root, None, &known), None);
        // No collections registered at all: nothing can match.
        assert_eq!(
            project_scope_token(root, Some(std::path::Path::new("/store/lattice")), &[]),
            None
        );
    }

    #[test]
    fn scope_token_tags_decide_in_scope_case_insensitively() {
        use crate::store::memory::{EntryType, SourceType};
        let now = 1_000_000_000;
        let mut entry = warmup_entry(
            "e1",
            EntryType::Topic,
            "body",
            SourceType::UserStatement,
            1,
            0,
            now,
        );

        entry.tags = vec!["Lattice".to_string(), "otr".to_string()];
        assert!(entry_in_scope(&entry, "lattice"));
        assert!(!entry_in_scope(&entry, "riscosity"));

        // Cross-cutting entries carry no project tag: out of scope, never dropped.
        entry.tags = vec!["writing-style".to_string()];
        assert!(!entry_in_scope(&entry, "lattice"));

        entry.tags = vec![];
        assert!(!entry_in_scope(&entry, "lattice"));
    }

    fn write_quarantine_report(dir: &std::path::Path) {
        let corrupt = dir.join("index.sqlite.corrupt-1700000000");
        std::fs::write(&corrupt, b"corrupt").unwrap();
        crate::store::heal::write_report(
            &corrupt,
            crate::store::heal::Salvage {
                entries: 673,
                edges: 12,
                ..Default::default()
            },
        );
    }

    #[test]
    fn quarantine_banner_tells_operator_to_update_when_docs_still_empty() {
        let tmp = TempDir::new().unwrap();
        write_quarantine_report(tmp.path());

        let banner = format_quarantine_banner(tmp.path(), 0).unwrap();
        assert!(banner.contains("CORRUPT"));
        assert!(banner.contains("run `mdkb update`"));
    }

    #[test]
    fn quarantine_banner_omits_update_instruction_once_docs_are_back() {
        let tmp = TempDir::new().unwrap();
        write_quarantine_report(tmp.path());

        // A post-heal auto-rebuild (or a prior manual `mdkb update`) already
        // repopulated the documents table — the banner must not re-ask for it.
        let banner = format_quarantine_banner(tmp.path(), 2405).unwrap();
        assert!(banner.contains("already re-indexed automatically"));
        assert!(banner.contains("2405"));
        assert!(!banner.contains("run `mdkb update`"));
    }

    #[test]
    fn quarantine_banner_is_none_on_healthy_store() {
        let tmp = TempDir::new().unwrap();
        assert!(format_quarantine_banner(tmp.path(), 0).is_none());
    }

    /// The one `RepoHandle::from_shared` in these tests.
    ///
    /// Every handle a test builds differs only in its root and in a line or
    /// two of config, so the seven-argument constructor lives here once and
    /// the callers say what is different about theirs.
    fn handle_at(root: std::path::PathBuf, tweak: impl FnOnce(&mut Config)) -> Arc<RepoHandle> {
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        let mut config = Config::default();
        tweak(&mut config);
        Arc::new(RepoHandle::from_shared(
            root,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            config,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ))
    }

    /// A handle rooted at `tmp`, with the config the caller asks for.
    fn make_handle_with(tmp: &TempDir, tweak: impl FnOnce(&mut Config)) -> Arc<RepoHandle> {
        handle_at(tmp.path().to_path_buf(), tweak)
    }

    /// A handle rooted at `tmp/name` — a store nested under another one, which
    /// is how the ancestor-isolation tests build their child repo.
    fn nested_handle(
        tmp: &TempDir,
        name: &str,
        tweak: impl FnOnce(&mut Config),
    ) -> Arc<RepoHandle> {
        handle_at(tmp.path().join(name), tweak)
    }

    fn make_handle(tmp: &TempDir) -> Arc<RepoHandle> {
        // Recall tests exercise the injection mechanics, not the sigil gate; the
        // gate now defaults on, so disable it here to keep prompts un-prefixed.
        // The gate itself is covered by `require_sigil_gates_injection_*`.
        make_handle_with(tmp, |config| {
            config.hooks.user_prompt_submit_require_sigil = false;
        })
    }

    #[tokio::test]
    async fn memory_mutation_releases_corrupt_context_for_next_open() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        ensure_handle_context(&handle)
            .await
            .expect("initialize context");

        let error = {
            let mut slot = handle.ctx.lock().await;
            run_handle_memory_mutation(&mut slot, "corruption regression test", |ctx| {
                ctx.conn
                    .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                    .expect("checkpoint before truncation");
                let len = std::fs::metadata(&ctx.db_path).unwrap().len();
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&ctx.db_path)
                    .unwrap();
                file.set_len(len / 2).unwrap();
                Ok(())
            })
            .expect_err("fresh-connection verification must detect the torn file")
        };

        assert!(error.message.contains("connection was closed"), "{error:?}");
        assert!(
            handle.ctx.lock().await.is_none(),
            "corruption must release the connection and live lock"
        );

        ensure_handle_context(&handle)
            .await
            .expect("next open quarantines and rebuilds");
        assert!(handle.ctx.lock().await.is_some());
        assert!(
            std::fs::read_dir(tmp.path().join(".mdkb"))
                .unwrap()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")),
            "the released generation must be quarantined on the next open"
        );
    }

    /// Build a warmup candidate entry with controllable type/content/age.
    fn warmup_entry(
        id: &str,
        ty: crate::store::memory::EntryType,
        content: &str,
        source_type: crate::store::memory::SourceType,
        access_count: u64,
        age_days: i64,
        now: i64,
    ) -> crate::store::memory::MemoryEntry {
        let ts = now - age_days * 86_400;
        crate::store::memory::MemoryEntry {
            id: id.to_string(),
            title: format!("Title {id}"),
            content: content.to_string(),
            entry_type: ty,
            tags: vec!["t".to_string()],
            status: crate::store::memory::EntryStatus::Active,
            created_at: ts,
            updated_at: ts,
            superseded_by: None,
            access_count,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type,
            expires_at: None,
            due_at: None,
        }
    }

    #[test]
    fn rank_confidence_floor_excludes_low_signal_entries() {
        use crate::store::memory::{EntryType, SourceType};
        let now = 1_000_000_000;
        let entries = vec![
            // Fresh user_statement → confidence ~0.425 ≥ 0.25 → kept.
            warmup_entry(
                "fresh",
                EntryType::Topic,
                "c",
                SourceType::UserStatement,
                0,
                0,
                now,
            ),
            // Old inference (~40 days) → confidence < 0.25 → dropped.
            warmup_entry(
                "stale",
                EntryType::Prior,
                "c",
                SourceType::Inference,
                0,
                40,
                now,
            ),
        ];
        let ranked = rank_warmup_entries(entries, 10, 0.25, now, None);
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"fresh"));
        assert!(
            !ids.contains(&"stale"),
            "low-confidence entry excluded: {ids:?}"
        );
    }

    /// Build a warmup entry tagged for `project` (empty = cross-cutting).
    fn tagged_entry(
        id: &str,
        project: &str,
        access_count: u64,
        now: i64,
    ) -> crate::store::memory::MemoryEntry {
        use crate::store::memory::{EntryType, SourceType};
        let mut e = warmup_entry(
            id,
            EntryType::Topic,
            "content",
            SourceType::UserStatement,
            access_count,
            0,
            now,
        );
        e.tags = if project.is_empty() {
            vec![]
        } else {
            vec![project.to_string()]
        };
        e
    }

    #[test]
    fn rank_warmup_promotes_in_scope_entries_over_hotter_out_of_scope_ones() {
        let now = 1_000_000_000;
        let entries = vec![
            tagged_entry("riscosity-hot", "riscosity", 99, now),
            tagged_entry("lattice-cold", "lattice", 1, now),
        ];

        let ranked = rank_warmup_entries(entries, 10, 0.0, now, Some("lattice"));
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["lattice-cold", "riscosity-hot"],
            "the project in play outranks the globally hottest entry: {ids:?}"
        );
    }

    #[test]
    fn rank_warmup_keeps_out_of_scope_entries_so_cross_cutting_knowledge_survives() {
        let now = 1_000_000_000;
        let entries = vec![
            tagged_entry("riscosity-hot", "riscosity", 99, now),
            // No project tag: browser rules, writing style — must reach every project.
            tagged_entry("cross-cutting", "", 50, now),
            tagged_entry("lattice-cold", "lattice", 1, now),
        ];

        let ranked = rank_warmup_entries(entries, 10, 0.0, now, Some("lattice"));
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["lattice-cold", "riscosity-hot", "cross-cutting"],
            "scoping is a bias, not a filter — everything is still emitted: {ids:?}"
        );
    }

    #[test]
    fn rank_warmup_reserved_prior_slot_survives_scoping() {
        use crate::store::memory::{EntryType, SourceType};
        let now = 1_000_000_000;
        let mut entries: Vec<_> = (0..4)
            .map(|i| tagged_entry(&format!("lattice-{i}"), "lattice", 100 - i as u64, now))
            .collect();
        // A curated prior nobody has read yet: it must still claim the last slot.
        let mut prior = warmup_entry(
            "curated-prior",
            EntryType::Prior,
            "content",
            SourceType::UserStatement,
            0,
            0,
            now,
        );
        prior.confirmations = 10;
        prior.tags = vec!["riscosity".to_string()];
        entries.push(prior);

        let ranked = rank_warmup_entries(entries, 3, 0.0, now, Some("lattice"));
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(
            ids.last(),
            Some(&"curated-prior"),
            "the reserved curated-prior slot is untouched by scoping: {ids:?}"
        );
    }

    #[test]
    fn rank_warmup_unscoped_ordering_is_the_pre_scoping_ordering() {
        let now = 1_000_000_000;
        let build = || {
            vec![
                tagged_entry("riscosity-hot", "riscosity", 99, now),
                tagged_entry("cross-cutting", "", 50, now),
                tagged_entry("lattice-cold", "lattice", 1, now),
            ]
        };

        let ranked = rank_warmup_entries(build(), 10, 0.0, now, None);
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["riscosity-hot", "cross-cutting", "lattice-cold"],
            "with no scope the sort key stays access_count DESC: {ids:?}"
        );

        // A scope token nothing is tagged with must not perturb that order either.
        let ranked = rank_warmup_entries(build(), 10, 0.0, now, Some("evoke"));
        let ids: Vec<&str> = ranked.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["riscosity-hot", "cross-cutting", "lattice-cold"]);
    }

    fn make_dctx() -> DispatchContext {
        DispatchContext {
            metrics: Arc::new(UsageMetrics::new()),
            session_id: Arc::new(AtomicI64::new(0)),
            persistent_call_count: Arc::new(AtomicU64::new(0)),
            optimize_interval_calls: 200,
            hook_dedup: Arc::new(StdMutex::new(Default::default())),
            background: None,
        }
    }

    #[tokio::test]
    async fn cli_mutate_dispatch_returns_the_typed_result() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let result = dispatch_call(
            "cli.mutate",
            json!({
                "command": "memory_add",
                "id": "typed-route",
                "title": "Typed route",
                "entry_type": "topic",
                "tags": null,
                "content": "written by daemon dispatch",
                "source_path": null,
                "ttl": null,
                "due_in": null,
                "source_type": null
            }),
            Arc::clone(&handle),
            &make_dctx(),
        )
        .await
        .expect("cli mutation dispatch");
        assert_eq!(result["result"], "memory_added");

        let context = handle.ctx.lock().await;
        let entry = crate::store::memory::get_entry_without_tracking(
            &context.as_ref().unwrap().conn,
            "typed-route",
        )
        .unwrap();
        assert!(entry.is_some());
    }

    fn additional_context(result: &Value) -> &str {
        result
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    #[tokio::test]
    async fn status_impl_returns_body_for_empty_repo() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let body = status_impl(&handle).await.expect("status impl");

        assert!(body.contains("## Index Status"), "body: {body}");
        assert!(body.contains("Documents:"), "body: {body}");
        assert!(body.contains("## Collections"), "body: {body}");
    }

    #[tokio::test]
    async fn dispatch_call_routes_status_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call("status", Value::Null, handle, &dctx)
            .await
            .expect("dispatch");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("## Index Status"), "result: {result}");
        assert!(
            result.get("tokens").and_then(Value::as_u64).unwrap_or(0) > 0,
            "tokens missing: {result}"
        );
    }

    /// A routed `update` must do what it was asked, not merely what it was
    /// named.
    ///
    /// The daemon used to take the method and discard the params: `--force` was
    /// parsed, sent and dropped, so a config change never reached the
    /// already-indexed documents, and `mdkb update --files one.md` reindexed
    /// the entire tree. Both printed a success summary, which is why neither
    /// was noticed.
    #[tokio::test]
    async fn update_honours_force_and_file_scoping() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        ensure_handle_context(&handle).await.expect("init ctx");

        let docs = tmp.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.md"), "# A\n\nalpha\n").unwrap();
        std::fs::write(docs.join("b.md"), "# B\n\nbeta\n").unwrap();
        {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard.as_ref().unwrap();
            let now = chrono::Utc::now().timestamp();
            crate::store::collections::add_collection(
                &ctx.conn,
                &crate::domain::Collection {
                    name: "docs".to_string(),
                    path: "./docs".to_string(),
                    pattern: "**/*.md".to_string(),
                    source: "manual".to_string(),
                    created_at: now,
                    updated_at: now,
                },
            )
            .expect("register collection");
        }

        let first = update_impl(&handle, &UpdateRequest::default())
            .await
            .expect("initial update");
        assert_eq!(first.docs.added, 2, "both files must index: {first:?}");

        // Nothing changed on disk, so a plain re-run touches nothing...
        let plain = update_impl(&handle, &UpdateRequest::default())
            .await
            .expect("plain update");
        assert_eq!(plain.docs.updated, 0, "{plain:?}");
        assert_eq!(plain.docs.unchanged, 2, "{plain:?}");

        // ...but --force reindexes regardless of mtime, which is the whole
        // point of the flag: the change is in the config, not the files.
        let forced = update_impl(
            &handle,
            &UpdateRequest {
                files: Vec::new(),
                force: true,
            },
        )
        .await
        .expect("forced update");
        assert_eq!(
            forced.docs.updated, 2,
            "--force must reach handle_update_force: {forced:?}"
        );

        // And naming one file must touch exactly that file.
        let scoped = update_impl(
            &handle,
            &UpdateRequest {
                files: vec!["docs/a.md".to_string()],
                force: true,
            },
        )
        .await
        .expect("scoped update");
        assert_eq!(
            scoped.docs.updated, 1,
            "a targeted update must not reindex the whole tree: {scoped:?}"
        );
        assert!(
            scoped.sessions.is_none(),
            "a targeted update names files, and no session has a name to give: {scoped:?}"
        );
    }

    /// The routed CLI reads `outcome`; everything else reads `text`. Both must
    /// be there, and the numbers must be the ones the phases produced.
    #[tokio::test]
    async fn dispatch_call_update_returns_numbers_and_text() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call("update", json!({ "force": true }), handle, &dctx)
            .await
            .expect("dispatch");

        let outcome: UpdateOutcome =
            serde_json::from_value(result["outcome"].clone()).expect("outcome must deserialize");
        assert_eq!(outcome.docs.added, 0, "empty repo indexes nothing");
        assert!(
            result["text"]
                .as_str()
                .unwrap_or("")
                .contains("## Documents"),
            "the rendered summary must survive for the callers that print it: {result}"
        );
    }

    #[tokio::test]
    async fn dispatch_call_unknown_tool_is_method_not_found() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("no_such_tool", Value::Null, handle, &dctx)
            .await
            .expect_err("must error");

        assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);
    }

    async fn seed_memory_entry(handle: &RepoHandle, id: &str) {
        ensure_handle_context(handle).await.expect("init ctx");
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let now = chrono::Utc::now().timestamp();
        let entry = crate::store::memory::MemoryEntry {
            id: id.to_string(),
            title: format!("Title for {id}"),
            // The identifier is load-bearing, not decoration: memory recall
            // is gated absolutely, and with no embedding service under test
            // the only arm that can admit an entry is a strong lexical match.
            // Every prompt that expects this entry names `recall_gate_fixture`.
            content: "Some content about the topic: the recall_gate_fixture knob.".to_string(),
            entry_type: crate::store::memory::EntryType::Topic,
            tags: vec!["alpha".to_string()],
            status: crate::store::memory::EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: crate::store::memory::SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        crate::store::memory::add_entry(&ctx.conn, &entry).expect("seed entry");
    }

    async fn seed_stale_handoff_entry(handle: &RepoHandle, id: &str, content: &str) {
        ensure_handle_context(handle).await.expect("init ctx");
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let now = chrono::Utc::now().timestamp();
        let entry = crate::store::memory::MemoryEntry {
            id: id.to_string(),
            title: format!("Title for {id}"),
            content: content.to_string(),
            entry_type: crate::store::memory::EntryType::Handoff,
            tags: vec!["recall".to_string()],
            status: crate::store::memory::EntryStatus::Active,
            created_at: now - 175 * 86_400,
            updated_at: now - 175 * 86_400,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: crate::store::memory::SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        assert!(
            entry.confidence() < 0.07,
            "control: stale handoff confidence should be about 0.06"
        );
        crate::store::memory::add_entry(&ctx.conn, &entry).expect("seed stale handoff");
    }

    /// Story 087, criterion 6. The table was written down before the code.
    ///
    /// Four confirmation histories, with the confidence each produces and
    /// whether the entry may be injected unasked. `Topic` is durable so decay
    /// is 1.0, and `OfficialDocs` has authority 1.0, so confidence here IS the
    /// belief term — the numbers are the formula, not an artefact of the fixture.
    ///
    /// | history        | c  | r | belief = (1+c)/(2+c+3r) | last signal | injectable |
    /// |----------------|----|---|-------------------------|-------------|------------|
    /// | 0c/1r          |  0 | 1 | 1/5  = 0.2000           | refuted     | no         |
    /// | 3c/1r          |  3 | 1 | 4/8  = 0.5000           | refuted     | no         |
    /// | 20c/1r recent  | 20 | 1 | 21/25 = 0.8400          | refuted     | no         |
    /// | 1r then 2c     |  2 | 1 | 3/7  = 0.4286           | confirmed   | YES        |
    ///
    /// Row 3 is the one that matters: 0.84 clears every threshold in the
    /// system, including the prior gate at 0.7, and it is still not injected.
    /// Row 4 is its mirror: the lowest confidence of the four, and the only one
    /// admitted, because the last thing anyone said about it is that it holds.
    /// Together they are the claim — the score is never the safety mechanism.
    #[test]
    fn a_refuted_entry_is_not_injected_however_well_confirmed_it_is() {
        let now = chrono::Utc::now().timestamp();
        let history =
            |id: &str, confirmations: u32, corrections: u32, confirmed_ago: Option<i64>| {
                crate::store::memory::MemoryEntry {
                    id: id.to_string(),
                    title: id.to_string(),
                    content: "Refutation table fixture".to_string(),
                    entry_type: crate::store::memory::EntryType::Topic,
                    tags: vec![],
                    status: crate::store::memory::EntryStatus::Active,
                    created_at: now - 10 * 86_400,
                    updated_at: now - 10 * 86_400,
                    superseded_by: None,
                    access_count: 0,
                    last_accessed: None,
                    source_path: None,
                    confirmations,
                    corrections,
                    last_confirmed_at: confirmed_ago.map(|ago| now - ago),
                    last_refuted_at: (corrections > 0).then_some(now - 3600),
                    source_type: crate::store::memory::SourceType::OfficialDocs,
                    expires_at: None,
                    due_at: None,
                }
            };

        // (entry, expected confidence, expected injection eligibility)
        let table = [
            (history("zero-c-one-r", 0, 1, None), 0.2000, false),
            (history("three-c-one-r", 3, 1, Some(7200)), 0.5000, false),
            (history("twenty-c-one-r", 20, 1, Some(7200)), 0.8400, false),
            (history("one-r-then-two-c", 2, 1, Some(1800)), 0.4286, true),
        ];

        for (entry, expected_confidence, _) in &table {
            let got = entry.confidence_at(now);
            assert!(
                (got - expected_confidence).abs() < 0.001,
                "{}: confidence should be {expected_confidence}, got {got}",
                entry.id
            );
        }

        // The one row that clears the prior gate is still refuted. Without the
        // suppression its score alone would carry it into the prompt.
        assert!(
            table[2].0.confidence_at(now) >= PRIOR_CONFIDENCE_GATE,
            "control: the 20c/1r row must out-score every threshold in the system"
        );

        let scored = table
            .iter()
            .map(|(entry, _, _)| memory::ScoredMemoryEntry {
                entry: entry.clone(),
                score: entry.confidence_at(now),
                distance: Some(0.4),
                strong_lexical: true,
            })
            .collect();
        let injected: Vec<String> = injectable(scored).into_iter().map(|e| e.id).collect();

        let expected: Vec<String> = table
            .iter()
            .filter(|(_, _, eligible)| *eligible)
            .map(|(entry, _, _)| entry.id.clone())
            .collect();
        assert_eq!(
            injected, expected,
            "only the reconfirmed entry may be injected unasked"
        );
    }

    /// Story 083 replaced the `min_recall_score` floor this test used to
    /// assert. That floor ran over `rrf_norm * 0.7 + confidence * 0.3` after
    /// max-normalization, so it could not reject an irrelevant entry — the
    /// best match to any prompt normalizes to 1.0 — and it *could* reject a
    /// relevant one for being unconfirmed. The subject under test is now the
    /// absolute gate, and what is asserted here is the half that belongs to
    /// this layer: confidence has left the admission decision entirely.
    #[test]
    fn confidence_does_not_decide_injection() {
        let now = chrono::Utc::now().timestamp();
        let stale = crate::store::memory::MemoryEntry {
            id: "stale-but-relevant".to_string(),
            title: "Stale but relevant".to_string(),
            content: "Recall gate test entry".to_string(),
            entry_type: crate::store::memory::EntryType::Handoff,
            tags: vec![],
            status: crate::store::memory::EntryStatus::Active,
            created_at: now - 175 * 86_400,
            updated_at: now - 175 * 86_400,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: crate::store::memory::SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        assert!(
            stale.confidence() < 0.07,
            "control: entry confidence should be about 0.06"
        );
        let fresh = crate::store::memory::MemoryEntry {
            id: "fresh".to_string(),
            created_at: now,
            updated_at: now,
            ..stale.clone()
        };
        assert!(
            fresh.confidence() > stale.confidence(),
            "control: the two entries must differ in confidence"
        );

        // Both were admitted by the store, one with a far lower final score.
        // Nothing here may re-filter them: the gate already ran, absolutely.
        let injected = injectable(vec![
            memory::ScoredMemoryEntry {
                entry: stale,
                score: 0.29,
                distance: Some(0.4),
                strong_lexical: false,
            },
            memory::ScoredMemoryEntry {
                entry: fresh,
                score: 0.83,
                distance: Some(0.4),
                strong_lexical: false,
            },
        ]);

        assert_eq!(
            injected
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            vec!["stale-but-relevant", "fresh"],
            "a 0.06-confidence entry the store admitted is still injected"
        );
    }

    /// The gate's end-to-end consequence on the hook path, in the
    /// configuration the tests run in: no embedding service, so no distance,
    /// so the semantic arm cannot speak and only a strong lexical match is
    /// admitted. A prompt that merely shares OR-expanded common words with an
    /// entry injects nothing — that is the whole point.
    #[tokio::test]
    async fn without_an_embedding_only_a_strong_lexical_match_is_injected() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.hooks.user_prompt_submit_require_sigil = false;
            config.hooks.recall_docs_limit = 0;
            config.hooks.recall_limit = 2;
        });
        // Shares the identifier `recall_gate_target` with the prompt: admitted
        // on the lexical arm, which exists because embeddings are weak here.
        seed_stale_handoff_entry(
            &handle,
            "identifier-match",
            "the recall_gate_target knob is read once per prompt",
        )
        .await;
        // Shares only the ordinary word "knob": in the BM25 result set, and
        // rejected anyway.
        seed_stale_handoff_entry(&handle, "common-word-only", "another knob entirely").await;

        let output = hook_user_prompt_submit_impl(&handle, "who reads recall_gate_target").await;
        let context = additional_context(&output);
        assert!(
            context.contains("identifier-match"),
            "an identifier the prompt wrote out is strong evidence: {context}"
        );
        assert!(
            !context.contains("common-word-only"),
            "one shared common word must not open the gate: {context}"
        );
    }

    /// The sentence story 083 exists for: a prompt about something we store
    /// nothing on gets nothing, not the best of a bad set.
    ///
    /// The second half is what makes it a real test. The same prompt with the
    /// floor at 0.0 does inject the entry, so the BM25 leg demonstrably found
    /// it and the floor is what rejected it — not an empty result set that
    /// would have been empty anyway.
    #[tokio::test]
    async fn a_prompt_unrelated_to_every_entry_injects_nothing() {
        const PROMPT: &str = "how large should the quarterly travel budget for the team be";

        async fn recall_with_floor(floor: f32) -> String {
            let tmp = TempDir::new().unwrap();
            let handle = make_handle_with(&tmp, |config| {
                config.hooks.user_prompt_submit_require_sigil = false;
                config.hooks.recall_docs_limit = 0;
                config.hooks.recall_limit = 5;
                config.search.memory.min_recall_cosine = floor;
            });
            // Shares exactly one ordinary word with the prompt ("budget"), so
            // the OR-expanded recall query matches it on the BM25 leg.
            seed_stale_handoff_entry(
                &handle,
                "shares-one-word",
                "requests retry with an exponential backoff budget",
            )
            .await;
            let output = hook_user_prompt_submit_impl(&handle, PROMPT).await;
            serde_json::to_string(&output).unwrap()
        }

        let gated = recall_with_floor(crate::config::MIN_RECALL_COSINE_DEFAULT).await;
        assert_eq!(gated, "{}", "an unrelated prompt must inject nothing");

        let ungated = recall_with_floor(0.0).await;
        assert!(
            ungated.contains("shares-one-word"),
            "the BM25 leg must have found the entry for the gate to be what \
             rejected it, yet with no floor nothing was injected either: {ungated}"
        );
    }

    /// Seed an indexed document so the recall documents leg has something to
    /// find. Content drives BM25 (the embedding service is absent under test,
    /// so the hybrid search degrades to BM25-only — deterministic).
    async fn seed_document(handle: &RepoHandle, path: &str, title: &str, content: &str) {
        ensure_handle_context(handle).await.expect("init ctx");
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let now = chrono::Utc::now().timestamp();
        // documents.collection is a FK — register it once, ignore re-adds.
        let _ = crate::store::collections::add_collection(
            &ctx.conn,
            &crate::domain::Collection {
                name: "default".to_string(),
                path: "./docs".to_string(),
                pattern: "**/*.md".to_string(),
                source: "manual".to_string(),
                created_at: now,
                updated_at: now,
            },
        );
        let doc = crate::domain::Document {
            id: 0,
            collection: "default".to_string(),
            relative_path: path.to_string(),
            hash: crate::store::documents::compute_hash(content),
            title: Some(title.to_string()),
            metadata: None,
            file_modified_at: now,
            indexed_at: now,
            status: Some("current".to_string()),
        };
        crate::store::documents::index_document(&ctx.conn, &doc, content).expect("seed doc");
    }

    #[tokio::test]
    async fn recall_injects_matching_docs_alongside_memory() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "recall-mem").await;
        seed_document(
            &handle,
            "docs/quarantine.md",
            "Quarantine handling",
            "The autoheal routine quarantines a corrupt index before rebuilding it.",
        )
        .await;

        let out = hook_user_prompt_submit_impl(&handle, "how does quarantine autoheal work").await;
        let body = additional_context(&out);
        assert!(
            body.contains("## mdkb: matching docs"),
            "documents leg should emit its own block: {body}"
        );
        assert!(
            body.contains("docs/quarantine.md"),
            "matching doc path must be injected: {body}"
        );
        assert!(
            body.contains("Quarantine handling"),
            "doc title carries the signal that makes the path worth opening: {body}"
        );
    }

    #[tokio::test]
    async fn recall_docs_limit_zero_injects_memory_only() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.hooks.user_prompt_submit_require_sigil = false;
            config.hooks.recall_docs_limit = 0;
        });
        seed_memory_entry(&handle, "topic-mem").await;
        seed_document(
            &handle,
            "docs/topic.md",
            "Topic doc",
            "Some content about the topic.",
        )
        .await;

        let out = hook_user_prompt_submit_impl(
            &handle,
            "what about the recall_gate_fixture topic content",
        )
        .await;
        let body = additional_context(&out);
        assert!(
            body.contains("topic-mem"),
            "memory recall must still fire: {body}"
        );
        assert!(
            !body.contains("docs/topic.md"),
            "recall_docs_limit = 0 must suppress the documents leg: {body}"
        );
    }

    #[tokio::test]
    async fn recall_docs_limit_caps_injected_documents() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.hooks.user_prompt_submit_require_sigil = false;
            config.hooks.recall_docs_limit = 2;
        });
        for i in 0..5 {
            seed_document(
                &handle,
                &format!("docs/quarantine-{i}.md"),
                &format!("Quarantine {i}"),
                "The autoheal routine quarantines a corrupt index before rebuilding it.",
            )
            .await;
        }

        let out = hook_user_prompt_submit_impl(&handle, "quarantine autoheal rebuilding").await;
        let body = additional_context(&out);
        let injected = body.matches("docs/quarantine-").count();
        assert_eq!(
            injected, 2,
            "5 matching docs must be capped at recall_docs_limit = 2: {body}"
        );
    }

    #[tokio::test]
    async fn recall_docs_leg_is_gated_by_the_sigil() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.hooks.user_prompt_submit_require_sigil = true;
        });
        seed_document(
            &handle,
            "docs/quarantine.md",
            "Quarantine handling",
            "The autoheal routine quarantines a corrupt index before rebuilding it.",
        )
        .await;

        // A doc-only match (no memory, no priors) must still respect the gate.
        let plain = hook_user_prompt_submit_impl(&handle, "quarantine autoheal rebuilding").await;
        assert_eq!(plain, json!({}), "sigil-less prompt must not inject docs");

        let opted = hook_user_prompt_submit_impl(&handle, "* quarantine autoheal rebuilding").await;
        assert!(
            additional_context(&opted).contains("docs/quarantine.md"),
            "sigil-prefixed prompt should surface matching docs: {opted}"
        );
    }

    #[tokio::test]
    async fn user_prompt_submit_dedups_memory_within_same_hook_session() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "dedup-topic").await;

        let params = json!({
            "prompt": "what do we know about the recall_gate_fixture topic content",
            "session_id": "s1"
        });
        let first = dispatch_call(
            "hook.user_prompt_submit",
            params.clone(),
            Arc::clone(&handle),
            &dctx,
        )
        .await
        .expect("first hook");
        assert!(
            additional_context(&first).contains("dedup-topic"),
            "first hook should inject memory: {first}"
        );

        let second = dispatch_call("hook.user_prompt_submit", params, handle, &dctx)
            .await
            .expect("second hook");
        assert_eq!(second, json!({}), "same session must not reinject memory");
    }

    #[tokio::test]
    async fn user_prompt_submit_allows_same_memory_in_different_hook_session() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "cross-session-topic").await;

        for session_id in ["s1", "s2"] {
            let result = dispatch_call(
                "hook.user_prompt_submit",
                json!({
                    "prompt": "what do we know about the recall_gate_fixture topic content",
                    "session_id": session_id
                }),
                Arc::clone(&handle),
                &dctx,
            )
            .await
            .expect("hook");
            assert!(
                additional_context(&result).contains("cross-session-topic"),
                "session {session_id} should inject memory: {result}"
            );
        }
    }

    #[tokio::test]
    async fn user_prompt_submit_clear_resets_session_dedup() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "clear-topic").await;

        let params = json!({
            "prompt": "what do we know about the recall_gate_fixture topic content",
            "session_id": "s1"
        });
        let first = dispatch_call(
            "hook.user_prompt_submit",
            params.clone(),
            Arc::clone(&handle),
            &dctx,
        )
        .await
        .expect("first hook");
        assert!(
            additional_context(&first).contains("clear-topic"),
            "first hook should inject memory: {first}"
        );

        let repeated = dispatch_call(
            "hook.user_prompt_submit",
            params.clone(),
            Arc::clone(&handle),
            &dctx,
        )
        .await
        .expect("repeat hook");
        assert_eq!(repeated, json!({}), "repeat should be silent before clear");

        let clear = dispatch_call(
            "hook.user_prompt_submit",
            json!({"prompt": "/clear", "session_id": "s1"}),
            Arc::clone(&handle),
            &dctx,
        )
        .await
        .expect("clear hook");
        assert_eq!(clear, json!({}), "clear command should remain silent");

        let after_clear = dispatch_call("hook.user_prompt_submit", params, handle, &dctx)
            .await
            .expect("after clear hook");
        assert!(
            additional_context(&after_clear).contains("clear-topic"),
            "memory should be eligible again after clear: {after_clear}"
        );
    }

    #[tokio::test]
    async fn warmup_does_not_layer_ancestor_store() {
        let tmp = TempDir::new().unwrap();
        // Parent store (the ancestor) with its own memory entry.
        let parent = make_handle(&tmp);
        seed_memory_entry(&parent, "parent-mem").await;

        // Primary store nested under the parent.
        let primary = nested_handle(&tmp, "nested-repo", |config| {
            config.hooks.user_prompt_submit_require_sigil = false;
        });
        seed_memory_entry(&primary, "child-mem").await;

        let out = hook_session_start_impl(&primary, None).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(body.contains("child-mem"), "primary entry missing: {body}");
        assert!(
            !body.contains("parent-mem"),
            "ancestor entry leaked into warmup: {body}"
        );
    }

    #[tokio::test]
    async fn recall_does_not_layer_ancestor_store() {
        let tmp = TempDir::new().unwrap();
        // Parent store (the ancestor) with its own memory entry.
        let parent = make_handle(&tmp);
        seed_memory_entry(&parent, "parent-mem").await;

        // Primary store nested under the parent.
        let primary = nested_handle(&tmp, "nested-repo", |config| {
            config.hooks.user_prompt_submit_require_sigil = false;
        });
        seed_memory_entry(&primary, "child-mem").await;

        // Prompt terms match the seeded entries' content ("...about the topic.").
        let out = hook_user_prompt_submit_impl(
            &primary,
            "what do we know about the recall_gate_fixture topic content",
        )
        .await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            body.contains("child-mem"),
            "primary recall entry missing: {body}"
        );
        assert!(
            !body.contains("parent-mem"),
            "ancestor recall entry leaked into hook context: {body}"
        );
    }

    #[tokio::test]
    async fn require_sigil_gates_injection_to_star_prefixed_prompts() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.hooks.user_prompt_submit_require_sigil = true;
        });
        seed_memory_entry(&handle, "sigil-mem").await;

        // Same recall-worthy prompt WITHOUT the sigil: no injection at all.
        let plain = hook_user_prompt_submit_impl(
            &handle,
            "what do we know about the recall_gate_fixture topic content",
        )
        .await;
        assert_eq!(
            plain,
            json!({}),
            "sigil-less prompt must not inject: {plain}"
        );

        // WITH the `*` sigil: recall fires and the sigil never leaks into output.
        let opted = hook_user_prompt_submit_impl(
            &handle,
            "* what do we know about the recall_gate_fixture topic content",
        )
        .await;
        let body = additional_context(&opted);
        assert!(
            body.contains("sigil-mem"),
            "sigil-prefixed prompt should surface recall: {body}"
        );
    }

    /// Seed a `Prior` memory entry with explicit confirmations so the test can
    /// control its confidence() above/below the recall gate.
    async fn seed_prior_entry(handle: &RepoHandle, id: &str, confirmations: u32) {
        ensure_handle_context(handle).await.expect("init ctx");
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let now = chrono::Utc::now().timestamp();
        let entry = crate::store::memory::MemoryEntry {
            id: id.to_string(),
            title: format!("Prior {id}"),
            content: "Prefer ripgrep over grep for codebase searches.".to_string(),
            entry_type: crate::store::memory::EntryType::Prior,
            tags: vec!["search".to_string()],
            status: crate::store::memory::EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations,
            corrections: 0,
            last_confirmed_at: Some(now),
            last_refuted_at: None,
            source_type: crate::store::memory::SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        crate::store::memory::add_entry(&ctx.conn, &entry).expect("seed prior");
    }

    #[tokio::test]
    async fn recall_surfaces_high_confidence_prior_and_gates_low_confidence() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        // High-confidence prior: many confirmations -> confidence() >= 0.7.
        seed_prior_entry(&handle, "prior-proof-high", 5).await;
        // Fresh prior: zero confirmations -> confidence() ~0.425, below the gate.
        seed_prior_entry(&handle, "prior-proof-low", 0).await;

        // Sanity-check the confidence math the gate relies on.
        {
            let ctx_guard = handle.ctx.lock().await;
            let conn = &ctx_guard.as_ref().unwrap().conn;
            let high = crate::store::memory::get_entry(conn, "prior-proof-high")
                .unwrap()
                .unwrap();
            let low = crate::store::memory::get_entry(conn, "prior-proof-low")
                .unwrap()
                .unwrap();
            assert!(
                high.confidence() >= 0.7,
                "high prior confidence below gate: {}",
                high.confidence()
            );
            assert!(
                low.confidence() < 0.7,
                "low prior confidence not below gate: {}",
                low.confidence()
            );
        }

        let out =
            hook_user_prompt_submit_impl(&handle, "should I use ripgrep or grep for searches")
                .await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            body.contains("prior-proof-high"),
            "high-confidence prior should surface in recall: {body}"
        );
        assert!(
            !body.contains("prior-proof-low"),
            "low-confidence prior must be gated out of recall: {body}"
        );
    }

    #[tokio::test]
    async fn recall_expands_one_hop_memory_neighbors_excluding_superseded() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        // Seed A (matches the prompt); B and C do NOT match the prompt, so they can
        // only appear via edge expansion.
        seed_topic_with_content(
            &handle,
            "zephyr-runbook",
            "Zephyr calibration runbook steps",
            0,
        )
        .await;
        seed_topic_with_content(
            &handle,
            "bolt-detail",
            "Bolt torque is four newton meters",
            0,
        )
        .await;
        seed_topic_with_content(&handle, "dead-detail", "Retired coolant mixture ratio", 0).await;
        {
            let guard = handle.ctx.lock().await;
            let conn = &guard.as_ref().unwrap().conn;
            memory_graph::add_edge(
                conn,
                "zephyr-runbook",
                "bolt-detail",
                TargetKind::Memory,
                MemoryRelation::Supports,
            )
            .unwrap();
            memory_graph::add_edge(
                conn,
                "zephyr-runbook",
                "dead-detail",
                TargetKind::Memory,
                MemoryRelation::RelatesTo,
            )
            .unwrap();
            // dead-detail is superseded → must be excluded from expansion.
            conn.execute(
                "UPDATE memory_entries SET status='superseded' WHERE id='dead-detail'",
                [],
            )
            .unwrap();
        }

        let out = hook_user_prompt_submit_impl(&handle, "zephyr calibration runbook").await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            body.contains("zephyr-runbook"),
            "seed A should recall: {body}"
        );
        assert!(
            body.contains("bolt-detail"),
            "neighbor B should be expanded: {body}"
        );
        assert!(
            body.contains("(via supports)"),
            "neighbor annotated with relation: {body}"
        );
        assert!(
            !body.contains("dead-detail"),
            "superseded neighbor must be excluded: {body}"
        );
    }

    /// Seed `n` entries (`n0`..) where `n0` has 3 outgoing edges, then return the
    /// MINIMUM per-call expansion time over many iterations. The min (least-
    /// preempted run) reflects true CPU cost and is stable under parallel test
    /// load, unlike an absolute p95 which flakes when the suite saturates cores.
    fn min_expand_us(conn: &rusqlite::Connection, n: usize) -> u128 {
        for i in 0..n {
            conn.execute(
                "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
                 VALUES (?1, ?2, 'body', 'topic', 1, 1)",
                rusqlite::params![format!("n{i}"), format!("Title {i}")],
            )
            .unwrap();
        }
        for j in 1..=3 {
            memory_graph::add_edge(
                conn,
                "n0",
                &format!("n{j}"),
                TargetKind::Memory,
                MemoryRelation::Supports,
            )
            .unwrap();
        }
        let seeds = vec![memory::get_entry(conn, "n0").unwrap().unwrap()];
        let mut best = u128::MAX;
        for _ in 0..50 {
            let t = std::time::Instant::now();
            let out = expand_recall_neighbors(conn, &seeds, 2, 3).unwrap();
            best = best.min(t.elapsed().as_micros());
            assert_eq!(out.len(), 3, "must expand exactly the 3 capped neighbors");
        }
        best
    }

    #[test]
    fn code_index_hits_block_states_what_it_hid() {
        use crate::code::symbol::{Symbol, Visibility};
        use crate::code::types::{FileId, Range, SymbolId, SymbolKind};

        let sym = |path: &str, line: u32| Symbol {
            id: SymbolId::new(1).unwrap(),
            name: "handler".into(),
            kind: SymbolKind::Function,
            file_id: FileId::new(1).unwrap(),
            range: Range::new(line, 0, line, 1),
            file_path: path.into(),
            signature: None,
            doc_comment: None,
            module_path: None,
            visibility: Visibility::Public,
            scope_context: None,
        };
        let six: Vec<Symbol> = (0..6).map(|i| sym("a.rs", i * 10)).collect();

        // Truncated: the block must say so. A hook that silently showed 5 of 6
        // taught the agent that there were 5.
        let block = render_code_index_hits("handler", &six, 5);
        assert_eq!(block.matches("- a.rs:").count(), 5);
        assert!(block.contains("and 1 more"), "{block}");

        // Not truncated: no claim about hidden definitions.
        let block = render_code_index_hits("handler", &six[..3], 5);
        assert_eq!(block.matches("- a.rs:").count(), 3);
        assert!(!block.contains("more definition"), "{block}");

        // 0-based tree-sitter rows are displayed 1-based.
        assert!(block.contains("- a.rs:1 "), "{block}");
    }

    #[test]
    fn a_non_rust_call_answer_discloses_that_receiver_inference_is_rust_only() {
        use crate::code::storage::TIER_UNPLACED;
        use crate::core::code::UnplacedCalls;

        let unknown = UnplacedCalls {
            external: Vec::new(),
            unknown: vec!["fetch".to_string()],
        };
        let none = UnplacedCalls::default();

        // A TypeScript method call whose receiver nothing inferred: the reader
        // is about to trust a name match, so say what placed it.
        let note = receiver_inference_note("src/api/client.ts", &unknown, &[]);
        assert!(note.contains("Rust only"), "{note}");
        assert!(note.contains("written name alone"), "{note}");

        // Same file, but the call did resolve — on the bare name, at the
        // unplaced tier. Equally in need of the disclosure.
        assert!(
            !receiver_inference_note("src/api/client.ts", &none, &[(TIER_UNPLACED, false)])
                .is_empty()
        );

        // Rust: inference ran. An unplaced call there means it ran and failed,
        // which `unplaced_suffix` already reports.
        assert!(
            receiver_inference_note("src/store/vectors.rs", &unknown, &[(TIER_UNPLACED, false)])
                .is_empty()
        );

        // Non-Rust, but every call landed at a near tier through a written
        // qualifier: as trustworthy as Rust, so the note would be noise.
        assert!(receiver_inference_note("src/api/client.ts", &none, &[(1, true)]).is_empty());
    }

    #[test]
    fn expand_recall_neighbors_does_not_count_as_a_read() {
        // Expansion is the machine following an edge, not the agent reading the
        // entry. Counting it would feed `access_recency_score` — the third RRF
        // signal — from the injection itself: an entry would become more likely
        // to be injected because it was injected, with no new evidence.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::schema::init_schema(&conn).unwrap();
        for i in 0..2 {
            conn.execute(
                "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
                 VALUES (?1, ?2, 'body', 'topic', 1, 1)",
                rusqlite::params![format!("n{i}"), format!("Title {i}")],
            )
            .unwrap();
        }
        memory_graph::add_edge(
            &conn,
            "n0",
            "n1",
            TargetKind::Memory,
            MemoryRelation::Supports,
        )
        .unwrap();
        // Read the seed WITHOUT tracking so the assertion below measures only
        // what expansion did.
        let seeds = vec![
            memory::get_entry_without_tracking(&conn, "n0")
                .unwrap()
                .unwrap(),
        ];

        assert_eq!(
            expand_recall_neighbors(&conn, &seeds, 2, 3).unwrap().len(),
            1
        );

        let neighbor = memory::get_entry_without_tracking(&conn, "n1")
            .unwrap()
            .unwrap();
        assert_eq!(
            neighbor.access_count, 0,
            "expansion must not increment the neighbor's access_count"
        );
        assert!(
            neighbor.last_accessed.is_none(),
            "expansion must not stamp last_accessed"
        );
    }

    #[test]
    fn expand_recall_neighbors_respects_config_caps() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::store::schema::init_schema(&conn).unwrap();
        for i in 0..4 {
            conn.execute(
                "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
                 VALUES (?1, ?2, 'body', 'topic', 1, 1)",
                rusqlite::params![format!("n{i}"), format!("Title {i}")],
            )
            .unwrap();
        }
        for j in 1..=3 {
            memory_graph::add_edge(
                &conn,
                "n0",
                &format!("n{j}"),
                TargetKind::Memory,
                MemoryRelation::Supports,
            )
            .unwrap();
        }
        let seeds = vec![memory::get_entry(&conn, "n0").unwrap().unwrap()];

        // Defaults (2 seeds, 3 neighbors) surface all three edges.
        assert_eq!(
            expand_recall_neighbors(&conn, &seeds, 2, 3).unwrap().len(),
            3
        );
        // A tighter neighbor cap truncates.
        assert_eq!(
            expand_recall_neighbors(&conn, &seeds, 2, 1).unwrap().len(),
            1
        );
        // Zero seeds disables expansion entirely.
        assert_eq!(
            expand_recall_neighbors(&conn, &seeds, 0, 3).unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn recall_expansion_is_o1_in_corpus_size_and_fast() {
        // Expansion is bounded (≤2 seeds × one indexed outgoing SELECT + ≤3 PK
        // resolves), so its cost must not scale with corpus size and must sit well
        // under the 10ms recall budget.
        let big_tmp = TempDir::new().unwrap();
        let big = make_handle(&big_tmp);
        ensure_handle_context(&big).await.expect("ctx");
        let big_us = {
            let g = big.ctx.lock().await;
            min_expand_us(&g.as_ref().unwrap().conn, 1000)
        };

        let small_tmp = TempDir::new().unwrap();
        let small = make_handle(&small_tmp);
        ensure_handle_context(&small).await.expect("ctx");
        let small_us = {
            let g = small.ctx.lock().await;
            min_expand_us(&g.as_ref().unwrap().conn, 10)
        };

        // Absolute backstop: real per-call cost is tens of µs; the min stays far
        // under the 10ms budget even on a saturated CI box.
        assert!(
            big_us < 10_000,
            "expansion min = {big_us}us exceeds the 10ms budget on a 1k corpus"
        );
        // O(1) in corpus size: the 1k-corpus min is not materially larger than the
        // 10-entry min. A size-dependent (O(n)) scan would blow this by ~100x; the
        // generous factor + floor absorb measurement noise.
        assert!(
            big_us <= small_us.max(1) * 10 + 200,
            "expansion appears to scale with corpus size: 1k={big_us}us vs 10={small_us}us"
        );
    }

    #[tokio::test]
    async fn recall_marks_stale_dependency_and_leaves_healthy_clean() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        seed_topic_with_content(
            &handle,
            "gizmo-derived",
            "Gizmo handshake uses protocol alpha",
            0,
        )
        .await;
        seed_topic_with_content(&handle, "gizmo-base", "Protocol alpha internal notes", 0).await;
        seed_topic_with_content(&handle, "gizmo-clean", "Gizmo handshake retry policy", 0).await;
        seed_topic_with_content(&handle, "clean-base", "Retry backoff internal notes", 0).await;
        {
            let guard = handle.ctx.lock().await;
            let conn = &guard.as_ref().unwrap().conn;
            memory_graph::add_edge(
                conn,
                "gizmo-derived",
                "gizmo-base",
                TargetKind::Memory,
                MemoryRelation::DerivedFrom,
            )
            .unwrap();
            memory_graph::add_edge(
                conn,
                "gizmo-clean",
                "clean-base",
                TargetKind::Memory,
                MemoryRelation::DerivedFrom,
            )
            .unwrap();
            // Supersede only gizmo-base → gizmo-derived becomes STALE-DEP; gizmo-clean stays clean.
            conn.execute(
                "UPDATE memory_entries SET status='superseded' WHERE id='gizmo-base'",
                [],
            )
            .unwrap();
        }

        let out = hook_user_prompt_submit_impl(&handle, "gizmo handshake protocol retry").await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            body.contains("[STALE-DEP] [gizmo-derived]"),
            "stale dep must be flagged: {body}"
        );
        assert!(
            !body.contains("[STALE-DEP] [gizmo-clean]"),
            "healthy dep must render clean: {body}"
        );

        // The flag is read-only: stored status/confidence of the flagged entry is untouched.
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        let e = memory::get_entry(conn, "gizmo-derived").unwrap().unwrap();
        assert_eq!(
            e.status,
            memory::EntryStatus::Active,
            "STALE-DEP must not mutate status"
        );
    }

    #[tokio::test]
    async fn warmup_marks_stale_dependency() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        seed_topic_with_content(&handle, "warm-derived", "Deployment checklist", 100).await;
        seed_topic_with_content(&handle, "warm-base", "Old deploy step", 0).await;
        {
            let guard = handle.ctx.lock().await;
            let conn = &guard.as_ref().unwrap().conn;
            memory_graph::add_edge(
                conn,
                "warm-derived",
                "warm-base",
                TargetKind::Memory,
                MemoryRelation::DerivedFrom,
            )
            .unwrap();
            conn.execute(
                "UPDATE memory_entries SET status='superseded' WHERE id='warm-base'",
                [],
            )
            .unwrap();
        }

        let out = hook_session_start_impl(&handle, None).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            body.contains("[STALE-DEP]"),
            "warmup must flag the stale entry: {body}"
        );
        assert!(
            body.contains("warm-derived"),
            "flagged entry present: {body}"
        );
    }

    #[tokio::test]
    async fn warmup_injects_newest_handoff_body_and_excludes_handoff_from_list() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        // A substantive handoff (full body) plus a topic for the compact list.
        let handoff_body = format!(
            "---\nsession: abc\n---\n# Session Handoff\n\n{}",
            "Pending: finish the warmup body injection work. ".repeat(4)
        );
        {
            ensure_handle_context(&handle).await.expect("init ctx");
            let guard = handle.ctx.lock().await;
            let conn = &guard.as_ref().unwrap().conn;
            let now = chrono::Utc::now().timestamp();
            let handoff = crate::store::memory::MemoryEntry {
                id: "handoff-2026-07-07-deadbeef".into(),
                // A deliberately truncated title (as journal-cli.js writes it) —
                // it must NOT surface; the full body must.
                title: "Committed KG stories: wiz compact-guard hermetic f".into(),
                content: handoff_body.clone(),
                entry_type: crate::store::memory::EntryType::Handoff,
                tags: vec!["handoff".into(), "session-abc".into()],
                status: crate::store::memory::EntryStatus::Active,
                created_at: now,
                updated_at: now,
                superseded_by: None,
                access_count: 0,
                last_accessed: Some(now),
                source_path: None,
                confirmations: 0,
                corrections: 0,
                last_confirmed_at: None,
                last_refuted_at: None,
                source_type: crate::store::memory::SourceType::UserStatement,
                expires_at: None,
                due_at: None,
            };
            crate::store::memory::add_entry(conn, &handoff).expect("seed handoff");
        }
        seed_topic_with_content(&handle, "topic-x", "Deployment checklist", 50).await;

        let out = hook_session_start_impl(&handle, None).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            body.contains("## Last session handoff"),
            "newest handoff body block injected: {body}"
        );
        assert!(
            body.contains("Pending: finish the warmup body injection work"),
            "full handoff body present (not the truncated title): {body}"
        );
        assert!(
            !body.contains("[handoff]"),
            "handoff must not appear as a compact title-line: {body}"
        );
        assert!(
            !body.contains("handoff-2026-07-07-deadbeef"),
            "handoff id excluded from compact list: {body}"
        );
        assert!(
            body.contains("topic-x"),
            "non-handoff entry still listed: {body}"
        );
    }

    /// Seed a handoff tagged for `project`, `age_days` old, into the store.
    async fn seed_project_handoff(handle: &RepoHandle, project: &str, age_days: i64) {
        ensure_handle_context(handle).await.expect("init ctx");
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        let ts = chrono::Utc::now().timestamp() - age_days * 86_400;
        let entry = crate::store::memory::MemoryEntry {
            id: format!("handoff-{project}"),
            title: format!("Session handoff for {project}"),
            content: format!(
                "---\nsession: {project}\n---\n# Session Handoff\n\n{}",
                format!("Pending work on the {project} project. ").repeat(3)
            ),
            entry_type: crate::store::memory::EntryType::Handoff,
            tags: vec!["handoff".to_string(), project.to_string()],
            status: crate::store::memory::EntryStatus::Active,
            created_at: ts,
            updated_at: ts,
            superseded_by: None,
            access_count: 0,
            last_accessed: Some(ts),
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: crate::store::memory::SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        crate::store::memory::add_entry(conn, &entry).expect("seed handoff");
    }

    /// Register `name` as a collection — the store's own statement that this
    /// subfolder is a project, which is what `project_scope_token` keys off.
    async fn register_collection(handle: &RepoHandle, name: &str) {
        ensure_handle_context(handle).await.expect("init ctx");
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        let now = chrono::Utc::now().timestamp();
        collections::add_collection(
            conn,
            &crate::domain::Collection {
                name: name.to_string(),
                path: name.to_string(),
                pattern: "**/*.md".to_string(),
                source: "manual".to_string(),
                created_at: now,
                updated_at: now,
            },
        )
        .expect("register collection");
    }

    /// One store, many projects: a session inside `lattice/` must be anchored by
    /// lattice's handoff, never by the globally newest one from another project.
    #[tokio::test]
    async fn warmup_injects_the_in_scope_handoff_not_another_projects() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        register_collection(&handle, "lattice").await;
        register_collection(&handle, "riscosity").await;
        seed_project_handoff(&handle, "lattice", 3).await;
        seed_project_handoff(&handle, "riscosity", 1).await; // newest overall

        let cwd = handle.root.join("lattice");
        let out = hook_session_start_impl(&handle, Some(&cwd)).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");

        assert!(
            body.contains("Pending work on the lattice project"),
            "in-scope handoff injected: {body}"
        );
        assert!(
            !body.contains("Pending work on the riscosity project"),
            "another project's handoff must never be injected: {body}"
        );
    }

    /// A project with no handoff of its own gets NO handoff block: a foreign
    /// handoff is worse than none.
    #[tokio::test]
    async fn warmup_injects_no_handoff_when_the_project_has_none() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        register_collection(&handle, "lattice").await;
        register_collection(&handle, "riscosity").await;
        seed_project_handoff(&handle, "riscosity", 1).await;
        seed_topic_with_content(&handle, "topic-x", "Deployment checklist", 50).await;

        let cwd = handle.root.join("lattice");
        let out = hook_session_start_impl(&handle, Some(&cwd)).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");

        assert!(
            !body.contains("## Last session handoff"),
            "no in-scope handoff → no handoff block: {body}"
        );
        assert!(
            !body.contains("Pending work on the riscosity project"),
            "the out-of-scope handoff body must not leak: {body}"
        );
        assert!(
            body.contains("topic-x"),
            "the rest of warmup is unaffected: {body}"
        );
    }

    /// End-to-end proof that the resolved scope reaches the ranker: a session in
    /// `lattice/` sees its own entry first, and still sees the hotter foreign one.
    #[tokio::test]
    async fn warmup_lists_the_in_scope_entry_first_without_dropping_the_rest() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        register_collection(&handle, "lattice").await;
        seed_topic_with_content(&handle, "riscosity-hot", "Proxy retry budget", 99).await;
        seed_topic_with_content(&handle, "lattice-cold", "OTR reformat rules", 1).await;
        {
            let guard = handle.ctx.lock().await;
            let conn = &guard.as_ref().unwrap().conn;
            conn.execute(
                r#"UPDATE memory_entries SET tags='["lattice"]' WHERE id='lattice-cold'"#,
                [],
            )
            .unwrap();
            conn.execute(
                r#"UPDATE memory_entries SET tags='["riscosity"]' WHERE id='riscosity-hot'"#,
                [],
            )
            .unwrap();
        }

        let cwd = handle.root.join("lattice");
        let out = hook_session_start_impl(&handle, Some(&cwd)).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");

        let in_scope = body.find("lattice-cold").expect("in-scope entry listed");
        let out_of_scope = body
            .find("riscosity-hot")
            .expect("out-of-scope entry still listed — bias, not filter");
        assert!(
            in_scope < out_of_scope,
            "in-scope entry ranks above the hotter foreign one: {body}"
        );
    }

    /// An unregistered subfolder is not a project: warmup stays global, so the
    /// newest handoff overall is still the anchor (pre-scoping behaviour).
    #[tokio::test]
    async fn warmup_falls_back_to_the_newest_handoff_when_cwd_is_not_a_project() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        register_collection(&handle, "lattice").await;
        seed_project_handoff(&handle, "lattice", 3).await;
        seed_project_handoff(&handle, "riscosity", 1).await;

        let cwd = handle.root.join("scratch");
        let out = hook_session_start_impl(&handle, Some(&cwd)).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");

        assert!(
            body.contains("Pending work on the riscosity project"),
            "unscoped → newest handoff overall: {body}"
        );
    }

    /// Seed a Topic entry with caller-supplied content (so it can match a
    /// recall/warmup query) and an explicit `access_count` (so warmup ranking
    /// can be exercised deterministically).
    async fn seed_topic_with_content(
        handle: &RepoHandle,
        id: &str,
        content: &str,
        access_count: u64,
    ) {
        ensure_handle_context(handle).await.expect("init ctx");
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let now = chrono::Utc::now().timestamp();
        let entry = crate::store::memory::MemoryEntry {
            id: id.to_string(),
            title: format!("Title for {id}"),
            content: content.to_string(),
            entry_type: crate::store::memory::EntryType::Topic,
            tags: vec![],
            status: crate::store::memory::EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count,
            last_accessed: Some(now),
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: crate::store::memory::SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        crate::store::memory::add_entry(&ctx.conn, &entry).expect("seed topic");
    }

    /// Build a primary store nested under `parent_tmp` and return its handle.
    fn nested_primary(parent_tmp: &TempDir, name: &str) -> Arc<RepoHandle> {
        nested_handle(parent_tmp, name, |_| {})
    }

    /// Isolation proof: a curated high-confidence prior living ONLY in an
    /// ancestor store must not surface in a child repo's automatic recall.
    #[tokio::test]
    async fn recall_ignores_ancestor_prior_despite_full_child_quota() {
        let tmp = TempDir::new().unwrap();

        // Parent (ancestor) holds the curated high-confidence prior.
        let parent = make_handle(&tmp);
        seed_prior_entry(&parent, "ancestor-curated-prior", 8).await;

        // Child fills the entire recall quota with matching entries.
        let primary = nested_primary(&tmp, "nested-repo");
        let limit = primary.config.hooks.recall_limit.max(1);
        for i in 0..(limit + 2) {
            seed_topic_with_content(
                &primary,
                &format!("child-topic-{i}"),
                "Use ripgrep or grep for codebase searches.",
                0,
            )
            .await;
        }

        let out = hook_user_prompt_submit_impl(
            &primary,
            "should I use ripgrep or grep for codebase searches",
        )
        .await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            !body.contains("ancestor-curated-prior"),
            "ancestor prior leaked into child recall: {body}"
        );
    }

    /// Warmup counterpart: with a confidence floor configured, a curated
    /// ancestor prior still must not be injected into the child repo.
    #[tokio::test]
    async fn warmup_ignores_ancestor_prior_under_confidence_floor() {
        let tmp = TempDir::new().unwrap();

        // Parent (ancestor) holds the curated high-confidence prior.
        let parent = make_handle(&tmp);
        seed_prior_entry(&parent, "ancestor-warmup-prior", 8).await;

        // Child: warmup_limit hot entries (high access_count) that would fill
        // every slot, plus a tight warmup_limit and a confidence floor.
        let primary = nested_handle(&tmp, "warmup-nested", |config| {
            config.hooks.warmup_limit = 3;
            config.hooks.warmup_min_confidence = 0.3;
        });
        for i in 0..5 {
            seed_topic_with_content(&primary, &format!("hot-{i}"), "hot entry content", 100).await;
        }

        let out = hook_session_start_impl(&primary, None).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            !body.contains("ancestor-warmup-prior"),
            "ancestor prior leaked into child warmup: {body}"
        );
    }

    /// Regression guard for the original bug: a legacy low-confidence prior
    /// (System B style — `prior-<hash>`, zero confirmations) must NOT leak into
    /// recall from an ancestor store.
    #[tokio::test]
    async fn recall_ignores_legacy_prior_from_ancestor_store() {
        let tmp = TempDir::new().unwrap();

        // Ancestor holds a legacy, low-confidence prior (would have surfaced
        // unconditionally under the old append merge).
        let parent = make_handle(&tmp);
        seed_prior_entry(&parent, "prior-deadbeefdeadbeef", 0).await;

        // Child has a genuine matching entry.
        let primary = nested_primary(&tmp, "nested-legacy");
        seed_topic_with_content(
            &primary,
            "child-real",
            "Prefer ripgrep over grep for codebase searches.",
            0,
        )
        .await;

        let out =
            hook_user_prompt_submit_impl(&primary, "should I use ripgrep or grep for searches")
                .await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            !body.contains("prior-deadbeefdeadbeef"),
            "legacy low-confidence prior leaked into recall from ancestor store: {body}"
        );
    }

    /// Seed a document `project.md` with an `owner -> alice` frontmatter edge.
    /// Returns the document id.
    async fn seed_graph_doc(handle: &RepoHandle) -> i64 {
        ensure_handle_context(handle).await.expect("init ctx");
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let conn = &ctx.conn;
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at)
             VALUES ('docs', './docs', '**/*.md', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('h1', '# P', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at)
             VALUES ('docs', 'project.md', 'h1', 1, 1)",
            [],
        )
        .unwrap();
        let doc_id = conn.last_insert_rowid();
        crate::store::graph::add_edge(
            conn,
            doc_id,
            "alice",
            "owner",
            crate::store::graph::KIND_FRONTMATTER,
            None,
        )
        .unwrap();
        doc_id
    }

    #[tokio::test]
    async fn graph_impl_links_backlinks_neighbors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_graph_doc(&handle).await;

        // links: outgoing from the document.
        let links = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: None,
                direction: "links".to_string(),
                relation: None,
                depth: 1,
                scope: None,
            },
        )
        .await
        .expect("links");
        assert!(links.contains("alice"), "links: {links}");
        assert!(links.contains("owner"), "links: {links}");

        // backlinks: by the raw dangling slug.
        let backlinks = graph_impl(
            &handle,
            &GraphParams {
                entity: "alice".to_string(),
                root: None,
                to: None,
                direction: "backlinks".to_string(),
                relation: None,
                depth: 1,
                scope: None,
            },
        )
        .await
        .expect("backlinks");
        assert!(
            backlinks.contains("backlinks for alice"),
            "backlinks: {backlinks}"
        );

        // neighbors: alice is one hop from project.md (undirected).
        let neighbors = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: None,
                direction: "neighbors".to_string(),
                relation: None,
                depth: 1,
                scope: None,
            },
        )
        .await
        .expect("neighbors");
        assert!(neighbors.contains("alice"), "neighbors: {neighbors}");

        // path: project.md -> alice (alice is a dangling one-hop target).
        let path = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: Some("alice".to_string()),
                direction: "path".to_string(),
                relation: None,
                depth: 1,
                scope: None,
            },
        )
        .await
        .expect("path");
        assert!(path.contains("project.md"), "path: {path}");
        assert!(path.contains("alice"), "path: {path}");
    }

    #[tokio::test]
    async fn graph_impl_path_requires_to() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_graph_doc(&handle).await;

        let err = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: None,
                direction: "path".to_string(),
                relation: None,
                depth: 1,
                scope: None,
            },
        )
        .await
        .expect_err("path without 'to' should error");
        assert!(err.to_string().contains("requires 'to'"), "err: {err}");
    }

    #[tokio::test]
    async fn graph_impl_rejects_unknown_direction() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_graph_doc(&handle).await;

        let err = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: None,
                direction: "sideways".to_string(),
                relation: None,
                depth: 1,
                scope: None,
            },
        )
        .await
        .expect_err("unknown direction should error");
        assert!(err.to_string().contains("Unknown direction"), "err: {err}");
    }

    #[tokio::test]
    async fn dispatch_call_routes_graph_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_graph_doc(&handle).await;

        let result = dispatch_call(
            "graph",
            json!({ "entity": "project.md", "direction": "links" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch graph");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("alice"), "result: {result}");
    }

    #[tokio::test]
    async fn memory_delete_impl_rejects_invalid_id() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let err = memory_delete_impl(&handle, "UPPER_CASE", false)
            .await
            .unwrap_err();
        assert!(
            err.message.contains("entry id"),
            "should reject invalid ID: {}",
            err.message
        );

        let err2 = memory_delete_impl(&handle, "", false).await.unwrap_err();
        assert!(err2.message.contains("entry id"), "{}", err2.message);
    }

    #[tokio::test]
    async fn memory_delete_impl_reports_not_found_for_missing_id() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let out = memory_delete_impl(&handle, "does-not-exist", false)
            .await
            .expect("delete impl");

        assert!(out.contains("not found"), "output: {out}");
    }

    #[tokio::test]
    async fn memory_delete_impl_removes_seeded_entry() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "to-delete").await;

        let out = memory_delete_impl(&handle, "to-delete", false)
            .await
            .expect("delete impl");

        assert!(out.contains("Deleted memory entry"), "output: {out}");

        // Second call must report not found now.
        let again = memory_delete_impl(&handle, "to-delete", false)
            .await
            .expect("delete impl second");
        assert!(again.contains("not found"), "second: {again}");
    }

    #[tokio::test]
    async fn memory_confirm_impl_rejects_invalid_outcome() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "confirm-me").await;

        let err = memory_confirm_impl(&handle, "confirm-me", "maybe")
            .await
            .expect_err("must reject");
        assert!(
            err.message.contains("Invalid outcome"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn memory_confirm_impl_accepts_confirmed() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "confirm-me").await;

        let out = memory_confirm_impl(&handle, "confirm-me", "confirmed")
            .await
            .expect("confirm impl");
        assert!(!out.is_empty(), "output should be non-empty");
    }

    #[tokio::test]
    async fn memory_list_impl_returns_placeholder_when_empty() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let (text, count) = memory_list_impl(&handle, 20, "recent")
            .await
            .expect("list impl");

        assert_eq!(count, 0);
        assert!(text.contains("No memory entries"), "text: {text}");
    }

    #[tokio::test]
    async fn memory_list_impl_lists_seeded_entries() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "entry-a").await;
        seed_memory_entry(&handle, "entry-b").await;

        let (text, count) = memory_list_impl(&handle, 10, "recent")
            .await
            .expect("list impl");

        assert_eq!(count, 2, "text: {text}");
        assert!(text.contains("entry-a"), "text: {text}");
        assert!(text.contains("entry-b"), "text: {text}");
    }

    #[tokio::test]
    async fn memory_list_impl_rejects_invalid_sort() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let err = memory_list_impl(&handle, 10, "bogus")
            .await
            .expect_err("must reject");
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_delete() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "from-dispatch").await;

        let result = dispatch_call(
            "memory_delete",
            json!({ "id": "from-dispatch" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("Deleted memory entry"), "result: {result}");
    }

    #[tokio::test]
    async fn dispatch_call_memory_delete_missing_id_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("memory_delete", Value::Null, handle, &dctx)
            .await
            .expect_err("must error");
        assert!(err.message.contains("id"), "msg: {}", err.message);
    }

    /// `memory_delete` parses its params through `MemoryDeleteParams`: a
    /// `dry_run` that is not a boolean is rejected instead of being read as
    /// `false` and deleting the entry for real.
    #[tokio::test]
    async fn dispatch_call_memory_delete_rejects_untyped_dry_run() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "keep-me").await;

        let err = dispatch_call(
            "memory_delete",
            json!({ "id": "keep-me", "dry_run": "yes" }),
            Arc::clone(&handle),
            &dctx,
        )
        .await
        .expect_err("a non-boolean dry_run must be rejected");
        assert!(
            err.message.contains("invalid params"),
            "msg: {}",
            err.message
        );

        let context = handle.ctx.lock().await;
        let entry = crate::store::memory::get_entry_without_tracking(
            &context.as_ref().unwrap().conn,
            "keep-me",
        )
        .unwrap();
        assert!(
            entry.is_some(),
            "the rejected call must not have deleted anything"
        );
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_confirm() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "confirm-via-dispatch").await;

        let result = dispatch_call(
            "memory_confirm",
            json!({ "id": "confirm-via-dispatch", "outcome": "confirmed" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        assert!(result.get("text").is_some(), "result: {result}");
    }

    fn entry_input(id: &str) -> MemoryWriteBatchEntry {
        MemoryWriteBatchEntry {
            id: id.to_string(),
            title: format!("Title {id}"),
            content: format!("Content for {id}"),
            source_file: None,
            entry_type: "topic".to_string(),
            tags: vec!["t".to_string()],
            source_type: Some("user_statement".to_string()),
            ttl: None,
            due_in: None,
            relates: vec![],
            agent: None,
            on_conflict: None,
        }
    }

    #[test]
    fn resolve_source_file_rejects_path_outside_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, "secret data outside root").unwrap();

        let err = resolve_source_file(&root, "", Some(outside.to_str().unwrap()))
            .expect_err("must reject path outside root");
        assert_eq!(err.message, SOURCE_FILE_ERROR);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_source_file_rejects_symlink_escape() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let outside = tmp.path().join("secret.txt");
        std::fs::write(&outside, "secret data outside root").unwrap();
        let link = root.join("escape.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let err = resolve_source_file(&root, "", Some(link.to_str().unwrap()))
            .expect_err("must reject symlink escape");
        assert_eq!(err.message, SOURCE_FILE_ERROR);
    }

    #[test]
    fn resolve_source_file_rejects_oversized_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let big = root.join("big.txt");
        // One byte over the cap; write via metadata-length check, no need to
        // actually allocate the whole cap in memory for the test to be valid.
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_SOURCE_FILE_BYTES + 1).unwrap();

        let err = resolve_source_file(&root, "", Some(big.to_str().unwrap()))
            .expect_err("must reject oversized file");
        assert_eq!(err.message, SOURCE_FILE_ERROR);
    }

    #[test]
    fn resolve_source_file_rejects_missing_file_with_generic_error() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let missing = root.join("does-not-exist.txt");

        let err = resolve_source_file(&root, "", Some(missing.to_str().unwrap()))
            .expect_err("must reject missing file");
        assert_eq!(err.message, SOURCE_FILE_ERROR);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_source_file_rejects_permission_denied_with_generic_error() {
        use std::os::unix::fs::PermissionsExt;

        // Skip when running as root (e.g. some CI/container setups), where
        // permission bits are not enforced.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let unreadable = root.join("unreadable.txt");
        std::fs::write(&unreadable, "top secret").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = resolve_source_file(&root, "", Some(unreadable.to_str().unwrap()));

        // Restore permissions so TempDir cleanup can remove the file.
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = result.expect_err("must reject permission-denied file");
        assert_eq!(err.message, SOURCE_FILE_ERROR);
    }

    #[test]
    fn resolve_source_file_accepts_file_under_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let file = root.join("note.txt");
        std::fs::write(&file, "hello from repo").unwrap();

        let (content, source_path) = resolve_source_file(&root, "", Some(file.to_str().unwrap()))
            .expect("must accept file under root");
        assert_eq!(content, "hello from repo");
        assert!(source_path.is_some());
    }

    #[tokio::test]
    async fn memory_write_impl_creates_then_updates() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let created = memory_write_impl(&handle, &entry_input("w-1"), None, false)
            .await
            .expect("write impl");
        assert!(
            created.starts_with("Created memory entry: w-1"),
            "out: {created}"
        );

        let mut second = entry_input("w-1");
        second.content = "Updated content body".to_string();
        let updated = memory_write_impl(&handle, &second, None, false)
            .await
            .expect("write impl update");
        assert!(
            updated.starts_with("Updated memory entry: w-1"),
            "out: {updated}"
        );
    }

    #[tokio::test]
    async fn memory_write_creates_edges_and_memory_scope_graph() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        // Seed the target, then write a source entry carrying a `relates` edge to it.
        memory_write_impl(&handle, &entry_input("edge-b"), None, false)
            .await
            .expect("write b");
        let mut a = entry_input("edge-a");
        a.relates = vec![RelatesInput {
            relation: "supports".to_string(),
            target: "edge-b".to_string(),
            target_kind: "memory".to_string(),
        }];
        memory_write_impl(&handle, &a, None, false)
            .await
            .expect("write a");

        // graph scope=memory: links from edge-a surface edge-b via supports.
        let links = graph_impl(
            &handle,
            &GraphParams {
                entity: "edge-a".to_string(),
                root: None,
                to: None,
                direction: "links".to_string(),
                relation: None,
                depth: 1,
                scope: Some("memory".to_string()),
            },
        )
        .await
        .expect("mem links");
        assert!(links.contains("edge-b"), "links: {links}");
        assert!(links.contains("supports"), "links: {links}");

        // backlinks from edge-b surface edge-a.
        let back = graph_impl(
            &handle,
            &GraphParams {
                entity: "edge-b".to_string(),
                root: None,
                to: None,
                direction: "backlinks".to_string(),
                relation: None,
                depth: 1,
                scope: Some("memory".to_string()),
            },
        )
        .await
        .expect("mem backlinks");
        assert!(back.contains("edge-a"), "backlinks: {back}");
    }

    #[tokio::test]
    async fn memory_write_invalid_relation_is_rejected_atomically() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let mut a = entry_input("bad-rel");
        a.relates = vec![RelatesInput {
            relation: "mentions".to_string(),
            target: "x".to_string(),
            target_kind: "memory".to_string(),
        }];
        let err = memory_write_impl(&handle, &a, None, false)
            .await
            .unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("supports"),
            "error should list closed set: {msg}"
        );

        // Validation happens before the write, so the entry must not persist.
        let got = get_impl(&handle, &get_params("bad-rel")).await;
        assert!(got.is_err(), "entry must not persist on invalid relation");
    }

    #[tokio::test]
    async fn get_impl_shows_provenance_and_edges() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        memory_write_impl(&handle, &entry_input("prov-b"), None, false)
            .await
            .expect("b");
        let mut a = entry_input("prov-a");
        a.agent = Some("codex".to_string());
        a.relates = vec![RelatesInput {
            relation: "derived_from".to_string(),
            target: "prov-b".to_string(),
            target_kind: "memory".to_string(),
        }];
        memory_write_impl(&handle, &a, Some("sess-9"), false)
            .await
            .expect("a");

        let (text, _, _) = get_impl(&handle, &get_params("prov-a")).await.expect("get");
        assert!(text.contains("Provenance:"), "text: {text}");
        assert!(text.contains("codex"), "text: {text}");
        assert!(text.contains("sess-9"), "text: {text}");
        assert!(text.contains("Edges:"), "text: {text}");
        assert!(text.contains("derived_from prov-b"), "text: {text}");
    }

    #[tokio::test]
    async fn memory_write_on_conflict_contradicts_records_edge() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        ensure_handle_context(&handle).await.expect("init ctx");
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;

        let emb = vec![0.1f32; crate::store::vectors::EMBEDDING_DIM];

        // Original entry with a stored embedding.
        crate::core::memory::write_memory(
            conn,
            crate::core::memory::WriteMemoryInput {
                id: "orig-dup",
                title: "Orig",
                content: "Auth notes",
                entry_type: "topic",
                source_type: Some("user_statement"),
                tags: &[],
                ttl: None,
                due_in: None,
                embedding: Some(&emb),
                embed_when_missing: false,
                source_path: None,
                relates: &[],
                session: None,
                agent: None,
                on_conflict: None,
                dry_run: false,
            },
        )
        .expect("orig write");

        // Near-identical embedding + on_conflict=contradicts: writes the entry AND
        // records a contradicts edge to the similar one, returning both ids.
        let out = crate::core::memory::write_memory(
            conn,
            crate::core::memory::WriteMemoryInput {
                id: "new-dup",
                title: "New",
                content: "Auth notes v2",
                entry_type: "topic",
                source_type: Some("user_statement"),
                tags: &[],
                ttl: None,
                due_in: None,
                embedding: Some(&emb),
                embed_when_missing: false,
                source_path: None,
                relates: &[],
                session: None,
                agent: None,
                on_conflict: Some("contradicts"),
                dry_run: false,
            },
        )
        .expect("contradicts write");
        assert!(out.contains("new-dup"), "new id in output: {out}");
        assert!(out.contains("orig-dup"), "conflicting id in output: {out}");

        let edges =
            memory_graph::outgoing(conn, "new-dup", Some(MemoryRelation::Contradicts)).unwrap();
        assert_eq!(edges.len(), 1, "contradicts edge missing");
        assert_eq!(edges[0].target_ref, "orig-dup");

        // Default (on_conflict absent): a near-duplicate is rejected verbatim.
        let err = crate::core::memory::write_memory(
            conn,
            crate::core::memory::WriteMemoryInput {
                id: "third-dup",
                title: "Third",
                content: "Auth notes v3",
                entry_type: "topic",
                source_type: Some("user_statement"),
                tags: &[],
                ttl: None,
                due_in: None,
                embedding: Some(&emb),
                embed_when_missing: false,
                source_path: None,
                relates: &[],
                session: None,
                agent: None,
                on_conflict: None,
                dry_run: false,
            },
        )
        .unwrap_err();
        assert!(
            format!("{err:?}").contains("Near-duplicate"),
            "default must still reject: {err:?}"
        );
    }

    #[tokio::test]
    async fn memory_write_impl_dry_run_does_not_persist() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let preview = memory_write_impl(&handle, &entry_input("dry-w"), None, true)
            .await
            .expect("dry-run write");
        assert_eq!(preview, "dry-run: would create memory entry 'dry-w'");

        // A real delete must report the entry was never written.
        let after = memory_delete_impl(&handle, "dry-w", false)
            .await
            .expect("delete impl");
        assert!(after.contains("not found"), "entry persisted: {after}");
    }

    #[tokio::test]
    async fn memory_delete_impl_dry_run_keeps_entry() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "dry-del").await;

        let preview = memory_delete_impl(&handle, "dry-del", true)
            .await
            .expect("dry-run delete");
        assert_eq!(preview, "dry-run: would delete memory entry 'dry-del'");

        // The entry must still be present for a real delete to remove.
        let real = memory_delete_impl(&handle, "dry-del", false)
            .await
            .expect("delete impl");
        assert!(real.contains("Deleted memory entry"), "entry gone: {real}");
    }

    #[tokio::test]
    async fn memory_write_batch_impl_rejects_empty() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let err = memory_write_batch_impl(&handle, &[], None, false)
            .await
            .expect_err("must reject");
        assert!(
            err.message.contains("must not be empty"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn memory_write_batch_impl_rejects_over_limit() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let entries: Vec<_> = (0..21).map(|i| entry_input(&format!("b-{i}"))).collect();

        let err = memory_write_batch_impl(&handle, &entries, None, false)
            .await
            .expect_err("must reject");
        assert!(
            err.message.contains("max 20 entries"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn memory_write_batch_impl_writes_all_entries() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let mut a = entry_input("b-a");
        a.title = "Authentication flow notes".to_string();
        a.content = "OAuth2 PKCE token rotation strategy".to_string();
        let mut b = entry_input("b-b");
        b.title = "Database migration runbook".to_string();
        b.content = "Postgres logical replication failover steps".to_string();
        let mut c = entry_input("b-c");
        c.title = "Frontend build config".to_string();
        c.content = "Vite chunk splitting and tree shaking knobs".to_string();

        let (text, count) = memory_write_batch_impl(&handle, &[a, b, c], None, false)
            .await
            .expect("batch impl");

        assert_eq!(count, 3);
        for id in ["b-a", "b-b", "b-c"] {
            assert!(text.contains(id), "missing {id} in: {text}");
        }
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_write() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call(
            "memory_write",
            json!({
                "id": "via-dispatch",
                "title": "Disp Title",
                "content": "Disp content body",
                "entry_type": "topic",
                "tags": [],
                "source_type": "user_statement"
            }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(
            text.contains("Created memory entry: via-dispatch"),
            "result: {result}"
        );
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_write_batch() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call(
            "memory_write_batch",
            json!({
                "entries": [
                    {
                        "id": "bd-1",
                        "title": "T1",
                        "content": "Body 1 content",
                        "entry_type": "topic",
                        "tags": [],
                        "source_type": "user_statement"
                    },
                    {
                        "id": "bd-2",
                        "title": "T2",
                        "content": "Body 2 content",
                        "entry_type": "topic",
                        "tags": [],
                        "source_type": "user_statement"
                    }
                ]
            }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        assert_eq!(result.get("count").and_then(Value::as_u64).unwrap_or(0), 2);
    }

    #[tokio::test]
    async fn dispatch_call_memory_write_batch_missing_entries_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("memory_write_batch", json!({}), handle, &dctx)
            .await
            .expect_err("must error");
        assert!(err.message.contains("'entries'"), "msg: {}", err.message);
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_list() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "listed").await;

        let result = dispatch_call("memory_list", json!({}), handle, &dctx)
            .await
            .expect("dispatch");

        assert_eq!(
            result.get("count").and_then(Value::as_u64).unwrap_or(0),
            1,
            "result: {result}"
        );
        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("listed"), "text: {text}");
    }

    fn search_params(query: &str, scope: Option<&str>) -> SearchParams {
        SearchParams {
            query: query.to_string(),
            root: None,
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: scope.map(str::to_string),
            kind: None,
            threshold: None,
            file: None,
            min_confidence: None,
            since: None,
        }
    }

    #[tokio::test]
    async fn search_impl_rejects_invalid_scope() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = search_params("anything", Some("bogus"));

        let err = search_impl(&handle, &params)
            .await
            .expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Invalid scope"), "msg: {msg}");
        assert!(msg.contains("bogus"), "msg: {msg}");
    }

    #[tokio::test]
    async fn search_impl_docs_scope_returns_empty_for_no_docs() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = search_params("nothing matches", Some("docs"));

        let (text, count) = search_impl(&handle, &params).await.expect("docs scope");
        assert_eq!(count, 0, "expected zero, text: {text}");
    }

    #[tokio::test]
    async fn search_impl_memory_scope_returns_empty_for_no_entries() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = search_params("anything", Some("memory"));

        let (_text, count) = search_impl(&handle, &params).await.expect("memory scope");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn search_impl_symbols_scope_returns_empty_for_no_index() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = search_params("anything", Some("symbols"));

        let (text, count) = search_impl(&handle, &params).await.expect("symbols scope");
        assert_eq!(count, 0, "text: {text}");
        assert!(text.contains("0 matches"), "text: {text}");
    }

    #[tokio::test]
    async fn cross_repo_search_impl_rejects_empty_handles() {
        let params = search_params("anything", None);
        let err = cross_repo_search_impl(&[], &params)
            .await
            .expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("No repos registered"), "msg: {msg}");
    }

    #[tokio::test]
    async fn dispatch_call_search_rejects_cross_repo() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call(
            "search",
            json!({ "query": "x", "root": "*" }),
            handle,
            &dctx,
        )
        .await
        .expect_err("should error");
        let msg = err.to_string();
        assert!(
            msg.contains("Cross-repo search via dispatch_call is not supported"),
            "msg: {msg}"
        );
    }

    fn get_params(id: &str) -> GetParams {
        GetParams {
            id: id.to_string(),
            root: None,
            lines: None,
            format: None,
        }
    }

    #[tokio::test]
    async fn get_impl_unknown_id_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = get_params("does-not-exist");

        let err = get_impl(&handle, &params).await.expect_err("should error");
        assert!(err.to_string().contains("Not found"), "msg: {err}");
    }

    #[tokio::test]
    async fn get_impl_glob_returns_no_match_for_empty_repo() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = get_params("**/*.md");

        let (text, count, truncated) = get_impl(&handle, &params).await.expect("glob get");
        assert_eq!(count, 0, "text: {text}");
        assert!(!truncated);
        assert!(
            text.contains("No documents matched pattern"),
            "text: {text}"
        );
    }

    #[tokio::test]
    async fn get_impl_batch_errors_when_no_items_resolve() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = get_params("missing-1, missing-2");

        let err = get_impl(&handle, &params).await.expect_err("should error");
        assert!(
            err.to_string()
                .contains("None of the requested items were found"),
            "msg: {err}"
        );
    }

    #[tokio::test]
    async fn get_impl_returns_seeded_memory_entry() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "seeded-get").await;

        let params = get_params("seeded-get");
        let (text, count, truncated) = get_impl(&handle, &params).await.expect("memory get");
        assert_eq!(count, 1);
        assert!(!truncated);
        assert!(text.contains("seeded-get"), "text: {text}");
        assert!(text.contains("Type:"), "text: {text}");
    }

    /// `render_document_content` now fetches through
    /// `core::ops::get_document_content`; a blob that vanished out from under an
    /// indexed document row must still map `ErrorKind::DocumentNotFound` to the
    /// reindex hint, not fall through to the generic store-error message.
    #[tokio::test]
    async fn render_document_content_missing_blob_returns_reindex_hint() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_document(&handle, "docs/gone.md", "Gone", "line one\nline two\n").await;

        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let mut doc = resolve_document(&ctx.conn, "docs/gone.md").expect("resolve seeded doc");
        // The blob is content-addressable, so a document row whose hash has no
        // `content` row is the missing-body case. The schema's
        // `documents.hash REFERENCES content(hash)` blocks deleting the blob, so
        // point the in-memory document at a hash that was never stored — what
        // the caller hands `render_document_content` either way.
        doc.hash = "0".repeat(64);

        let err =
            render_document_content(&handle, ctx, &doc, None).expect_err("missing blob errors");
        assert!(
            err.to_string().contains("Try `update` to reindex"),
            "hint must survive the core::ops error mapping: {err}"
        );
    }

    /// A `lines` range must still apply when fetched through
    /// `core::ops::get_document_content` via the actual `get` MCP entry point.
    #[tokio::test]
    async fn get_impl_document_lines_range_applies_through_mcp_path() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_document(
            &handle,
            "docs/ranged.md",
            "Ranged",
            "line one\nline two\nline three\nline four\n",
        )
        .await;

        let mut params = get_params("docs/ranged.md");
        params.lines = Some("2:3".to_string());
        let (text, count, truncated) = get_impl(&handle, &params).await.expect("ranged get");
        assert_eq!(count, 1);
        assert!(!truncated);
        assert_eq!(text, "line two\nline three", "text: {text}");
    }

    /// The superseded-status suffix appended after the fetched content must
    /// still render once the fetch itself goes through `core::ops`.
    #[tokio::test]
    async fn render_document_content_superseded_status_suffix_renders() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_document(&handle, "docs/old.md", "Old", "old body").await;
        seed_document(&handle, "docs/new.md", "New", "new body").await;

        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let old = resolve_document(&ctx.conn, "docs/old.md").expect("resolve old");
        let new = resolve_document(&ctx.conn, "docs/new.md").expect("resolve new");
        evolution::add_evolution(
            &ctx.conn,
            new.id,
            old.id,
            evolution::RelationshipType::Supersedes,
            None,
            Some("replaced"),
        )
        .expect("record supersede");

        let text = render_document_content(&handle, ctx, &old, None).expect("render old doc");
        assert!(text.contains("**Status:** Superseded"), "text: {text}");
        assert!(
            text.contains("**Superseded by:** docs/new.md"),
            "text: {text}"
        );
    }

    /// Truncation of the rendered output (content + status suffix) must still
    /// kick in and leave its continuation marker once the fetch goes through
    /// `core::ops::get_document_content`.
    #[tokio::test]
    async fn render_document_content_truncation_still_renders() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.hooks.user_prompt_submit_require_sigil = false;
            // Above the continuation message's own token cost, or
            // `truncate_with_continuation` falls back to the "Content too large"
            // stub and never emits the line marker this test is about.
            config.mcp.max_response_tokens = 200;
        });

        let long_content = "line\n".repeat(2000);
        seed_document(&handle, "docs/long.md", "Long", &long_content).await;

        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let doc = resolve_document(&ctx.conn, "docs/long.md").expect("resolve seeded doc");

        let text = render_document_content(&handle, ctx, &doc, None).expect("render long doc");
        assert!(
            text.contains("[Truncated at line"),
            "truncation marker must survive: {text}"
        );
    }

    #[tokio::test]
    async fn dispatch_call_routes_get_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "dispatched-get").await;

        let result = dispatch_call("get", json!({ "id": "dispatched-get" }), handle, &dctx)
            .await
            .expect("dispatch");

        assert_eq!(
            result.get("count").and_then(Value::as_u64).unwrap_or(0),
            1,
            "result: {result}"
        );
        assert_eq!(
            result.get("truncated").and_then(Value::as_bool),
            Some(false),
            "result: {result}"
        );
    }

    #[tokio::test]
    async fn dispatch_call_get_missing_id_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("get", json!({}), handle, &dctx)
            .await
            .expect_err("should error");
        assert!(
            err.to_string().contains("get: invalid params"),
            "msg: {err}"
        );
    }

    #[tokio::test]
    async fn dispatch_call_routes_search_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call(
            "search",
            json!({ "query": "anything", "scope": "docs" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        assert_eq!(
            result.get("count").and_then(Value::as_u64).unwrap_or(99),
            0,
            "result: {result}"
        );
        assert!(result.get("tokens").is_some(), "result: {result}");
    }

    #[tokio::test]
    async fn usage_impl_returns_empty_session_when_no_session_id() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = UsageParams {
            session_only: true,
            root: None,
        };

        let body = usage_impl(&handle, &params, 0).await.expect("usage impl");
        let parsed: Value = serde_json::from_str(&body).expect("json");

        assert!(parsed.get("session").is_some_and(Value::is_null));
        assert_eq!(parsed["per_tool"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            parsed["top_5_most_called"].as_array().map(Vec::len),
            Some(0)
        );
        assert!(parsed.get("lifetime").is_none());
    }

    #[tokio::test]
    async fn dispatch_call_routes_usage_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call("usage", json!({}), handle, &dctx)
            .await
            .expect("dispatch");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        let parsed: Value = serde_json::from_str(text).expect("usage text is json");
        assert!(parsed.get("session").is_some(), "result: {result}");
        assert!(result.get("tokens").is_some(), "result: {result}");
    }

    #[tokio::test]
    async fn code_graph_impl_returns_rebuild_hint_when_no_index() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        // Mark reindex active so acquire_handle_code_index returns the (None) lock.
        handle
            .code_reindex_active
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let params = CodeGraphParams {
            name: "anything".into(),
            root: None,
            direction: "calls".into(),
            symbol_id: None,
            max_depth: 3,
        };

        let out = code_graph_impl(&handle, &params)
            .await
            .expect("code_graph impl");
        assert!(
            out.text.contains("Code index is being rebuilt"),
            "text: {}",
            out.text
        );
        assert!(out.symbols.is_empty());
    }

    #[tokio::test]
    async fn dispatch_call_code_graph_invalid_params_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("code_graph", json!({}), handle, &dctx)
            .await
            .expect_err("missing name");
        assert!(
            err.message.contains("invalid params"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn dispatch_call_unknown_method_remains_method_not_found_after_new_routes() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("nonexistent_tool_xyz", json!({}), handle, &dctx)
            .await
            .expect_err("should be method-not-found");
        assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);
    }

    // --- memory_list limit cap ---

    #[tokio::test]
    async fn memory_list_limit_clamped_to_200() {
        // The clamp lives in `memory_list_impl`, the one function both routers
        // reach. 201 rows and a limit of 100000 must come back as 200.
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        for i in 0..201u16 {
            seed_memory_entry(&handle, &format!("entry-{i}")).await;
        }
        let (text, count) = memory_list_impl(&handle, 100_000, "newest")
            .await
            .expect("memory_list must succeed");
        assert_eq!(count, MEMORY_LIST_MAX_LIMIT, "returned {count} entries");
        assert!(text.starts_with("Found 200 memory entries"), "text: {text}");
    }

    /// `memory_list` parses its params through `MemoryListParams`, so a limit
    /// that is not a number is a rejected call, not a silent default of 20.
    #[tokio::test]
    async fn dispatch_call_memory_list_rejects_untyped_limit() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let err = dispatch_call(
            "memory_list",
            json!({ "limit": "many" }),
            handle,
            &make_dctx(),
        )
        .await
        .expect_err("a non-numeric limit must be rejected");
        assert!(
            err.message.contains("invalid params"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn memory_list_default_limit_is_below_200() {
        // No explicit limit → default of 20, well within the 200 cap.
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "solo").await;
        let result = dispatch_call(
            "memory_list",
            json!({ "sort": "recent" }),
            handle,
            &make_dctx(),
        )
        .await
        .expect("memory_list default");
        let count = result.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        assert_eq!(count, 1);
    }

    // --- get_batch_impl ID cap ---

    #[tokio::test]
    async fn get_batch_rejects_more_than_50_ids() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        ensure_handle_context(&handle).await.unwrap();

        // Build a comma-separated string of 51 IDs
        let ids: String = (1..=51)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let err = get_batch_impl(&handle, &ids, None)
            .await
            .expect_err("must reject >50 IDs");
        assert!(
            err.message.contains("too many IDs"),
            "unexpected msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn get_batch_accepts_exactly_50_ids() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        ensure_handle_context(&handle).await.unwrap();

        // 50 IDs that don't exist — should succeed (returning "Not found" entries)
        let ids: String = (1..=50)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // Panics only if the cap itself errors — "not found" is acceptable output
        let _ = get_batch_impl(&handle, &ids, None).await;
    }

    // ── hook method tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn hook_session_start_advertises_power_features_on_empty_index() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join(".mdkb")).unwrap();
        let handle = make_handle(&tmp);
        let result = hook_session_start_impl(&handle, None).await;
        let context = result["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .expect("initialized repositories always advertise power features");
        assert!(context.contains("* query"));
        assert!(context.contains("cheatsheet"));
        assert!(context.contains("search/code/graph/audit/memory"));
    }

    #[tokio::test]
    async fn hook_session_start_refreshes_stale_code_index_in_background() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "warmup-entry").await;

        let code_db = handle.root.join(".mdkb/code.sqlite");
        let conn = rusqlite::Connection::open(&code_db).unwrap();
        crate::code::storage::schema::init_schema(&conn).unwrap();
        let old = chrono::Utc::now().timestamp() - 8 * 86_400;
        conn.execute(
            "INSERT INTO code_metadata (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                crate::code::storage::schema::LAST_INDEX_SCAN_KEY,
                old.to_string(),
            ],
        )
        .unwrap();
        drop(conn);

        let result = hook_session_start_impl(&handle, None).await;
        let body = result
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            body.contains("refreshing in background"),
            "stale code index must auto-refresh: {body}"
        );
        assert!(
            !body.contains(" code index` to refresh"),
            "must not ask for manual refresh when background refresh is scheduled: {body}"
        );

        for _ in 0..50 {
            if !handle.code_reindex_active.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !handle.code_reindex_active.load(Ordering::Relaxed),
            "background refresh did not finish"
        );
        let conn = rusqlite::Connection::open(&code_db).unwrap();
        let scan_at = crate::code::storage::schema::last_index_scan_at(&conn)
            .unwrap()
            .unwrap();
        assert!(scan_at > old, "scan marker was not refreshed");
    }

    #[tokio::test]
    async fn hook_session_start_disabled_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.hooks.session_start_enabled = false;
        });
        let result = hook_session_start_impl(&handle, None).await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_user_prompt_submit_silent_on_empty_prompt() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let result = hook_user_prompt_submit_impl(&handle, "").await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_user_prompt_submit_silent_on_wrapup() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let result = hook_user_prompt_submit_impl(&handle, "/clear").await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_user_prompt_submit_silent_when_no_results() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        // Empty DB → no results
        let result = hook_user_prompt_submit_impl(&handle, "find authentication bug").await;
        assert_eq!(result, json!({}));
    }

    // ── Phase 7: trigger-matched prior injection ──────────────────────────────

    /// Seed a promoted, injectable cluster directly into the handle's store.
    async fn seed_promoted_prior(handle: &RepoHandle, kind: &str, matcher: &str, lesson: &str) {
        seed_promoted_prior_with_signature(handle, kind, matcher, lesson, None).await;
    }

    /// As [`seed_promoted_prior`], with the failure signature the belief loop
    /// checks recurrence against. Returns the cluster id.
    async fn seed_promoted_prior_with_signature(
        handle: &RepoHandle,
        kind: &str,
        matcher: &str,
        lesson: &str,
        signature: Option<&str>,
    ) -> String {
        use crate::store::priors::{
            PriorCluster, canonical_trigger_key, cluster_id_for_key, upsert_cluster,
        };
        ensure_handle_context(handle).await.unwrap();
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        let key = canonical_trigger_key(kind, matcher);
        let cluster_id = cluster_id_for_key(&key);
        let now = chrono::Utc::now().timestamp();
        // The lesson the cluster projects has to exist: story 093 made the
        // injection path join `memory_entries`, so a cluster with nothing to
        // show is not injectable however promoted it looks.
        let memory_id = format!("prior-{cluster_id}");
        conn.execute(
            "INSERT OR IGNORE INTO memory_entries
                 (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES (?1, ?2, ?2, 'prior', '[]', ?3, ?3)",
            rusqlite::params![&memory_id, lesson, now],
        )
        .unwrap();
        upsert_cluster(
            conn,
            &PriorCluster {
                id: cluster_id.clone(),
                canonical_trigger_key: key,
                trigger_kind: kind.into(),
                trigger_matcher: matcher.into(),
                lesson: lesson.into(),
                scope: r#"{"repo":"current"}"#.into(),
                evidence_count: 2,
                distinct_sessions: 2, // clears the injection score threshold
                injected_count: 0,
                confirmed_count: 0,
                refuted_count: 0,
                state: "promoted".into(),
                promoted_memory_id: Some(memory_id),
                created_at: now,
                last_seen_at: now, // maximally fresh
                error_signature: signature.map(str::to_string),
            },
        )
        .unwrap();
        cluster_id
    }

    #[tokio::test]
    async fn hook_pre_tool_use_injects_trigger_matched_prior_on_edit() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_promoted_prior(
            &handle,
            "pre_tool",
            r#"{"pattern":"src/generated/**"}"#,
            "Do not edit generated files; edit the generator instead.",
        )
        .await;

        // Edit is neither Grep nor Bash — the prior must still fire.
        let path = tmp.path().join("src/generated/api.rs");
        let event = json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": path.to_string_lossy()}
        });
        let result = hook_pre_tool_use_impl(&handle, &event).await;
        let ctx = result["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or("");
        assert!(ctx.contains("mdkb prior:"), "expected prior, got: {result}");
        assert!(ctx.contains("Do not edit generated files"));
    }

    #[tokio::test]
    async fn hook_pre_tool_use_silent_when_prior_trigger_does_not_match() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_promoted_prior(
            &handle,
            "pre_tool",
            r#"{"pattern":"src/generated/**"}"#,
            "Do not edit generated files.",
        )
        .await;

        let path = tmp.path().join("src/handwritten.rs");
        let event = json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": path.to_string_lossy()}
        });
        let result = hook_pre_tool_use_impl(&handle, &event).await;
        assert_eq!(result, json!({}), "non-matching path must not inject");
    }

    #[tokio::test]
    async fn hook_pre_tool_use_prior_suppressed_when_injection_disabled() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let handle = make_handle_with(&tmp, |config| {
            config.priors.injection_enabled = false;
        });
        seed_promoted_prior(
            &handle,
            "pre_tool",
            r#"{"pattern":"src/generated/**"}"#,
            "Do not edit generated files.",
        )
        .await;

        let path = root.join("src/generated/api.rs");
        let event = json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": path.to_string_lossy()}
        });
        let result = hook_pre_tool_use_impl(&handle, &event).await;
        assert_eq!(result, json!({}), "flag off → no prior injection");
    }

    #[tokio::test]
    async fn hook_user_prompt_submit_injects_prompt_matched_prior() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_promoted_prior(
            &handle,
            "prompt",
            r#"{"pattern":"ripgrep"}"#,
            "Prefer ripgrep over grep for repository search.",
        )
        .await;

        let result =
            hook_user_prompt_submit_impl(&handle, "should I use ripgrep for searching?").await;
        let ctx = result["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or("");
        assert!(ctx.contains("## mdkb: priors"), "got: {result}");
        assert!(ctx.contains("Prefer ripgrep over grep"));
    }

    #[tokio::test]
    async fn user_prompt_submit_dedups_prompt_prior_within_same_hook_session() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_promoted_prior(
            &handle,
            "prompt",
            r#"{"pattern":"ripgrep"}"#,
            "Prefer ripgrep over grep for repository search.",
        )
        .await;

        let params = json!({
            "prompt": "should I use ripgrep for searching?",
            "session_id": "s1"
        });
        let first = dispatch_call(
            "hook.user_prompt_submit",
            params.clone(),
            Arc::clone(&handle),
            &dctx,
        )
        .await
        .expect("first hook");
        assert!(
            additional_context(&first).contains("Prefer ripgrep over grep"),
            "first hook should inject prior: {first}"
        );

        let second = dispatch_call("hook.user_prompt_submit", params, handle, &dctx)
            .await
            .expect("second hook");
        assert_eq!(second, json!({}), "same session must not reinject prior");
    }

    // ── Phase 3+5: Stop hook + async distiller ────────────────────────────────

    #[tokio::test]
    async fn hook_stop_noop_when_mining_disabled() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp); // mining_enabled=false by default
        let event = json!({"transcript_path": "/nonexistent", "session_id": "s1"});
        assert_eq!(hook_stop_impl(handle, &event, &make_dctx()), json!({}));
    }

    #[tokio::test]
    async fn stop_hook_triggers_backfill_even_when_mining_disabled() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp); // mining disabled by default
        assert!(!handle.backfill_in_flight.load(Ordering::Acquire));

        let event = json!({"transcript_path": "/nonexistent", "session_id": "s1"});
        let out = hook_stop_impl(Arc::clone(&handle), &event, &make_dctx());
        assert_eq!(out, json!({}), "stop still no-ops the mining path");

        // The drain must be scheduled regardless of the mining kill-switch: the
        // single-flight guard is held by the just-spawned (not-yet-polled) task.
        // hook_stop_impl is sync with no await after the spawn, so this is
        // deterministic under the current-thread test runtime.
        assert!(
            handle.backfill_in_flight.load(Ordering::Acquire),
            "stop hook must trigger a backfill even when mining is disabled"
        );
    }

    #[tokio::test]
    async fn session_start_hook_triggers_backfill() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        assert!(!handle.backfill_in_flight.load(Ordering::Acquire));

        let _ = hook_session_start_impl(&handle, None).await;

        // The spawn is the last statement before the return (no trailing await),
        // so the guard is still held when control returns here.
        assert!(
            handle.backfill_in_flight.load(Ordering::Acquire),
            "session_start hook must trigger a backfill"
        );
    }

    #[tokio::test]
    async fn session_start_disabled_skips_backfill() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.hooks.session_start_enabled = false;
        });
        let _ = hook_session_start_impl(&handle, None).await;
        assert!(
            !handle.backfill_in_flight.load(Ordering::Acquire),
            "a disabled session_start must not trigger a backfill"
        );
    }

    // ── The belief loop: an injected prior is answered at Stop ────────────────

    /// A dispatch context that keeps its background tasks, so a test can wait
    /// for the work the Stop hook detaches.
    fn make_collecting_dctx() -> DispatchContext {
        DispatchContext {
            background: Some(Arc::new(StdMutex::new(Vec::new()))),
            ..make_dctx()
        }
    }

    async fn cluster_belief(handle: &RepoHandle, cluster_id: &str) -> (i64, i64) {
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        let c = crate::store::priors::get_cluster(conn, cluster_id)
            .unwrap()
            .expect("cluster exists");
        (c.confirmed_count, c.refuted_count)
    }

    /// The outcome written on the injection row, or `None` while it is open.
    async fn injection_outcome(handle: &RepoHandle, cluster_id: &str) -> Option<String> {
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        conn.query_row(
            "SELECT outcome FROM prior_injections WHERE cluster_id = ?1 AND session = 's1'",
            rusqlite::params![cluster_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Inject a prior on PreToolUse, then end the session, and read the verdict:
    /// the cluster's two counters plus the outcome on the injection row, which
    /// is where a settlement that moves no counter is recorded.
    /// Mining stays OFF throughout: settling is gated on injection, not on
    /// mining, and gating it on mining would leave every prior in such a repo
    /// unsettled forever.
    async fn inject_then_stop(transcript: &str) -> (i64, i64, Option<String>) {
        let tmp = TempDir::new().unwrap();
        let handle = Arc::new(make_handle_with(&tmp, |config| {
            config.priors.mining_enabled = false;
        }));
        let cluster_id = seed_promoted_prior_with_signature(
            &handle,
            "pre_tool",
            r#"{"pattern":"src/generated/**"}"#,
            "Do not edit generated files; edit the generator instead.",
            Some("error[E0433]: failed to resolve"),
        )
        .await;

        let path = tmp.path().join("src/generated/api.rs");
        let injected = hook_pre_tool_use_impl(
            &handle,
            &json!({
                "tool_name": "Edit",
                "tool_input": {"file_path": path.to_string_lossy()},
                "session_id": "s1"
            }),
        )
        .await;
        assert_ne!(injected, json!({}), "the prior must have been injected");

        let transcript_path = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, transcript).unwrap();
        let dctx = make_collecting_dctx();
        hook_stop_impl(
            Arc::clone(&handle),
            &json!({
                "transcript_path": transcript_path.to_string_lossy(),
                "session_id": "s1"
            }),
            &dctx,
        );
        dctx.join_background().await;

        let (confirmed, refuted) = cluster_belief(&handle, &cluster_id).await;
        (
            confirmed,
            refuted,
            injection_outcome(&handle, &cluster_id).await,
        )
    }

    /// Story 092, criterion 6, end to end. This is the `pre_tool` case, the one
    /// where the operation provably ran — and it still earns nothing, because
    /// PreToolUse returns `additionalContext` and never a deny. The Edit was
    /// already committed when the prior appeared, so a quiet session shows the
    /// lesson being ignored without consequence, not the lesson working. The row
    /// closes as `unrefuted` so the next Stop hook does not ask again.
    #[tokio::test]
    async fn a_session_that_did_not_trip_the_error_does_not_confirm_the_prior() {
        let (confirmed, refuted, outcome) = inject_then_stop(MINE_BORING_TRANSCRIPT).await;
        assert_eq!(
            confirmed, 0,
            "a quiet session is not evidence the lesson held"
        );
        assert_eq!(refuted, 0);
        assert_eq!(
            outcome.as_deref(),
            Some("unrefuted"),
            "settled, not reopened"
        );
    }

    #[tokio::test]
    async fn the_warned_error_happening_anyway_refutes_the_prior() {
        // MINE_FIX_TRANSCRIPT carries exactly the failure the seeded cluster
        // warns about: the model was told, and hit it regardless.
        let (confirmed, refuted, outcome) = inject_then_stop(MINE_FIX_TRANSCRIPT).await;
        assert_eq!(refuted, 1, "the failure came back after the warning");
        assert_eq!(confirmed, 0);
        assert_eq!(outcome.as_deref(), Some("refuted"));
    }

    /// A prior nobody was shown has nothing to answer for. Without this, every
    /// Stop hook would confirm every promoted prior in the store.
    #[tokio::test]
    async fn a_prior_that_was_never_injected_is_not_settled() {
        let tmp = TempDir::new().unwrap();
        let handle = Arc::new(make_handle_with(&tmp, |config| {
            config.priors.mining_enabled = false;
        }));
        let cluster_id = seed_promoted_prior_with_signature(
            &handle,
            "pre_tool",
            r#"{"pattern":"src/generated/**"}"#,
            "Do not edit generated files.",
            Some("error[E0433]: failed to resolve"),
        )
        .await;

        let transcript_path = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript_path, MINE_BORING_TRANSCRIPT).unwrap();
        let dctx = make_collecting_dctx();
        hook_stop_impl(
            Arc::clone(&handle),
            &json!({
                "transcript_path": transcript_path.to_string_lossy(),
                "session_id": "s1"
            }),
            &dctx,
        );
        dctx.join_background().await;

        assert_eq!(cluster_belief(&handle, &cluster_id).await, (0, 0));
    }

    #[tokio::test]
    async fn hook_stop_noop_when_no_distiller_configured() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle_with(&tmp, |config| {
            config.priors.mining_enabled = true; // on, but no distiller_program → still off
        });
        let event = json!({"transcript_path": "/nonexistent", "session_id": "s1"});
        assert_eq!(hook_stop_impl(handle, &event, &make_dctx()), json!({}));
    }

    /// A transcript where a Bash error is followed by a corrective Edit and a
    /// clean result — the candidate detector's ErrorFixed signal.
    const MINE_FIX_TRANSCRIPT: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"refactor the parser"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build"}}]}}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"error[E0433]: failed to resolve"}]}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/lib.rs"}}]}}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#,
    );

    /// An episode the cheap detector turns down: no error, no user correction.
    const MINE_BORING_TRANSCRIPT: &str = concat!(
        r#"{"type":"user","message":{"role":"user","content":"add a doc comment"}}"#,
        "\n",
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/lib.rs"}}]}}"#,
        "\n",
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
    );

    /// Every `prior_mining` event written to `hook-events.jsonl` under `root`.
    fn mining_events(root: &std::path::Path) -> Vec<Value> {
        let dir = crate::store::namespace::store_dir(root).unwrap_or_else(|_| root.join(".mdkb"));
        let Ok(content) = std::fs::read_to_string(dir.join("hook-events.jsonl")) else {
            return Vec::new();
        };
        content
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v.get("event").and_then(Value::as_str) == Some("prior_mining"))
            .collect()
    }

    /// Mining says what it actually did, once per run.
    ///
    /// `hook_stop_impl` returns `{}` before the distiller has even started, so
    /// the `stop` event logged `outcome=skipped` on 105 of 105 Stop events while
    /// mining was completely dead. The detached task is the only place that knows
    /// whether the run gated, distilled, was rejected or failed, so it is the
    /// place that has to record it — with the reason, since "failed" without the
    /// error text is what made the six-week outage invisible.
    #[tokio::test]
    async fn mine_episode_records_the_outcome_it_reached() {
        let distilled = r#"{"is_reusable":true,"trigger":{"kind":"pre_tool","when":"editing generated code","pattern":"src/generated/**"},"lesson":"Do not edit generated files; edit the generator template.","scope":{"repo":"current","languages":["rust"]},"evidence":{"failure":"build error after direct edit","fix":"edited the generator"},"ttl_days":30}"#;
        let not_reusable = distilled.replace(r#""is_reusable":true"#, r#""is_reusable":false"#);

        // (transcript, distiller shell script, expected outcome, a substring the
        // reason must carry — empty when no reason belongs on that outcome).
        let cases: Vec<(&str, String, &str, &str)> = vec![
            (
                MINE_BORING_TRANSCRIPT,
                format!("cat >/dev/null; printf '%s' '{distilled}'"),
                "gated",
                "",
            ),
            (
                MINE_FIX_TRANSCRIPT,
                format!("cat >/dev/null; printf '%s' '{distilled}'"),
                "distilled",
                "",
            ),
            (
                MINE_FIX_TRANSCRIPT,
                format!("cat >/dev/null; printf '%s' '{not_reusable}'"),
                "rejected",
                "reusable",
            ),
            (
                MINE_FIX_TRANSCRIPT,
                "cat >/dev/null; echo 'model overloaded' >&2; exit 1".to_string(),
                "failed",
                "model overloaded",
            ),
        ];

        for (transcript_body, script, expected, reason_needle) in cases {
            let tmp = TempDir::new().unwrap();
            let handle = make_handle(&tmp);
            let transcript = tmp.path().join("transcript.jsonl");
            std::fs::write(&transcript, transcript_body).unwrap();

            mine_episode(
                Arc::clone(&handle),
                transcript.to_string_lossy().into_owned(),
                format!("sess-{expected}"),
                "sh".to_string(),
                vec!["-c".to_string(), script],
            )
            .await;

            let events = mining_events(tmp.path());
            assert_eq!(
                events.len(),
                1,
                "exactly one prior_mining event per run, got {events:?} for {expected}"
            );
            assert_eq!(
                events[0]["outcome"], expected,
                "wrong outcome recorded: {:?}",
                events[0]
            );
            if reason_needle.is_empty() {
                assert!(
                    events[0].get("reason").is_none(),
                    "{expected} carries no reason: {:?}",
                    events[0]
                );
            } else {
                let reason = events[0]["reason"].as_str().unwrap_or_default();
                assert!(
                    reason.contains(reason_needle),
                    "{expected} must quote why ({reason_needle:?}), got {reason:?}"
                );
            }
        }
    }

    /// Promotion is the event worth watching, so it is its own outcome.
    ///
    /// A mined prior does nothing until its cluster recurs across two distinct
    /// sessions and gets promoted — that is the point at which it starts being
    /// injected. Folding promotion into `distilled` would hide the difference
    /// between "the pipeline works" and "the pipeline works and taught the model
    /// something", which is the whole question `mdkb stats` is asked.
    #[tokio::test]
    async fn mine_episode_records_promotion_separately_from_distillation() {
        let distilled = r#"{"is_reusable":true,"trigger":{"kind":"pre_tool","when":"editing generated code","pattern":"src/generated/**"},"lesson":"Do not edit generated files; edit the generator template.","scope":{"repo":"current","languages":["rust"]},"evidence":{"failure":"build error after direct edit","fix":"edited the generator"},"ttl_days":30}"#;

        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let transcript = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript, MINE_FIX_TRANSCRIPT).unwrap();

        // The same lesson from two distinct sessions: the recurrence gate.
        for session in ["sess-a", "sess-b"] {
            mine_episode(
                Arc::clone(&handle),
                transcript.to_string_lossy().into_owned(),
                session.to_string(),
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    format!("cat >/dev/null; printf '%s' '{distilled}'"),
                ],
            )
            .await;
        }

        let outcomes: Vec<String> = mining_events(tmp.path())
            .iter()
            .map(|e| e["outcome"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            outcomes,
            vec!["distilled", "promoted"],
            "the first session integrates, the second promotes"
        );
    }

    #[tokio::test]
    async fn mine_episode_persists_candidate_via_fake_distiller() {
        use crate::store::priors::{canonical_trigger_key, cluster_id_for_key, get_cluster};

        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let transcript = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript, MINE_FIX_TRANSCRIPT).unwrap();

        // Fake distiller: consume stdin (the prompt), emit a valid distilled prior.
        // JSON uses only double quotes so it survives single-quote shell wrapping.
        let distilled = r#"{"is_reusable":true,"trigger":{"kind":"pre_tool","when":"editing generated code","pattern":"src/generated/**"},"lesson":"Do not edit generated files; edit the generator template.","scope":{"repo":"current","languages":["rust"]},"evidence":{"failure":"build error after direct edit","fix":"edited the generator"},"ttl_days":30}"#;
        let program = "sh".to_string();
        let args = vec![
            "-c".to_string(),
            format!("cat >/dev/null; printf '%s' '{distilled}'"),
        ];

        mine_episode(
            Arc::clone(&handle),
            transcript.to_string_lossy().into_owned(),
            "sess-1".to_string(),
            program,
            args,
        )
        .await;

        // One session → a candidate cluster exists (not yet promoted).
        ensure_handle_context(&handle).await.unwrap();
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        let key = canonical_trigger_key(
            "pre_tool",
            r#"{"pattern":"src/generated/**","when":"editing generated code"}"#,
        );
        let cluster = get_cluster(conn, &cluster_id_for_key(&key))
            .unwrap()
            .expect("mining created a candidate cluster");
        assert_eq!(cluster.state, "candidate");
        assert_eq!(cluster.distinct_sessions, 1);
        assert!(cluster.lesson.contains("Do not edit generated files"));
    }

    /// End-to-end proof of the fence tolerance, at the level that actually
    /// failed: `claude -p` wraps its answer in a ```json fence, mdkb rejected it
    /// as NotJson, and mining produced nothing for six weeks. Same episode, same
    /// lesson, only the wrapping differs — a cluster must appear.
    #[tokio::test]
    async fn mine_episode_accepts_a_distiller_that_fences_its_json() {
        use crate::store::priors::{canonical_trigger_key, cluster_id_for_key, get_cluster};

        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let transcript = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript, MINE_FIX_TRANSCRIPT).unwrap();

        let distilled = r#"{"is_reusable":true,"trigger":{"kind":"pre_tool","when":"editing generated code","pattern":"src/generated/**"},"lesson":"Do not edit generated files; edit the generator template.","scope":{"repo":"current","languages":["rust"]},"evidence":{"failure":"build error after direct edit","fix":"edited the generator"},"ttl_days":30}"#;
        let args = vec![
            "-c".to_string(),
            format!("cat >/dev/null; printf 'Here you go:\\n```json\\n%s\\n```\\n' '{distilled}'"),
        ];

        mine_episode(
            Arc::clone(&handle),
            transcript.to_string_lossy().into_owned(),
            "sess-fenced".to_string(),
            "sh".to_string(),
            args,
        )
        .await;

        ensure_handle_context(&handle).await.unwrap();
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        let key = canonical_trigger_key(
            "pre_tool",
            r#"{"pattern":"src/generated/**","when":"editing generated code"}"#,
        );
        let cluster = get_cluster(conn, &cluster_id_for_key(&key))
            .unwrap()
            .expect("a fenced answer must still mine a candidate cluster");
        assert_eq!(cluster.distinct_sessions, 1);
    }

    /// A distiller that takes its prompt in argv (`grok -p`) mines just as well,
    /// and one that fails writes nothing — the failure is reported, not absorbed
    /// into a half-built cluster.
    #[tokio::test]
    async fn mine_episode_honours_the_prompt_placeholder_and_persists_nothing_on_failure() {
        use crate::store::priors::{canonical_trigger_key, cluster_id_for_key, get_cluster};

        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let transcript = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript, MINE_FIX_TRANSCRIPT).unwrap();
        let key = canonical_trigger_key(
            "pre_tool",
            r#"{"pattern":"src/generated/**","when":"editing generated code"}"#,
        );
        let cluster_id = cluster_id_for_key(&key);

        // Exits non-zero with a usage message, the shape of a misconfigured CLI.
        mine_episode(
            Arc::clone(&handle),
            transcript.to_string_lossy().into_owned(),
            "sess-broken".to_string(),
            "sh".to_string(),
            vec![
                "-c".to_string(),
                "printf 'usage: distill [OPTIONS]'; exit 2".to_string(),
            ],
        )
        .await;
        ensure_handle_context(&handle).await.unwrap();
        {
            let guard = handle.ctx.lock().await;
            let conn = &guard.as_ref().unwrap().conn;
            assert!(
                get_cluster(conn, &cluster_id).unwrap().is_none(),
                "a failed distiller must leave the store untouched"
            );
        }

        // The prompt arrives in argv: the stub echoes $1 back as the answer, so a
        // cluster appears only if substitution happened.
        let distilled = r#"{"is_reusable":true,"trigger":{"kind":"pre_tool","when":"editing generated code","pattern":"src/generated/**"},"lesson":"Do not edit generated files; edit the generator template.","scope":{"repo":"current","languages":["rust"]},"evidence":{"failure":"build error after direct edit","fix":"edited the generator"},"ttl_days":30}"#;
        mine_episode(
            Arc::clone(&handle),
            transcript.to_string_lossy().into_owned(),
            "sess-argv".to_string(),
            "sh".to_string(),
            vec![
                "-c".to_string(),
                format!("test -n \"$1\" && printf '%s' '{distilled}'"),
                "sh".to_string(),
                "{prompt}".to_string(),
            ],
        )
        .await;
        let guard = handle.ctx.lock().await;
        let conn = &guard.as_ref().unwrap().conn;
        let cluster = get_cluster(conn, &cluster_id)
            .unwrap()
            .expect("the prompt must reach argv and the answer must be mined");
        assert_eq!(cluster.distinct_sessions, 1);
    }

    /// The full flagship loop: two independent sessions distill the same lesson,
    /// the cluster crosses the promotion gate, and a trigger-matching PreToolUse
    /// injects it. This lives in-module (not a `tests/` binary) on purpose: the
    /// Stop hook detaches `mine_episode` via `tokio::spawn` and returns before it
    /// finishes, and `pretool_prior_block` reads only an ALREADY-open context — so
    /// a one-shot CLI invocation can neither await mining nor warm the ctx. The
    /// end-to-end promote→inject path is only observable with a live handle.
    #[tokio::test]
    async fn mine_episode_promotes_across_two_sessions_and_injects_matched_prior() {
        use crate::store::priors::{canonical_trigger_key, cluster_id_for_key, get_cluster};

        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let transcript = tmp.path().join("transcript.jsonl");
        std::fs::write(&transcript, MINE_FIX_TRANSCRIPT).unwrap();

        // Fake distiller: consume the prompt on stdin, emit a valid pre_tool prior
        // whose glob targets generated files. Identical output both sessions → one
        // trigger key → one cluster whose distinct_sessions climbs to the promotion
        // gate (PROMOTION_MIN_SESSIONS = 2).
        let distilled = r#"{"is_reusable":true,"trigger":{"kind":"pre_tool","when":"editing generated code","pattern":"src/generated/**"},"lesson":"Do not edit generated files; edit the generator template instead.","scope":{"repo":"current","languages":["rust"]},"evidence":{"failure":"build error after direct edit","fix":"edited the generator"},"ttl_days":30}"#;
        let program = "sh".to_string();
        let args = vec![
            "-c".to_string(),
            format!("cat >/dev/null; printf '%s' '{distilled}'"),
        ];

        for session in ["sess-1", "sess-2"] {
            mine_episode(
                Arc::clone(&handle),
                transcript.to_string_lossy().into_owned(),
                session.to_string(),
                program.clone(),
                args.clone(),
            )
            .await;
        }

        // Two distinct sessions crossed the gate: the cluster is promoted and has
        // minted a backing memory id.
        let key = canonical_trigger_key(
            "pre_tool",
            r#"{"pattern":"src/generated/**","when":"editing generated code"}"#,
        );
        let cluster_id = cluster_id_for_key(&key);
        {
            let guard = handle.ctx.lock().await;
            let conn = &guard.as_ref().unwrap().conn;
            let cluster = get_cluster(conn, &cluster_id)
                .unwrap()
                .expect("mining created a cluster");
            assert_eq!(
                cluster.state, "promoted",
                "two distinct sessions must promote the cluster"
            );
            assert_eq!(cluster.distinct_sessions, 2);
            assert!(
                cluster.promoted_memory_id.is_some(),
                "promotion mints a backing memory id"
            );
        }

        // A PreToolUse whose repo-relative path matches the prior's glob injects
        // the lesson verbatim.
        let edit_path = tmp.path().join("src/generated/schema.rs");
        let hit = pretool_prior_block(
            &handle,
            "Edit",
            &json!({"file_path": edit_path.to_string_lossy()}),
            "sess-inject",
        )
        .await
        .expect("promoted prior must inject on a matching PreToolUse");
        assert!(
            hit.contains("mdkb prior: Do not edit generated files"),
            "injected block must carry the lesson: {hit}"
        );

        // A path outside the glob surfaces nothing — injection is trigger-scoped,
        // never global.
        let unrelated_path = tmp.path().join("src/hand_written.rs");
        let unrelated = pretool_prior_block(
            &handle,
            "Edit",
            &json!({"file_path": unrelated_path.to_string_lossy()}),
            "sess-inject",
        )
        .await;
        assert!(
            unrelated.is_none(),
            "a path outside the glob must not inject the prior: {unrelated:?}"
        );
    }

    /// A `post_tool` prior reaches the model it was mined for.
    ///
    /// Before this, `post_tool` was an accepted trigger kind with no matcher and
    /// no injection point, so a lesson like "regenerate after touching the
    /// template" was mined, promoted and then silently stranded. The lesson has
    /// to arrive at PostToolUse, alongside the reindex signal rather than
    /// instead of it.
    #[tokio::test]
    async fn hook_post_tool_use_injects_a_matching_post_tool_prior() {
        use crate::store::priors::{canonical_trigger_key, cluster_id_for_key, promote_cluster};

        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let file = tmp.path().join("src").join("schema.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "// generated").unwrap();

        let matcher = r#"{"pattern":"src/**"}"#;
        let key = canonical_trigger_key("post_tool", matcher);
        let cluster_id = cluster_id_for_key(&key);
        ensure_handle_context(&handle).await.unwrap();
        {
            let mut guard = handle.ctx.lock().await;
            let ctx = guard.as_mut().unwrap();
            crate::store::priors::upsert_cluster(
                &ctx.conn,
                &crate::store::priors::PriorCluster {
                    id: cluster_id.clone(),
                    canonical_trigger_key: key,
                    trigger_kind: "post_tool".into(),
                    trigger_matcher: matcher.into(),
                    lesson: "Run the generator after editing the template.".into(),
                    scope: r#"{"repo":"current"}"#.into(),
                    evidence_count: 2,
                    distinct_sessions: 2,
                    injected_count: 0,
                    confirmed_count: 0,
                    refuted_count: 0,
                    state: "candidate".into(),
                    promoted_memory_id: None,
                    created_at: chrono::Utc::now().timestamp(),
                    last_seen_at: chrono::Utc::now().timestamp(),
                    error_signature: None,
                },
            )
            .unwrap();
            promote_cluster(&ctx.conn, &cluster_id, chrono::Utc::now().timestamp()).unwrap();
        }

        let event = json!({
            "tool_name": "Write",
            "tool_input": {"file_path": file.to_str().unwrap()},
        });
        let result = hook_post_tool_use_impl(&handle, &event).await;
        let injected = result["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default();
        assert!(
            injected.contains("Run the generator after editing the template."),
            "a matching post_tool prior must be injected, got: {result}"
        );
        assert_eq!(
            result["hookSpecificOutput"]["hookEventName"], "PostToolUse",
            "the block must be labelled for the event it answers: {result}"
        );
        assert_eq!(
            result["queued"], true,
            "injecting a prior must not cancel the reindex the hook exists for: {result}"
        );

        // A tool call outside the glob gets the reindex and no lesson.
        let other = tmp.path().join("notes.md");
        std::fs::write(&other, "text").unwrap();
        let result = hook_post_tool_use_impl(
            &handle,
            &json!({"tool_name": "Write", "tool_input": {"file_path": other.to_str().unwrap()}}),
        )
        .await;
        assert!(
            result.get("hookSpecificOutput").is_none(),
            "injection is trigger-scoped, never global: {result}"
        );
    }

    /// A lesson about a tool that never reindexes still arrives.
    ///
    /// `Bash` is not in `REINDEX_TOOLS`, so the hook used to return early for it.
    /// Matching priors before that gate is what makes "run the generator after
    /// the build" reachable — the majority of post-hoc lessons are about commands,
    /// not file writes.
    #[tokio::test]
    async fn hook_post_tool_use_injects_a_prior_for_a_tool_that_never_reindexes() {
        use crate::store::priors::{canonical_trigger_key, cluster_id_for_key, promote_cluster};

        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let matcher = r#"{"pattern":"cargo build"}"#;
        let key = canonical_trigger_key("post_tool", matcher);
        let cluster_id = cluster_id_for_key(&key);
        ensure_handle_context(&handle).await.unwrap();
        {
            let mut guard = handle.ctx.lock().await;
            let ctx = guard.as_mut().unwrap();
            crate::store::priors::upsert_cluster(
                &ctx.conn,
                &crate::store::priors::PriorCluster {
                    id: cluster_id.clone(),
                    canonical_trigger_key: key,
                    trigger_kind: "post_tool".into(),
                    trigger_matcher: matcher.into(),
                    lesson: "Check mbx explain --last before blaming the build.".into(),
                    scope: r#"{"repo":"current"}"#.into(),
                    evidence_count: 2,
                    distinct_sessions: 2,
                    injected_count: 0,
                    confirmed_count: 0,
                    refuted_count: 0,
                    state: "candidate".into(),
                    promoted_memory_id: None,
                    created_at: chrono::Utc::now().timestamp(),
                    last_seen_at: chrono::Utc::now().timestamp(),
                    error_signature: None,
                },
            )
            .unwrap();
            promote_cluster(&ctx.conn, &cluster_id, chrono::Utc::now().timestamp()).unwrap();
        }

        let result = hook_post_tool_use_impl(
            &handle,
            &json!({"tool_name": "Bash", "tool_input": {"command": "cargo build --lib"}}),
        )
        .await;
        let injected = result["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default();
        assert!(
            injected.contains("Check mbx explain --last before blaming the build."),
            "a post_tool prior on a non-reindex tool must still be injected, got: {result}"
        );
        assert!(
            result.get("queued").is_none(),
            "Bash queues no reindex; only the lesson is added: {result}"
        );
    }

    #[tokio::test]
    async fn hook_post_tool_use_ignores_unknown_tool() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let event = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        let result = hook_post_tool_use_impl(&handle, &event).await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_post_tool_use_injects_valid_path() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        // Create a real file so canonicalize_under_cwd can resolve the parent dir
        let file = tmp.path().join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}").unwrap();
        let event = json!({
            "tool_name": "Write",
            "tool_input": {"file_path": file.to_str().unwrap()},
        });
        let result = hook_post_tool_use_impl(&handle, &event).await;
        assert_eq!(
            result,
            json!({"queued": true}),
            "post_tool_use returns queued on success"
        );
    }

    #[tokio::test]
    async fn hook_pre_tool_use_suggests_symbol_search() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let event = json!({
            "tool_name": "Grep",
            "tool_input": {"pattern": "handle_session_start"},
        });
        let result = hook_pre_tool_use_impl(&handle, &event).await;
        assert!(
            result.get("hookSpecificOutput").is_some(),
            "must suggest alternative for plain identifier"
        );
    }

    #[tokio::test]
    async fn hook_pre_tool_use_silent_for_non_grep_tool() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let event = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "src/lib.rs"},
        });
        let result = hook_pre_tool_use_impl(&handle, &event).await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn dispatch_call_routes_hook_session_start() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        let result = dispatch_call("hook.session_start", json!({}), handle, &dctx)
            .await
            .expect("must not error");
        // Empty index → silent, but the call must succeed
        assert!(result == json!({}) || result.get("hookSpecificOutput").is_some());
    }

    #[tokio::test]
    async fn dispatch_call_routes_hook_pre_tool_use() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        let event = json!({
            "tool_name": "Grep",
            "tool_input": {"pattern": "dispatch_call"},
        });
        let result = dispatch_call("hook.pre_tool_use", event, handle, &dctx)
            .await
            .expect("must not error");
        assert!(result.get("hookSpecificOutput").is_some());
    }

    // ── PERF-A1: query embedding runs off the ctx lock (story 056) ──────

    // ── ARCH-A1: RAII guard for reindex flags (story 066) ───────────────

    #[test]
    fn active_flag_guard_clears_flag_on_panic() {
        // The reindex wedge: a panic mid-reindex must not leave the in-flight
        // flag stuck true (which would make every future reindex a no-op).
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_closure = Arc::clone(&flag);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = ActiveFlagGuard::arm(flag_for_closure).expect("arm");
            panic!("simulated reindex panic");
        }));
        assert!(result.is_err(), "panic should propagate to catch_unwind");
        assert!(
            !flag.load(Ordering::Relaxed),
            "guard must clear the flag on a panic unwind, not leave the handle wedged"
        );
    }

    #[test]
    fn active_flag_guard_is_single_flight_and_rearmable() {
        let flag = Arc::new(AtomicBool::new(false));
        let g1 = ActiveFlagGuard::arm(Arc::clone(&flag));
        assert!(g1.is_some(), "first arm succeeds");
        assert!(
            ActiveFlagGuard::arm(Arc::clone(&flag)).is_none(),
            "second arm is blocked while the first is active"
        );
        drop(g1);
        assert!(
            ActiveFlagGuard::arm(Arc::clone(&flag)).is_some(),
            "re-armable once the guard drops"
        );
    }

    // ── PERF-A3: hook_dedup eviction (story 067) ────────────────────────

    #[test]
    fn hook_dedup_lru_caps_session_count() {
        // Many short-lived, distinct sessions (e.g. abnormally-ended clients that
        // never send a Stop) must not grow the map without bound.
        let dctx = make_dctx();
        for i in 0..(MAX_HOOK_SESSIONS + 100) {
            dctx.with_hook_session(&format!("repo|session:{i}"), |s| {
                s.memory_ids.insert("m".to_string());
            });
        }
        let count = dctx.hook_dedup.lock().unwrap().sessions.len();
        assert!(
            count <= MAX_HOOK_SESSIONS,
            "session map must stay bounded by the LRU cap, got {count}"
        );
    }

    #[test]
    fn hook_dedup_ttl_evicts_stale_sessions() {
        let dctx = make_dctx();
        dctx.with_hook_session("repo|session:stale", |s| {
            s.memory_ids.insert("m".to_string());
        });

        // Backdate the stale session beyond the TTL, then touch a different one:
        // the TTL sweep in with_hook_session must drop the stale entry.
        {
            let mut state = dctx.hook_dedup.lock().unwrap();
            let stale = state.sessions.get_mut("repo|session:stale").unwrap();
            stale.last_touched = std::time::Instant::now()
                .checked_sub(HOOK_SESSION_TTL + std::time::Duration::from_secs(1))
                .expect("instant underflow");
        }

        dctx.with_hook_session("repo|session:fresh", |s| {
            s.memory_ids.insert("m".to_string());
        });

        let state = dctx.hook_dedup.lock().unwrap();
        assert!(
            !state.sessions.contains_key("repo|session:stale"),
            "a session untouched past the TTL must be evicted"
        );
        assert!(
            state.sessions.contains_key("repo|session:fresh"),
            "the freshly-touched session must remain"
        );
    }

    #[tokio::test]
    async fn embed_query_off_lock_completes_without_hanging() {
        // Deterministic + portable: the helper every recall/search site now
        // calls before locking must complete (never hang/panic) whether or not
        // an ONNX model is present. With a model it yields a non-empty vector;
        // without one it degrades to None (BM25 fallback).
        let out = embed_query_off_lock("hook dispatcher architecture").await;
        if let Some(v) = out {
            assert!(!v.is_empty(), "an embedding vector must be non-empty");
        }
    }

    #[tokio::test]
    #[ignore = "requires ONNX model download (see tests/e2e_llm.rs convention)"]
    async fn embeds_run_concurrently_off_the_runtime() {
        // PERF-A1 proof: embedding is on the blocking pool, not serialized by
        // the async runtime or a shared lock. N concurrent embeds must overlap,
        // so wall-time stays far below N × single-embed latency. If a future
        // change moved embedding back under a single held mutex, the calls would
        // serialize and this margin would collapse.
        crate::llm::release_cached_service();
        let single_t0 = std::time::Instant::now();
        embed_query_off_lock("warm up the model").await;
        let single = single_t0.elapsed();

        const N: usize = 8;
        let all_t0 = std::time::Instant::now();
        let handles: Vec<_> = (0..N)
            .map(|i| tokio::spawn(async move { embed_query_off_lock(&format!("query {i}")).await }))
            .collect();
        for h in handles {
            h.await.unwrap();
        }
        let concurrent = all_t0.elapsed();
        assert!(
            concurrent < single * (N as u32),
            "concurrent embeds ({concurrent:?}) should overlap, not serialize \
             to N×single ({:?})",
            single * (N as u32)
        );
    }
}
