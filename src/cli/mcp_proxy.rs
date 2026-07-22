//! `mdkb mcp` — a thin rmcp NDJSON proxy from Claude's stdio to the daemon's
//! unix socket.
//!
//! The proxy is a near-pure byte forwarder: each direction reads one
//! newline-delimited JSON-RPC message, forwards it untouched, and the rmcp
//! peers handshake/dispatch as if directly connected. The only structured
//! work the proxy does is track in-flight request ids so it can synthesize
//! `mdkb daemon unavailable` error responses if the socket dies mid-call,
//! instead of leaving Claude staring at a half-closed pipe.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::daemon::spawn::ensure_daemon_running;
use crate::error::{Error, Result};

/// Connect to the daemon's MCP socket (auto-spawning if necessary) and
/// bidirectionally forward NDJSON between Claude's stdio and the socket.
///
/// A daemon restart does not close the stdio transport. The next client
/// message reconnects and replays only the MCP initialization handshake; an
/// in-flight application request is failed rather than replayed because it may
/// have committed before the old connection disappeared.
pub async fn run_proxy(socket_path: PathBuf) -> Result<()> {
    run_proxy_io(socket_path, tokio::io::stdin(), tokio::io::stdout()).await
}

#[derive(Debug, Default)]
struct Handshake {
    initialize: Option<String>,
    initialized: Option<String>,
}

impl Handshake {
    fn observe(&mut self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
            return;
        };
        match value.get("method").and_then(Value::as_str) {
            Some("initialize") if value.get("id").is_some_and(|id| !id.is_null()) => {
                self.initialize = Some(ensure_newline(line));
            }
            Some("notifications/initialized") => {
                self.initialized = Some(ensure_newline(line));
            }
            _ => {}
        }
    }

    fn replay_messages(&self) -> Option<(&str, &str)> {
        Some((self.initialize.as_deref()?, self.initialized.as_deref()?))
    }
}

#[derive(Debug)]
enum SocketEvent {
    Line { generation: u64, line: String },
    Closed { generation: u64 },
}

