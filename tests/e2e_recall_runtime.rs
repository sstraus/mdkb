//! Recall through a real daemon, deterministic (no model weights): restart
//! keeps data and answers, and concurrent reads and updates across two
//! repositories served by one daemon never mix their results.
//!
//! The daemon runs under the sandbox `HOME` only; its sockets, pid file and
//! registry never touch the developer's. Queries quote titles, the evidence
//! BM25-only recall admits (see `e2e_recall_multilingual.rs`).

#![cfg(unix)]

#[path = "common/cli.rs"]
mod cli;
#[path = "common/recall.rs"]
mod recall;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use recall::{Model, Sandbox};

/// Stops the sandbox daemon when the test ends, passed or not.
struct Daemon<'a>(&'a Sandbox);

impl<'a> Daemon<'a> {
    fn start(s: &'a Sandbox) -> Self {
        let out = s
            .cmd()
            .args(["serve", "--daemon", "--detach"])
            .output()
            .expect("start daemon");
        assert!(
            out.status.success(),
            "daemon start: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let daemon = Self(s);
        daemon.wait_ready();
        daemon
    }

    fn wait_ready(&self) {
        let sock = self.0.home.join(".mdkb/daemon-hook.sock");
        let start = Instant::now();
        while !sock.exists() {
            // A bound against a daemon that never starts, not a timing assertion.
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "daemon socket never appeared"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A command in `dir` that goes through the daemon.
    fn cmd(&self, dir: &Path) -> Command {
        let mut cmd = self.0.cmd();
        cmd.env_remove("MDKB_NO_DAEMON").current_dir(dir);
        cmd
    }

    fn hook(&self, dir: &Path, prompt: &str) -> String {
        let (context, stderr, code) = recall::hook_with(self.cmd(dir), prompt);
        assert_eq!(code, 0, "{stderr}");
        context
    }
}

impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let _ = self.0.cmd().args(["daemon", "stop"]).output();
    }
}

fn second_repo(s: &Sandbox, name: &str) -> PathBuf {
    let dir = s.home.join(name);
    std::fs::create_dir_all(&dir).expect("second repo");
    let out = s
        .cmd()
        .current_dir(&dir)
        .arg("init")
        .output()
        .expect("init second repo");
    assert!(
        out.status.success(),
        "init: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(
        dir.join(".mdkb/config.toml"),
        "[hooks]\nuser_prompt_submit_deadline_ms = 60000\n",
    )
    .expect("second repo config");
    dir
}

fn write(dir: &Path, rel: &str, title: &str, body: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("doc dir");
    std::fs::write(&path, format!("# {title}\n\n{body}\n")).expect("write doc");
    // `update` skips an mtime in the second of the last index; a later mtime
    // keeps every rewrite visible.
    let later = std::time::SystemTime::now() + Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(later))
        .expect("mtime");
}

// Scenario 9. Catches: a restart that loses entries written through the
// daemon, or answers from a store the old process left half-open.
#[test]
fn a_daemon_restart_keeps_data_and_answers() {
    let s = Sandbox::new(Model::Absent);
    s.write_doc(
        "docs/cache.md",
        "zq_cache_policy notes",
        "Catalog pages are cached for ten minutes.",
    );
    let daemon = Daemon::start(&s);
    let ok = |args: &[&str]| {
        let out = daemon
            .cmd(&s.root)
            .args(args)
            .output()
            .expect("mdkb via daemon");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    ok(&["update"]);
    ok(&[
        "memory",
        "add",
        "zq-restart-note",
        "--title",
        "zq_restart_note value",
        "-c",
        "Kept across restarts.",
    ]);
    let before = (
        daemon.hook(&s.root, "* zq_cache_policy notes"),
        daemon.hook(&s.root, "* zq_restart_note value"),
    );
    assert!(
        before.0.contains("cache.md"),
        "doc recall before restart: {}",
        before.0
    );
    assert!(
        before.1.contains("zq-restart-note"),
        "memory recall before restart: {}",
        before.1
    );

    ok(&["daemon", "restart"]);
    daemon.wait_ready();
    let after = (
        daemon.hook(&s.root, "* zq_cache_policy notes"),
        daemon.hook(&s.root, "* zq_restart_note value"),
    );
    assert_eq!(strip_age(&before.0), strip_age(&after.0));
    assert_eq!(strip_age(&before.1), strip_age(&after.1));
}

/// The injected items, without their relative age ("just now", "1m ago"),
/// which a restart may move, and without the per-session footer.
fn strip_age(context: &str) -> String {
    context
        .lines()
        .filter(|l| l.starts_with("- "))
        .map(|l| l.split(" (").next().unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

// Scenario 10. Catches: one daemon serving two repositories that answers a
// prompt in one with the other's documents, or fails a read while the other
// repository updates.
#[test]
fn concurrent_reads_and_updates_in_two_repositories_never_mix() {
    let s = Sandbox::new(Model::Absent);
    let other = second_repo(&s, "other");
    write(
        &s.root,
        "docs/shared.md",
        "zq_shared_title",
        "This is repo one.",
    );
    write(
        &other,
        "docs/shared-other.md",
        "zq_shared_title",
        "This is repo two.",
    );
    let daemon = Daemon::start(&s);
    for dir in [&s.root, &other] {
        assert!(
            daemon
                .cmd(dir)
                .arg("update")
                .output()
                .expect("update")
                .status
                .success()
        );
    }

    // Readers loop until both writers are done, so every update overlaps reads.
    let writers = std::sync::atomic::AtomicUsize::new(2);
    let reads = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for (dir, rel, body) in [
            (&s.root, "docs/shared.md", "This is repo one, version"),
            (&other, "docs/shared-other.md", "This is repo two, version"),
        ] {
            let (daemon, writers) = (&daemon, &writers);
            scope.spawn(move || {
                for v in 0..3 {
                    write(dir, rel, "zq_shared_title", &format!("{body} {v}."));
                    let out = daemon.cmd(dir).arg("update").output().expect("update");
                    assert!(
                        out.status.success(),
                        "update: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                writers.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            });
        }
        for (dir, mine, theirs) in [
            (&s.root, "shared.md", "shared-other.md"),
            (&other, "shared-other.md", "shared.md"),
        ] {
            let (daemon, writers, reads) = (&daemon, &writers, &reads);
            scope.spawn(move || {
                loop {
                    let context = daemon.hook(dir, "* zq_shared_title");
                    assert!(
                        !context.contains(theirs),
                        "{} got the other repository: {context}",
                        dir.display()
                    );
                    assert!(
                        context.contains(mine),
                        "{} lost its own document: {context}",
                        dir.display()
                    );
                    reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if writers.load(std::sync::atomic::Ordering::SeqCst) == 0 {
                        break;
                    }
                }
            });
        }
    });
    assert!(reads.into_inner() >= 2, "readers must have run");
}
