//! Integration tests for claude_sessions retention (story 044): archive on
//! missing source, remain searchable when explicitly scoped, and opt-in prune.

use std::path::PathBuf;
use std::process::Command;

use mdkb::cli::handlers::{
    Context, handle_init, handle_prune_sessions, handle_session_index, parse_retention_secs,
};
use mdkb::domain::{COLLECTION_CLAUDE_SESSIONS, SearchQuery};
use mdkb::store::{documents, search};
use tempfile::TempDir;

/// A minimal 3-user-turn transcript (min_turns = 3) whose content is searchable.
fn transcript(topic: &str) -> String {
    let ts = "2026-01-09T08:43:52.235Z";
    format!(
        "{u1}\n{a1}\n{u2}\n{a2}\n{u3}\n",
        u1 = format!(
            r#"{{"type":"user","message":{{"role":"user","content":"question about {topic} handling"}},"timestamp":"{ts}"}}"#
        ),
        a1 = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"an answer"}]},"timestamp":"2026-01-09T08:44:00.000Z"}"#,
        u2 = r#"{"type":"user","message":{"role":"user","content":"follow up two"},"timestamp":"2026-01-09T08:45:00.000Z"}"#,
        a2 = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"answer two"}]},"timestamp":"2026-01-09T08:46:00.000Z"}"#,
        u3 = r#"{"type":"user","message":{"role":"user","content":"follow up three"},"timestamp":"2026-01-09T08:47:00.000Z"}"#,
    )
}

struct Env {
    _dir: TempDir,
    root: PathBuf,
    sessions_base: PathBuf,
    session_dir: PathBuf,
    ctx: Context,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        handle_init(&root).expect("init");
        let ctx = Context::open(&root).expect("open");

        // Claude session layout: <base>/<encoded-project-path>/<file>.jsonl
        let sessions_base = root.join("claude_projects");
        let encoded = root.to_string_lossy().replace(['/', '.'], "-");
        let session_dir = sessions_base.join(&encoded);
        std::fs::create_dir_all(&session_dir).expect("mk session dir");

        Self {
            _dir: dir,
            root,
            sessions_base,
            session_dir,
            ctx,
        }
    }

    fn write_session(&self, name: &str, topic: &str) {
        std::fs::write(
            self.session_dir.join(format!("{name}.jsonl")),
            transcript(topic),
        )
        .expect("write session");
    }

    fn index(&self) -> mdkb::domain::UpdateResult {
        handle_session_index(&self.ctx, &self.sessions_base, &self.root.to_string_lossy())
            .expect("session index")
    }

    fn status_of(&self, relative_path: &str) -> Option<String> {
        self.ctx
            .conn
            .query_row(
                "SELECT status FROM documents WHERE collection = ?1 AND relative_path = ?2",
                rusqlite::params![COLLECTION_CLAUDE_SESSIONS, relative_path],
                |r| r.get::<_, Option<String>>(0),
            )
            .expect("query status")
    }
}

fn scoped_query(text: &str, collection: Option<&str>) -> SearchQuery {
    SearchQuery {
        text: text.to_string(),
        limit: 20,
        collection: collection.map(String::from),
        tags: vec![],
        include_superseded: false,
    }
}

#[test]
fn update_archives_missing_source_but_keeps_it_searchable_when_scoped() {
    let env = Env::new();
    env.write_session("sess-alpha", "kafka rebalance");
    env.write_session("sess-beta", "postgres replication");
    let r = env.index();
    assert_eq!(r.added, 2, "two sessions indexed");
    assert_eq!(r.sessions_archived, 0);

    // Delete beta's source jsonl, re-index.
    std::fs::remove_file(env.session_dir.join("sess-beta.jsonl")).unwrap();
    let r2 = env.index();
    assert_eq!(r2.sessions_archived, 1, "the orphaned session is archived");

    // alpha still current (NULL status); beta archived.
    assert_ne!(env.status_of("sess-alpha").as_deref(), Some("archived"));
    assert_eq!(env.status_of("sess-beta").as_deref(), Some("archived"));

    // Explicit --collection claude_sessions still finds the archived transcript.
    let scoped = search::search(
        &env.ctx.conn,
        &scoped_query("postgres replication", Some(COLLECTION_CLAUDE_SESSIONS)),
    )
    .expect("scoped search");
    assert!(
        scoped.iter().any(|r| r.path == "sess-beta"),
        "archived session must remain searchable when explicitly scoped: {scoped:?}"
    );

    // Default (unscoped) search excludes the archived transcript.
    let unscoped = search::search(&env.ctx.conn, &scoped_query("postgres replication", None))
        .expect("unscoped search");
    assert!(
        !unscoped.iter().any(|r| r.path == "sess-beta"),
        "archived session must NOT appear in default search"
    );
}

#[test]
fn prune_lists_only_archived_older_than_cutoff() {
    let env = Env::new();
    env.write_session("sess-alpha", "kafka rebalance");
    env.write_session("sess-beta", "postgres replication");
    env.index();
    std::fs::remove_file(env.session_dir.join("sess-beta.jsonl")).unwrap();
    env.index(); // beta archived

    let now = chrono::Utc::now().timestamp();
    // Cutoff in the future → archived beta is eligible; alpha (current) is not.
    let eligible = documents::list_prunable_sessions(&env.ctx.conn, now + 3600).unwrap();
    assert_eq!(eligible.len(), 1);
    assert_eq!(eligible[0].relative_path, "sess-beta");

    // Cutoff before indexing → nothing eligible yet.
    let none = documents::list_prunable_sessions(&env.ctx.conn, now - 86_400).unwrap();
    assert!(none.is_empty(), "nothing older than a day-ago cutoff");
}

