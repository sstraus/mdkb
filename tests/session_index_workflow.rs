//! Real CLI -> transcript index -> search/get -> lifecycle checks.
//!
//! The JSONL input is a synthetic transcript using the same user/assistant
//! fields as the parser fixtures. It does not validate Claude Code's format.

#[path = "common/cli.rs"]
mod cli;

use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Output;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    sessions_base: PathBuf,
    session_dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        let sessions_base = home.join(".claude/projects");
        let session_dir = sessions_base.join(mdkb::domain::sessions::encode_project_path(
            root.to_str().unwrap(),
        ));
        fs::create_dir_all(&session_dir).unwrap();
        let f = Self {
            _dir: dir,
            root,
            home,
            sessions_base,
            session_dir,
        };
        f.ok(&["init"]);
        f
    }

    fn call(&self, args: &[&str]) -> Output {
        cli::command()
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.call(args);
        assert!(
            out.status.success(),
            "mdkb {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn index(&self) -> Value {
        serde_json::from_str(&self.ok(&[
            "--format",
            "json",
            "session",
            "index",
            "--sessions-path",
            self.sessions_base.to_str().unwrap(),
            "--project-root",
            self.root.to_str().unwrap(),
        ]))
        .unwrap()
    }

    fn transcript(&self, name: &str, markers: &[&str]) {
        let path = self.session_dir.join(format!("{name}.jsonl"));
        let mut content = String::new();
        for marker in markers {
            content.push_str(
                &json!({"type":"user", "sessionId":name,
                "message":{"role":"user","content":marker}})
                .to_string(),
            );
            content.push('\n');
            content.push_str(
                &json!({"type":"assistant", "sessionId":name,
                "message":{"role":"assistant","content":[{"type":"text","text":"Acknowledged."}]}})
                .to_string(),
            );
            content.push('\n');
        }
        fs::write(path, content).unwrap();
    }

    fn status(&self, name: &str) -> String {
        let conn = rusqlite::Connection::open(self.root.join(".mdkb/index.sqlite")).unwrap();
        conn.query_row(
            "SELECT status FROM documents WHERE collection='claude_sessions' AND relative_path=?1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }
}

/// Catches: the CLI reports indexed sessions that search/get cannot retrieve,
/// or an append reindexes old content instead of exposing the new turn.
#[test]
fn session_index_round_trips_search_get_append_and_archive() {
    let f = Fixture::new();
    f.transcript(
        "story",
        &["orbitalquartzalpha", "second turn", "third turn"],
    );
    assert_eq!(f.index()["added"], 1);
    assert!(
        f.ok(&[
            "search",
            "orbitalquartzalpha",
            "--collection",
            "claude_sessions"
        ])
        .contains("story")
    );
    assert!(
        f.ok(&["get", "claude_sessions/story"])
            .contains("orbitalquartzalpha")
    );
    assert_eq!(f.index()["unchanged"], 1);

    f.transcript(
        "story",
        &[
            "orbitalquartzalpha",
            "second turn",
            "third turn",
            "orbitalquartzbeta",
        ],
    );
    let updated = f.index();
    assert!(updated["updated"].as_u64().unwrap() + updated["added"].as_u64().unwrap() > 0);
    assert!(
        f.ok(&[
            "search",
            "orbitalquartzbeta",
            "--collection",
            "claude_sessions"
        ])
        .contains("story")
    );
    assert!(
        f.ok(&["get", "claude_sessions/story"])
            .contains("orbitalquartzbeta")
    );

    fs::remove_file(f.session_dir.join("story.jsonl")).unwrap();
    assert_eq!(f.index()["sessions_archived"], 1);
    assert_eq!(f.status("story"), "archived");
    assert!(
        f.ok(&["get", "claude_sessions/story"])
            .contains("orbitalquartzbeta")
    );

    // Catches: content-hash dedup leaves a restored source archived forever.
    f.transcript(
        "story",
        &[
            "orbitalquartzalpha",
            "second turn",
            "third turn",
            "orbitalquartzbeta",
        ],
    );
    f.index();
    assert_eq!(
        f.status("story"),
        "current",
        "a restored source must be live again"
    );
}

/// Catches: a temporarily invalid transcript silently archives its last good
/// snapshot, including when another transcript in the same pass is healthy.
#[test]
fn invalid_session_preserves_both_indexed_snapshots_until_a_complete_pass() {
    let f = Fixture::new();
    f.transcript(
        "first",
        &["firstuniquealpha", "firstuniqueone", "firstuniquetwo"],
    );
    f.transcript(
        "second",
        &["seconduniquealpha", "seconduniqueone", "seconduniquetwo"],
    );
    assert_eq!(f.index()["added"], 2);
    fs::write(
        f.session_dir.join("first.jsonl"),
        "{\"type\":\"user\",\"message\":",
    )
    .unwrap();
    let result = f.index();
    assert!(
        result["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().contains("first.jsonl")),
        "{result}"
    );
    assert_eq!(
        f.status("first"),
        "current",
        "an unreadable source is not a deletion"
    );
    assert_eq!(f.status("second"), "current");
    assert!(
        f.ok(&["get", "claude_sessions/first"])
            .contains("firstuniquealpha")
    );
    assert!(
        f.ok(&[
            "search",
            "firstuniquealpha",
            "--collection",
            "claude_sessions"
        ])
        .contains("first")
    );

    f.transcript(
        "first",
        &["firstuniquealpha", "firstuniqueone", "firstuniquetwo"],
    );
    fs::remove_file(f.session_dir.join("second.jsonl")).unwrap();
    assert_eq!(f.index()["sessions_archived"], 1);
    assert_eq!(f.status("first"), "current");
    assert_eq!(
        f.status("second"),
        "archived",
        "genuine deletion still archives"
    );
}

/// Catches: a partially written last JSONL line archives a prior snapshot or
/// prevents indexing after the writer completes that line.
#[test]
fn partial_last_record_waits_for_completion_without_losing_the_snapshot() {
    let f = Fixture::new();
    f.transcript("live", &["initialuniquealpha", "second turn", "third turn"]);
    assert_eq!(f.index()["added"], 1);
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(f.session_dir.join("live.jsonl"))
        .unwrap();
    write!(file, "{{\"type\":\"user\",\"message\":").unwrap();
    drop(file);
    assert!(!f.index()["errors"].as_array().unwrap().is_empty());
    assert_eq!(f.status("live"), "current");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(f.session_dir.join("live.jsonl"))
        .unwrap();
    writeln!(
        file,
        "{{\"role\":\"user\",\"content\":\"freshuniqueomega\"}}}}"
    )
    .unwrap();
    drop(file);
    assert!(f.index()["errors"].as_array().unwrap().is_empty());
    assert!(
        f.ok(&[
            "search",
            "freshuniqueomega",
            "--collection",
            "claude_sessions"
        ])
        .contains("live")
    );
}
