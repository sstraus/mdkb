//! The recall hint must be executable when the same prior reaches several sessions.
use std::io::Write;
use std::process::Stdio;

use mdkb::cli::handlers::{handle_init, handle_memory_add};
use mdkb::core::Context;
use mdkb::store::priors::{PriorCluster, record_injection, upsert_cluster};

#[path = "common/cli.rs"]
mod cli;

fn fixture() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    handle_init(dir.path()).unwrap();
    let cfg = dir.path().join(".mdkb/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[hooks]\nuser_prompt_submit_deadline_ms = 60000\n");
    std::fs::write(cfg, text).unwrap();
    let ctx = Context::open(dir.path()).unwrap();
    handle_memory_add(
        &ctx,
        "lesson",
        "Hook dispatcher architecture",
        "topic",
        None,
        "The hook dispatcher reads stdin and writes JSON to stdout for lifecycle events.",
        None,
        None,
        None,
        None,
        &[],
        None,
        None,
        false,
    )
    .unwrap();
    upsert_cluster(
        &ctx.conn,
        &PriorCluster {
            id: "clu-hint".into(),
            canonical_trigger_key: "pre_tool|{}".into(),
            trigger_kind: "pre_tool".into(),
            trigger_matcher: "{}".into(),
            lesson: "Hook dispatcher architecture".into(),
            scope: "{}".into(),
            evidence_count: 2,
            distinct_sessions: 2,
            injected_count: 0,
            confirmed_count: 0,
            refuted_count: 0,
            misfired_count: 0,
            state: "promoted".into(),
            promoted_memory_id: Some("lesson".into()),
            created_at: 100,
            last_seen_at: 100,
            last_unrefuted_injection_at: None,
            error_signature: None,
        },
    )
    .unwrap();
    for session in ["caller-session", "other-session"] {
        record_injection(&ctx.conn, "clu-hint", session, 200).unwrap();
    }
    (dir, ctx)
}

// Catches: the hook recommends a confirmation command that cannot select one
// of two open injections, or settles the other session instead of its caller.
#[test]
fn recall_hint_confirms_only_the_callers_open_prior_session() {
    let (dir, ctx) = fixture();
    drop(ctx);
    let mut child = cli::command()
        .args(["hook", "user-prompt-submit"])
        .current_dir(dir.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(
        br#"{"session_id":"caller-session","prompt":"* Explain the hook dispatcher architecture and lifecycle events"}"#,
    ).unwrap();
    let hook = child.wait_with_output().unwrap();
    assert!(
        hook.status.success(),
        "{}",
        String::from_utf8_lossy(&hook.stderr)
    );
    let response: serde_json::Value = serde_json::from_slice(&hook.stdout).unwrap();
    let body = response["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    let command = body
        .split('`')
        .find(|s| s.starts_with("mdkb memory confirm "))
        .unwrap_or_else(|| panic!("missing confirmation hint: {body}"))
        .replace("<id>", "clu-hint");
    // The fixture session is a single shell-quoted word with no embedded quotes.
    let args: Vec<&str> = command
        .split_whitespace()
        .skip(1)
        .map(|arg| arg.trim_matches('\''))
        .collect();
    let out = cli::run(&args, dir.path());
    assert!(
        out.status.success(),
        "hint failed: {command}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ctx = Context::open(dir.path()).unwrap();
    let mut stmt = ctx.conn.prepare("SELECT session, outcome FROM prior_injections WHERE cluster_id = 'clu-hint' ORDER BY session").unwrap();
    let outcomes: Vec<(String, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        outcomes,
        vec![
            ("caller-session".into(), Some("confirmed".into())),
            ("other-session".into(), None)
        ]
    );
}

// Catches: daemon routing drops --session or silently judges another injection
// when an explicit session is unknown, blank, or already settled.
#[test]
fn routed_confirm_preserves_session_and_refuses_to_judge_another_session() {
    use clap::Parser;
    use mdkb::cli::Cli;
    use mdkb::core::cli_mutation::{CliMutation, execute_context_mutation};
    use mdkb::core::routing::mutation_request;

    let (dir, ctx) = fixture();
    for (session, succeeds) in [
        ("missing-session", false),
        ("", false),
        ("caller-session", true),
        ("caller-session", false),
    ] {
        let mut cli = Cli::try_parse_from([
            "mdkb",
            "memory",
            "confirm",
            "clu-hint",
            "--outcome",
            "confirmed",
            "--session",
            session,
        ])
        .unwrap();
        let request = mutation_request(&mut cli.command, dir.path(), dir.path())
            .unwrap()
            .unwrap();
        let wire = serde_json::to_vec(&request).unwrap();
        let request: CliMutation = serde_json::from_slice(&wire).unwrap();
        let result = execute_context_mutation(&ctx, request);
        assert_eq!(
            result.is_ok(),
            succeeds,
            "session={session:?}, result={result:?}"
        );
    }
    let cluster = mdkb::store::priors::get_cluster(&ctx.conn, "clu-hint")
        .unwrap()
        .unwrap();
    assert_eq!(cluster.confirmed_count, 1);
    let other: Option<String> = ctx.conn.query_row(
        "SELECT outcome FROM prior_injections WHERE cluster_id = 'clu-hint' AND session = 'other-session'",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(other, None);
}
