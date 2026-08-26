//! E2E tests for the daemon IPC sockets (Story 006-8d61).
//!
//! Boots `mdkb serve --daemon` with HOME overridden to a tempdir and asserts:
//! - both sockets appear with mode 0600
//! - hook socket responds to a length-prefixed JSON-RPC `ping` call
//! - MCP socket is connectable (full rmcp handshake lives in Story 4)
//! - SIGTERM cleanup unlinks both sockets

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");

struct DaemonProc {
    child: Child,
    home: TempDir,
}

impl DaemonProc {
    fn spawn() -> Self {
        let home = TempDir::new().unwrap();
        let child = Command::new(BIN)
            .arg("serve")
            .arg("--daemon")
            .env("HOME", home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn daemon");
        let d = DaemonProc { child, home };
        d.wait_for_sockets();
        d
    }

    fn base(&self) -> PathBuf {
        self.home.path().join(".mdkb")
    }

    fn mcp_socket(&self) -> PathBuf {
        self.base().join("daemon.sock")
    }

    fn hook_socket(&self) -> PathBuf {
        self.base().join("daemon-hook.sock")
    }

    fn wait_for_sockets(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.mcp_socket().exists() && self.hook_socket().exists() {
                return;
            }
            sleep(Duration::from_millis(50));
        }
        panic!("daemon did not create sockets within 5s");
    }

    /// Seed a repo with a code index the daemon is allowed to open.
    ///
    /// It must live under the daemon's HOME: an empty `whitelist_dirs` falls
    /// back to the home directory, so a repo anywhere else is refused before
    /// dispatch and every call comes back as `-32602`.
    fn repo_with_code_index(&self) -> PathBuf {
        let root = self.home.path().join("repo");
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        // `farewell` calls `greet`, so `greet` has exactly one indexed caller.
        std::fs::write(
            src.join("lib.rs"),
            "pub fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n\
             pub fn farewell(name: &str) -> String {\n    greet(name); format!(\"bye {name}\")\n}\n",
        )
        .unwrap();

        // Index `src/`, not the repo root: an argument bounds the walk but must
        // not change what a file is called. Key these symbols under `lib.rs`
        // instead of `src/lib.rs` and the daemon's own watcher files a second
        // copy of every one of them, leaving every lookup by name ambiguous.
        for args in [vec!["init"], vec!["code", "index", "src"]] {
            let out = Command::new(BIN)
                .args(&args)
                .current_dir(&root)
                .env("HOME", self.home.path())
                .output()
                .unwrap_or_else(|e| panic!("run mdkb {args:?}: {e}"));
            assert!(
                out.status.success(),
                "mdkb {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        root
    }

    /// One length-prefixed JSON-RPC round trip against the hook socket.
    fn hook_call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let mut sock = UnixStream::connect(self.hook_socket()).expect("connect hook socket");
        sock.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        sock.set_write_timeout(Some(Duration::from_secs(20)))
            .unwrap();

        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params,
        })
        .to_string();
        sock.write_all(&(req.len() as u32).to_le_bytes()).unwrap();
        sock.write_all(req.as_bytes()).unwrap();

        let mut hdr = [0u8; 4];
        sock.read_exact(&mut hdr).expect("read response length");
        let mut body = vec![0u8; u32::from_le_bytes(hdr) as usize];
        sock.read_exact(&mut body).expect("read response body");
        serde_json::from_slice(&body).expect("response is json")
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn file_mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn daemon_creates_both_sockets_mode_0600() {
    let d = DaemonProc::spawn();
    assert_eq!(file_mode(&d.mcp_socket()), 0o600, "mcp socket must be 0600");
    assert_eq!(
        file_mode(&d.hook_socket()),
        0o600,
        "hook socket must be 0600"
    );
}

#[test]
fn hook_socket_responds_to_ping() {
    let d = DaemonProc::spawn();
    let resp = d.hook_call("ping", serde_json::json!({}));

    assert_eq!(resp["id"], 1, "response must echo id: {resp}");
    assert_eq!(resp["result"]["pong"], true, "must pong: {resp}");
    // The version is the whole point of `ping` for an embedding client: it
    // decides whether the daemon it reached matches the binary it installed.
    assert_eq!(
        resp["result"]["version"],
        env!("CARGO_PKG_VERSION"),
        "ping must report the daemon's own version: {resp}"
    );
}

// ── Wire contract of the code-intelligence methods ───────────────────────────
//
// These pin the SHAPE of what the hook socket returns, not just that a call
// succeeds. An embedding client (TUICommander's outline, go-to-definition and
// find-references) deserializes these payloads directly, and a shape change is
// silent on both ends: the daemon still answers 200, the client still parses
// "successfully" into an empty list. Only an assertion on the shape catches it.

#[test]
fn hook_symbols_in_file_returns_a_bare_symbol_array() {
    let d = DaemonProc::spawn();
    let root = d.repo_with_code_index();

    let resp = d.hook_call(
        "symbols_in_file",
        serde_json::json!({ "root": root, "file": "src/lib.rs" }),
    );
    let text = resp["result"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("result.text must be a string: {resp}"));
    let symbols: serde_json::Value = serde_json::from_str(text).expect("text is json");

    let arr = symbols
        .as_array()
        .unwrap_or_else(|| panic!("symbols_in_file text must be a bare array: {symbols}"));
    let greet = arr
        .iter()
        .find(|s| s["name"] == "greet")
        .unwrap_or_else(|| panic!("greet must be indexed: {symbols}"));
    assert_eq!(greet["file_path"], "src/lib.rs");
    // Ranges are 0-BASED. `greet` is on the first line of the file. This is the
    // opposite of `symbol_at_position`, whose `line` input is 1-based, so a
    // client that feeds one into the other must convert.
    assert_eq!(greet["line_start"], 0);
    assert!(greet["kind"].is_string(), "kind must be a string: {greet}");
}

#[test]
fn hook_code_find_wraps_symbols_in_a_total_envelope() {
    let d = DaemonProc::spawn();
    let root = d.repo_with_code_index();

    let resp = d.hook_call(
        "code_find",
        serde_json::json!({ "root": root, "name": "greet" }),
    );
    let text = resp["result"]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("result.text must be a string: {resp}"));
    let found: serde_json::Value = serde_json::from_str(text).expect("text is json");

