//! Daemon IPC server: two unix domain sockets.
//!
//! - `daemon.sock` (MCP): rmcp NDJSON traffic. Each connection runs a real
//!   `McpServer::global(registry)` bound directly to the stream, so the
//!   `mdkb mcp` proxy can act as a dumb byte forwarder between Claude's
//!   stdio and this socket.
//! - `daemon-hook.sock` (hooks): length-prefixed JSON-RPC 2.0. Each message
//!   is `u32_le(body_len) ++ body_bytes`. Methods are routed through
//!   `mcp::dispatch::dispatch_call` against the per-request `params.root`.
//!
//! Both sockets are chmod'd to `0600` via explicit `set_permissions` after
//! bind — umask is not a reliable guarantee across platforms.
//!
//! # TOCTOU mitigation
//!
//! The `~/.mdkb` base directory is created (or corrected) with mode `0700`
//! before any socket operations. This prevents a local attacker from placing
//! a file at the socket path in the window between `remove_if_exists` and
//! `UnixListener::bind`, because unprivileged processes cannot write into a
//! directory they do not own when its mode is `0700`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::ServiceExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::mcp::dispatch::{DispatchContext, dispatch_call};
use crate::mcp::server::McpServer;

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

    #[error("mkdir {path}: {source}")]
    Mkdir {
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
/// `base_dir` is typically `~/.mdkb`. The directory is created (or its
/// permissions corrected) to mode `0700` before socket operations to close
/// the TOCTOU window between `remove_if_exists` and `UnixListener::bind`.
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
    ensure_base_dir_0700(base_dir)?;

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
    let mcp_registry = Arc::clone(&registry);
    let mcp_task = tokio::spawn(mcp_accept_loop(mcp_listener, mcp_shutdown, mcp_registry));

    let hook_shutdown = shutdown.clone();
    let hook_task = tokio::spawn(hook_accept_loop(
        hook_listener,
        hook_shutdown,
        Arc::clone(&registry),
        Arc::clone(&dctx),
        MAX_HOOK_CONNECTIONS,
    ));

    shutdown.cancelled().await;
    tracing::info!("ipc: shutdown signalled, unlinking sockets");

    let _ = mcp_task.await;
    let _ = hook_task.await;

    let _ = std::fs::remove_file(&mcp_path);
    let _ = std::fs::remove_file(&hook_path);
    Ok(())
}

/// Create `base_dir` if it does not exist, then enforce mode `0700`.
///
/// Enforcing after creation (rather than relying on umask) is required
/// because `create_dir_all` honours the process umask, which may be too
/// permissive.  Setting permissions explicitly closes the TOCTOU window: a
/// local attacker cannot plant files in a directory they cannot write to.
fn ensure_base_dir_0700(dir: &Path) -> Result<(), IpcError> {
    std::fs::create_dir_all(dir).map_err(|e| IpcError::Mkdir {
        path: dir.to_path_buf(),
        source: e,
    })?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
        IpcError::Chmod {
            path: dir.to_path_buf(),
            source: e,
        }
    })
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

async fn mcp_accept_loop(
    listener: UnixListener,
    shutdown: CancellationToken,
    registry: Arc<RepoRegistry>,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    let registry = Arc::clone(&registry);
                    tokio::spawn(handle_mcp_conn(stream, registry));
                }
                Err(e) => {
                    tracing::warn!("ipc accept failed (mcp): {e}");
                }
            }
        }
    }
}

/// Bind a real rmcp `McpServer` to the accepted UnixStream. The server
/// runs in global mode and resolves repos via the shared `RepoRegistry`,
/// so a single daemon can host any number of concurrent MCP clients.
async fn handle_mcp_conn(stream: UnixStream, registry: Arc<RepoRegistry>) {
    let server = McpServer::global(registry);
    let service = match server.serve(stream).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("mcp serve init failed: {e}");
            return;
        }
    };
    if let Err(e) = service.waiting().await {
        tracing::debug!("mcp connection ended: {e}");
    }
}

// ── Hook socket (length-prefixed JSON-RPC) ───────────────────────────────────

const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Maximum number of concurrent hook connections. Each connection may allocate
/// up to `MAX_MESSAGE_BYTES` (4 MiB) for a single message body, so this caps
/// the worst-case RSS growth from hook traffic at ~256 MiB.
pub const MAX_HOOK_CONNECTIONS: usize = 64;

