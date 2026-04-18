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
    let mut sock = UnixStream::connect(d.hook_socket()).expect("connect hook socket");
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(2))).unwrap();

    // Length-prefixed JSON-RPC: 4-byte LE u32 length + JSON body.
    let req = br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;
    let len = (req.len() as u32).to_le_bytes();
    sock.write_all(&len).unwrap();
    sock.write_all(req).unwrap();

    let mut hdr = [0u8; 4];
    sock.read_exact(&mut hdr).expect("read response length");
    let resp_len = u32::from_le_bytes(hdr) as usize;
    assert!(resp_len > 0 && resp_len < 4096, "sane response length");

    let mut body = vec![0u8; resp_len];
    sock.read_exact(&mut body).expect("read response body");
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("\"id\":1"), "response must echo id: {text}");
    assert!(text.contains("pong"), "response must contain pong: {text}");
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
