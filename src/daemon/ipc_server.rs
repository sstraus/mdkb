//! Daemon IPC server: two unix domain sockets.
//!
//! - `daemon.sock` (MCP): rmcp traffic. Story 4 wires the actual protocol;
//!   here we only accept connections so the listener path is testable.
//! - `daemon-hook.sock` (hooks): length-prefixed JSON-RPC 2.0. Each message
//!   is `u32_le(body_len) ++ body_bytes`. Dispatcher is a stub until Story 3.
//!
//! Both sockets are chmod'd to `0600` via explicit `set_permissions` after
//! bind — umask is not a reliable guarantee across platforms.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

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
pub async fn serve(base_dir: &Path, shutdown: CancellationToken) -> Result<(), IpcError> {
    let mcp_path = base_dir.join(MCP_SOCKET_NAME);
    let hook_path = base_dir.join(HOOK_SOCKET_NAME);

    // Remove any stale sockets from a prior crashed daemon. Errors other
    // than NotFound are fatal — the new daemon must own the path.
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
    let mcp_task = tokio::spawn(accept_loop(mcp_listener, mcp_shutdown, handle_mcp_conn));

    let hook_shutdown = shutdown.clone();
    let hook_task = tokio::spawn(accept_loop(hook_listener, hook_shutdown, handle_hook_conn));

    shutdown.cancelled().await;
    tracing::info!("ipc: shutdown signalled, unlinking sockets");

    // Wait briefly for accept loops to notice the cancel — ignore join errors.
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

async fn accept_loop<F, Fut>(listener: UnixListener, shutdown: CancellationToken, handler: F)
where
    F: Fn(UnixStream) -> Fut + Send + Sync + 'static + Copy,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _addr)) => {
                    tokio::spawn(handler(stream));
                }
                Err(e) => {
                    tracing::warn!("ipc accept failed: {e}");
                }
            }
        }
    }
}

// ── MCP socket (rmcp stream) ────────────────────────────────────────────────

/// Minimal MCP connection handler. Story 4 replaces this with the real
/// rmcp server bound to the stream. Today it only holds the stream open
/// so clients confirm the listener is accepting.
async fn handle_mcp_conn(mut stream: UnixStream) {
    // Drain anything the client sends; noop response. Story 4 replaces this.
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                // discard
            }
        }
    }
}

// ── Hook socket (length-prefixed JSON-RPC) ───────────────────────────────────

const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

async fn handle_hook_conn(mut stream: UnixStream) {
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

        let response = dispatch_hook_message(&body);
        let resp_bytes = response.as_bytes();
        let resp_len = (resp_bytes.len() as u32).to_le_bytes();
        if stream.write_all(&resp_len).await.is_err() {
            return;
        }
        if stream.write_all(resp_bytes).await.is_err() {
            return;
        }
    }
}

/// Parse a JSON-RPC request and route it. Story 3 replaces the match arms
/// with `dispatch_call` on real tools; for now only `ping` is supported,
/// which lets infrastructure tests verify framing without booting the
/// entire MCP stack.
fn dispatch_hook_message(body: &[u8]) -> String {
    let req: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return rpc_error(Value::Null, -32700, &format!("parse error: {e}")),
    };

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    match method {
        "ping" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"pong": true}
        })
        .to_string(),
        other => rpc_error(id, -32601, &format!("method not found: {other}")),
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

    #[test]
    fn ping_dispatch_returns_pong() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"ping","params":{}}"#;
        let resp = dispatch_hook_message(body);
        assert!(resp.contains("\"id\":7"));
        assert!(resp.contains("pong"));
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"no_such"}"#;
        let resp = dispatch_hook_message(body);
        assert!(resp.contains("-32601"));
    }

    #[test]
    fn invalid_json_returns_parse_error() {
        let resp = dispatch_hook_message(b"not json");
        assert!(resp.contains("-32700"));
    }
}
