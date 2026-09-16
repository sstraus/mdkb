//! Shared helpers for spawning the `mdkb` binary in CLI integration tests.
//!
//! Included per-suite via `#[path = "common/cli.rs"] mod cli;` rather than
//! nested under `tests/common/mod.rs`, since each integration test file
//! compiles as its own crate and only some suites need every helper here
//! (e.g. `bin()` is only called directly by suites with their own
//! env/stdin variants). `#![allow(dead_code)]` covers the helpers a given
//! suite doesn't reference.
//!
//! Every process spawned through [`command`] is hermetic (story 067-5ab6).
//! Before that, a suite of 72 smoke tests sent 78 requests to the socket at
//! `$HOME/.mdkb/daemon-hook.sock` — the developer's real daemon — and spawned
//! one when none was running. The binary sees a throwaway `HOME`, so the
//! daemon socket, pid file and `daemon.toml` it looks for do not exist, and
//! `MDKB_NO_DAEMON` plus `MDKB_NO_SPAWN` keep it from starting one. A test that
//! needs a daemon starts its own under its own `HOME` and says so.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::LazyLock;

pub fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mdkb"))
}

/// One throwaway `HOME` per test process. Nothing under it is shared with the
/// developer's account: the daemon home `~/.mdkb` resolves inside it.
static ISOLATED_HOME: LazyLock<tempfile::TempDir> =
    LazyLock::new(|| tempfile::tempdir().expect("isolated HOME tempdir"));

pub fn isolated_home() -> &'static Path {
    ISOLATED_HOME.path()
}

/// Where the spawned binary looks for embedding model weights.
///
/// The isolated `HOME` has no `.cache/fastembed`, and a binary that cannot find
/// the model downloads it. The model cache is read-only data, not daemon state,
/// so the spawned binary is pointed at the same cache this test process would
/// use — behaviour on a given machine is unchanged, and an in-process caller
/// and a spawned one see the same model availability.
pub fn model_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("FASTEMBED_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    // `daemon::config::home_dir` owns how a home directory is resolved, per
    // platform. Asking it keeps the spawned binary and this helper agreeing on
    // where the cache is, which is the whole point of pointing one at the other.
    let home = mdkb::daemon::config::home_dir().expect("locate the model cache");
    home.join(".cache/fastembed")
}

/// A hermetic `mdkb` invocation. Callers add arguments, a working directory
/// and any per-test environment on top; a later `.env()` on the same key wins,
/// so a test that wants a daemon route removes `MDKB_NO_DAEMON` explicitly.
pub fn command() -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", isolated_home())
        .env("MDKB_NO_DAEMON", "1")
        .env("MDKB_NO_SPAWN", "1")
        .env("FASTEMBED_CACHE_DIR", model_cache_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

pub fn run(args: &[&str], cwd: &Path) -> Output {
    command()
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("spawn failed for `mdkb {}`: {e}", args.join(" ")))
}
