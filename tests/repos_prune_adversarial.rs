//! Critic round for story 051-b9c6: `mdkb repos prune [PATH...] [--dry-run]`
//! through the real CLI against a real daemon in an isolated HOME, and the CLI
//! against two daemons it cannot trust (one that predates the method, one that
//! takes the request and never answers).
//!
//! The fake daemons answer with text recorded from `dispatch_hook_message`:
//! `missing 'params.root' (absolute repo path)` with code -32602 is what every
//! released daemon answers for a method it does not know (unchanged since
//! 312a2bff, `git log -S"missing 'params.root'"`).

#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");

struct Home {
    home: TempDir,
    repos: TempDir,
    daemon: Option<Child>,
    /// Directories whose mode this test changed; restored on drop so the
    /// tempdir can be removed.
    locked: Vec<PathBuf>,
}

impl Home {
    fn new() -> Self {
        let home = TempDir::new().unwrap();
        let repos = TempDir::new().unwrap();
        let dot = home.path().join(".mdkb");
        std::fs::create_dir_all(&dot).unwrap();
        let parent = canon(repos.path());
        std::fs::write(
            dot.join("daemon.toml"),
            format!("whitelist_dirs = [{:?}]\n", parent.display().to_string()),
        )
        .unwrap();
        Home {
            home,
            repos,
            daemon: None,
            locked: Vec::new(),
        }
    }

    fn dot(&self) -> PathBuf {
        self.home.path().join(".mdkb")
    }

    fn map(&self) -> PathBuf {
        self.dot().join("repos.json")
    }

    fn hook_socket(&self) -> PathBuf {
        self.dot().join("daemon-hook.sock")
    }

    fn start_daemon(&mut self) {
        let child = Command::new(BIN)
            .args(["serve", "--daemon"])
            .env("HOME", self.home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon");
        self.daemon = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.hook_socket().exists() {
            assert!(Instant::now() < deadline, "daemon socket did not appear");
            sleep(Duration::from_millis(25));
        }
    }

    /// A repo with a store, registered in the live daemon's map by a real
    /// repo call. Returned under the canonical key the map stores.
    fn register(&self, name: &str) -> PathBuf {
        let root = self.repos.path().join(name);
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        // Registry fixtures do not need code indexing. Its asynchronous watcher
        // bootstrap can recreate .mdkb/code.sqlite after the root is deleted,
        // turning the intended Gone fixture into an existing directory.
        std::fs::write(root.join(".mdkb/config.toml"), "[code]\nenabled = false\n").unwrap();
        let root = canon(&root);
        let reply = hook(&self.hook_socket(), "status", json!({"root": root}));
        assert!(reply.get("error").is_none(), "register {name}: {reply}");
        root
    }

    fn mdkb(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("HOME", self.home.path())
            .env("MDKB_NO_SPAWN", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn prune<P: AsRef<Path>>(&self, extra: &[P], dry_run: bool) -> (Output, Value) {
        let mut args = vec!["--format", "json", "repos", "prune"];
        if dry_run {
            args.push("--dry-run");
        }
        let owned: Vec<String> = extra
            .iter()
            .map(|p| p.as_ref().display().to_string())
            .collect();
        args.extend(owned.iter().map(String::as_str));
        let out = self.mdkb(&args);
        let report = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        (out, report)
    }

    /// What the running daemon lists, read the way an operator reads it.
    fn listed(&self) -> BTreeSet<PathBuf> {
        let out = self.mdkb(&["--format", "json", "repos", "list"]);
        assert!(out.status.success(), "{}", stderr(&out));
        let rows: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
        rows.iter()
            .map(|r| PathBuf::from(r["path"].as_str().unwrap()))
            .collect()
    }

    fn lock(&mut self, dir: &Path) {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        self.locked.push(dir.to_path_buf());
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        for dir in &self.locked {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
        }
        if let Some(child) = &mut self.daemon {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn canon(p: &Path) -> PathBuf {
    mdkb::domain::canonicalize_plain(p).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn paths(v: &Value) -> BTreeSet<PathBuf> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|p| PathBuf::from(p.as_str().unwrap()))
                .collect()
        })
        .unwrap_or_default()
}

fn refused_paths(report: &Value) -> BTreeSet<PathBuf> {
    report["refused"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| PathBuf::from(r["path"].as_str().unwrap()))
                .collect()
        })
        .unwrap_or_default()
}

fn map_roots(map: &Path) -> BTreeSet<PathBuf> {
    let file: Value = serde_json::from_slice(&std::fs::read(map).unwrap()).unwrap();
    file["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| PathBuf::from(e["root"].as_str().unwrap()))
        .collect()
}

fn frame(stream: &mut UnixStream, body: &[u8]) {
    stream
        .write_all(&u32::try_from(body.len()).unwrap().to_le_bytes())
        .unwrap();
    stream.write_all(body).unwrap();
}

fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut body).unwrap();
    body
}

