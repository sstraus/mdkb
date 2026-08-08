//! `mdkb hook <cmd>` — one-shot JSON-RPC client.
//!
//! Connects to the daemon hook socket (`~/.mdkb/daemon-hook.sock`),
//! auto-spawning the daemon on `ECONNREFUSED`/`ENOENT`, sends exactly one
//! length-prefixed JSON-RPC request, prints the result text to stdout,
//! and exits.
//!
//! Framing and method names must match `daemon::ipc_server::handle_hook_conn`
//! and `mcp::dispatch::dispatch_call` — this file only knows how to talk to
//! them, not what they do.
//!
//! Hook contract: host CLIs (Claude Code, Codex) invoke this binary from
//! their lifecycle hooks. It must **never** block or fail-exit the host.
//! Every error path prints to stderr and returns `Ok(())` so the parent
//! shell sees exit 0. The daemon signals whitelist rejection with a
//! JSON-RPC `error` envelope (code `-32602`), which we log and swallow.
//!
//! `MDKB_NO_DAEMON=1` short-circuits to an in-process dispatch that opens
//! an ephemeral `RepoRegistry` and calls `dispatch_call` directly. Same
//! wire-level result, no daemon required — useful for scripts and tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::DaemonConfig;
use crate::daemon::ipc_server::DISPATCHED_ERROR_CODE;
use crate::daemon::registry::RepoRegistry;
use crate::daemon::spawn::ensure_daemon_running;
use crate::error::Result;
use crate::mcp::dispatch::{DispatchContext, dispatch_call};
use crate::metrics::UsageMetrics;

/// Hook socket filename, relative to the daemon base dir.
const HOOK_SOCKET_NAME: &str = "daemon-hook.sock";

/// Upper bound on a single JSON-RPC message. Matches the daemon side so
/// oversized payloads fail fast with a clear error instead of hanging.
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Per-call socket I/O timeout. Generous; the daemon should answer in
/// single-digit ms. A timeout here means the daemon hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for a routed mutation to come back.
///
/// Not a correctness bound — [`MutationFailure::Undetermined`] is what keeps a slow
/// daemon from becoming a second writer. This is only a liveness bound so a
/// wedged daemon does not hang the terminal forever, which is why it is an hour
/// rather than a tuned figure: `mdkb update` on a large store walks the tree,
/// embeds through ONNX and reindexes the whole code graph, and guessing low
/// costs a spurious failure on exactly the command that most needs to finish.
const MUTATION_TIMEOUT: Duration = Duration::from_hours(1);

// ── Public entry points ──────────────────────────────────────────────────────

/// Send a `status` call.
pub async fn call_status(root: Option<PathBuf>) -> Result<()> {
    run("status", json!({}), root).await
}

/// Send an `update` (reindex) call, scoped to `files` when given.
pub async fn call_reindex(files: Vec<String>, root: Option<PathBuf>) -> Result<()> {
    let mut params = json!({});
    if !files.is_empty() {
        params["files"] = json!(files);
    }
    run("update", params, root).await
}

/// Send a `search` call with optional scope and limit.
pub async fn call_search(
    query: String,
    scope: Option<String>,
    limit: Option<usize>,
    root: Option<PathBuf>,
) -> Result<()> {
    let mut params = json!({ "query": query });
    if let Some(s) = scope {
        params["scope"] = json!(s);
    }
    if let Some(n) = limit {
        params["limit"] = json!(n);
    }
    run("search", params, root).await
}

/// Send a `memory_write` call.
#[allow(clippy::too_many_arguments)]
pub async fn call_memory_write(
    id: String,
    title: String,
    entry_type: String,
    content: String,
    tags: Option<String>,
    ttl: Option<u64>,
    root: Option<PathBuf>,
) -> Result<()> {
    let mut params = json!({
        "id": id,
        "title": title,
        "entry_type": entry_type,
        "content": content,
    });
    if let Some(t) = tags {
        params["tags"] = json!(parse_comma_separated_tags(&t));
    }
    if let Some(t) = ttl {
        params["ttl"] = json!(t);
    }
    run("memory_write", params, root).await
}

fn parse_comma_separated_tags(tags: &str) -> Vec<&str> {
    tags.split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .collect()
}

/// Send a `memory_confirm` call.
pub async fn call_memory_confirm(id: String, outcome: String, root: Option<PathBuf>) -> Result<()> {
    run(
        "memory_confirm",
        json!({ "id": id, "outcome": outcome }),
        root,
    )
    .await
}

// ── Core dispatch ────────────────────────────────────────────────────────────

