//! Sandbox for the bilingual recall end-to-end suites.
//!
//! Each [`Sandbox`] owns a temporary `HOME`, a project inside it and a model
//! cache of its own. Nothing reads or writes the developer's store, hooks,
//! daemon or global config. The model cache is either empty (deterministic
//! suites: inference cannot run, so behavior does not depend on weights) or a
//! hard-linked copy of the production model snapshot (acceptance suites).
//! No test can download: fastembed ignores `HF_HUB_OFFLINE` but reads
//! `HF_ENDPOINT`, which points at a closed local port, so a fetch of missing
//! weights fails at once instead of reaching the network.
//!
//! Included with `#[path = "common/recall.rs"] mod recall;` next to `cli.rs`.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

use crate::cli;

/// hf-hub cache folder of the production embedding model.
const MODEL_DIR: &str = "models--Qdrant--all-MiniLM-L6-v2-onnx";
/// hf-hub cache folder of the multilingual Granite embedding model.
const GRANITE_DIR: &str = "models--ibm-granite--granite-embedding-107m-multilingual";
/// Granite loads from its pinned snapshot, which has no `refs/main`.
const GRANITE_REVISION: &str = "d6cffd338414d6a1c1f5decfad5fec62eebc90d5";
const DEADLINE_KEY: &str = "user_prompt_submit_deadline_ms";

pub enum Model {
    /// Empty cache: every embedding call fails, the store runs on BM25.
    Absent,
    /// The production weights, linked from the cache named by
    /// `MDKB_TEST_MODEL_CACHE` or the default fastembed cache. Panics when
    /// they are missing: an acceptance run without inference is not a pass.
    Production,
    /// The Granite weights, linked like [`Model::Production`], with
    /// `embedding.model = "granite107m"` selected in the sandbox config.
    Granite,
}

/// Fail at once, naming the prerequisite, when the machine lacks the production
/// MiniLM weights that [`cli::command`] points every spawned binary at. Suites
/// that assert real inference (vectors written, semantic hits ranked) cannot
/// pass without them, and the failure otherwise surfaces as "0 vectors" or an
/// unrelated assertion. Never downloads.
pub fn require_production_model() {
    let dir = cli::model_cache_dir().join(MODEL_DIR);
    assert!(
        mdkb::llm::embeddings::model_cached_at(&dir),
        "this test needs the complete pinned production model snapshot in {}; fetch it with \
         `mdkb embed` or set FASTEMBED_CACHE_DIR to a cache that holds it",
        dir.display()
    );
}

pub struct Sandbox {
    _tmp: tempfile::TempDir,
    pub home: PathBuf,
    pub root: PathBuf,
    pub cache: PathBuf,
}

impl Sandbox {
    pub fn new(model: Model) -> Self {
        let tmp = tempfile::tempdir().expect("sandbox tempdir");
        let home = tmp.path().join("home");
        let root = home.join("repo");
        let cache = tmp.path().join("models");
        std::fs::create_dir_all(&root).expect("sandbox repo");
        std::fs::create_dir_all(home.join(".mdkb")).expect("sandbox daemon home");
        std::fs::create_dir_all(&cache).expect("sandbox model cache");
        match model {
            Model::Absent => {}
            Model::Production => link_model(&cache, MODEL_DIR, None),
            Model::Granite => link_model(&cache, GRANITE_DIR, Some(GRANITE_REVISION)),
        }
        let sandbox = Self {
            _tmp: tmp,
            home,
            root,
            cache,
        };
        sandbox.ok(&["init"]);
        if let Model::Granite = model {
            sandbox.append_config("[embedding]\nmodel = \"granite107m\"\n");
        }
        // Deterministic suites assert what recall returns, not how fast: a
        // loaded host can exceed the 1 s production deadline on a cold store.
        sandbox.append_config(&format!("[hooks]\n{DEADLINE_KEY} = 60000\n"));
        sandbox
    }

    /// Replace the hook deadline written by [`Sandbox::new`].
    pub fn set_hook_deadline(&self, ms: u64) {
        let path = self.root.join(".mdkb/config.toml");
        let cfg = std::fs::read_to_string(&path).expect("read sandbox config");
        let cfg = cfg.replace(
            &format!("{DEADLINE_KEY} = 60000"),
            &format!("{DEADLINE_KEY} = {ms}"),
        );
        std::fs::write(path, cfg).expect("write sandbox config");
    }