fn hook(socket: &Path, method: &str, params: Value) -> Value {
    let mut stream = UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    frame(&mut stream, body.to_string().as_bytes());
    serde_json::from_slice(&read_frame(&mut stream)).unwrap()
}

// ── against the real daemon ─────────────────────────────────────────────────

/// Catches: a named prune that removes every Gone root (or a different
/// spelling's root), a dry run that writes the file or the live map, a prune
/// that deletes a store or rewrites `daemon.toml`, and a pruned root coming
/// back through the daemon's cached handle. A recreated repo must register
/// again: the prune is not a tombstone.
#[test]
fn named_prune_through_the_cli_removes_only_that_gone_root_and_dry_run_writes_nothing() {
    let mut h = Home::new();
    h.start_daemon();
    let keep = h.register("keep");
    let stale_a = h.register("stale-a");
    let stale_b = h.register("stale-b");
    std::fs::remove_dir_all(&stale_a).unwrap();
    std::fs::remove_dir_all(&stale_b).unwrap();
    let toml_before = std::fs::read(h.dot().join("daemon.toml")).unwrap();
    let map_before = std::fs::read(h.map()).unwrap();

    let (dry, report) = h.prune(&[&stale_a], true);
    assert!(dry.status.success(), "{}", stderr(&dry));
    assert_eq!(report["dry_run"], true);
    assert_eq!(paths(&report["removed"]), BTreeSet::from([stale_a.clone()]));
    assert_eq!(std::fs::read(h.map()).unwrap(), map_before, "dry run wrote");
    assert!(
        h.listed().contains(&stale_a),
        "dry run changed the live map"
    );

    let (done, report) = h.prune(&[&stale_a], false);
    assert!(done.status.success(), "{}", stderr(&done));
    assert_eq!(paths(&report["removed"]), BTreeSet::from([stale_a.clone()]));
    let listed = h.listed();
    assert!(!listed.contains(&stale_a));
    assert!(
        listed.contains(&stale_b),
        "an unselected Gone root was pruned"
    );
    assert!(listed.contains(&keep));
    let file = map_roots(&h.map());
    assert!(!file.contains(&stale_a) && file.contains(&stale_b) && file.contains(&keep));
    assert!(
        keep.join(".mdkb/index.sqlite").is_file(),
        "a store was deleted"
    );
    assert_eq!(
        std::fs::read(h.dot().join("daemon.toml")).unwrap(),
        toml_before
    );

    // The cached handle cannot re-record the vanished root.
    let again = hook(&h.hook_socket(), "status", json!({"root": stale_a}));
    assert!(again.get("error").is_some(), "{again}");
    assert!(!h.listed().contains(&stale_a));

    // Not a tombstone: the same repo, back on disk, registers again.
    std::fs::create_dir_all(stale_a.join(".mdkb")).unwrap();
    h.register("stale-a");
    assert!(h.listed().contains(&stale_a));
}

