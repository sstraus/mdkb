//! Daemon IPC server: two unix domain sockets.
//!
//! - `daemon.sock` (MCP): rmcp traffic. Story 4 wires the actual protocol;
//!   here we only accept connections so the listener path is testable.
//! - `daemon-hook.sock` (hooks): length-prefixed JSON-RPC 2.0. Each message
//!   is `u32_le(body_len) ++ body_bytes`. Methods are routed through
//!   `mcp::dispatch::dispatch_call` against the per-request `params.root`.
//!
//! Both sockets are chmod'd to `0600` via explicit `set_permissions` after
//! bind — umask is not a reliable guarantee across platforms.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::mcp::dispatch::{DispatchContext, dispatch_call};

use super::registry::RepoRegistry;

/// Names of the two sockets under the daemon base directory (`~/.mdkb`).
pub const MCP_SOCKET_NAME: &str = "daemon.sock";
pub const HOOK_SOCKET_NAME: &str = "daemon-hook.sock";

/// Errors raised while serving IPC.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    #[error("bind {path}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("chmod {path}: {source}")]
    Chmod {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Bind the two IPC sockets under `base_dir`, serve until `shutdown` is
/// triggered, then unlink both socket files.
///
/// `base_dir` is typically `~/.mdkb`. The directory must already exist
/// (the daemon singleton lock creates it).
///
/// The hook socket routes JSON-RPC method calls through
/// `mcp::dispatch::dispatch_call`, resolving the target repo via
/// `registry.get_or_open(params.root)`.
pub async fn serve(
    base_dir: &Path,
    shutdown: CancellationToken,
    registry: Arc<RepoRegistry>,
    dctx: Arc<DispatchContext>,
) -> Result<(), IpcError> {
    let mcp_path = base_dir.join(MCP_SOCKET_NAME);
    let hook_path = base_dir.join(HOOK_SOCKET_NAME);

    remove_if_exists(&mcp_path)?;
    remove_if_exists(&hook_path)?;

    let mcp_listener = UnixListener::bind(&mcp_path).map_err(|e| IpcError::Bind {
        path: mcp_path.clone(),
        source: e,
    })?;
    chmod_0600(&mcp_path)?;

    let hook_listener = UnixListener::bind(&hook_path).map_err(|e| IpcError::Bind {
        path: hook_path.clone(),
        source: e,
    })?;
    chmod_0600(&hook_path)?;

    tracing::info!(
        "ipc: listening on {} (mcp) and {} (hook)",
        mcp_path.display(),
        hook_path.display()
    );

    let mcp_shutdown = shutdown.clone();
    let mcp_task = tokio::spawn(mcp_accept_loop(mcp_listener, mcp_shutdown));

    let hook_shutdown = shutdown.clone();
    let hook_task = tokio::spawn(hook_accept_loop(
        hook_listener,
        hook_shutdown,
        Arc::clone(&registry),
        Arc::clone(&dctx),
    ));

    shutdown.cancelled().await;
    tracing::info!("ipc: shutdown signalled, unlinking sockets");

    let _ = mcp_task.await;
    let _ = hook_task.await;

    let _ = std::fs::remove_file(&mcp_path);
    let _ = std::fs::remove_file(&hook_path);
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), IpcError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(IpcError::Io(e)),
    }
}

fn chmod_0600(path: &Path) -> Result<(), IpcError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| {
        IpcError::Chmod {
            path: path.to_path_buf(),
            source: e,
        }
    })
}

// ── MCP socket (rmcp stream) ────────────────────────────────────────────────

async fn mcp_accept_loop(listener: UnixListener, shutdown: CancellationToken) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    tokio::spawn(handle_mcp_conn(stream));
                }
                Err(e) => {
                    tracing::warn!("ipc accept failed (mcp): {e}");
                }
            }
        }
    }
}

