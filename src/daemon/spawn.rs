//! Auto-spawn the mdkb daemon and wait for its socket to become reachable.
//!
//! Used by `mdkb mcp` (and the hook client in story 015) so end-users don't
//! have to manage the daemon lifecycle manually. A second spawn while one is
//! already running is a no-op: the singleton lock in `daemon::singleton`
//! makes the duplicate exit cleanly, and we just keep polling the socket.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::sleep;

use crate::error::{Error, Result};

/// Backoff schedule for the connect retry loop after spawning the daemon.
/// Total wait ≈ 1.55 s before giving up.
const BACKOFF_MS: &[u64] = &[50, 100, 200, 400, 800];

/// Ensure a daemon is running and its MCP socket is reachable.
///
/// 1. Try to connect to `socket_path`. On success, return immediately.
/// 2. Spawn `mdkb serve --daemon` detached. If another daemon raced us
///    and won the singleton lock, the duplicate exits cleanly — we still
///    poll for the existing socket.
/// 3. Retry connect with exponential backoff. Bail out on the final attempt.
pub async fn ensure_daemon_running(socket_path: &Path) -> Result<()> {
    if try_connect(socket_path).await.is_ok() {
        return Ok(());
    }

    // `MDKB_NO_SPAWN=1` forbids forking a daemon: report unreachable at once
    // instead of paying the full backoff waiting for one that will never come.
    // For a sandbox or CI runner that must not leave a background process
    // behind, and for exercising the caller's no-daemon path deterministically.
    // Distinct from `MDKB_NO_DAEMON`, which bypasses the daemon entirely and
    // runs in-process.
    if std::env::var_os("MDKB_NO_SPAWN").is_some() {
        return Err(Error::other(format!(
            "no daemon at {} and MDKB_NO_SPAWN is set",
            socket_path.display()
        )));
    }

    spawn_daemon_detached()?;

    for &delay in BACKOFF_MS {
        sleep(Duration::from_millis(delay)).await;
        if try_connect(socket_path).await.is_ok() {
            return Ok(());
        }
    }

    Err(Error::other(format!(
        "mdkb daemon did not become reachable at {} within {}ms",
        socket_path.display(),
        BACKOFF_MS.iter().sum::<u64>()
    )))
}

/// Probe the socket with a fresh connection. The connection is dropped
/// immediately — we only care that the listener accepted us.
async fn try_connect(socket_path: &Path) -> std::io::Result<()> {
    let _ = UnixStream::connect(socket_path).await?;
    Ok(())
}

/// Spawn `mdkb serve --daemon` as a detached background process.
///
/// stdio is nulled and the child is placed in its own process group so
/// killing the parent (e.g. Claude tearing down the MCP transport) does
/// not propagate SIGHUP/SIGINT to the daemon.
fn spawn_daemon_detached() -> Result<()> {
    let exe = current_exe()?;

    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("serve")
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    cmd.process_group(0);

    cmd.spawn()
        .map(|_| ())
        .map_err(|e| Error::other(format!("spawn mdkb daemon ({}): {e}", exe.display())))
}

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().map_err(|e| Error::other(format!("current_exe: {e}")))
}