/// Catches: a prune-all that removes healthy or Unreadable roots (an unreadable
/// store is not evidence of deletion) or keeps a NoStore root that load would
/// drop, an explicit selection of kept roots that is obeyed, an unknown or
/// differently spelled path that matches something, and a refusal that still
/// exits 0. Also: a hand-written scope of the pruned root is kept, and pruning
/// a NoStore root touches nothing on disk.
#[test]
fn prune_removes_gone_and_nostore_keeps_unreadable_and_unknown_and_exits_nonzero_on_refusal() {
    let mut h = Home::new();
    h.start_daemon();
    let keep = h.register("keep");
    let nostore = h.register("nostore");
    let locked = h.register("locked");
    let stale = h.register("stale");
    // `.mdkb` as a file: NoStore, and nothing can recreate the directory.
    std::fs::remove_dir_all(nostore.join(".mdkb")).unwrap();
    std::fs::write(nostore.join(".mdkb"), b"").unwrap();
    h.lock(&locked.join(".mdkb"));
    let privileged = std::fs::read_dir(locked.join(".mdkb")).is_ok();
    std::fs::remove_dir_all(&stale).unwrap();
    let mut file: Value = serde_json::from_slice(&std::fs::read(h.map()).unwrap()).unwrap();
    for entry in file["repos"].as_array_mut().unwrap() {
        if entry["root"] == stale.display().to_string() {
            entry["scope"] = "home".into();
        }
    }
    std::fs::write(h.map(), serde_json::to_vec_pretty(&file).unwrap()).unwrap();

    // Explicit selection of roots that are healthy or unreadable, plus an
    // unknown path and a non-canonical spelling of a Gone root: all refused,
    // nothing removed.
    let unknown = h.repos.path().join("never-registered");
    let dotted = stale.join("..").join("stale");
    let mut selection = vec![keep.as_path(), unknown.as_path(), dotted.as_path()];
    if !privileged {
        selection.push(locked.as_path());
    }
    let map_before = std::fs::read(h.map()).unwrap();
    let (out, report) = h.prune(&selection, false);
    assert!(!out.status.success(), "refusals must fail the command");
    assert!(paths(&report["removed"]).is_empty(), "{report}");
    assert_eq!(
        report["refused"].as_array().map(Vec::len),
        Some(selection.len())
    );
    assert!(refused_paths(&report).contains(&keep), "{report}");
    assert_eq!(std::fs::read(h.map()).unwrap(), map_before);

    // Prune-all: the Gone and the NoStore root go.
    assert!(
        !stale.exists(),
        "the daemon recreated the vanished root: {}",
        stale.display()
    );
    assert!(
        h.listed().contains(&stale),
        "the Gone root left the live map before prune-all"
    );
    let (out, report) = h.prune::<&Path>(&[], false);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        paths(&report["removed"]),
        BTreeSet::from([stale.clone(), nostore.clone()]),
        "prune-all report: {report}"
    );
    let listed = h.listed();
    for kept in [&keep, &locked] {
        assert!(listed.contains(kept), "{} was pruned", kept.display());
    }
    assert!(!listed.contains(&nostore), "the NoStore root stayed listed");
    assert!(nostore.join(".mdkb").is_file() && locked.join(".mdkb").exists());
    let after: Value = serde_json::from_slice(&std::fs::read(h.map()).unwrap()).unwrap();
    let scoped = after["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["root"] == stale.display().to_string());
    assert_eq!(
        scoped.map(|e| e["scope"].clone()),
        Some(json!("home")),
        "the hand-written scope was erased: {after}"
    );
}

// ── without a daemon, or with one the CLI cannot trust ──────────────────────

/// A persisted map that names one existing and one vanished root.
fn offline_map(h: &Home) -> (PathBuf, Vec<u8>) {
    let live = h.repos.path().join("live");
    std::fs::create_dir_all(live.join(".mdkb")).unwrap();
    let gone = canon(h.repos.path()).join("gone");
    let body = json!({"version": 2, "repos": [
        {"root": canon(&live).display().to_string(), "last_opened_at": 0},
        {"root": gone.display().to_string(), "last_opened_at": 0},
    ]})
    .to_string()
    .into_bytes();
    std::fs::write(h.map(), &body).unwrap();
    (gone, body)
}

/// Catches: an offline fallback that edits `repos.json` behind the daemon's
/// back, a stale pid file read as a live daemon, a prune that starts a daemon,
/// and a failure without the next step.
#[test]
fn without_a_running_daemon_prune_fails_with_the_next_step_and_touches_nothing() {
    let h = Home::new();
    let (gone, before) = offline_map(&h);
    for stale_pid in [None, Some("4294967294\n")] {
        if let Some(pid) = stale_pid {
            std::fs::write(h.dot().join("daemon.pid"), pid).unwrap();
        }
        for dry_run in [false, true] {
            let (out, _) = h.prune(&[&gone], dry_run);
            assert!(!out.status.success());
            let err = stderr(&out);
            assert!(err.contains("no mdkb daemon is running"), "{err}");
            assert!(err.contains("daemon restart"), "{err}");
            assert_eq!(std::fs::read(h.map()).unwrap(), before);
            assert!(!h.hook_socket().exists(), "prune started a daemon");
        }
    }
}

