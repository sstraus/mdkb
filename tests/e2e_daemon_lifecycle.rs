//! E2E tests for `mdkb daemon status|stop|restart` and `--detach` (story 018-e88f).
//!
//! These run a real `mdkb` binary against an isolated `$HOME`. Each test
//! claims its own tempdir, so they don't race over the singleton lock.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");

fn hook_socket(home: &Path) -> PathBuf {
    home.join(".mdkb").join("daemon-hook.sock")
}

fn pid_file(home: &Path) -> PathBuf {
    home.join(".mdkb").join("daemon.pid")
}

fn mcp_socket(home: &Path) -> PathBuf {
    home.join(".mdkb").join("daemon.sock")
}

fn read_pid(home: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(pid_file(home)).ok()?;
    s.trim().parse().ok()
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 never mutates target process state.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn wait_until<F: Fn() -> bool>(deadline: Duration, check: F) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if check() {
            return true;
        }
        sleep(Duration::from_millis(50));
    }
    false
}

fn prepare_home() -> TempDir {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(home.path().join(".mdkb")).unwrap();
    home
}

/// `mdkb serve --daemon --detach` returns immediately; the real daemon is
/// a grandchild and survives the initial parent's death.
#[test]
fn detach_survives_parent_exit() {
    let home = prepare_home();

    let output = Command::new(BIN)
        .arg("serve")
        .arg("--daemon")
        .arg("--detach")
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn mdkb serve --daemon --detach");

    assert!(
        output.status.success(),
        "parent must exit 0 after double-fork; got {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // The grandchild should bring up both sockets and write the pid file.
    let ok = wait_until(Duration::from_secs(5), || {
        hook_socket(home.path()).exists()
            && mcp_socket(home.path()).exists()
            && read_pid(home.path()).is_some_and(pid_alive)
    });
    assert!(ok, "detached daemon did not come up within 5s");

    let pid = read_pid(home.path()).expect("pid file");
    // SAFETY: kill is safe; SIGTERM is well-defined.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let _ = wait_until(Duration::from_secs(5), || !pid_alive(pid));
}

/// `mdkb daemon status` → `stop` → ensure sockets and pid file are gone.
/// Then `restart` brings the daemon back.
#[test]
fn status_stop_restart_cycle() {
    let home = prepare_home();

    // Start the daemon (detached so the test doesn't have to wait on it).
    let start = Command::new(BIN)
        .arg("serve")
        .arg("--daemon")
        .arg("--detach")
        .env("HOME", home.path())
        .output()
        .expect("spawn daemon");
    assert!(start.status.success());
    assert!(wait_until(Duration::from_secs(5), || {
        hook_socket(home.path()).exists()
    }));

    // status: should print "running"
    let status = Command::new(BIN)
        .arg("daemon")
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("daemon status");
    let status_text = String::from_utf8_lossy(&status.stdout).into_owned();
    assert!(status.status.success(), "status must exit 0");
    assert!(
        status_text.contains("running"),
        "status should report running: {status_text}"
    );

    let pid_before = read_pid(home.path()).unwrap();

    // stop: should reap sockets and release the lock.
    let stop = Command::new(BIN)
        .arg("daemon")
        .arg("stop")
        .env("HOME", home.path())
        .output()
        .expect("daemon stop");
    assert!(
        stop.status.success(),
        "stop must succeed; stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(!pid_alive(pid_before), "daemon pid should be gone");
    assert!(!hook_socket(home.path()).exists());
    assert!(!mcp_socket(home.path()).exists());

    // restart from a stopped state: fresh pid, sockets back.
    let restart = Command::new(BIN)
        .arg("daemon")
        .arg("restart")
        .env("HOME", home.path())
        .output()
        .expect("daemon restart");
    assert!(
        restart.status.success(),
        "restart must succeed; stderr: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert!(wait_until(Duration::from_secs(5), || {
        hook_socket(home.path()).exists() && mcp_socket(home.path()).exists()
    }));

    let pid_after = read_pid(home.path()).unwrap();
    assert!(pid_alive(pid_after), "fresh daemon should be alive");

    // Clean up.
    unsafe { libc::kill(pid_after as libc::pid_t, libc::SIGTERM) };
    let _ = wait_until(Duration::from_secs(5), || !pid_alive(pid_after));
}
