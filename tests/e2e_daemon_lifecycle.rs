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
    std::fs::create_dir_all(home.path().join("repo")).unwrap();
    home
}

/// The binary under an isolated `HOME`, run from a scratch repo inside it.
///
/// The store is per repo, not per `HOME`: a process started from the
/// developer checkout indexes and mutates that checkout's live `.mdkb/`
/// next to the developer's daemon, which is how the live index went corrupt
/// on 2026-09-12. `serve` creates a fresh store in an empty directory.
fn mdkb(home: &TempDir) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env("HOME", home.path())
        .env("MDKB_NO_DAEMON", "1")
        .current_dir(home.path().join("repo"));
    cmd
}

/// `mdkb serve --daemon --detach` returns immediately; the real daemon is
/// a grandchild and survives the initial parent's death.
#[test]
fn detach_survives_parent_exit() {
    let home = prepare_home();

    let output = mdkb(&home)
        .arg("serve")
        .arg("--daemon")
        .arg("--detach")
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

/// Catches: `daemon status` guessing a repo list from the disk, or spawning a
/// daemon, when none runs. With no daemon it must say so, list no repos and
/// leave no socket or pid behind.
#[test]
fn status_without_a_daemon_says_so_and_starts_nothing() {
    let home = prepare_home();

    let out = mdkb(&home)
        .arg("daemon")
        .arg("status")
        .output()
        .expect("daemon status");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    assert!(out.status.success(), "status must exit 0: {text}");
    assert!(text.contains("not running"), "{text}");
    assert!(text.contains("repos:      unknown"), "{text}");
    assert!(
        !text.contains("[known]") && !text.contains("[discovered]"),
        "{text}"
    );
    assert!(!hook_socket(home.path()).exists(), "status must not spawn");
    assert!(read_pid(home.path()).is_none(), "status must not spawn");
}

/// Catches: `daemon status` skipping the daemon query against a running
/// daemon: the socket answer must be reported, not "unknown" or an error.
#[test]
fn status_lists_the_repos_the_running_daemon_holds() {
    let home = prepare_home();
    let start = mdkb(&home)
        .args(["serve", "--daemon", "--detach"])
        .output()
        .expect("spawn daemon");
    assert!(start.status.success());
    assert!(wait_until(Duration::from_secs(5), || {
        hook_socket(home.path()).exists()
    }));

    let out = mdkb(&home).args(["daemon", "status"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    let pid = read_pid(home.path()).expect("pid file");
    // SAFETY: kill is safe; SIGTERM is well-defined.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let _ = wait_until(Duration::from_secs(5), || !pid_alive(pid));

    assert!(text.contains("running (pid"), "{text}");
    assert!(text.contains("repos:      "), "{text}");
    assert!(
        !text.contains("unavailable"),
        "the query must succeed: {text}"
    );
    assert!(!text.contains("unknown"), "{text}");
}

/// `mdkb daemon status` → `stop` → ensure sockets and pid file are gone.
/// Then `restart` brings the daemon back.
#[test]
fn status_stop_restart_cycle() {
    let home = prepare_home();

    // Start the daemon (detached so the test doesn't have to wait on it).
    let start = mdkb(&home)
        .arg("serve")
        .arg("--daemon")
        .arg("--detach")
        .output()
        .expect("spawn daemon");
    assert!(start.status.success());
    assert!(wait_until(Duration::from_secs(5), || {
        hook_socket(home.path()).exists()
    }));

    // status: should print "running"
    let status = mdkb(&home)
        .arg("daemon")
        .arg("status")
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
    let stop = mdkb(&home)
        .arg("daemon")
        .arg("stop")
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
    let restart = mdkb(&home)
        .arg("daemon")
        .arg("restart")
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

/// `mdkb serve --http` exits 0 within 5s of SIGTERM.
///
/// The HTTP transport used to watch only Ctrl-C (SIGINT), ignoring SIGTERM
/// entirely — the signal `docker stop` and systemd both send. It now shares
/// `mdkb::mcp::wait_for_shutdown_signal` with the daemon (see `run_daemon` in
/// `main.rs`), which watches both.
#[cfg(feature = "http-server")]
#[test]
fn http_server_exits_on_sigterm() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);

    let home = prepare_home();
    let mut child = mdkb(&home)
        .arg("serve")
        .arg("--http")
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--allow-no-auth")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mdkb serve --http");

    // Startup latency is not what this test measures — a debug build cold-starting
    // on a loaded machine has been seen to need more than 5s, which made the test
    // flake. Only the SIGTERM response below is held to a tight deadline.
    let up = wait_until(Duration::from_secs(30), || {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert!(
        up,
        "http server did not start listening on 127.0.0.1:{port} within 30s"
    );

    let pid = child.id();
    // SAFETY: kill is safe; SIGTERM is well-defined.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("http server did not exit within 5s of SIGTERM");
        }
        sleep(Duration::from_millis(50));
    };

    assert!(
        status.success(),
        "http server should exit 0 on SIGTERM; got {status:?}"
    );
}
