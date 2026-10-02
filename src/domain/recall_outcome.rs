//! What became of a recall candidate, read from the session transcript.
//!
//! Only strong signals label a candidate. Silence is not a signal: an entry
//! the model read and quietly used looks exactly like one it ignored, so a
//! candidate with no event stays unlabelled, never negative. Story 183-1f3a.
//!
//! Pure: no I/O, deterministic.

use serde::Deserialize;

/// The label settlement writes to `recall_candidates.outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecallOutcome {
    /// A user correction after the injection names what the entry is about.
    Corrected,
    /// The model fetched an injected entry by id.
    Used,
    /// The model reached an entry recall did not inject: by id, or through a
    /// later search. The false negative the candidate floor exists to catch.
    Missed,
    /// An explicit `memory_confirm` verdict.
    Confirmed,
    Refuted,
}

impl RecallOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RecallOutcome::Corrected => "corrected",
            RecallOutcome::Used => "used",
            RecallOutcome::Missed => "missed",
            RecallOutcome::Confirmed => "confirmed",
            RecallOutcome::Refuted => "refuted",
        }
    }
}

/// One unsettled ledger row, with what labelling needs to know about it.
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerCandidate {
    pub prompt_id: i64,
    pub prompt_at: i64,
    pub entry_id: String,
    /// Empty when the entry has since been deleted.
    pub title: String,
    pub injected: bool,
}

/// A transcript event that can label a candidate. `at` is Unix seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecallEvent {
    Fetched {
        id: String,
        at: i64,
    },
    Verdict {
        id: String,
        confirmed: bool,
        at: i64,
    },
    /// The text of a memory search result.
    Found {
        text: String,
        at: i64,
    },
    Correction {
        text: String,
        at: i64,
    },
}

