//! Replay harness for story 090-45ae / plan Step 8 (D4).
//!
//! The plan requires the stored candidates to be replayed against recorded tool
//! events under both the old and the new matcher, with matches, false matches
//! and match-zero patterns counted BEFORE the schema changes.
//!
//! Neither corpus is committed: the events are real shell commands from a
//! developer's transcripts, and their absolute paths carry the local username —
//! the same leak the plan's Security Considerations section flags for Step 12.
//! So the harness reads both from files named by environment variables and does
//! nothing when they are absent.
//!
//! ```text
//! MDKB_REPLAY_CANDIDATES=/tmp/prior-candidates.json \
//! MDKB_REPLAY_EVENTS=/tmp/tool-events.json \
//! MDKB_REPLAY_ROOT=/Users/me/Gits/personal/mdkb \
//! cargo nextest run --test prior_matcher_replay -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;

use mdkb::store::priors::{TriggerContext, trigger_matches};

#[derive(serde::Deserialize)]
struct Candidate {
    id: String,
    trigger_kind: String,
    trigger_matcher: String,
}

#[derive(serde::Deserialize)]
struct Event {
    tool: String,
    path: Option<String>,
    command: Option<String>,
}

fn load<T: serde::de::DeserializeOwned>(var: &str) -> Option<Vec<T>> {
    let path = std::env::var(var).ok()?;
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{var} points at {path}, which cannot be read: {e}"));
    Some(
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{path} is not the expected JSON: {e}")),
    )
}

/// Counts per candidate, then the three totals the plan asks for.
#[test]
#[ignore = "needs a candidate export and a transcript event corpus (see module docs)"]
fn replay_stored_candidates_against_recorded_tool_events() {
    let Some(candidates) = load::<Candidate>("MDKB_REPLAY_CANDIDATES") else {
        eprintln!("MDKB_REPLAY_CANDIDATES unset — nothing to replay");
        return;
    };
    let Some(events) = load::<Event>("MDKB_REPLAY_EVENTS") else {
        eprintln!("MDKB_REPLAY_EVENTS unset — nothing to replay");
        return;
    };
    // The matcher globs against repo-relative paths, so an absolute one from a
    // transcript has to be relativised or every path glob misses for the wrong
    // reason.
    let root = std::env::var("MDKB_REPLAY_ROOT").unwrap_or_default();
    let rel = |p: &str| -> String {
        p.strip_prefix(&root)
            .map(|s| s.trim_start_matches('/').to_string())
            .unwrap_or_else(|| p.to_string())
    };

    let mut hits: BTreeMap<&str, usize> = BTreeMap::new();
    let mut tool_name_only: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &candidates {
        let mut n = 0usize;
        let mut bare_tool = 0usize;
        for e in &events {
            let path = e.path.as_deref().map(rel);
            let ctx = match c.trigger_kind.as_str() {
                "pre_tool" => TriggerContext::PreTool {
                    tool: &e.tool,
                    path: path.as_deref(),
                    command: e.command.as_deref(),
                },
                "post_tool" => TriggerContext::PostTool {
                    tool: &e.tool,
                    path: path.as_deref(),
                    command: e.command.as_deref(),
                },
                // `stop`, `repo` and `prompt` have no tool-call arm; they are
                // reported as match-zero, which is the honest reading.
                _ => continue,
            };
            if trigger_matches(&c.trigger_kind, &c.trigger_matcher, &ctx) {
                n += 1;
                // A hit that survives with the command and path removed came
                // from the bare tool-name arm — the "fires on every Bash call"
                // shape the plan wants counted as a false match.
                let stripped = match c.trigger_kind.as_str() {
                    "pre_tool" => TriggerContext::PreTool {
                        tool: &e.tool,
                        path: None,
                        command: None,
                    },
                    _ => TriggerContext::PostTool {
                        tool: &e.tool,
                        path: None,
                        command: None,
                    },
                };
                if trigger_matches(&c.trigger_kind, &c.trigger_matcher, &stripped) {
                    bare_tool += 1;
                }
            }
        }
        hits.insert(&c.id, n);
        tool_name_only.insert(&c.id, bare_tool);
    }

    let tool_kind = |c: &&Candidate| matches!(c.trigger_kind.as_str(), "pre_tool" | "post_tool");
    let tool_candidates: Vec<&Candidate> = candidates.iter().filter(tool_kind).collect();
    let match_zero = tool_candidates
        .iter()
        .filter(|c| hits[c.id.as_str()] == 0)
        .count();
    let false_matches: usize = tool_candidates
        .iter()
        .map(|c| tool_name_only[c.id.as_str()])
        .sum();
    let total: usize = tool_candidates.iter().map(|c| hits[c.id.as_str()]).sum();

    println!("=== replay: {} events ===", events.len());
    println!("tool-kind candidates : {}", tool_candidates.len());
    println!("total matches        : {total}");
    println!("false matches (bare tool name): {false_matches}");
    println!("match-zero patterns  : {match_zero}");
    println!("--- worst offenders ---");
    let mut ranked: Vec<&&Candidate> = tool_candidates.iter().collect();
    ranked.sort_by_key(|c| std::cmp::Reverse(hits[c.id.as_str()]));
    for c in ranked.iter().take(10) {
        println!(
            "{:6} (bare {:6})  {} {}",
            hits[c.id.as_str()],
            tool_name_only[c.id.as_str()],
            c.trigger_kind,
            c.trigger_matcher
        );
    }
}