/// Minimal MCP connection handler. Story 4 replaces this with the real
/// rmcp server bound to the stream. Today it only holds the stream open
/// so clients confirm the listener is accepting.
async fn handle_mcp_conn(mut stream: UnixStream) {
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

// ── Hook socket (length-prefixed JSON-RPC) ───────────────────────────────────

const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

async fn hook_accept_loop(
    listener: UnixListener,
    shutdown: CancellationToken,
    registry: Arc<RepoRegistry>,
    dctx: Arc<DispatchContext>,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    let registry = Arc::clone(&registry);
                    let dctx = Arc::clone(&dctx);
                    tokio::spawn(handle_hook_conn(stream, registry, dctx));
                }
                Err(e) => {
                    tracing::warn!("ipc accept failed (hook): {e}");
                }
            }
        }
    }
}

async fn handle_hook_conn(
    mut stream: UnixStream,
    registry: Arc<RepoRegistry>,
    dctx: Arc<DispatchContext>,
) {
    loop {
        let mut hdr = [0u8; 4];
        if stream.read_exact(&mut hdr).await.is_err() {
            return;
        }
        let len = u32::from_le_bytes(hdr) as usize;
        if len == 0 || len > MAX_MESSAGE_BYTES {
            tracing::warn!("hook: bogus message length {len}, closing");
            return;
        }

        let mut body = vec![0u8; len];
        if stream.read_exact(&mut body).await.is_err() {
            return;
        }

        let response = dispatch_hook_message(&body, &registry, &dctx).await;
        let resp_bytes = response.as_bytes();
        let resp_len = u32::try_from(resp_bytes.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes();
        if stream.write_all(&resp_len).await.is_err() {
            return;
        }
        if stream.write_all(resp_bytes).await.is_err() {
            return;
        }
    }
}

/// Parse a JSON-RPC request and route it through `dispatch_call`.
///
/// `params.root` is the absolute path to the target repository — required for
/// every method except `ping`. The handle is acquired via
/// `registry.get_or_open(root)`, which honours the daemon whitelist.
async fn dispatch_hook_message(
    body: &[u8],
    registry: &Arc<RepoRegistry>,
    dctx: &Arc<DispatchContext>,
) -> String {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return rpc_error(Value::Null, -32700, &format!("parse error: {e}")),
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);

    if method == "ping" {
        return json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"pong": true}
        })
        .to_string();
    }

    if method.is_empty() {
        return rpc_error(id, -32600, "missing 'method'");
    }

    let root = match params.get("root").and_then(Value::as_str) {
        Some(r) => r,
        None => return rpc_error(id, -32602, "missing 'params.root' (absolute repo path)"),
    };

    let handle = match registry.get_or_open(Path::new(root)) {
        Ok(h) => h,
        Err(e) => return rpc_error(id, -32602, &format!("repo registry: {e}")),
    };

    match dispatch_call(method, params, handle, dctx).await {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        })
        .to_string(),
        Err(err) => rpc_error(id, err.code.0, &err.message),
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::config::DaemonConfig;
    use crate::metrics::UsageMetrics;
    use std::sync::atomic::{AtomicI64, AtomicU64};
    use tempfile::TempDir;

    fn make_registry() -> Arc<RepoRegistry> {
        Arc::new(RepoRegistry::new(DaemonConfig::default()))
    }

    fn make_dctx() -> Arc<DispatchContext> {
        Arc::new(DispatchContext {
            metrics: Arc::new(UsageMetrics::new()),
            session_id: Arc::new(AtomicI64::new(0)),
            persistent_call_count: Arc::new(AtomicU64::new(0)),
            optimize_interval_calls: 200,
        })
    }

    #[tokio::test]
    async fn ping_dispatch_returns_pong() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"ping","params":{}}"#;
        let resp = dispatch_hook_message(body, &make_registry(), &make_dctx()).await;
        assert!(resp.contains("\"id\":7"));
        assert!(resp.contains("pong"));
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mdkb")).unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"no_such","params":{{"root":"{}"}}}}"#,
            root.display()
        );
        let resp = dispatch_hook_message(body.as_bytes(), &make_registry(), &make_dctx()).await;
        assert!(resp.contains("-32601"), "resp: {resp}");
    }

    #[tokio::test]
    async fn invalid_json_returns_parse_error() {
        let resp = dispatch_hook_message(b"not json", &make_registry(), &make_dctx()).await;
        assert!(resp.contains("-32700"));
    }

    #[tokio::test]
    async fn missing_root_returns_invalid_params() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"status","params":{}}"#;
        let resp = dispatch_hook_message(body, &make_registry(), &make_dctx()).await;
        assert!(resp.contains("-32602"), "resp: {resp}");
        assert!(resp.contains("params.root"), "resp: {resp}");
    }

    #[tokio::test]
    async fn missing_method_returns_invalid_request() {
        let body = br#"{"jsonrpc":"2.0","id":1}"#;
        let resp = dispatch_hook_message(body, &make_registry(), &make_dctx()).await;
        assert!(resp.contains("-32600"), "resp: {resp}");
    }

    #[tokio::test]
    async fn status_dispatch_routes_through_dispatch_call() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mdkb")).unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let registry = make_registry();
        let dctx = make_dctx();
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":42,"method":"status","params":{{"root":"{}"}}}}"#,
            root.display()
        );

        let resp = dispatch_hook_message(body.as_bytes(), &registry, &dctx).await;
        let parsed: Value = serde_json::from_str(&resp).expect("json-rpc envelope");

        assert_eq!(parsed["id"], json!(42));
        assert!(parsed.get("error").is_none(), "resp: {resp}");
        let text = parsed["result"]["text"]
            .as_str()
            .expect("result.text string");
        assert!(text.contains("## Index Status"), "text: {text}");
    }

    /// Behavior-equivalence: same tool call via `dispatch_call` (the McpServer
    /// path now also funnels through this) and via the hook JSON-RPC path
    /// returns equal `text` bodies. McpServer's tool methods are thin
    /// transport delegations to `dispatch_call`, so equality of `text` here
    /// implies equality across both transports.
    #[tokio::test]
    async fn hook_and_dispatch_call_produce_equal_text_for_status() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mdkb")).unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let registry = make_registry();
        let dctx = make_dctx();

        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"status","params":{{"root":"{}"}}}}"#,
            root.display()
        );
        let hook_resp = dispatch_hook_message(body.as_bytes(), &registry, &dctx).await;
        let hook_parsed: Value = serde_json::from_str(&hook_resp).unwrap();
        let hook_text = hook_parsed["result"]["text"].as_str().unwrap().to_string();

        let direct_handle = registry.get_or_open(&root).unwrap();
        let direct = dispatch_call("status", json!({}), direct_handle, &dctx)
            .await
            .unwrap();
        let direct_text = direct["text"].as_str().unwrap().to_string();

        assert_eq!(hook_text, direct_text);
    }

    #[tokio::test]
    async fn hook_and_dispatch_call_produce_equal_text_for_search() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mdkb")).unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let registry = make_registry();
        let dctx = make_dctx();

        let body = format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"search","params":{{"root":"{}","query":"anything","scope":"docs"}}}}"#,
            root.display()
        );
        let hook_resp = dispatch_hook_message(body.as_bytes(), &registry, &dctx).await;
        let hook_parsed: Value = serde_json::from_str(&hook_resp).unwrap();
        let hook_text = hook_parsed["result"]["text"].as_str().unwrap().to_string();

        let direct_handle = registry.get_or_open(&root).unwrap();
        let direct = dispatch_call(
            "search",
            json!({"query":"anything","scope":"docs"}),
            direct_handle,
            &dctx,
        )
        .await
        .unwrap();
        let direct_text = direct["text"].as_str().unwrap().to_string();

        assert_eq!(hook_text, direct_text);
    }
}