/// The events in a transcript window.
pub fn parse_events(jsonl: &str) -> Vec<RecallEvent> {
    let mut searches: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut events = Vec::new();
    for line in jsonl.lines() {
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        let Some(at) = record
            .timestamp
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.timestamp())
        else {
            continue;
        };
        let Some(message) = record.message else {
            continue;
        };
        match (record.kind.as_str(), message.content) {
            ("assistant", Content::Blocks(blocks)) => {
                for block in blocks {
                    let Block::ToolUse { id, name, input } = block else {
                        continue;
                    };
                    match memory_call(&name, &input) {
                        Some(Call::Get(entry)) => {
                            events.push(RecallEvent::Fetched { id: entry, at });
                        }
                        Some(Call::Confirm(entry, confirmed)) => {
                            events.push(RecallEvent::Verdict {
                                id: entry,
                                confirmed,
                                at,
                            });
                        }
                        Some(Call::Search) => {
                            searches.insert(id);
                        }
                        None => {}
                    }
                }
            }
            ("user", Content::Text(text)) => push_correction(&mut events, text, at),
            ("user", Content::Blocks(blocks)) => {
                for block in blocks {
                    match block {
                        Block::ToolResult {
                            tool_use_id,
                            content,
                        } => {
                            if searches.contains(&tool_use_id) {
                                events.push(RecallEvent::Found {
                                    text: content.to_text(),
                                    at,
                                });
                            }
                        }
                        Block::Text { text } => push_correction(&mut events, text, at),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    events
}

fn push_correction(events: &mut Vec<RecallEvent>, text: String, at: i64) {
    use crate::domain::prior_detect::{is_correction, is_hook_generated_prompt};
    if !is_hook_generated_prompt(&text) && is_correction(&text) {
        events.push(RecallEvent::Correction { text, at });
    }
}

enum Call {
    Get(String),
    Confirm(String, bool),
    Search,
}

/// The mdkb memory call a tool use makes, through MCP or the CLI.
fn memory_call(name: &str, input: &ToolInput) -> Option<Call> {
    if name.contains("mdkb") {
        let tool = name.rsplit("__").next().unwrap_or(name);
        return match tool {
            "get" => input.id.clone().map(Call::Get),
            "memory_confirm" => {
                let id = input.id.clone()?;
                Some(Call::Confirm(
                    id,
                    input.outcome.as_deref() == Some("confirmed"),
                ))
            }
            "search" => Some(Call::Search),
            _ => None,
        };
    }
    if name != "Bash" {
        return None;
    }
    cli_call(input.command.as_deref()?)
}

/// `mdkb [--format X] [memory] get|confirm|search …` in a shell command.
fn cli_call(command: &str) -> Option<Call> {
    let mut words = command.split_whitespace();
    words.find(|w| *w == "mdkb" || w.ends_with("/mdkb"))?;
    let mut args: Vec<&str> = Vec::new();
    let mut skip_value = false;
    for word in words {
        if matches!(word, "|" | "&&" | ";" | "||") {
            break;
        }
        if skip_value {
            skip_value = false;
        } else if matches!(word, "--format" | "--root") {
            skip_value = true;
        } else if !word.starts_with('-') || word == "--outcome" {
            args.push(word);
        }
    }
    let args: Vec<&str> = args.into_iter().skip_while(|a| *a == "memory").collect();
    match args.as_slice() {
        ["get", id, ..] => Some(Call::Get(id.trim_matches(['"', '\'']).to_string())),
        ["search", ..] => Some(Call::Search),
        ["confirm", id, rest @ ..] => {
            let outcome = rest
                .iter()
                .position(|a| *a == "--outcome")
                .and_then(|i| rest.get(i + 1));
            Some(Call::Confirm(id.to_string(), outcome == Some(&"confirmed")))
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    message: Option<Message>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    content: Content,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum Block {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: ToolInput,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        #[serde(default)]
        content: ResultContent,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default)]
struct ToolInput {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    outcome: Option<String>,
    #[serde(default)]
    command: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(untagged)]
enum ResultContent {
    Text(String),
    Blocks(Vec<ResultBlock>),
    #[default]
    None,
}

#[derive(Deserialize)]
struct ResultBlock {
    #[serde(default)]
    text: Option<String>,
}

impl ResultContent {
    fn to_text(&self) -> String {
        match self {
            ResultContent::Text(s) => s.clone(),
            ResultContent::Blocks(bs) => bs
                .iter()
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join(" "),
            ResultContent::None => String::new(),
        }
    }
}

/// `(prompt_id, entry_id, outcome)` for every candidate an event labels.
pub fn label(
    candidates: &[LedgerCandidate],
    events: &[RecallEvent],
) -> Vec<(i64, String, RecallOutcome)> {
    use std::collections::HashMap;
    let mut best: HashMap<(i64, String), RecallOutcome> = HashMap::new();
    let mut credit = |cand: &LedgerCandidate, outcome: RecallOutcome| {
        let slot = best
            .entry((cand.prompt_id, cand.entry_id.clone()))
            .or_insert(outcome);
        *slot = (*slot).max(outcome);
    };
    // The candidate row for `id` on the latest prompt at or before `at`.
    let latest = |id: &str, at: i64| {
        candidates
            .iter()
            .filter(|c| c.entry_id == id && c.prompt_at <= at)
            .max_by_key(|c| c.prompt_at)
    };
    for event in events {
        match event {
            RecallEvent::Fetched { id, at } => {
                if let Some(c) = latest(id, *at) {
                    credit(
                        c,
                        if c.injected {
                            RecallOutcome::Used
                        } else {
                            RecallOutcome::Missed
                        },
                    );
                }
            }
            RecallEvent::Verdict { id, confirmed, at } => {
                if let Some(c) = latest(id, *at) {
                    credit(
                        c,
                        if *confirmed {
                            RecallOutcome::Confirmed
                        } else {
                            RecallOutcome::Refuted
                        },
                    );
                }
            }
            RecallEvent::Found { text, at } => {
                for c in candidates.iter().filter(|c| !c.injected) {
                    if names_id(text, &c.entry_id)
                        && latest(&c.entry_id, *at).is_some_and(|l| l.prompt_id == c.prompt_id)
                    {
                        credit(c, RecallOutcome::Missed);
                    }
                }
            }
            RecallEvent::Correction { text, at } => {
                let Some(prompt_at) = candidates
                    .iter()
                    .filter(|c| c.prompt_at <= *at)
                    .map(|c| c.prompt_at)
                    .max()
                else {
                    continue;
                };
                let said = words(text);
                for c in candidates
                    .iter()
                    .filter(|c| c.injected && c.prompt_at == prompt_at)
                {
                    let shared = words(&c.title).iter().filter(|w| said.contains(*w)).count();
                    if shared >= 2 {
                        credit(c, RecallOutcome::Corrected);
                    }
                }
            }
        }
    }
    best.into_iter()
        .map(|((prompt_id, id), outcome)| (prompt_id, id, outcome))
        .collect()
}

/// Whether `text` contains `id` as a whole id, not inside a longer one.
fn names_id(text: &str, id: &str) -> bool {
    let is_id_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '.');
    text.match_indices(id).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let after = text[i + id.len()..].chars().next();
        !before.is_some_and(is_id_char) && !after.is_some_and(is_id_char)
    })
}

/// The distinctive words of a text: lowercase, four letters or more.
fn words(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(prompt_id: i64, prompt_at: i64, id: &str, injected: bool) -> LedgerCandidate {
        LedgerCandidate {
            prompt_id,
            prompt_at,
            entry_id: id.into(),
            title: format!("SQLite WAL checkpoint starvation {id}"),
            injected,
        }
    }

    fn outcomes(c: &[LedgerCandidate], e: &[RecallEvent]) -> Vec<(i64, String, &'static str)> {
        let mut out: Vec<_> = label(c, e)
            .into_iter()
            .map(|(p, id, o)| (p, id, o.as_str()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn fetching_an_injected_entry_after_the_prompt_labels_it_used() {
        let c = [cand(1, 100, "wal", true)];
        let e = [RecallEvent::Fetched {
            id: "wal".into(),
            at: 150,
        }];
        assert_eq!(outcomes(&c, &e), vec![(1, "wal".into(), "used")]);
    }

    /// A fetch that happened before the prompt says nothing about what the
    /// prompt injected.
    #[test]
    fn a_fetch_before_the_prompt_labels_nothing() {
        let c = [cand(1, 100, "wal", true)];
        let e = [RecallEvent::Fetched {
            id: "wal".into(),
            at: 50,
        }];
        assert!(outcomes(&c, &e).is_empty());
    }

    #[test]
    fn a_later_search_that_returns_an_uninjected_entry_labels_it_missed() {
        let c = [cand(1, 100, "wal", false), cand(1, 100, "shown", true)];
        let e = [RecallEvent::Found {
            text: "[wal] SQLite WAL checkpoint starvation (problem)\n[shown] other".into(),
            at: 200,
        }];
        assert_eq!(outcomes(&c, &e), vec![(1, "wal".into(), "missed")]);
    }

    /// `wal` must not match inside `wal-archive`: a substring hit would
    /// credit the wrong entry.
    #[test]
    fn a_search_hit_matches_whole_ids_only() {
        let c = [cand(1, 100, "wal", false)];
        let e = [RecallEvent::Found {
            text: "[wal-archive] Old".into(),
            at: 200,
        }];
        assert!(outcomes(&c, &e).is_empty());
    }

    #[test]
    fn memory_confirm_verdicts_label_confirmed_and_refuted() {
        let c = [cand(1, 100, "yes", true), cand(1, 100, "no", true)];
        let e = [
            RecallEvent::Verdict {
                id: "yes".into(),
                confirmed: true,
                at: 150,
            },
            RecallEvent::Verdict {
                id: "no".into(),
                confirmed: false,
                at: 150,
            },
        ];
        assert_eq!(
            outcomes(&c, &e),
            vec![(1, "no".into(), "refuted"), (1, "yes".into(), "confirmed")]
        );
    }

    #[test]
    fn a_correction_naming_the_entry_labels_it_corrected() {
        let c = [cand(1, 100, "wal", true)];
        let e = [RecallEvent::Correction {
            text: "no, the checkpoint starvation note is outdated".into(),
            at: 150,
        }];
        assert_eq!(outcomes(&c, &e), vec![(1, "wal".into(), "corrected")]);
    }

    /// One shared word is how every English sentence overlaps with every
    /// title; it must not blame the entry.
    #[test]
    fn a_correction_sharing_one_word_labels_nothing() {
        let c = [cand(1, 100, "wal", true)];
        let e = [RecallEvent::Correction {
            text: "no, use the other checkpoint".into(),
            at: 150,
        }];
        assert!(outcomes(&c, &e).is_empty());
    }

    #[test]
    fn no_signal_leaves_every_candidate_unlabelled() {
        let c = [cand(1, 100, "wal", true), cand(1, 100, "other", false)];
        assert!(outcomes(&c, &[]).is_empty());
    }

    #[test]
    fn two_injected_entries_credit_only_the_one_fetched() {
        let c = [cand(1, 100, "a", true), cand(1, 100, "b", true)];
        let e = [RecallEvent::Fetched {
            id: "b".into(),
            at: 150,
        }];
        assert_eq!(outcomes(&c, &e), vec![(1, "b".into(), "used")]);
    }

    /// The same entry offered on two prompts: the fetch is credited to the
    /// latest prompt before it, not to both.
    #[test]
    fn a_fetch_credits_the_latest_prompt_before_it() {
        let c = [cand(1, 100, "wal", true), cand(2, 300, "wal", true)];
        let e = [RecallEvent::Fetched {
            id: "wal".into(),
            at: 200,
        }];
        assert_eq!(outcomes(&c, &e), vec![(1, "wal".into(), "used")]);
    }

    /// An explicit verdict outranks the inference drawn from a fetch.
    #[test]
    fn a_verdict_outranks_a_fetch() {
        let c = [cand(1, 100, "wal", true)];
        let e = [
            RecallEvent::Fetched {
                id: "wal".into(),
                at: 150,
            },
            RecallEvent::Verdict {
                id: "wal".into(),
                confirmed: false,
                at: 160,
            },
        ];
        assert_eq!(outcomes(&c, &e), vec![(1, "wal".into(), "refuted")]);
    }

    #[test]
    fn a_fetched_entry_that_was_not_injected_is_missed() {
        let c = [cand(1, 100, "wal", false)];
        let e = [RecallEvent::Fetched {
            id: "wal".into(),
            at: 150,
        }];
        assert_eq!(outcomes(&c, &e), vec![(1, "wal".into(), "missed")]);
    }

    #[test]
    fn transcript_tool_calls_become_events() {
        let jsonl = [
            r#"{"type":"assistant","timestamp":"2026-09-29T10:00:00Z","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__mdkb__get","input":{"id":"wal"}}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-09-29T10:00:01Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"mdkb memory confirm other --outcome refuted"}}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-09-29T10:00:02Z","message":{"content":[{"type":"tool_use","id":"t3","name":"mcp__mdkb__search","input":{"query":"wal"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-09-29T10:00:03Z","message":{"content":[{"type":"tool_result","tool_use_id":"t3","content":"[wal] SQLite WAL"}]}}"#,
            r#"{"type":"user","timestamp":"2026-09-29T10:00:04Z","message":{"content":"no, that note is wrong"}}"#,
            r#"{"type":"assistant","timestamp":"2026-09-29T10:00:05Z","message":{"content":[{"type":"tool_use","id":"t4","name":"Bash","input":{"command":"mdkb get wal-two"}}]}}"#,
        ]
        .join("\n");
        let t0 = 1_790_676_000; // 2026-09-29T10:00:00Z
        assert_eq!(
            parse_events(&jsonl),
            vec![
                RecallEvent::Fetched {
                    id: "wal".into(),
                    at: t0
                },
                RecallEvent::Verdict {
                    id: "other".into(),
                    confirmed: false,
                    at: t0 + 1
                },
                RecallEvent::Found {
                    text: "[wal] SQLite WAL".into(),
                    at: t0 + 3
                },
                RecallEvent::Correction {
                    text: "no, that note is wrong".into(),
                    at: t0 + 4
                },
                RecallEvent::Fetched {
                    id: "wal-two".into(),
                    at: t0 + 5
                },
            ]
        );
    }

    const T0: i64 = 1_790_676_000; // 2026-09-29T10:00:00Z

    fn assistant_tool_use(second: u32, id: &str, name: &str, input: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-09-29T10:00:{second:02}Z","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#
        )
    }

    fn user_record(second: u32, content: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-09-29T10:00:{second:02}Z","message":{{"content":{content}}}}}"#
        )
    }

    fn verdicts(jsonl: &str) -> Vec<(String, bool)> {
        parse_events(jsonl)
            .into_iter()
            .filter_map(|e| match e {
                RecallEvent::Verdict { id, confirmed, .. } => Some((id, confirmed)),
                _ => None,
            })
            .collect()
    }

    /// Catches: the outcome word read from the wrong place (`--outcome` kept as
    /// a positional, its position compared with `!=`, the value index
    /// multiplied), which turns every CLI verdict into "refuted".
    #[test]
    fn parse_events_reads_cli_confirm_with_outcome_confirmed() {
        let confirmed = |command: &str| {
            let jsonl =
                assistant_tool_use(0, "t1", "Bash", &format!(r#"{{"command":"{command}"}}"#));
            verdicts(&jsonl)
        };

        assert_eq!(
            confirmed("mdkb memory confirm wal --outcome confirmed"),
            [("wal".to_string(), true)]
        );
        assert_eq!(
            confirmed("mdkb memory confirm wal --quiet --outcome confirmed"),
            [("wal".to_string(), true)]
        );
        assert_eq!(
            confirmed("mdkb memory confirm wal --outcome refuted"),
            [("wal".to_string(), false)]
        );
    }

    /// Catches: the MCP `memory_confirm` tool not recognised, or its outcome
    /// compared with `!=`, which labels a confirmation refuted and vice versa.
    #[test]
    fn parse_events_reads_mcp_confirm_tool_both_outcomes() {
        let call = |outcome: &str| {
            verdicts(&assistant_tool_use(
                0,
                "t1",
                "mcp__mdkb__memory_confirm",
                &format!(r#"{{"id":"wal","outcome":"{outcome}"}}"#),
            ))
        };

        assert_eq!(call("confirmed"), [("wal".to_string(), true)]);
        assert_eq!(call("refuted"), [("wal".to_string(), false)]);
    }

    /// Catches: the CLI `search` arm missing, so a search run from a shell never
    /// produces a `Found` event and an uninjected hit is never labelled missed.
    #[test]
    fn parse_events_reads_cli_search_result_as_found() {
        let jsonl = [
            assistant_tool_use(0, "t1", "Bash", r#"{"command":"mdkb search wal"}"#),
            user_record(
                1,
                r#"[{"type":"tool_result","tool_use_id":"t1","content":"[wal] SQLite WAL"}]"#,
            ),
        ]
        .join("\n");

        assert_eq!(
            parse_events(&jsonl),
            vec![RecallEvent::Found {
                text: "[wal] SQLite WAL".into(),
                at: T0 + 1
            }]
        );
    }

    /// Catches: the `text` block arm of a user record dropped, which loses every
    /// correction the harness delivers as a content block.
    #[test]
    fn parse_events_emits_correction_for_user_text_block() {
        let jsonl = user_record(
            0,
            r#"[{"type":"text","text":"no, the checkpoint starvation note is wrong"}]"#,
        );

        assert_eq!(
            parse_events(&jsonl),
            vec![RecallEvent::Correction {
                text: "no, the checkpoint starvation note is wrong".into(),
                at: T0
            }]
        );
    }

    /// Catches: `!hook && correction` turned `||`: a neutral message, or a hook
    /// message that happens to contain a correction word, becomes a correction.
    #[test]
    fn parse_events_ignores_plain_and_hook_generated_user_text() {
        let neutral = "please run the tests";
        let hook = "Stop hook feedback: no, you should have run the tests";
        for text in [neutral, hook] {
            let as_string = user_record(0, &format!("{text:?}"));
            let as_block = user_record(0, &format!(r#"[{{"type":"text","text":{text:?}}}]"#));

            assert!(parse_events(&as_string).is_empty(), "string: {text}");
            assert!(parse_events(&as_block).is_empty(), "block: {text}");
        }
    }

    /// Catches: `injected && prompt_at == latest` turned `||`: a correction
    /// would also credit an earlier prompt's candidate and any candidate recall
    /// never injected, labelling a false negative as corrected.
    #[test]
    fn a_correction_credits_only_injected_candidates_of_the_latest_prompt() {
        let c = [
            cand(2, 200, "latest-injected", true),
            cand(1, 100, "earlier-injected", true),
            cand(2, 200, "latest-not-injected", false),
        ];
        let e = [RecallEvent::Correction {
            text: "no, the checkpoint starvation note is outdated".into(),
            at: 250,
        }];

        assert_eq!(
            outcomes(&c, &e),
            vec![(2, "latest-injected".into(), "corrected")]
        );
    }
}
