//! HTTP transport end-to-end: `mdkb serve --http` behind a bearer token.
//!
//! Story 075-19c6. Runs the real binary, drives `initialize` and `tools/list`
//! over HTTP/1.1 and asserts the DNS-rebinding guard that rmcp 1.4 added
//! (RUSTSEC-2026-0189): a request whose `Host` header names a foreign host is
//! refused with 403 even when it carries the right token. Without that guard
//! a web page the operator visits can reach this server through a name the
//! attacker controls.
//!
//! The client is a raw TCP socket on purpose: the assertions are about wire
//! details (`Host`, `Mcp-Session-Id`, status codes) that an HTTP client
//! library normalises away.

#![cfg(feature = "http-server")]

use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");
const TOKEN: &str = "test-token-075";

/// A `mdkb serve --http` child on a free loopback port with its own `$HOME`.
struct HttpServer {
    child: Child,
    port: u16,
    stderr_path: PathBuf,
    _home: TempDir,
    _repo: TempDir,
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A parsed HTTP/1.1 response with a de-chunked body.
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The JSON-RPC message in the body, whether it came as plain JSON or as
    /// the `data:` line of an SSE event.
    fn json(&self) -> Value {
        let is_sse = self
            .header("content-type")
            .is_some_and(|ct| ct.starts_with("text/event-stream"));
        let payload = if is_sse {
            // The stream opens with a priming event whose `data:` is empty;
            // the message is the first event that carries one.
            self.body
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .find(|data| !data.is_empty())
                .unwrap_or_else(|| panic!("no SSE data line in body: {}", self.body))
                .to_string()
        } else {
            self.body.clone()
        };
        serde_json::from_str(&payload)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {payload}"))
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_server() -> HttpServer {
    let home = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let port = free_port();
    let stderr_path = home.path().join("serve.stderr");
    let stderr = File::create(&stderr_path).unwrap();
    let child = Command::new(BIN)
        .args([
            "serve",
            "--http",
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--token",
            TOKEN,
        ])
        .current_dir(repo.path())
        .env("HOME", home.path())
        .env("MDKB_NO_DAEMON", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .expect("spawn mdkb serve --http");
    let server = HttpServer {
        child,
        port,
        stderr_path,
        _home: home,
        _repo: repo,
    };

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(resp) = server.try_request("GET", "/health", &server.own_host(), &[], None)
            && resp.status == 200
        {
            return server;
        }
        assert!(
            Instant::now() < deadline,
            "server did not answer /health in time; stderr:\n{}",
            std::fs::read_to_string(&server.stderr_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

impl HttpServer {
    fn own_host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// One HTTP/1.1 exchange on a fresh connection. `host` is sent verbatim
    /// as the `Host` header so a test can forge it.
    fn try_request(
        &self,
        method: &str,
        path: &str,
        host: &str,
        extra_headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> std::io::Result<Response> {
        let body = body.map(Value::to_string).unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
             Accept: application/json, text/event-stream\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in extra_headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(&body);

        let mut stream = TcpStream::connect(("127.0.0.1", self.port))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.write_all(request.as_bytes())?;

        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&chunk[..n]);
                    // A chunked body is complete at its terminating chunk; the
                    // server may keep an SSE connection open after it.
                    if raw.ends_with(b"\r\n0\r\n\r\n") {
                        break;
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(parse_response(&raw))
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        host: &str,
        extra_headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Response {
        self.try_request(method, path, host, extra_headers, body)
            .unwrap_or_else(|e| panic!("{method} {path} failed: {e}"))
    }

    /// POST a JSON-RPC message to `/mcp` with the right token and a `Host`
    /// of the test's choosing.
    fn post_mcp(&self, host: &str, session: Option<&str>, message: &Value) -> Response {
        let auth = format!("Bearer {TOKEN}");
        let mut headers = vec![("Authorization", auth.as_str())];
        if let Some(id) = session {
            headers.push(("Mcp-Session-Id", id));
        }
        self.request("POST", "/mcp", host, &headers, Some(message))
    }
}

fn parse_response(raw: &[u8]) -> Response {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("no header terminator in response: {text}"));
    let mut lines = head.lines();
    let status_line = lines.next().expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("bad status line: {status_line}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_string()
    };
    Response {
        status,
        headers,
        body,
    }
}

/// Join the data of an HTTP/1.1 chunked body.
fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.trim(), 16).unwrap_or(0);
        if size == 0 || after.len() < size {
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].strip_prefix("\r\n").unwrap_or("");
    }
    out
}

fn initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "e2e-http", "version": "0"}
        }
    })
}

fn tools_list_request() -> Value {
    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
}

/// The whole legacy handshake over HTTP: initialize, initialized, tools/list.
/// The tool inventory must be the one stdio advertises.
#[test]
fn http_initialize_and_tools_list() {
    let server = start_server();
    let host = server.own_host();

    let init = server.post_mcp(&host, None, &initialize_request());
    assert_eq!(init.status, 200, "initialize body: {}", init.body);
    let init_json = init.json();
    assert_eq!(init_json["result"]["serverInfo"]["name"], "mdkb");
    assert_eq!(init_json["result"]["protocolVersion"], "2025-06-18");
    let session = init
        .header("mcp-session-id")
        .expect("legacy session mode assigns Mcp-Session-Id on initialize")
        .to_string();

    let ack = server.post_mcp(
        &host,
        Some(&session),
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    );
    assert_eq!(ack.status, 202, "notification body: {}", ack.body);

    let list = server.post_mcp(&host, Some(&session), &tools_list_request());
    assert_eq!(list.status, 200, "tools/list body: {}", list.body);
    let tools = list.json()["result"]["tools"]
        .as_array()
        .cloned()
        .expect("tools array");
    let mut names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    names.sort_unstable();
    let mut advertised = mdkb::mcp::server::advertised_tool_names();
    advertised.sort_unstable();
    assert_eq!(names, advertised, "HTTP advertises the same tools as stdio");
    assert_eq!(names.len(), 13);
}

/// The DNS-rebinding guard: a valid token does not help a request that
/// arrived under a `Host` this server was not bound as. rmcp answers 403
/// before the message reaches the handler.
#[test]
fn http_rejects_foreign_host_header() {
    let server = start_server();

    let foreign = format!("attacker.example:{}", server.port);
    let refused = server.post_mcp(&foreign, None, &initialize_request());
    assert_eq!(
        refused.status, 403,
        "foreign Host must be refused; body: {}",
        refused.body
    );
    assert!(
        !refused.body.contains("serverInfo"),
        "a refused request must not leak the initialize result"
    );

    // The same message under the bound host succeeds, so the refusal above is
    // the Host check and not a broken request.
    let accepted = server.post_mcp(&server.own_host(), None, &initialize_request());
    assert_eq!(accepted.status, 200, "own Host body: {}", accepted.body);
    // `localhost` is in rmcp's loopback allow-list, so a browser on the same
    // machine still reaches the server by name.
    let by_name = server.post_mcp(
        &format!("localhost:{}", server.port),
        None,
        &initialize_request(),
    );
    assert_eq!(by_name.status, 200, "localhost Host body: {}", by_name.body);
}

/// The bearer token is still checked first: no token means 401 regardless
/// of the Host header, and `/health` stays open.
#[test]
fn http_requires_token_and_serves_health() {
    let server = start_server();
    let host = server.own_host();

    let health = server.request("GET", "/health", &host, &[], None);
    assert_eq!(health.status, 200);
    assert_eq!(health.json()["status"], "ok");

    let no_token = server.request("POST", "/mcp", &host, &[], Some(&initialize_request()));
    assert_eq!(no_token.status, 401);
}