#[test]
fn prune_exports_markdown_before_deleting() {
    let env = Env::new();
    env.write_session("sess-alpha", "kafka rebalance");
    env.write_session("sess-beta", "postgres replication");
    env.index();
    std::fs::remove_file(env.session_dir.join("sess-beta.jsonl")).unwrap();
    env.index();

    let export_dir = env.root.join("exported");
    let now = chrono::Utc::now().timestamp();
    let summary = handle_prune_sessions(&env.ctx, now + 3600, Some(&export_dir)).expect("prune");

    assert_eq!(summary.pruned, 1, "one archived session pruned");
    assert_eq!(summary.exported, 1);
    // Filename is collision-proof (`{safe}-{id}-{hash8}.md`), so match by prefix.
    let exports: Vec<_> = std::fs::read_dir(&export_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(exports.len(), 1, "one export file written");
    let fname = exports[0].file_name().to_string_lossy().into_owned();
    assert!(
        fname.starts_with("sess-beta-") && fname.ends_with(".md"),
        "collision-proof export filename: {fname}"
    );
    let md = std::fs::read_to_string(exports[0].path()).expect("export written");
    assert!(
        md.contains("postgres replication"),
        "export carries the body"
    );

    // beta row hard-deleted; alpha (current) untouched.
    let beta = env
        .ctx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE relative_path = 'sess-beta'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(beta, 0, "beta row hard-deleted");
    let alpha = env
        .ctx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE relative_path = 'sess-alpha'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(alpha, 1, "current session must survive pruning");
}

#[test]
fn prune_skips_delete_when_content_missing_under_export() {
    // DATA-1: the FK (documents.hash → content.hash) normally guarantees a body
    // exists, but if it ever goes missing, `--export` must NOT delete the only
    // copy unexported. Simulate the integrity slip by orphaning beta's content row
    // with the FK temporarily off, then assert the prune skips the delete.
    let env = Env::new();
    env.write_session("sess-alpha", "kafka rebalance");
    env.write_session("sess-beta", "postgres replication");
    env.index();
    std::fs::remove_file(env.session_dir.join("sess-beta.jsonl")).unwrap();
    env.index(); // beta archived

    env.ctx
        .conn
        .execute_batch("PRAGMA foreign_keys = OFF;")
        .unwrap();
    env.ctx
        .conn
        .execute(
            "DELETE FROM content WHERE hash = \
             (SELECT hash FROM documents WHERE relative_path = 'sess-beta')",
            [],
        )
        .unwrap();
    env.ctx
        .conn
        .execute_batch("PRAGMA foreign_keys = ON;")
        .unwrap();

    let export_dir = env.root.join("exported");
    let now = chrono::Utc::now().timestamp();
    let summary = handle_prune_sessions(&env.ctx, now + 3600, Some(&export_dir)).expect("prune");

    assert_eq!(summary.exported, 0, "no body available to export");
    assert_eq!(
        summary.pruned, 0,
        "must not delete without a successful export"
    );
    let beta = env
        .ctx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM documents WHERE relative_path = 'sess-beta'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(
        beta, 1,
        "beta transcript preserved — not deleted without export"
    );
}

#[test]
fn parse_retention_secs_accepts_units_and_rejects_garbage() {
    assert_eq!(parse_retention_secs("90d").unwrap(), 90 * 86_400);
    assert_eq!(parse_retention_secs("12h").unwrap(), 12 * 3_600);
    assert_eq!(parse_retention_secs("2w").unwrap(), 2 * 604_800);
    assert!(parse_retention_secs("90").is_err(), "missing unit");
    assert!(parse_retention_secs("90y").is_err(), "bad unit");
    assert!(parse_retention_secs("abc").is_err());
    // SEC-1: an oversized value must be rejected, not silently wrapped to a
    // small/negative cutoff that would over-delete.
    assert!(
        parse_retention_secs("99999999999999w").is_err(),
        "overflowing duration must be rejected, not wrapped"
    );
}

fn mdkb_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mdkb"))
}

#[test]
fn compact_prune_sessions_refuses_without_older_than() {
    let env = Env::new();
    let out = mdkb_bin()
        .args(["compact", "--prune-sessions"])
        .current_dir(&env.root)
        .env("MDKB_NO_DAEMON", "1")
        .output()
        .expect("run compact");
    assert!(
        !out.status.success(),
        "--prune-sessions without --older-than must be rejected"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("older-than"),
        "error must name the missing flag: {stderr}"
    );
}

/// A plain `mdkb compact` (no prune flags) still vacuums and exits 0.
#[test]
fn compact_without_prune_still_vacuums() {
    let env = Env::new();
    let out = mdkb_bin()
        .args(["compact"])
        .current_dir(&env.root)
        .env("MDKB_NO_DAEMON", "1")
        .output()
        .expect("run compact");
    assert!(out.status.success(), "plain compact must succeed");
}