/// Pick the target repo root, host-agnostically.
///
/// An explicit `--root` always wins. Otherwise the anchor is resolved by
/// walking up from the current directory: nearest existing `.mdkb/` →
/// nearest git root → `CLAUDE_PROJECT_DIR` (the stable launch dir on Claude
/// Code) → cwd. This makes the store immune to working-directory drift (the
/// hook process cwd changes as the agent `cd`s, on both Claude Code and
/// Codex) and lets concurrent agents on one project share a single store.
/// The result is collapsed to the main worktree and canonicalized (the
/// daemon whitelist compares canonical paths).
fn resolve_root(explicit: Option<PathBuf>) -> PathBuf {
    let resolved = if let Some(path) = explicit {
        crate::git::resolve_main_worktree(&path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let hint = std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from);
        crate::git::resolve_project_root(&cwd, hint.as_deref())
    };
    resolved.canonicalize().unwrap_or(resolved)
}

/// Resolve the target repository for a lifecycle hook event.
///
/// Hook hosts provide the session working directory in `event.cwd`. Prefer it
/// over the hook subprocess working directory: global hooks can be launched
/// from a stale or unrelated cwd when several repositories are active in the
/// same host process. An explicit root still has highest priority.
pub fn resolve_hook_root(event: &Value, explicit: Option<PathBuf>) -> PathBuf {
    if explicit.is_some() {
        return resolve_root(explicit);
    }

    if let Some(cwd) = event
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
    {
        let cwd = PathBuf::from(cwd);
        if cwd.is_absolute() && cwd.is_dir() {
            let hint = std::env::var_os("CLAUDE_PROJECT_DIR").map(PathBuf::from);
            let resolved = crate::git::resolve_project_root(&cwd, hint.as_deref());
            return resolved.canonicalize().unwrap_or(resolved);
        }
    }

    resolve_root(None)
}

fn hook_socket_path() -> PathBuf {
    // Hook socket always lives beside daemon.sock under ~/.mdkb — see
    // `daemon::ipc_server::serve(base_dir = lock.parent())` and
    // `main.rs` where base_dir is derived from the pid lock path under
    // `DaemonConfig::daemon_home()`.
    DaemonConfig::daemon_home().join(HOOK_SOCKET_NAME)
}

// ── Per-event hook timeouts (daemon must answer within these) ────────────────

const HOOK_TIMEOUT_SESSION_START: Duration = Duration::from_secs(2);
const HOOK_TIMEOUT_USER_PROMPT_SUBMIT: Duration = Duration::from_secs(1);
const HOOK_TIMEOUT_POST_TOOL_USE: Duration = Duration::from_millis(500);
const HOOK_TIMEOUT_PRE_TOOL_USE: Duration = Duration::from_millis(300);
// Stop returns immediately: distillation is spawned as a detached background
// task in the daemon, so the socket only has to cover the enqueue.
const HOOK_TIMEOUT_STOP: Duration = Duration::from_millis(500);

/// Return the per-event socket timeout for a hook method.
fn hook_timeout(method: &str) -> Duration {
    match method {
        "hook.session_start" => HOOK_TIMEOUT_SESSION_START,
        "hook.user_prompt_submit" => HOOK_TIMEOUT_USER_PROMPT_SUBMIT,
        "hook.post_tool_use" => HOOK_TIMEOUT_POST_TOOL_USE,
        "hook.pre_tool_use" => HOOK_TIMEOUT_PRE_TOOL_USE,
        "hook.stop" => HOOK_TIMEOUT_STOP,
        _ => CALL_TIMEOUT,
    }
}

/// Send a lifecycle hook event to the daemon and emit the result to stdout.
///
/// `event` is the raw event JSON received on stdin by the hook binary.
/// On any error the function logs to stderr and returns `Ok(())` — host CLIs
/// must see exit 0.
pub async fn call_hook_event(method: &str, event: Value, root: Option<PathBuf>) -> Result<()> {
    run_hook(method, event, root).await
}

