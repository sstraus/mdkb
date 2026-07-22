//! Shared MCP test harness for PTY-based E2E tests.
//!
//! Spawns a real `mdkb serve` process and communicates via JSON-RPC over stdin/stdout.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

/// Test harness that spawns an MCP server process and communicates via JSON-RPC.
pub struct McpTestHarness {
    child: Child,
    _temp_dir: TempDir,
    pub root: PathBuf,
    next_id: u64,
    reader: BufReader<ChildStdout>,
}

impl McpTestHarness {
    /// Create a new test harness with an initialized mdkb directory.
    pub fn new() -> Self {
        Self::with_env(&[])
    }

    /// Create a new test harness with custom environment variables.
    pub fn with_env(env_vars: &[(&str, &str)]) -> Self {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();

        // Initialize mdkb
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .arg("init")
            .current_dir(&root)
            .status()
            .expect("Failed to run mdkb init");
        assert!(status.success(), "mdkb init failed");

        // Spawn mdkb serve with optional env vars
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkb"));
        cmd.arg("serve")
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, val) in env_vars {
            cmd.env(key, val);
        }
        let mut child = cmd.spawn().expect("Failed to spawn mdkb serve");

        let stdout = child.stdout.take().expect("No stdout");
        let reader = BufReader::new(stdout);

        Self {
            child,
            _temp_dir: temp_dir,
            root,
            next_id: 1,
            reader,
        }
    }

    /// Send a JSON-RPC request and return the response.
    pub fn send_request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let stdin = self.child.stdin.as_mut().expect("No stdin");
        let request_str = serde_json::to_string(&request).unwrap();
        writeln!(stdin, "{}", request_str).expect("Failed to write request");
        stdin.flush().expect("Failed to flush");

        let mut response_line = String::new();
        let start = std::time::Instant::now();
        loop {
            match self.reader.read_line(&mut response_line) {
                Ok(0) => panic!("Server closed stdout unexpectedly"),
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        start.elapsed() <= Duration::from_secs(10),
                        "Timeout waiting for response to {}",
                        method
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("Read error: {}", e),
            }
        }

        let response: Value = serde_json::from_str(&response_line)
            .unwrap_or_else(|e| panic!("Invalid JSON response: {} - {}", e, response_line));
        assert_eq!(response["id"], id, "Response ID mismatch");
        response
    }

    /// Send a notification (no response expected).
    pub fn send_notification(&mut self, method: &str, params: Value) {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let stdin = self.child.stdin.as_mut().expect("No stdin");
        let s = serde_json::to_string(&notification).unwrap();
        writeln!(stdin, "{}", s).expect("Failed to write notification");
        stdin.flush().expect("Failed to flush");
    }

    /// Perform MCP initialization handshake and wait for server readiness.
    pub fn initialize(&mut self) -> Value {
        let response = self.send_request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }),
        );
        self.send_notification("notifications/initialized", json!({}));

        // Poll until the server's startup reindex finishes and ctx is available.
        // The status tool needs ctx, so a successful call means the server is ready.
        let start = std::time::Instant::now();
        loop {
            let status = self.call_tool("status", json!({}));
            if status.get("error").is_none() {
                break;
            }
            assert!(
                start.elapsed() <= Duration::from_secs(10),
                "Server for {} did not become ready within 10s",
                self.root.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }

        response
    }

    /// Call an MCP tool and return the result.
    pub fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.send_request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    /// Extract text content from a tool call result.
    pub fn get_text_content(result: &Value) -> String {
        result["result"]["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c["text"].as_str())
            .unwrap_or("")
            .to_string()
    }
}

impl Drop for McpTestHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