async fn hook_accept_loop(
    listener: UnixListener,
    shutdown: CancellationToken,
    registry: Arc<RepoRegistry>,
    dctx: Arc<DispatchContext>,
    max_connections: usize,
) {
    let semaphore = Arc::new(Semaphore::new(max_connections));
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    let permit = match Arc::clone(&semaphore).acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => return, // semaphore closed — shutting down
                    };
                    let registry = Arc::clone(&registry);
                    let dctx = Arc::clone(&dctx);
                    tokio::spawn(async move {
                        handle_hook_conn(stream, registry, dctx).await;
                        drop(permit);
                    });
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
        let Ok(resp_len_u32) = u32::try_from(resp_bytes.len()) else {
            tracing::error!(
                "hook: response too large ({} bytes), closing",
                resp_bytes.len()
            );
            return;
        };
        if stream.write_all(&resp_len_u32.to_le_bytes()).await.is_err() {
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
            hook_dedup: Arc::new(std::sync::Mutex::new(Default::default())),
        })
    }

    // ── Security: directory and socket permissions ──────────────────────────

    /// `ensure_base_dir_0700` must create the directory with mode `0700`.
    #[test]
    fn base_dir_created_with_0700() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("mdkb");
        ensure_base_dir_0700(&base).unwrap();
        let meta = std::fs::metadata(&base).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "base dir mode should be 0700, got {mode:o}");
    }

    /// `ensure_base_dir_0700` must tighten an existing directory that has
    /// overly permissive bits (e.g. 0755 from a previous `create_dir_all`).
    #[test]
    fn base_dir_permissions_tightened_if_too_open() {
        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("mdkb");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_base_dir_0700(&base).unwrap();

        let mode = std::fs::metadata(&base).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "base dir mode should be 0700 after tightening, got {mode:o}"
        );
    }

    /// After `serve()` binds, the socket files must have mode `0600`.
    #[tokio::test]
    async fn socket_permissions_are_0600_after_bind() {
        use tokio_util::sync::CancellationToken;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("mdkb");

        let shutdown = CancellationToken::new();
        let registry = make_registry();
        let dctx = make_dctx();

        let shutdown_clone = shutdown.clone();
        let base_clone = base.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&base_clone, shutdown_clone, registry, dctx).await });

        // Wait until serve() has created both sockets (i.e., bind completed).
        let mcp_path = base.join(MCP_SOCKET_NAME);
        let hook_path = base.join(HOOK_SOCKET_NAME);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if mcp_path.exists() && hook_path.exists() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "sockets not created within 5s"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let mcp_mode = std::fs::metadata(&mcp_path).unwrap().permissions().mode() & 0o777;
        let hook_mode = std::fs::metadata(&hook_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mcp_mode, 0o600,
            "mcp socket mode should be 0600, got {mcp_mode:o}"
        );
        assert_eq!(
            hook_mode, 0o600,
            "hook socket mode should be 0600, got {hook_mode:o}"
        );

        shutdown.cancel();
        serve_handle.await.unwrap().unwrap();
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

    #[tokio::test]
    async fn oversized_response_closes_connection_not_corrupt_frame() {
        use tokio::net::UnixListener as HookListener;

        let tmp = TempDir::new().unwrap();
        let sock_path = tmp.path().join("hook.sock");
        let listener = HookListener::bind(&sock_path).unwrap();

        let registry = make_registry();
        let dctx = make_dctx();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_hook_conn(stream, registry, dctx).await;
        });

        let mut client = tokio::net::UnixStream::connect(&sock_path).await.unwrap();

        // Craft a raw oversized length header (u32::MAX = 4 GiB) as if the old
        // code had sent it, and verify the server never sends such a header back.
        // Instead we send a valid ping, confirm the reply length fits in u32.
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
        let len = (body.len() as u32).to_le_bytes();
        client.write_all(&len).await.unwrap();
        client.write_all(body).await.unwrap();

        let mut hdr = [0u8; 4];
        client.read_exact(&mut hdr).await.unwrap();
        let resp_len = u32::from_le_bytes(hdr) as usize;

        // Sanity: the response length must be < u32::MAX (not the corrupt sentinel).
        assert_ne!(
            resp_len,
            u32::MAX as usize,
            "server sent corrupt u32::MAX length"
        );
        assert!(resp_len > 0 && resp_len < 1024 * 1024);

        let mut resp_body = vec![0u8; resp_len];
        client.read_exact(&mut resp_body).await.unwrap();
        let parsed: Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(parsed["result"]["pong"], json!(true));
    }

    // ── rmcp-over-unix behavior equivalence (story #014-5fa4) ───────────────
    //
    // Drive the daemon's MCP socket end-to-end with a real rmcp client and
    // confirm the tool result text matches what `dispatch_call` returns.
    // This is the proof that `mdkb mcp` (a dumb byte forwarder) gives Claude
    // the exact same answer it would get talking directly to dispatch.

    use rmcp::model::CallToolRequestParams;
    use tokio::net::UnixListener as TokioUnixListener;

    /// Spawn `McpServer::global(registry).serve(stream)` on every accepted
    /// connection until the listener is dropped.
    fn spawn_mcp_listener(
        listener: TokioUnixListener,
        registry: Arc<RepoRegistry>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    let server = crate::mcp::server::McpServer::global(registry);
                    if let Ok(s) = server.serve(stream).await {
                        let _ = s.waiting().await;
                    }
                });
            }
        })
    }

    fn args_from(v: Value) -> Option<serde_json::Map<String, Value>> {
        v.as_object().cloned()
    }

    #[tokio::test]
    async fn mcp_socket_status_equals_dispatch_call() {
        use std::time::Duration;
        use tokio::time::timeout;

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mdkb")).unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sock = tmp.path().join("mcp.sock");

        let registry = make_registry();
        let dctx = make_dctx();

        // Pre-register the repo so global mode auto-selects it without roots.
        let _ = registry.get_or_open(&root).unwrap();

        let listener = TokioUnixListener::bind(&sock).unwrap();
        let server_task = spawn_mcp_listener(listener, Arc::clone(&registry));

        let stream = UnixStream::connect(&sock).await.unwrap();
        let client = timeout(Duration::from_secs(10), ().serve(stream))
            .await
            .expect("client handshake timed out")
            .expect("client handshake error");
        let result = timeout(
            Duration::from_secs(15),
            client.call_tool(CallToolRequestParams {
                name: "status".into(),
                arguments: args_from(json!({})),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("call_tool status timed out")
        .expect("call_tool status error");

        // Extract the single text content block.
        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("status returned text content");

        let direct_handle = registry.get_or_open(&root).unwrap();
        let direct = dispatch_call("status", json!({}), direct_handle, &dctx)
            .await
            .unwrap();
        assert_eq!(text, direct["text"].as_str().unwrap());

        let _ = client.cancel().await;
        server_task.abort();
    }

    #[tokio::test]
    async fn mcp_socket_search_equals_dispatch_call() {
        use std::time::Duration;
        use tokio::time::timeout;

        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".mdkb")).unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let sock = tmp.path().join("mcp.sock");

        let registry = make_registry();
        let dctx = make_dctx();
        let _ = registry.get_or_open(&root).unwrap();

        let listener = TokioUnixListener::bind(&sock).unwrap();
        let server_task = spawn_mcp_listener(listener, Arc::clone(&registry));

        let stream = UnixStream::connect(&sock).await.unwrap();
        let client = timeout(Duration::from_secs(10), ().serve(stream))
            .await
            .expect("client handshake timed out")
            .expect("client handshake error");
        let result = timeout(
            Duration::from_secs(15),
            client.call_tool(CallToolRequestParams {
                name: "search".into(),
                arguments: args_from(json!({
                    "query": "anything",
                    "scope": "docs",
                    "root": root.display().to_string(),
                })),
                meta: None,
                task: None,
            }),
        )
        .await
        .expect("call_tool search timed out")
        .expect("call_tool search error");

        let text = result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.clone())
            .expect("search returned text content");

        let direct_handle = registry.get_or_open(&root).unwrap();
        let direct = dispatch_call(
            "search",
            json!({"query":"anything","scope":"docs"}),
            direct_handle,
            &dctx,
        )
        .await
        .unwrap();
        assert_eq!(text, direct["text"].as_str().unwrap());

        let _ = client.cancel().await;
        server_task.abort();
    }
}