/// Like `run` but routes through `emit_hook_response` (raw envelope → stdout)
/// rather than `print_tool_text` (text extraction). Uses per-event timeouts.
async fn run_hook(method: &str, mut params: Value, root: Option<PathBuf>) -> Result<()> {
    let root = resolve_hook_root(&params, root);
    params["root"] = json!(root.display().to_string());

    if std::env::var_os("MDKB_NO_DAEMON").is_some() {
        return run_hook_in_process(method, params, &root).await;
    }

    let socket_path = hook_socket_path();
    let timeout = hook_timeout(method);
    match call_daemon_with_timeout(&socket_path, method, &params, timeout).await {
        Ok(response) => {
            emit_hook_response(&response);
            Ok(())
        }
        // The daemon could not answer. Do the work here instead of returning
        // silence.
        //
        // The generated wiring used to be
        // `if ! mdkb hook <event>; then MDKB_NO_DAEMON=1 mdkb hook <event>; fi`,
        // which can never fire: this function returns `Ok(())` on every failure
        // by contract, because the host hook must exit 0. So the branch was
        // unreachable and a dead daemon meant hooks silently did nothing, while
        // the settings file advertised a rail that did not exist (021-0636).
        //
        // Falling back in-process rather than fixing the exit code keeps that
        // contract intact — the host still never sees a non-zero exit for an
        // ordinary hook failure — and is strictly better than the shell retry
        // would have been: no second process launch, and no way for the two
        // invocations to disagree about which repo they are in.
        Err(e) => {
            tracing::warn!("hook {method}: daemon unavailable ({e}); running in-process");
            run_hook_in_process(method, params, &root).await
        }
    }
}

/// Daemon config for the in-process (`MDKB_NO_DAEMON`) fallback. The `root` here
/// is locally resolved (the user's own repo), not a client-supplied path, so it
/// is added to the whitelist: the default-deny confinement (SEC-3, story 070)
/// targets the networked/global daemon serving arbitrary client roots, not this
/// trusted single-repo in-process path.
fn in_process_config(root: &Path) -> Result<DaemonConfig> {
    let mut config = DaemonConfig::load_or_default(&DaemonConfig::config_path())?;
    config
        .whitelist_dirs
        .push(root.to_string_lossy().to_string());
    Ok(config)
}

/// In-process fallback for `MDKB_NO_DAEMON=1`. Same logic as `run_in_process`
/// but uses `emit_hook_response`.
async fn run_hook_in_process(method: &str, params: Value, root: &Path) -> Result<()> {
    let registry = Arc::new(RepoRegistry::new(in_process_config(root)?));
    let dctx = DispatchContext {
        metrics: Arc::new(UsageMetrics::new()),
        session_id: Arc::new(AtomicI64::new(0)),
        persistent_call_count: Arc::new(AtomicU64::new(0)),
        optimize_interval_calls: 200,
        hook_dedup: Arc::new(std::sync::Mutex::new(Default::default())),
    };

    match registry.get_or_open(root) {
        Ok(handle) => match dispatch_call(method, params, handle, &dctx).await {
            Ok(result) => emit_hook_response(&result),
            Err(e) => eprintln!("mdkb hook {method}: {}", e.message),
        },
        Err(e) => eprintln!("mdkb hook {method}: repo registry: {e}"),
    }
    Ok(())
}

/// Write the hook result envelope to stdout when it contains output.
/// Silent when the result is `{}` — the host CLI must receive nothing in
/// that case (stdout silence = "hook has nothing to add").
pub fn emit_hook_response(result: &Value) {
    match result {
        Value::Null => {}
        Value::Object(o) if o.is_empty() => {}
        _ => {
            if let Ok(s) = serde_json::to_string(result) {
                println!("{s}");
            }
        }
    }
}

/// Run a single JSON-RPC call. On any daemon-reported or transport error,
/// log to stderr and return Ok(()) — the host hook must exit 0.
async fn run(method: &str, mut params: Value, root: Option<PathBuf>) -> Result<()> {
    let root = resolve_root(root);
    params["root"] = json!(root.display().to_string());

    if std::env::var_os("MDKB_NO_DAEMON").is_some() {
        return run_in_process(method, params, &root).await;
    }

    let socket_path = hook_socket_path();
    match call_daemon(&socket_path, method, &params).await {
        Ok(response) => print_tool_text(method, &response),
        Err(e) => eprintln!("mdkb hook {method}: {e}"),
    }
    Ok(())
}

/// Direct in-process dispatch for `MDKB_NO_DAEMON=1`. Opens an ephemeral
/// `RepoRegistry` in the current process and routes the call through
/// `dispatch_call` — identical code path to the daemon, no IPC.
async fn run_in_process(method: &str, params: Value, root: &Path) -> Result<()> {
    let registry = Arc::new(RepoRegistry::new(in_process_config(root)?));
    let dctx = DispatchContext {
        metrics: Arc::new(UsageMetrics::new()),
        session_id: Arc::new(AtomicI64::new(0)),
        persistent_call_count: Arc::new(AtomicU64::new(0)),
        optimize_interval_calls: 200,
        hook_dedup: Arc::new(std::sync::Mutex::new(Default::default())),
    };

    match registry.get_or_open(root) {
        Ok(handle) => match dispatch_call(method, params, handle, &dctx).await {
            Ok(result) => print_tool_text(method, &result),
            Err(e) => eprintln!("mdkb hook {method}: {}", e.message),
        },
        Err(e) => eprintln!("mdkb hook {method}: repo registry: {e}"),
    }
    Ok(())
}