#[derive(Debug)]
struct DaemonConnection {
    writer: OwnedWriteHalf,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for DaemonConnection {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

async fn run_proxy_io<R, W>(socket_path: PathBuf, input: R, mut output: W) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let mut generation = 1_u64;
    let mut connection = Some(connect(&socket_path, generation, None, &event_tx).await?);
    let mut input = BufReader::new(input);
    let mut input_line = String::new();
    let mut handshake = Handshake::default();
    let mut in_flight = HashMap::new();

    loop {
        tokio::select! {
            read = input.read_line(&mut input_line) => {
                let bytes = read.map_err(|e| Error::other(format!("read MCP stdin: {e}")))?;
                if bytes == 0 {
                    return Ok(());
                }

                handshake.observe(&input_line);
                track_request(&input_line, &mut in_flight);

                if connection.is_none() {
                    generation = generation.wrapping_add(1);
                    match connect(
                        &socket_path,
                        generation,
                        handshake.replay_messages(),
                        &event_tx,
                    )
                    .await
                    {
                        Ok(new_connection) => connection = Some(new_connection),
                        Err(error) => {
                            tracing::warn!("MCP daemon reconnect failed: {error}");
                            flush_pending_errors(&mut in_flight, &mut output).await?;
                            input_line.clear();
                            continue;
                        }
                    }
                }

                let write_result = if let Some(active) = connection.as_mut() {
                    active.writer.write_all(input_line.as_bytes()).await
                } else {
                    unreachable!("connection was established above")
                };
                if let Err(error) = write_result {
                    tracing::warn!("MCP daemon connection write failed: {error}");
                    connection = None;
                    flush_pending_errors(&mut in_flight, &mut output).await?;
                }
                input_line.clear();
            }
            event = event_rx.recv() => {
                let Some(event) = event else {
                    return Err(Error::other("MCP daemon reader channel closed"));
                };
                match event {
                    SocketEvent::Line { generation: event_generation, line }
                        if event_generation == generation =>
                    {
                        clear_response(&line, &mut in_flight);
                        output.write_all(line.as_bytes()).await
                            .map_err(|e| Error::other(format!("write MCP stdout: {e}")))?;
                        output.flush().await
                            .map_err(|e| Error::other(format!("flush MCP stdout: {e}")))?;
                    }
                    SocketEvent::Closed { generation: event_generation }
                        if event_generation == generation =>
                    {
                        connection = None;
                        flush_pending_errors(&mut in_flight, &mut output).await?;
                    }
                    SocketEvent::Line { .. } | SocketEvent::Closed { .. } => {
                        // A replaced reader may report its final event after the new
                        // generation is live. It cannot affect the current socket.
                    }
                }
            }
        }
    }
}

async fn connect(
    socket_path: &std::path::Path,
    generation: u64,
    replay: Option<(&str, &str)>,
    event_tx: &mpsc::Sender<SocketEvent>,
) -> Result<DaemonConnection> {
    let stream = if let Ok(stream) = UnixStream::connect(socket_path).await {
        stream
    } else {
        ensure_daemon_running(socket_path).await?;
        UnixStream::connect(socket_path)
            .await
            .map_err(|e| Error::other(format!("connect {}: {e}", socket_path.display())))?
    };
    let (read, mut writer) = stream.into_split();
    let mut read = BufReader::new(read);

    if let Some((initialize, initialized)) = replay {
        writer
            .write_all(initialize.as_bytes())
            .await
            .map_err(|e| Error::other(format!("replay MCP initialize to daemon: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| Error::other(format!("flush replayed MCP initialize: {e}")))?;

        let mut response = String::new();
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read.read_line(&mut response),
        )
        .await
        .map_err(|_| Error::other("daemon timed out replaying MCP initialize"))?
        .map_err(|e| Error::other(format!("read replayed MCP initialize response: {e}")))?;
        if bytes == 0 || !is_successful_initialize_response(initialize, &response) {
            return Err(Error::other(
                "daemon rejected the replayed MCP initialization handshake",
            ));
        }

        writer
            .write_all(initialized.as_bytes())
            .await
            .map_err(|e| Error::other(format!("replay MCP initialized notification: {e}")))?;
        writer.flush().await.map_err(|e| {
            Error::other(format!("flush replayed MCP initialized notification: {e}"))
        })?;
    }

    let reader_tx = event_tx.clone();
    let reader = tokio::spawn(async move {
        read_socket(read, generation, reader_tx).await;
    });
    Ok(DaemonConnection { writer, reader })
}

async fn read_socket(
    mut read: BufReader<OwnedReadHalf>,
    generation: u64,
    event_tx: mpsc::Sender<SocketEvent>,
) {
    loop {
        let mut line = String::new();
        match read.read_line(&mut line).await {
            Ok(0) | Err(_) => {
                let _ = event_tx.send(SocketEvent::Closed { generation }).await;
                return;
            }
            Ok(_) => {
                if event_tx
                    .send(SocketEvent::Line { generation, line })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

fn is_successful_initialize_response(request: &str, response: &str) -> bool {
    let Ok(request) = serde_json::from_str::<Value>(request.trim_end()) else {
        return false;
    };
    let Ok(response) = serde_json::from_str::<Value>(response.trim_end()) else {
        return false;
    };
    response.get("id") == request.get("id")
        && response.get("result").is_some()
        && response.get("error").is_none()
}

fn ensure_newline(line: &str) -> String {
    if line.ends_with('\n') {
        line.to_owned()
    } else {
        format!("{line}\n")
    }
}

/// Inspect an outbound NDJSON line for a request id and remember it.
/// Notifications (`id` absent or null) and responses are ignored.
fn track_request(line: &str, in_flight: &mut HashMap<String, Value>) {
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
    in_flight.insert(key, id);
}

/// Drop a tracked request id once its response/error has been forwarded back.
fn clear_response(line: &str, in_flight: &mut HashMap<String, Value>) {
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
    in_flight.remove(&id.to_string());
}

/// On disconnect, emit a JSON-RPC error for every still-pending request id
/// so Claude sees a structured failure instead of a broken pipe.
async fn flush_pending_errors<W>(
    in_flight: &mut HashMap<String, Value>,
    output: &mut W,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let pending: Vec<Value> = in_flight.drain().map(|(_, id)| id).collect();
    if pending.is_empty() {
        return Ok(());
    }
    for id in pending {
        let err = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32603, "message": "mdkb daemon unavailable"},
        });
        output
            .write_all(err.to_string().as_bytes())
            .await
            .map_err(|e| Error::other(format!("write MCP disconnect error: {e}")))?;
        output
            .write_all(b"\n")
            .await
            .map_err(|e| Error::other(format!("write MCP disconnect newline: {e}")))?;
    }
    output
        .flush()
        .await
        .map_err(|e| Error::other(format!("flush MCP disconnect errors: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn track_request_records_id() {
        let mut map = HashMap::new();
        track_request(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#,
            &mut map,
        );
        assert!(map.contains_key("7"));
    }

    #[tokio::test]
    async fn track_request_ignores_notifications() {
        let mut map = HashMap::new();
        track_request(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &mut map,
        );
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn clear_response_removes_id() {
        let mut map = HashMap::new();
        track_request(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#,
            &mut map,
        );
        clear_response(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#, &mut map);
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn clear_response_ignores_unmatched_lines() {
        let mut map = HashMap::new();
        track_request(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#,
            &mut map,
        );
        clear_response(r#"{"jsonrpc":"2.0","method":"ping"}"#, &mut map);
        assert!(map.contains_key("7"));
    }

    #[tokio::test]
    async fn track_request_handles_string_ids() {
        let mut map = HashMap::new();
        track_request(r#"{"jsonrpc":"2.0","id":"abc","method":"x"}"#, &mut map);
        assert!(map.contains_key("\"abc\""));
    }

    #[test]
    fn handshake_is_replayable_only_after_initialized_notification() {
        let mut handshake = Handshake::default();
        handshake.observe(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        assert!(handshake.replay_messages().is_none());

        handshake.observe(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        let (initialize, initialized) = handshake.replay_messages().unwrap();
        assert!(initialize.ends_with('\n'));
        assert!(initialized.ends_with('\n'));
    }

    #[test]
    fn initialize_replay_rejects_mismatched_or_error_response() {
        let request = r#"{"jsonrpc":"2.0","id":7,"method":"initialize"}"#;
        assert!(is_successful_initialize_response(
            request,
            r#"{"jsonrpc":"2.0","id":7,"result":{}}"#
        ));
        assert!(!is_successful_initialize_response(
            request,
            r#"{"jsonrpc":"2.0","id":8,"result":{}}"#
        ));
        assert!(!is_successful_initialize_response(
            request,
            r#"{"jsonrpc":"2.0","id":7,"error":{"code":-1}}"#
        ));
    }

    #[tokio::test]
    async fn daemon_disconnect_keeps_stdio_open_and_replays_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();

        let fake_daemon = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            let (first_read, mut first_write) = first.into_split();
            let mut first_read = BufReader::new(first_read);
            assert_method(&mut first_read, "initialize").await;
            first_write
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
                .await
                .unwrap();
            assert_method(&mut first_read, "notifications/initialized").await;
            assert_method(&mut first_read, "tools/list").await;
            drop(first_write);
            drop(first_read);

            let (second, _) = listener.accept().await.unwrap();
            let (second_read, mut second_write) = second.into_split();
            let mut second_read = BufReader::new(second_read);
            assert_method(&mut second_read, "initialize").await;
            second_write
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
                .await
                .unwrap();
            assert_method(&mut second_read, "notifications/initialized").await;
            assert_method(&mut second_read, "tools/call").await;
            second_write
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":true}}\n")
                .await
                .unwrap();
        });

        let (client_input, proxy_input) = tokio::io::duplex(8 * 1024);
        let (proxy_output, client_output) = tokio::io::duplex(8 * 1024);
        let proxy = tokio::spawn(run_proxy_io(socket_path, proxy_input, proxy_output));
        let mut client_input = client_input;
        let mut client_output = BufReader::new(client_output);

        client_input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
            .await
            .unwrap();
        assert_eq!(read_json_line(&mut client_output).await["id"], 1);
        client_input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        client_input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await
            .unwrap();

        let disconnect = read_json_line(&mut client_output).await;
        assert_eq!(disconnect["id"], 2);
        assert_eq!(disconnect["error"]["code"], -32603);

        client_input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\"}\n")
            .await
            .unwrap();
        let response = read_json_line(&mut client_output).await;
        assert_eq!(response["id"], 3);
        assert_eq!(response["result"]["ok"], true);

        drop(client_input);
        tokio::time::timeout(std::time::Duration::from_secs(2), proxy)
            .await
            .expect("proxy must exit when its stdio input closes")
            .unwrap()
            .unwrap();
        fake_daemon.await.unwrap();
    }

    async fn assert_method<R>(reader: &mut BufReader<R>, expected: &str)
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let value = read_json_line(reader).await;
        assert_eq!(value["method"], expected);
    }

    async fn read_json_line<R>(reader: &mut BufReader<R>) -> Value
    where
        R: tokio::io::AsyncRead + Unpin,
    {
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_line(&mut line),
        )
        .await
        .expect("timed out waiting for NDJSON line")
        .unwrap();
        serde_json::from_str(&line).unwrap()
    }
}
