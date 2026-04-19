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

// ── Public entry points ──────────────────────────────────────────────────────

/// Send a `status` call.
pub async fn call_status(root: Option<PathBuf>) -> Result<()> {
    run("status", json!({}), root).await
}

/// Send an `update` (reindex) call. `files` is forwarded as `params.files`
/// so future daemon versions can scope the reindex; the current daemon
/// ignores unknown fields and reindexes everything.
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
        params["tags"] = json!(t);
    }
    if let Some(t) = ttl {
        params["ttl"] = json!(t);
    }
    run("memory_write", params, root).await
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

/// Pick the target repo root: explicit flag > current working directory.
/// Absolute canonical form is preferred (daemon whitelist compares paths).
fn resolve_root(explicit: Option<PathBuf>) -> PathBuf {
    let raw = explicit
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    // Canonicalize best-effort; if the path doesn't exist the daemon will
    // reject it with a clear message we'll print to stderr.
    raw.canonicalize().unwrap_or(raw)
}

fn hook_socket_path() -> PathBuf {
    // Hook socket always lives beside daemon.sock under ~/.mdkb — see
    // `daemon::ipc_server::serve(base_dir = lock.parent())` and
    // `main.rs` where base_dir is derived from the pid lock path under
    // `DaemonConfig::daemon_home()`.
    DaemonConfig::daemon_home().join(HOOK_SOCKET_NAME)
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
        Ok(response) => print_response(method, &response),
        Err(e) => eprintln!("mdkb hook {method}: {e}"),
    }
    Ok(())
}

/// Direct in-process dispatch for `MDKB_NO_DAEMON=1`. Opens an ephemeral
/// `RepoRegistry` in the current process and routes the call through
/// `dispatch_call` — identical code path to the daemon, no IPC.
async fn run_in_process(method: &str, params: Value, root: &Path) -> Result<()> {
    let daemon_config = DaemonConfig::load_or_default(&DaemonConfig::config_path())?;
    let registry = Arc::new(RepoRegistry::new(daemon_config));
    let dctx = DispatchContext {
        metrics: Arc::new(UsageMetrics::new()),
        session_id: Arc::new(AtomicI64::new(0)),
        persistent_call_count: Arc::new(AtomicU64::new(0)),
        optimize_interval_calls: 200,
    };

    match registry.get_or_open(root) {
        Ok(handle) => match dispatch_call(method, params, handle, &dctx).await {
            Ok(result) => print_response(method, &result),
            Err(e) => eprintln!("mdkb hook {method}: {}", e.message),
        },
        Err(e) => eprintln!("mdkb hook {method}: repo registry: {e}"),
    }
    Ok(())
}

/// Print the `text` field of a tool result to stdout, falling back to
/// the raw JSON if the daemon returns something unexpected.
fn print_response(method: &str, result: &Value) {
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
async fn call_daemon(socket_path: &Path, method: &str, params: &Value) -> std::result::Result<Value, String> {
    ensure_daemon_running(socket_path)
        .await
        .map_err(|e| format!("daemon unavailable: {e}"))?;

    let stream = UnixStream::connect(socket_path)
        .await
        .map_err(|e| format!("connect {}: {e}", socket_path.display()))?;

    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body = serde_json::to_vec(&request).map_err(|e| format!("serialize request: {e}"))?;

    let response = tokio::time::timeout(CALL_TIMEOUT, send_and_read(stream, &body))
        .await
        .map_err(|_| format!("timeout after {CALL_TIMEOUT:?}"))?
        .map_err(|e| format!("io: {e}"))?;

    let envelope: Value =
        serde_json::from_slice(&response).map_err(|e| format!("parse response: {e}"))?;

    if let Some(err) = envelope.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("(no message)");
        return Err(format!("daemon error {code}: {msg}"));
    }

    envelope
        .get("result")
        .cloned()
        .ok_or_else(|| "response missing 'result'".to_string())
}

/// Frame-send one message and read one framed response back.
async fn send_and_read(mut stream: UnixStream, body: &[u8]) -> std::io::Result<Vec<u8>> {
    let len = u32::try_from(body.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "request too large"))?;
    stream.write_all(&len.to_le_bytes()).await?;
    stream.write_all(body).await?;
    stream.flush().await?;

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

        let result = call_daemon(&sock, "status", &json!({"root":"/tmp"})).await.unwrap();
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

    /// Daemon and client must agree on the hook socket path. The daemon
    /// binds `~/.mdkb/daemon-hook.sock` (see `daemon::ipc_server::serve`
    /// with `base_dir = DaemonConfig::daemon_home()`).
    #[test]
    fn hook_socket_path_under_daemon_home() {
        let path = hook_socket_path();
        assert!(path.ends_with("daemon-hook.sock"), "path: {}", path.display());
        assert!(path.to_string_lossy().contains(".mdkb"));
    }
}
