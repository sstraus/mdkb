//! `mdkb daemon` subcommands: `status`, `stop`, `restart`.
//!
//! These read the pid file populated by `run_daemon` and drive the running
//! daemon via Unix signals. None of them require the daemon to be healthy —
//! a stale pid file with a dead process is reported as "not running".

use std::time::Duration;

use crate::error::{Error, Result};

/// Maximum time `stop` waits for the daemon to exit after SIGTERM.
#[cfg(unix)]
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum time `restart` waits for the fresh daemon's sockets to appear.
#[cfg(unix)]
const RESTART_READY_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(unix)]
mod platform {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime};

    use crate::DaemonConfig;
    use crate::daemon::ipc_server::{HOOK_SOCKET_NAME, MCP_SOCKET_NAME};
    use crate::daemon::singleton::{default_lock_path, read_pid};
    use crate::error::{Error, Result};

    use super::{RESTART_READY_TIMEOUT, STOP_TIMEOUT, format_duration, process_alive, signal_term};

    struct DaemonState {
        lock_path: PathBuf,
        base_dir: PathBuf,
        mcp_sock: PathBuf,
        hook_sock: PathBuf,
        pid: Option<u32>,
        pid_alive: bool,
        uptime: Option<Duration>,
    }

    impl DaemonState {
        fn probe() -> Self {
            let lock_path = default_lock_path();
            let base_dir = lock_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(DaemonConfig::daemon_home);
            let mcp_sock = base_dir.join(MCP_SOCKET_NAME);
            let hook_sock = base_dir.join(HOOK_SOCKET_NAME);
            let pid = read_pid(&lock_path);
            let pid_alive = pid.is_some_and(process_alive);

            let uptime = std::fs::metadata(&lock_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok());

            DaemonState {
                lock_path,
                base_dir,
                mcp_sock,
                hook_sock,
                pid,
                pid_alive,
                uptime,
            }
        }

        fn running_pid(&self) -> Option<u32> {
            if self.pid_alive { self.pid } else { None }
        }
    }

    fn present(p: &Path) -> &'static str {
        if p.exists() { "(present)" } else { "(absent)" }
    }

    pub fn handle_status() -> Result<()> {
        let s = DaemonState::probe();
        if let Some(pid) = s.running_pid() {
            println!("mdkb daemon: running (pid {pid})");
        } else if let Some(pid) = s.pid {
            println!(
                "mdkb daemon: not running (stale pid {pid} in {})",
                s.lock_path.display()
            );
        } else {
            println!("mdkb daemon: not running");
        }
        println!("  base:       {}", s.base_dir.display());
        println!(
            "  mcp  sock:  {} {}",
            s.mcp_sock.display(),
            present(&s.mcp_sock)
        );
        println!(
            "  hook sock:  {} {}",
            s.hook_sock.display(),
            present(&s.hook_sock)
        );
        if let Some(up) = s.uptime {
            if s.running_pid().is_some() {
                println!("  uptime:     {}", format_duration(up));
            }
        }
        Ok(())
    }

    pub async fn handle_stop() -> Result<()> {
        let s = DaemonState::probe();
        let Some(pid) = s.running_pid() else {
            println!("mdkb daemon: not running");
            return Ok(());
        };
        signal_term(pid)?;
        println!("SIGTERM sent to pid {pid}; awaiting shutdown…");

        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if !process_alive(pid) && !s.mcp_sock.exists() && !s.hook_sock.exists() {
                println!("mdkb daemon: stopped");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(Error::other(format!(
            "daemon pid {pid} did not exit within {:?} (sockets still present: mcp={} hook={})",
            STOP_TIMEOUT,
            s.mcp_sock.exists(),
            s.hook_sock.exists()
        )))
    }

    pub async fn handle_restart() -> Result<()> {
        let before = DaemonState::probe();
        if before.running_pid().is_some() {
            handle_stop().await?;
        }

        spawn_daemon_detached()?;

        let deadline = Instant::now() + RESTART_READY_TIMEOUT;
        while Instant::now() < deadline {
            let s = DaemonState::probe();
            if let Some(pid) = s.running_pid() {
                if s.mcp_sock.exists() && s.hook_sock.exists() {
                    println!("mdkb daemon: restarted (pid {pid})");
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(Error::other(format!(
            "daemon did not become ready within {RESTART_READY_TIMEOUT:?}"
        )))
    }

    fn spawn_daemon_detached() -> Result<()> {
        use std::os::unix::process::CommandExt;

        let exe = std::env::current_exe().map_err(|e| Error::other(format!("current_exe: {e}")))?;

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("serve")
            .arg("--daemon")
            .arg("--detach")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);

        cmd.spawn()
            .map(|_| ())
            .map_err(|e| Error::other(format!("spawn mdkb daemon ({}): {e}", exe.display())))
    }
}

#[cfg(unix)]
pub use platform::{handle_restart, handle_status, handle_stop};

#[cfg(not(unix))]
pub fn handle_status() -> Result<()> {
    Err(Error::other("Daemon commands require Unix"))
}

#[cfg(not(unix))]
pub async fn handle_stop() -> Result<()> {
    Err(Error::other("Daemon commands require Unix"))
}

#[cfg(not(unix))]
pub async fn handle_restart() -> Result<()> {
    Err(Error::other("Daemon commands require Unix"))
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86_400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    }
}

/// Unix: `kill(pid, 0)` to probe without delivering a signal. Returns true
/// iff the pid exists and we have permission to signal it.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // PIDs above i32::MAX cannot exist on any Unix platform; treat as dead.
    let Ok(pid_t) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: libc::kill is safe to call; signal 0 does nothing but errno-set.
    let rc = unsafe { libc::kill(pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM means the process exists but we can't signal it — still alive.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    // Daemon is unix-only; on other platforms treat as dead.
    false
}

#[cfg(unix)]
fn signal_term(pid: u32) -> Result<()> {
    let pid_t = libc::pid_t::try_from(pid)
        .map_err(|_| Error::other(format!("pid {pid} overflows libc::pid_t (i32)")))?;
    let rc = unsafe { libc::kill(pid_t, libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(Error::other(format!(
            "kill({pid}, SIGTERM): {}",
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(not(unix))]
fn signal_term(_pid: u32) -> Result<()> {
    Err(Error::other(
        "daemon control not supported on this platform",
    ))
}

/// Double-fork + setsid + stdio redirection. Must be called BEFORE tokio
/// or any other long-lived thread is started — we're about to fork.
///
/// After this returns, the caller is the grandchild: detached from the
/// controlling terminal, session leader of a fresh session, with
/// stdin=/dev/null and stdout/stderr pointing at ~/.mdkb/logs/daemon.log.
/// Parents (original + intermediate) have already `_exit`ed.
#[cfg(unix)]
pub fn detach_current_process() -> Result<()> {
    // First fork: parent exits so the shell returns. The child is still
    // in the same process group.
    match unsafe { libc::fork() } {
        -1 => {
            return Err(Error::other(format!(
                "fork: {}",
                std::io::Error::last_os_error()
            )));
        }
        0 => {}
        _ => {
            // Parent: exit without running destructors (tokio runtime, etc).
            unsafe { libc::_exit(0) };
        }
    }

    // New session — detaches from the controlling terminal.
    if unsafe { libc::setsid() } < 0 {
        return Err(Error::other(format!(
            "setsid: {}",
            std::io::Error::last_os_error()
        )));
    }

    // Second fork: guarantees the final process is not a session leader,
    // so it can never reacquire a controlling terminal.
    match unsafe { libc::fork() } {
        -1 => {
            return Err(Error::other(format!(
                "fork 2: {}",
                std::io::Error::last_os_error()
            )));
        }
        0 => {}
        _ => unsafe { libc::_exit(0) },
    }

    redirect_stdio_to_log()?;
    Ok(())
}

#[cfg(not(unix))]
pub fn detach_current_process() -> Result<()> {
    Err(Error::other("--detach is unix-only"))
}

#[cfg(unix)]
fn redirect_stdio_to_log() -> Result<()> {
    use std::os::fd::AsRawFd;

    let logs_dir = crate::DaemonConfig::daemon_home().join("logs");
    std::fs::create_dir_all(&logs_dir)
        .map_err(|e| Error::other(format!("mkdir {}: {e}", logs_dir.display())))?;
    let log_path = logs_dir.join("daemon.log");

    let devnull = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .map_err(|e| Error::other(format!("open /dev/null: {e}")))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| Error::other(format!("open {}: {e}", log_path.display())))?;

    unsafe {
        if libc::dup2(devnull.as_raw_fd(), 0) < 0
            || libc::dup2(log.as_raw_fd(), 1) < 0
            || libc::dup2(log.as_raw_fd(), 2) < 0
        {
            return Err(Error::other(format!(
                "dup2 stdio: {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(5)), "5s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(125)), "2m5s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3725)), "1h2m");
    }

    #[test]
    fn format_duration_days() {
        assert_eq!(format_duration(Duration::from_secs(90_061)), "1d1h");
    }

    #[test]
    #[cfg(unix)]
    fn process_alive_is_true_for_current_process() {
        assert!(process_alive(std::process::id()));
    }

    #[test]
    fn process_alive_is_false_for_unused_high_pid() {
        // PID 0xFFFF_FFFE is effectively guaranteed not to exist.
        assert!(!process_alive(0xFFFF_FFFE));
    }
}