    // `code_find` is the one code method that does NOT return a bare array:
    // `total` travels with the rows so a capped result cannot read as the whole
    // set. Clients must reach through `.symbols`.
    assert!(
        found.get("symbols").and_then(|s| s.as_array()).is_some(),
        "code_find must wrap rows under `symbols`: {found}"
    );
    assert!(found["total"].is_number(), "total must be present: {found}");
    assert_eq!(found["symbols"][0]["name"], "greet");
    assert_eq!(found["symbols"][0]["file_path"], "src/lib.rs");
}

#[test]
fn hook_code_graph_returns_prose_and_resolved_symbols() {
    let d = DaemonProc::spawn();
    let root = d.repo_with_code_index();

    let resp = d.hook_call(
        "code_graph",
        serde_json::json!({ "root": root, "name": "greet", "direction": "callers" }),
    );
    let result = &resp["result"];
    assert!(resp.get("error").is_none(), "code_graph errored: {resp}");

    // `text` stays prose — that is what the MCP agents read.
    let text = result["text"]
        .as_str()
        .unwrap_or_else(|| panic!("result.text must be a string: {resp}"));
    assert!(
        text.contains("is called by"),
        "text must stay agent-readable prose: {text}"
    );

    // `symbols` is the machine-readable half, same row shape as
    // `symbols_in_file`, so a client never scrapes the prose for locations.
    let symbols = result["symbols"]
        .as_array()
        .unwrap_or_else(|| panic!("result.symbols must be an array: {resp}"));
    assert_eq!(symbols.len(), 1, "greet has one caller: {resp}");
    assert_eq!(symbols[0]["name"], "farewell");
    assert_eq!(symbols[0]["file_path"], "src/lib.rs");
    // 0-based, like every other range mdkb emits: `farewell` starts on the
    // fourth line of the fixture.
    assert_eq!(symbols[0]["line_start"], 3);
}

#[test]
fn hook_code_graph_reports_no_callers_without_inventing_symbols() {
    let d = DaemonProc::spawn();
    let root = d.repo_with_code_index();

    let resp = d.hook_call(
        "code_graph",
        serde_json::json!({ "root": root, "name": "farewell", "direction": "callers" }),
    );
    let result = &resp["result"];
    assert!(
        result["text"]
            .as_str()
            .is_some_and(|t| t.contains("no indexed callers")),
        "text must say there are none: {resp}"
    );
    // The empty case must still be an array: a client that treats a missing
    // `symbols` as "old daemon" would otherwise misread "no callers".
    assert_eq!(
        result["symbols"].as_array().map(Vec::len),
        Some(0),
        "symbols must be an empty array, not absent: {resp}"
    );
}

#[test]
fn mcp_socket_accepts_connection() {
    let d = DaemonProc::spawn();
    let _sock = UnixStream::connect(d.mcp_socket()).expect("connect mcp socket");
    // Full rmcp handshake is Story 4; here we only verify the listener accepts.
}

#[test]
fn sigterm_unlinks_sockets_and_releases_lock() {
    let d = DaemonProc::spawn();
    let mcp = d.mcp_socket();
    let hook = d.hook_socket();
    let pid_path = d.base().join("daemon.pid");
    assert!(mcp.exists() && hook.exists() && pid_path.exists());

    // Take ownership of the child so we can wait; replace guard's child first.
    let mut dp = d;
    #[allow(clippy::cast_possible_wrap)]
    let pid = dp.child.id() as i32;

    // Send SIGTERM.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let exit = dp.child.wait().expect("wait daemon");
    assert!(
        exit.success() || exit.code().is_some(),
        "daemon should exit cleanly, got {exit:?}"
    );

    assert!(!mcp.exists(), "mcp socket should be unlinked on shutdown");
    assert!(!hook.exists(), "hook socket should be unlinked on shutdown");

    // Fresh daemon should start now — lock must have been released.
    let d2 = DaemonProc::spawn();
    drop(d2);
}