/// A one-connection hook socket that records the request and answers with
/// `reply` (or hangs up after reading when `None`). `daemon.pid` names this
/// test process, which is alive, so the CLI probe sees a running daemon.
fn fake_daemon(h: &Home, reply: Option<Value>) -> std::thread::JoinHandle<Vec<Value>> {
    std::fs::write(h.dot().join("daemon.pid"), std::process::id().to_string()).unwrap();
    let listener = UnixListener::bind(h.hook_socket()).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        // Wait for the first request, then a short grace for a second one (a
        // retry or a fallback method) before reporting what arrived.
        let mut until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    seen.push(serde_json::from_slice(&read_frame(&mut stream)).unwrap());
                    if let Some(reply) = &reply {
                        frame(&mut stream, reply.to_string().as_bytes());
                    }
                    until = Instant::now() + Duration::from_millis(500);
                }
                Err(_) => sleep(Duration::from_millis(20)),
            }
        }
        seen
    })
}

/// Catches: a refusal from a daemon built before `daemon.repos_prune` read as a
/// generic failure (no next step), retried as another method, or followed by an
/// offline edit of `repos.json`.
#[test]
fn a_daemon_that_predates_the_method_gets_one_request_and_an_actionable_error() {
    let h = Home::new();
    let (gone, before) = offline_map(&h);
    let server = fake_daemon(
        &h,
        Some(json!({"jsonrpc": "2.0", "id": 1, "error": {
            "code": -32602, "message": "missing 'params.root' (absolute repo path)"}})),
    );

    let (out, _) = h.prune(&[&gone], false);
    let seen = server.join().unwrap();

    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("predates") && err.contains("daemon restart"),
        "{err}"
    );
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["method"], "daemon.repos_prune");
    assert_eq!(seen[0]["params"]["paths"], json!([gone]));
    assert_eq!(std::fs::read(h.map()).unwrap(), before);
}

/// Catches: a request the daemon received and never answered reported as
/// "did not prune". The daemon may have pruned (the RPC was delivered, the
/// client lost the answer), so the message must say the outcome is
/// unconfirmed and name how to inspect it, and the CLI must not retry, fall
/// back to another method, or edit `repos.json` itself.
#[test]
fn a_delivered_request_without_an_answer_is_reported_as_unconfirmed() {
    let h = Home::new();
    let (gone, before) = offline_map(&h);
    let server = fake_daemon(&h, None);

    let (out, _) = h.prune(&[&gone], false);
    let seen = server.join().unwrap();

    assert_eq!(seen.len(), 1, "retried or fell back: {seen:?}");
    assert_eq!(seen[0]["method"], "daemon.repos_prune");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        !err.contains("did not prune"),
        "outcome is unknown, not negative: {err}"
    );
    assert!(err.contains("unconfirmed"), "{err}");
    assert!(err.contains("mdkb repos list"), "{err}");
    assert_eq!(std::fs::read(h.map()).unwrap(), before, "no offline edit");
}

/// Catches the documented partial-apply contract breaking either way: a
/// mixed selection that removes nothing because one path is refused, one that
/// removes the refused Healthy root or an unselected Gone root, or one that
/// exits 0 while it refused a path.
#[test]
fn a_mixed_selection_removes_only_its_gone_root_reports_the_refusal_and_exits_nonzero() {
    let mut h = Home::new();
    h.start_daemon();
    let healthy = h.register("healthy");
    let selected = h.register("selected");
    let unselected = h.register("unselected");
    std::fs::remove_dir_all(&selected).unwrap();
    std::fs::remove_dir_all(&unselected).unwrap();

    let (out, report) = h.prune(&[&selected, &healthy], false);

    assert!(!out.status.success(), "a refusal must fail the command");
    assert_eq!(report["dry_run"], false);
    assert_eq!(
        paths(&report["removed"]),
        BTreeSet::from([selected.clone()])
    );
    assert_eq!(refused_paths(&report), BTreeSet::from([healthy.clone()]));
    let listed = h.listed();
    assert!(
        !listed.contains(&selected),
        "the Gone selection was not applied"
    );
    assert!(listed.contains(&healthy) && listed.contains(&unselected));
    let file = map_roots(&h.map());
    assert!(!file.contains(&selected));
    assert!(file.contains(&healthy) && file.contains(&unselected));
    assert!(healthy.join(".mdkb").is_dir());
}