    /// A hermetic `mdkb` in this sandbox, never routed to a daemon.
    pub fn cmd(&self) -> Command {
        let mut cmd = cli::command();
        cmd.env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("FASTEMBED_CACHE_DIR", &self.cache)
            .env_remove("HF_HOME")
            .env("HF_ENDPOINT", "http://127.0.0.1:9")
            .current_dir(&self.root);
        cmd
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("spawn mdkb")
    }

    /// Run and require exit 0; returns stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "`mdkb {}` failed: {}\nstdout: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    pub fn append_config(&self, toml: &str) {
        let path = self.root.join(".mdkb/config.toml");
        let mut cfg = std::fs::read_to_string(&path).unwrap_or_default();
        cfg.push('\n');
        cfg.push_str(toml);
        std::fs::write(path, cfg).expect("write sandbox config");
    }

    pub fn write_doc(&self, rel: &str, title: &str, body: &str) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().expect("doc parent")).expect("doc dir");
        std::fs::write(path, format!("# {title}\n\n{body}\n")).expect("write doc");
    }

    pub fn add_memory(&self, id: &str, title: &str, content: &str, kind: &str, extra: &[&str]) {
        let mut args = vec![
            "memory", "add", id, "--title", title, "-c", content, "-T", kind,
        ];
        args.extend_from_slice(extra);
        self.ok(&args);
    }

    /// `search --format json`, as `(id-or-path, title, status)` per hit.
    pub fn search(&self, query: &str, scope: &str) -> Vec<(String, String, String)> {
        let out = self.ok(&["--format", "json", "search", query, "--scope", scope]);
        let v: Value =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("search json: {e}: {out}"));
        let hits = v.get("hits").cloned().unwrap_or(v);
        hits.as_array()
            .expect("hits array")
            .iter()
            .map(|h| {
                let h = h.get("value").unwrap_or(h);
                let key = h
                    .get("path")
                    .or_else(|| h.get("id"))
                    .map(text)
                    .unwrap_or_default();
                let status = h.get("status").map(text).unwrap_or_default();
                (key, h.get("title").map(text).unwrap_or_default(), status)
            })
            .collect()
    }

    /// The `UserPromptSubmit` hook; returns (injected context, stderr, exit code).
    pub fn hook(&self, prompt: &str) -> (String, String, i32) {
        hook_with(self.cmd(), prompt)
    }

    /// A count from the store, read-only.
    pub fn count(&self, sql: &str) -> i64 {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
        let conn =
            rusqlite::Connection::open_with_flags(self.root.join(".mdkb/index.sqlite"), flags)
                .expect("open sandbox store");
        conn.query_row(sql, [], |r| r.get(0)).expect("count query")
    }

    /// A stdio MCP server in this sandbox.
    pub fn mcp(&self) -> Mcp {
        let mut cmd = self.cmd();
        cmd.arg("serve").stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn mdkb serve");
        let stdout = BufReader::new(child.stdout.take().expect("serve stdout"));
        let mut mcp = Mcp {
            child,
            stdout,
            next: 1,
        };
        mcp.request(
            "initialize",
            json!({"protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "recall-e2e", "version": "1"}}),
        );
        mcp.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        // The server answers "initializing" until its startup reindex ends.
        // The bound only stops a hung server; it does not time anything.
        let start = std::time::Instant::now();
        while mcp
            .request("tools/call", json!({"name": "status", "arguments": {}}))
            .get("error")
            .is_some()
        {
            assert!(
                start.elapsed().as_secs() < 120,
                "MCP server never became ready"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        mcp
    }
}

fn text(v: &Value) -> String {
    v.as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.to_string())
}

/// One hook call as its own session: recall deduplicates entries and repeated
/// prompts within a session, which would turn a repeated check into silence.
pub fn hook_with(mut cmd: Command, prompt: &str) -> (String, String, i32) {
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let session = format!(
        "recall-e2e-{}-{}",
        std::process::id(),
        CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut child = cmd
        .args(["hook", "user-prompt-submit"])
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    let payload = json!({"prompt": prompt, "session_id": session}).to_string();
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(payload.as_bytes())
        .expect("hook payload");
    let out = child.wait_with_output().expect("hook output");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let context = serde_json::from_str::<Value>(stdout.trim())
        .ok()
        .and_then(|v| v.pointer("/hookSpecificOutput/additionalContext").map(text))
        .unwrap_or_default();
    (
        context,
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

pub struct Mcp {
    child: std::process::Child,
    stdout: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Mcp {
    fn send(&mut self, msg: Value) {
        let stdin = self.child.stdin.as_mut().expect("serve stdin");
        writeln!(stdin, "{msg}").expect("write to serve");
        stdin.flush().expect("flush serve");
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                self.stdout.read_line(&mut line).expect("read serve"),
                0,
                "serve closed stdout"
            );
            let v: Value =
                serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad JSON-RPC {e}: {line}"));
            if v["id"] == id {
                return v;
            }
        }
    }

    /// `tools/call` and the text of its first content item.
    pub fn tool(&mut self, name: &str, args: Value) -> String {
        let v = self.request("tools/call", json!({"name": name, "arguments": args}));
        v.pointer("/result/content/0/text")
            .map(text)
            .unwrap_or_else(|| panic!("{name} failed: {v}"))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Hard-link (or copy, across file systems) a model snapshot into
/// `cache`. hf-hub resolves `refs/main` to the snapshot directory, so the
/// links and the ref are all fastembed needs to load without a network.
fn link_model(cache: &Path, model_dir: &str, pinned: Option<&str>) {
    let source = std::env::var_os("MDKB_TEST_MODEL_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(cli::model_cache_dir)
        .join(model_dir);
    let rev = pinned
        .map(str::to_string)
        .map_or_else(|| std::fs::read_to_string(source.join("refs/main")), Ok)
        .unwrap_or_else(|_| {
            panic!(
                "acceptance suite needs the production model at {}; fetch it with `mdkb embed` \
             or point MDKB_TEST_MODEL_CACHE at a cache that holds it",
                source.display()
            )
        });
    let rev = rev.trim();
    let snapshot = source.join("snapshots").join(rev);
    let target = cache.join(model_dir);
    std::fs::create_dir_all(target.join("refs")).expect("model refs dir");
    std::fs::write(target.join("refs/main"), rev).expect("model ref");
    let dest = target.join("snapshots").join(rev);
    std::fs::create_dir_all(&dest).expect("model snapshot dir");
    for entry in std::fs::read_dir(&snapshot)
        .expect("read model snapshot")
        .flatten()
    {
        let real = entry.path().canonicalize().expect("resolve model file");
        let to = dest.join(entry.file_name());
        if std::fs::hard_link(&real, &to).is_err() {
            std::fs::copy(&real, &to).expect("copy model file");
        }
    }
}

/// One logical fixture document, both languages (from the frozen benchmark
/// fixtures, `assets/eval/mlbench/controlled`).
pub struct FixtureDoc {
    pub id: String,
    pub kind: String,
    pub entry_type: String,
    pub en: (String, String),
    pub it: (String, String),
}

pub struct FixtureIntent {
    pub id: String,
    pub target: String,
    /// `doc` or `memory`: where the grade-3 target is stored.
    pub target_kind: String,
    pub en: String,
    pub it: String,
}

/// Documents and intents of the named families.
pub fn fixture(families: &[&str]) -> (Vec<FixtureDoc>, Vec<FixtureIntent>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/eval/mlbench/controlled");
    let (mut docs, mut intents) = (Vec::new(), Vec::new());
    for part in ["a", "b", "c", "d"] {
        let raw = std::fs::read_to_string(dir.join(format!("families-{part}.json")))
            .expect("fixture file");
        let all: Vec<Value> = serde_json::from_str(&raw).expect("fixture json");
        for fam in all
            .iter()
            .filter(|f| families.contains(&f["family"].as_str().unwrap_or("")))
        {
            for d in fam["docs"].as_array().expect("docs") {
                let pair = |l: &str| (text(&d[l][0]), text(&d[l][1]));
                docs.push(FixtureDoc {
                    id: text(&d["id"]),
                    kind: text(&d["kind"]),
                    entry_type: text(&d["type"]),
                    en: pair("en"),
                    it: pair("it"),
                });
            }
            for i in fam["intents"].as_array().expect("intents") {
                let target = i["rel"]
                    .as_object()
                    .expect("rel")
                    .iter()
                    .find(|(_, g)| g.as_i64() == Some(3))
                    .map(|(k, _)| k.clone())
                    .expect("grade-3 target");
                let target_kind = docs
                    .iter()
                    .find(|d: &&FixtureDoc| d.id == target)
                    .map(|d| d.kind.clone())
                    .expect("target is a fixture document");
                intents.push(FixtureIntent {
                    id: text(&i["id"]),
                    target,
                    target_kind,
                    en: text(&i["en"]),
                    it: text(&i["it"]),
                });
            }
        }
    }
    assert!(!docs.is_empty(), "no fixture families matched {families:?}");
    (docs, intents)
}