/// Print the `text` field of a tool result to stdout, falling back to
/// the raw JSON if the daemon returns something unexpected.
pub fn print_tool_text(method: &str, result: &Value) {
    if let Some(text) = result.get("text").and_then(Value::as_str) {
        println!("{text}");
        return;
    }
    // No text field — dump as pretty JSON so the user sees something useful.
    match serde_json::to_string_pretty(result) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("mdkb hook {method}: serialize response: {e}"),
    }
}

/// Connect (auto-spawn on absence), send one request, read one response.
///
/// Returns `Err` for transport failures and for JSON-RPC `error` envelopes;
/// the caller is expected to log and swallow both.
async fn call_daemon(
    socket_path: &Path,
    method: &str,
    params: &Value,
) -> std::result::Result<Value, String> {
    call_daemon_with_timeout(socket_path, method, params, CALL_TIMEOUT).await
}

/// [`call_daemon_phased`] flattened to one error string.
///
/// Hook events are advisory — a missed one costs a little context, never
/// correctness — so their callers log and swallow every failure alike and have
/// no use for the delivery phase. Mutations do; they call the phased form.
async fn call_daemon_with_timeout(
    socket_path: &Path,
    method: &str,
    params: &Value,
    timeout: Duration,
) -> std::result::Result<Value, String> {
    call_daemon_phased(socket_path, method, params, timeout)
        .await
        .map_err(|e| e.to_string())
}

/// Write one length-prefixed frame, flipping `delivered` the instant the last
/// byte of the body is out.
///
/// The flag is the delivery boundary, and it is set from inside the future
/// rather than inferred from its result so that a cancelled write is classified
/// by how far it actually got. The daemon reads the 4-byte header and then
/// exactly that many bytes before it dispatches anything
/// (`daemon::ipc_server::handle_hook_conn`), so a frame that was cut short can
/// never have started a mutation: the peer is still blocked in `read_exact`
/// and aborts when this side drops the stream.
async fn send_request(
    stream: &mut UnixStream,
    body: &[u8],
    delivered: &mut bool,
) -> std::io::Result<()> {
    let len = u32::try_from(body.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "request too large"))?;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(body).await?;
    *delivered = true;
    stream.flush().await
}

/// Read one length-prefixed frame back.
async fn read_response(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut hdr = [0u8; 4];
    stream.read_exact(&mut hdr).await?;
    let resp_len = u32::from_le_bytes(hdr) as usize;
    if resp_len == 0 || resp_len > MAX_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("bogus response length {resp_len}"),
        ));
    }
    let mut buf = vec![0u8; resp_len];
    stream.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Whether this process may run the mutation itself, and why not when it may
/// not.
///
/// The distinction is the whole point of the type. The first version of this
/// routing gave up after 30 seconds and fell back unconditionally, so a
/// `mdkb update` that the daemon was still working on had the CLI open its own
/// write connection alongside it — a routing layer built to remove a second
/// writer manufacturing one, on the longest write in the program.
///
/// The variants name the evidence, not the symptom. Where the call broke is
/// only a means of answering the one question that matters: can the daemon have
/// started writing?
#[derive(Debug)]
pub enum MutationFailure {
    /// The daemon provably did not run it — the request never got there, or it
    /// arrived and was refused before dispatch. Running in-process is safe, and
    /// required: a routing layer that turns a daemon outage into a broken CLI is
    /// worse than no routing at all (story 021-0636), and a repo the daemon
    /// declines to serve has no writer other than this process.
    Unstarted(String),
    /// The daemon may be executing the mutation right now, or may have executed
    /// part of it and failed. Nothing observable here rules that out, so this
    /// process must not also run it — that is the second writer the routing
    /// exists to remove.
    Undetermined(String),
}

impl std::fmt::Display for MutationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unstarted(m) | Self::Undetermined(m) => f.write_str(m),
        }
    }
}

