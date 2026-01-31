//! `mdkb mcp` — a thin rmcp NDJSON proxy from Claude's stdio to the daemon's
//! unix socket.
//!
//! The proxy is a near-pure byte forwarder: each direction reads one
//! newline-delimited JSON-RPC message, forwards it untouched, and the rmcp
//! peers handshake/dispatch as if directly connected. The only structured
//! work the proxy does is track in-flight request ids so it can synthesize
//! `mdkb daemon unavailable` error responses if the socket dies mid-call,
//! instead of leaving Claude staring at a half-closed pipe.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::daemon::spawn::ensure_daemon_running;
use crate::error::{Error, Result};

/// Connect to the daemon's MCP socket (auto-spawning if necessary) and
/// bidirectionally forward NDJSON between Claude's stdio and the socket.
///
/// Returns when either direction closes. Any in-flight request ids that
/// were forwarded to the daemon but never answered get a synthetic
/// `-32603 mdkb daemon unavailable` response written to stdout before exit.
pub async fn run_proxy(socket_path: PathBuf) -> Result<()> {
    ensure_daemon_running(&socket_path).await?;

    let stream = UnixStream::connect(&socket_path)
        .await
        .map_err(|e| Error::other(format!("connect {}: {e}", socket_path.display())))?;

    let in_flight: Arc<Mutex<HashMap<String, Value>>> = Arc::new(Mutex::new(HashMap::new()));

    let (sock_read, mut sock_write) = stream.into_split();
    let (in_flight_up, in_flight_down) = (Arc::clone(&in_flight), Arc::clone(&in_flight));

    // stdin → daemon: peek at requests so we can synthesize a clean error
    // response if the daemon dies before answering.
    let upstream = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e),
            }
            track_request(&line, &in_flight_up).await;
            sock_write.write_all(line.as_bytes()).await?;
            sock_write.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    });

    // daemon → stdout: clear in-flight ids when their response comes through.
    let downstream = tokio::spawn(async move {
        let mut reader = BufReader::new(sock_read);
        let mut stdout = tokio::io::stdout();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => return Err(e),
            }
            clear_response(&line, &in_flight_down).await;
            stdout.write_all(line.as_bytes()).await?;
            stdout.flush().await?;
        }
        Ok::<(), std::io::Error>(())
    });

    // Either side terminating signals the connection is over. We don't care
    // which: the surviving task will be aborted when the JoinHandle is dropped.
    tokio::select! {
        _ = upstream => {}
        _ = downstream => {}
    }

    flush_pending_errors(&in_flight).await;
    Ok(())
}

/// Inspect an outbound NDJSON line for a request id and remember it.
/// Notifications (`id` absent or null) and responses are ignored.
async fn track_request(line: &str, in_flight: &Mutex<HashMap<String, Value>>) {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return;
    };
    if v.get("method").is_none() {
        return;
    }
    let Some(id) = v.get("id").filter(|i| !i.is_null()).cloned() else {
        return;
    };
    let key = id.to_string();
    in_flight.lock().await.insert(key, id);
}

/// Drop a tracked request id once its response/error has been forwarded back.
async fn clear_response(line: &str, in_flight: &Mutex<HashMap<String, Value>>) {
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
        return;
    };
    if v.get("result").is_none() && v.get("error").is_none() {
        return;
    }
    let Some(id) = v.get("id").filter(|i| !i.is_null()) else {
        return;
    };
    in_flight.lock().await.remove(&id.to_string());
}

/// On disconnect, emit a JSON-RPC error for every still-pending request id
/// so Claude sees a structured failure instead of a broken pipe.
async fn flush_pending_errors(in_flight: &Mutex<HashMap<String, Value>>) {
    let pending: Vec<Value> = {
        let guard = in_flight.lock().await;
        guard.values().cloned().collect()
    };
    if pending.is_empty() {
        return;
    }
    let mut stdout = tokio::io::stdout();
    for id in pending {
        let err = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": "mdkb daemon unavailable"},
        });
        let _ = stdout.write_all(err.to_string().as_bytes()).await;
        let _ = stdout.write_all(b"\n").await;
    }
    let _ = stdout.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn track_request_records_id() {
        let map = Mutex::new(HashMap::new());
        track_request(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#, &map).await;
        assert!(map.lock().await.contains_key("7"));
    }

    #[tokio::test]
    async fn track_request_ignores_notifications() {
        let map = Mutex::new(HashMap::new());
        track_request(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &map,
        )
        .await;
        assert!(map.lock().await.is_empty());
    }

    #[tokio::test]
    async fn clear_response_removes_id() {
        let map = Mutex::new(HashMap::new());
        track_request(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#, &map).await;
        clear_response(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#, &map).await;
        assert!(map.lock().await.is_empty());
    }

    #[tokio::test]
    async fn clear_response_ignores_unmatched_lines() {
        let map = Mutex::new(HashMap::new());
        track_request(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#, &map).await;
        clear_response(r#"{"jsonrpc":"2.0","method":"ping"}"#, &map).await;
        assert!(map.lock().await.contains_key("7"));
    }

    #[tokio::test]
    async fn track_request_handles_string_ids() {
        let map = Mutex::new(HashMap::new());
        track_request(r#"{"jsonrpc":"2.0","id":"abc","method":"x"}"#, &map).await;
        assert!(map.lock().await.contains_key("\"abc\""));
    }
}
