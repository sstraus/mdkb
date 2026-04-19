//! E2E tests for `mdkb hook <cmd>` one-shot JSON-RPC client (story 015-e13b).
//!
//! Boots a real daemon in a tempdir HOME, confirms the client end-to-end
//! agrees with the daemon on framing and method names, and runs a warm
//! microbench to prove the story-level p50 < 50 ms budget for a `status`
//! round-trip.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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
    fn spawn_with_repo(repo: &std::path::Path) -> Self {
        let home = TempDir::new().unwrap();
        // Pre-register the repo so the whitelist accepts it.
        let mdkb_dir = home.path().join(".mdkb");
        std::fs::create_dir_all(&mdkb_dir).unwrap();
        let cfg = format!(
            "whitelist_dirs = [{:?}]\n",
            repo.parent().unwrap().display().to_string()
        );
        std::fs::write(mdkb_dir.join("daemon.toml"), cfg).unwrap();

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

    fn hook_socket(&self) -> PathBuf {
        self.home.path().join(".mdkb").join("daemon-hook.sock")
    }

    fn wait_for_sockets(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.hook_socket().exists() {
                return;
            }
            sleep(Duration::from_millis(25));
        }
        panic!("daemon did not create hook socket within 5s");
    }
}

impl Drop for DaemonProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn make_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join(".mdkb")).unwrap();
    tmp
}

/// Round-trip one framed JSON-RPC request through the hook socket. Returns
/// the elapsed wall-clock for the exchange.
fn call_once(socket: &std::path::Path, body: &[u8]) -> (Duration, Vec<u8>) {
    let mut sock = UnixStream::connect(socket).expect("connect hook socket");
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sock.set_write_timeout(Some(Duration::from_secs(5))).unwrap();

    let start = Instant::now();
    let len = u32::try_from(body.len()).unwrap().to_le_bytes();
    sock.write_all(&len).unwrap();
    sock.write_all(body).unwrap();

    let mut hdr = [0u8; 4];
    sock.read_exact(&mut hdr).unwrap();
    let resp_len = u32::from_le_bytes(hdr) as usize;
    let mut out = vec![0u8; resp_len];
    sock.read_exact(&mut out).unwrap();
    (start.elapsed(), out)
}

#[test]
fn hook_client_status_round_trip_succeeds() {
    let repo = make_repo();
    let root = repo.path().canonicalize().unwrap();
    let d = DaemonProc::spawn_with_repo(&root);

    let req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"status","params":{{"root":{:?}}}}}"#,
        root.display().to_string()
    );
    let (_elapsed, body) = call_once(&d.hook_socket(), req.as_bytes());
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("\"id\":1"), "envelope must echo id: {text}");
    assert!(!text.contains("\"error\""), "status should succeed: {text}");
    assert!(text.contains("Index Status"), "status body: {text}");
}

/// Story 015 AC: median round-trip for `status` < 50 ms over 100 iterations
/// on a warm daemon.
///
/// Warm-up pass primes ONNX loading and SQLite pragmas; only the steady-state
/// numbers are measured.
#[test]
fn hook_client_status_p50_under_50ms_warm() {
    let repo = make_repo();
    let root = repo.path().canonicalize().unwrap();
    let d = DaemonProc::spawn_with_repo(&root);
    let sock = d.hook_socket();

    let req = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"status","params":{{"root":{:?}}}}}"#,
        root.display().to_string()
    );
    let body = req.as_bytes();

    // Warm-up: first call triggers lazy init inside the daemon.
    for _ in 0..5 {
        let _ = call_once(&sock, body);
    }

    let mut samples: Vec<Duration> = Vec::with_capacity(100);
    for _ in 0..100 {
        let (elapsed, _) = call_once(&sock, body);
        samples.push(elapsed);
    }
    samples.sort();
    let p50 = samples[samples.len() / 2];
    let p95 = samples[(samples.len() * 95) / 100];

    assert!(
        p50 < Duration::from_millis(50),
        "status p50={p50:?} exceeds 50ms budget (p95={p95:?})"
    );
}

/// Whitelist rejection: if a root is outside the configured whitelist, the
/// daemon returns a JSON-RPC error envelope. The client prints it to stderr
/// and exits 0 — but that end of the contract is exercised via the binary
/// below. Here we just verify the daemon produces the structured error the
/// client relies on.
#[test]
fn hook_client_whitelist_rejection_returns_structured_error() {
    let repo = make_repo();
    let root = repo.path().canonicalize().unwrap();
    let d = DaemonProc::spawn_with_repo(&root);

    // A path that definitely isn't under the whitelist.
    let outside = "/tmp/definitely-not-whitelisted-xyz";
    let req = format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"status","params":{{"root":{:?}}}}}"#,
        outside
    );
    let (_elapsed, body) = call_once(&d.hook_socket(), req.as_bytes());
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("\"error\""), "should reject: {text}");
    assert!(text.contains("-32602"), "code should be -32602: {text}");
}

/// End-to-end: `mdkb hook status --root <repo>` run as a subprocess exits 0
/// even on whitelist rejection (host hook must not be blocked).
#[test]
fn hook_cli_exits_zero_on_whitelist_rejection() {
    let repo = make_repo();
    let root = repo.path().canonicalize().unwrap();
    let d = DaemonProc::spawn_with_repo(&root);
    drop(d); // daemon lifetime only needed for socket bind; now test a cold call

    // Point the client at a HOME with an empty whitelist (no repos allowed)
    // so the call is rejected. Using MDKB_NO_DAEMON avoids the auto-spawn
    // race and exercises the in-process fallback path, which shares the
    // same error handling contract.
    let empty_home = TempDir::new().unwrap();
    std::fs::create_dir_all(empty_home.path().join(".mdkb")).unwrap();
    std::fs::write(
        empty_home.path().join(".mdkb").join("daemon.toml"),
        "whitelist_dirs = [\"/no/such/dir\"]\n",
    )
    .unwrap();

    let output = Command::new(BIN)
        .arg("hook")
        .arg("status")
        .arg("--root")
        .arg(&root)
        .env("HOME", empty_home.path())
        .env("MDKB_NO_DAEMON", "1")
        .output()
        .expect("run hook status");

    assert!(
        output.status.success(),
        "hook client must exit 0 on whitelist rejection; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}