/// Like [`call_daemon_with_timeout`], but reporting whether the daemon can have
/// started the work.
///
/// Everything up to and including the last byte of the request is
/// [`MutationFailure::Unstarted`]; after that only the daemon's own refusals
/// are, and every remaining failure is [`MutationFailure::Undetermined`].
async fn call_daemon_phased(
    socket_path: &Path,
    method: &str,
    params: &Value,
    timeout: Duration,
) -> std::result::Result<Value, MutationFailure> {
    use MutationFailure::{Undetermined, Unstarted};

    ensure_daemon_running(socket_path)
        .await
        .map_err(|e| Unstarted(format!("daemon unavailable: {e}")))?;

    let mut stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| Unstarted(format!("connect {}: {e}", socket_path.display())))?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body =
        serde_json::to_vec(&request).map_err(|e| Unstarted(format!("serialize request: {e}")))?;

    // One deadline over both phases: `delivered` — not which phase timed out —
    // decides whether a fallback is safe.
    let deadline = tokio::time::Instant::now() + timeout;
    let mut delivered = false;
    let sent = tokio::time::timeout_at(deadline, send_request(&mut stream, &body, &mut delivered))
        .await
        .map_err(|_| "timeout sending request".to_string())
        .and_then(|r| r.map_err(|e| format!("io: {e}")));
    if let Err(e) = sent {
        return Err(if delivered {
            Undetermined(e)
        } else {
            Unstarted(e)
        });
    }

    let response = tokio::time::timeout_at(deadline, read_response(&mut stream))
        .await
        .map_err(|_| Undetermined(format!("no answer within {timeout:?}")))?
        .map_err(|e| Undetermined(format!("io: {e}")))?;

    let envelope: Value = serde_json::from_slice(&response)
        .map_err(|e| Undetermined(format!("parse response: {e}")))?;

    if let Some(err) = envelope.get("error") {
        // An unreadable code is treated as post-dispatch. Guessing the other way
        // would hand the write back to a client that cannot show the daemon
        // declined it.
        let code = err
            .get("code")
            .and_then(Value::as_i64)
            .unwrap_or(i64::from(DISPATCHED_ERROR_CODE));
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("(no message)");
        let text = format!("daemon error {code}: {msg}");
        // A refusal is the daemon declining before it dispatched anything — most
        // often "this repo is not in my whitelist", which means it is not this
        // store's writer at all and the CLI has to be. Only a failure from
        // inside a dispatched method leaves the outcome open.
        return Err(if code == i64::from(DISPATCHED_ERROR_CODE) {
            Undetermined(text)
        } else {
            Unstarted(text)
        });
    }

    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| Undetermined("response missing 'result'".to_string()))
}

/// Send a store mutation to the daemon over the hook socket.
///
/// The daemon is the sole writer (story 018-56b2): the plain CLI used to open
/// its own write-capable connection for every `mdkb memory add`, racing the
/// long-lived daemon on one file. This is the same client pattern `mdkb hook`
/// uses, deliberately — one socket, one protocol, one thing to debug.
///
/// Errors are never swallowed, which is the difference from `run_hook`: a hook
/// that silently does nothing is acceptable (the host must exit 0), a mutation
/// that silently does nothing is data loss. The caller falls back in-process on
/// [`MutationFailure::Unstarted`] and only on that — see the type.
///
/// The deadline is [`MUTATION_TIMEOUT`], not [`hook_timeout`]: a hook budget is
/// sized so an editor keystroke never stalls, and applying it to a write means
/// abandoning `mdkb update` half a minute into a job that legitimately runs for
/// many.
pub async fn call_store_mutation(
    method: &str,
    params: Value,
    root: &Path,
) -> std::result::Result<Value, MutationFailure> {
    let mut params = params;
    params["root"] = json!(root.display().to_string());

    let socket_path = hook_socket_path();
    call_daemon_phased(&socket_path, method, &params, MUTATION_TIMEOUT).await
}

/// Send the one typed internal CLI mutation request and decode its typed result.
pub async fn call_cli_mutation(
    mutation: &crate::core::cli_mutation::CliMutation,
    root: &Path,
) -> std::result::Result<crate::core::cli_mutation::CliMutationResult, MutationFailure> {
    let params = serde_json::to_value(mutation)
        .map_err(|e| MutationFailure::Unstarted(format!("serialize cli mutation: {e}")))?;
    let value = call_store_mutation("cli.mutate", params, root).await?;
    serde_json::from_value(value)
        .map_err(|e| MutationFailure::Undetermined(format!("decode cli mutation result: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;
    use tokio::net::UnixListener;

    /// Spawn a fake hook server that echoes back a fixed JSON-RPC result
    /// for whatever request it receives. Uses the same wire format the
    /// real daemon does.
    fn spawn_fake_hook_server(
        socket_path: PathBuf,
        response: Value,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&socket_path).expect("bind hook sock");
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut hdr = [0u8; 4];
                    if stream.read_exact(&mut hdr).await.is_err() {
                        return;
                    }
                    let len = u32::from_le_bytes(hdr) as usize;
                    let mut buf = vec![0u8; len];
                    if stream.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    let req: Value = serde_json::from_slice(&buf).unwrap();
                    let mut envelope = response.clone();
                    envelope["id"] = req.get("id").cloned().unwrap_or(Value::Null);
                    let body = serde_json::to_vec(&envelope).unwrap();
                    let len = u32::try_from(body.len()).unwrap().to_le_bytes();
                    let _ = stream.write_all(&len).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        })
    }

    #[tokio::test]
    async fn call_daemon_returns_result_envelope() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let _srv = spawn_fake_hook_server(
            sock.clone(),
            json!({"jsonrpc":"2.0","result":{"text":"ok","tokens":1}}),
        );

        let result = call_daemon(&sock, "status", &json!({"root":"/tmp"}))
            .await
            .unwrap();
        assert_eq!(result["text"], "ok");
    }

    #[tokio::test]
    async fn call_daemon_propagates_error_envelope() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let _srv = spawn_fake_hook_server(
            sock.clone(),
            json!({"jsonrpc":"2.0","error":{"code":-32602,"message":"repo not in whitelist"}}),
        );

        let err = call_daemon(&sock, "status", &json!({"root":"/tmp"}))
            .await
            .unwrap_err();
        assert!(err.contains("-32602"), "err: {err}");
        assert!(err.contains("whitelist"), "err: {err}");
    }

    #[tokio::test]
    async fn resolve_root_prefers_explicit() {
        let tmp = TempDir::new().unwrap();
        let explicit = tmp.path().to_path_buf();
        let got = resolve_root(Some(explicit.clone()));
        assert_eq!(got, explicit.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn resolve_root_falls_back_to_cwd() {
        // resolve_root uses current_dir; just assert it returns an absolute
        // path (canonicalize guarantees that for existing dirs).
        let got = resolve_root(None);
        assert!(got.is_absolute() || got.components().count() > 0);
    }

    #[test]
    fn resolve_hook_root_prefers_event_cwd() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("event-repo");
        std::fs::create_dir_all(repo.join(".mdkb")).unwrap();

        let got = resolve_hook_root(&json!({"cwd": repo.to_string_lossy()}), None);

        assert_eq!(got, repo.canonicalize().unwrap());
    }

    #[test]
    fn resolve_hook_root_explicit_beats_event_cwd() {
        let tmp = TempDir::new().unwrap();
        let explicit = tmp.path().join("explicit-repo");
        let event_repo = tmp.path().join("event-repo");
        std::fs::create_dir_all(&explicit).unwrap();
        std::fs::create_dir_all(&event_repo).unwrap();

        let got = resolve_hook_root(
            &json!({"cwd": event_repo.to_string_lossy()}),
            Some(explicit.clone()),
        );

        assert_eq!(got, explicit.canonicalize().unwrap());
    }

    #[test]
    fn memory_write_tags_are_sent_as_a_trimmed_sequence() {
        assert_eq!(
            parse_comma_separated_tags("mdkb, mcp, ,fixed"),
            vec!["mdkb", "mcp", "fixed"]
        );
    }

    #[tokio::test]
    async fn run_in_process_executes_status_without_daemon() {
        // Build a minimal repo and invoke in-process dispatch.
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mdkb")).unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // run_in_process prints to stdout; we can't capture easily here,
        // so just verify it returns Ok. Dispatch failures print to stderr
        // and still return Ok — that is the contract.
        let params = json!({"root": root.display().to_string()});
        let result = run_in_process("status", params, &root).await;
        assert!(result.is_ok());
    }

    /// Confirm the daemon's reply to a whitelist-rejection error is
    /// surfaced as an Err that the caller prints and swallows (exit 0).
    #[tokio::test]
    async fn whitelist_rejection_surfaces_as_error_string() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let _srv = spawn_fake_hook_server(
            sock.clone(),
            json!({"jsonrpc":"2.0","error":{"code":-32602,"message":"repo registry: path not in whitelist: /foo"}}),
        );

        let err = call_daemon(&sock, "status", &json!({"root":"/foo"}))
            .await
            .unwrap_err();
        assert!(err.contains("repo registry"));
    }

    /// A fake daemon that swallows the whole request and never answers it.
    ///
    /// This is the shape of the failure the delivery split exists for: the
    /// request is on the daemon's side of the socket, so nothing this process
    /// observes afterwards can prove the mutation did not run. `hold_open`
    /// picks which way the silence arrives — dropping the connection (the
    /// daemon died mid-write) or keeping it and never replying (the daemon is
    /// still working, or wedged).
    fn spawn_deaf_hook_server(
        socket_path: PathBuf,
        hold_open: bool,
    ) -> tokio::task::JoinHandle<()> {
        let listener = UnixListener::bind(&socket_path).expect("bind hook sock");
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut hdr = [0u8; 4];
                    if stream.read_exact(&mut hdr).await.is_err() {
                        return;
                    }
                    let len = u32::from_le_bytes(hdr) as usize;
                    let mut buf = vec![0u8; len];
                    if stream.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    // The request is now fully on this side. Say nothing.
                    if hold_open {
                        tokio::time::sleep(Duration::from_mins(5)).await;
                    }
                });
            }
        })
    }

    /// A daemon that took the request and then died must NOT license a
    /// fallback.
    ///
    /// The connection dropping is not evidence the mutation did not run — the
    /// daemon may have committed it and crashed on the way back. Running it
    /// again here is the second writer this routing exists to remove.
    #[tokio::test]
    async fn a_daemon_that_took_the_request_and_died_is_not_a_fallback() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let _srv = spawn_deaf_hook_server(sock.clone(), false);

        let err = call_daemon_phased(&sock, "update", &json!({"root":"/tmp"}), CALL_TIMEOUT)
            .await
            .expect_err("a daemon that never answers must not look like success");
        assert!(
            matches!(err, MutationFailure::Undetermined(_)),
            "the request was fully written, so the outcome is unknowable: {err:?}"
        );
    }

    /// Nor must a daemon that took the request and is still holding it.
    ///
    /// This is the original bug: `mdkb update` outran the 30s budget, the
    /// client called it a failure and reindexed the whole store itself while
    /// the daemon was mid-write.
    #[tokio::test]
    async fn a_daemon_that_took_the_request_and_went_quiet_is_not_a_fallback() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let _srv = spawn_deaf_hook_server(sock.clone(), true);

        let short = Duration::from_millis(150);
        let err = call_daemon_phased(&sock, "update", &json!({"root":"/tmp"}), short)
            .await
            .expect_err("a daemon that holds the request must not look like success");
        assert!(
            matches!(err, MutationFailure::Undetermined(_)),
            "a read timeout says nothing about whether the daemon is writing: {err:?}"
        );
    }

    /// A method that ran and failed inside the daemon leaves the outcome open.
    ///
    /// It may have written half of what it was asked to before it gave up.
    /// Repeating it here is a second writer on a store already in an unknown
    /// state, and it would bury the daemon's error under a local success.
    #[tokio::test]
    async fn a_failure_inside_the_daemon_is_not_a_fallback() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let _srv = spawn_fake_hook_server(
            sock.clone(),
            json!({"jsonrpc":"2.0","error":{
                "code": DISPATCHED_ERROR_CODE,
                "message": "update: disk full halfway through",
            }}),
        );

        let err = call_daemon_phased(&sock, "update", &json!({"root":"/tmp"}), CALL_TIMEOUT)
            .await
            .expect_err("an error envelope is a failure");
        match err {
            MutationFailure::Undetermined(m) => assert!(m.contains("disk full"), "message: {m}"),
            MutationFailure::Unstarted(m) => {
                panic!("the method ran, so it may have written: {m}")
            }
        }
    }

    /// A refusal at admission is the one answer that DOES license a fallback.
    ///
    /// "This repo is not in my whitelist" is the daemon declining to be its
    /// writer at all, decided before dispatch. Treating that as an open outcome
    /// broke every `mdkb update` outside the whitelist — the routing turning a
    /// supported configuration into a hard failure, which is the 021-0636
    /// lesson arriving by a different road.
    #[tokio::test]
    async fn a_refusal_before_dispatch_is_a_fallback() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let _srv = spawn_fake_hook_server(
            sock.clone(),
            json!({"jsonrpc":"2.0","error":{
                "code": -32602,
                "message": "repo registry: path not in whitelist: /foo",
            }}),
        );

        let err = call_daemon_phased(&sock, "update", &json!({"root":"/foo"}), CALL_TIMEOUT)
            .await
            .expect_err("an error envelope is a failure");
        match err {
            MutationFailure::Unstarted(m) => assert!(m.contains("whitelist"), "message: {m}"),
            MutationFailure::Undetermined(m) => {
                panic!("the daemon refused before dispatch, so it wrote nothing: {m}")
            }
        }
    }

    /// An error envelope the client cannot read is treated as post-dispatch.
    ///
    /// The permissive reading would hand the write to a client that cannot show
    /// the daemon declined it — guessing in the direction of two writers.
    #[tokio::test]
    async fn an_unreadable_error_code_is_not_a_fallback() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let _srv = spawn_fake_hook_server(
            sock.clone(),
            json!({"jsonrpc":"2.0","error":{"message":"something went wrong"}}),
        );

        let err = call_daemon_phased(&sock, "update", &json!({"root":"/tmp"}), CALL_TIMEOUT)
            .await
            .expect_err("an error envelope is a failure");
        assert!(
            matches!(err, MutationFailure::Undetermined(_)),
            "an envelope with no code proves nothing: {err:?}"
        );
    }

    /// A write is not held to a hook's budget.
    ///
    /// Every hook deadline is sized so an editor keystroke never stalls, and
    /// `hook_timeout` falls through to 30s for anything not in its table —
    /// which is what mutations used to get. A full `mdkb update` walks the
    /// tree, embeds and reindexes the code graph, and routinely runs longer
    /// than that, so the budget itself manufactured the fallback.
    #[test]
    fn a_mutation_is_not_held_to_a_hook_budget() {
        assert_eq!(
            hook_timeout("update"),
            CALL_TIMEOUT,
            "if `update` ever gains a hook-table entry, this test is checking the wrong thing"
        );
        assert!(
            MUTATION_TIMEOUT > CALL_TIMEOUT * 10,
            "a mutation budget of {MUTATION_TIMEOUT:?} is close enough to the {CALL_TIMEOUT:?} \
             hook fallthrough to abandon a long update again"
        );
        for event in [
            "hook.session_start",
            "hook.user_prompt_submit",
            "hook.post_tool_use",
            "hook.pre_tool_use",
            "hook.stop",
        ] {
            assert!(
                MUTATION_TIMEOUT > hook_timeout(event),
                "{event} must never bound a write"
            );
        }
    }

    /// Daemon and client must agree on the hook socket path. The daemon
    /// binds `~/.mdkb/daemon-hook.sock` (see `daemon::ipc_server::serve`
    /// with `base_dir = DaemonConfig::daemon_home()`).
    #[test]
    fn hook_socket_path_under_daemon_home() {
        let path = hook_socket_path();
        assert!(
            path.ends_with("daemon-hook.sock"),
            "path: {}",
            path.display()
        );
        assert!(path.to_string_lossy().contains(".mdkb"));
    }

    // ── emit_hook_response ────────────────────────────────────────────────────

    #[test]
    fn emit_hook_response_silent_on_empty_object() {
        // Can't capture stdout in unit tests, but we can verify it doesn't panic
        // and returns quickly. The real output contract is verified in e2e tests.
        emit_hook_response(&json!({}));
    }

    #[test]
    fn emit_hook_response_silent_on_null() {
        emit_hook_response(&Value::Null);
    }

    #[test]
    fn hook_timeout_returns_per_event_durations() {
        assert_eq!(
            hook_timeout("hook.session_start"),
            HOOK_TIMEOUT_SESSION_START
        );
        assert_eq!(
            hook_timeout("hook.user_prompt_submit"),
            HOOK_TIMEOUT_USER_PROMPT_SUBMIT
        );
        assert_eq!(
            hook_timeout("hook.post_tool_use"),
            HOOK_TIMEOUT_POST_TOOL_USE
        );
        assert_eq!(hook_timeout("hook.pre_tool_use"), HOOK_TIMEOUT_PRE_TOOL_USE);
        assert_eq!(hook_timeout("status"), CALL_TIMEOUT);
    }

    #[tokio::test]
    async fn call_hook_event_via_fake_daemon() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("hook.sock");
        let envelope = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "additionalContext": "Use mdkb search instead",
            }
        });
        let _srv =
            spawn_fake_hook_server(sock.clone(), json!({"jsonrpc":"2.0","result": envelope}));

        // call_daemon_with_timeout returns the result envelope
        let result = call_daemon_with_timeout(
            &sock,
            "hook.pre_tool_use",
            &json!({"root": "/tmp", "tool_name": "Grep", "tool_input": {"pattern": "foo"}}),
            HOOK_TIMEOUT_PRE_TOOL_USE,
        )
        .await
        .unwrap();

        assert!(
            result.get("hookSpecificOutput").is_some(),
            "result: {result}"
        );
    }
}
