//! Raw episode parser for behavioral-prior mining.
//!
//! Unlike [`crate::domain::sessions`], which deliberately discards tool blocks
//! to build searchable conversation text, this parser PRESERVES the evidence a
//! behavioral lesson lives in: the tool sequence, tool errors and their
//! signatures, whether an error was followed by a corrective action, and the
//! user's own messages (where corrections like "no, do X instead" appear).
//!
//! Output is a structured [`Episode`] consumed by the Phase-4 candidate
//! detector and Phase-5 LLM distiller. Pure: no I/O, deterministic.

use serde::Deserialize;

/// A single tool invocation observed in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub file_path: Option<String>,
    pub command: Option<String>,
}

/// A tool invocation whose result was an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub tool: String,
    /// First ~120 chars of the error output, whitespace-collapsed.
    pub signature: String,
    pub tool_use_id: String,
}

/// The distilled raw episode for one transcript window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Episode {
    pub tools: Vec<ToolUse>,
    pub errors: Vec<ToolError>,
    /// Real user messages (not tool_result carriers) — the source of explicit
    /// corrections the Phase-4 detector keys on.
    pub user_messages: Vec<String>,
    /// The last tool_result in the window was not an error.
    pub ended_clean: bool,
    /// An error occurred and at least one tool ran after it (a recovery attempt).
    pub had_corrective_action: bool,
}

// ============================================================================
// JSONL deserialization (preserves tool blocks)
// ============================================================================

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    record_type: String,
    message: Option<Message>,
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
        is_error: bool,
        #[serde(default)]
        content: ResultContent,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default)]
struct ToolInput {
    #[serde(default)]
    file_path: Option<String>,
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

/// Collapse whitespace and cap an error result to a stable signature.
fn preview(content: &str) -> String {
    let trimmed: String = content.trim().chars().take(120).collect();
    trimmed.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a transcript window (JSONL text) into a structured [`Episode`].
///
/// Mirrors the action→consequence→handling extraction: tool_use blocks in
/// assistant messages become the tool sequence; tool_result blocks in user
/// messages become results (errors when `is_error`); user text messages are
/// captured verbatim for correction detection.
pub fn parse_episode(jsonl: &str) -> Episode {
    let mut tools: Vec<ToolUse> = Vec::new();
    let mut errors: Vec<ToolError> = Vec::new();
    let mut user_messages: Vec<String> = Vec::new();
    let mut tool_name_by_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // (tool_use_id, is_error) in result order — to find the last result.
    let mut result_order: Vec<bool> = Vec::new();

    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record: Record = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let Some(message) = record.message else {
            continue;
        };
        let blocks = match message.content {
            Content::Blocks(b) => b,
            Content::Text(s) => {
                if record.record_type == "user" && !s.trim().is_empty() {
                    user_messages.push(s);
                }
                continue;
            }
        };

        for block in blocks {
            match block {
                Block::ToolUse { id, name, input } if record.record_type == "assistant" => {
                    tool_name_by_id.insert(id.clone(), name.clone());
                    tools.push(ToolUse {
                        id,
                        name,
                        file_path: input.file_path,
                        command: input.command,
                    });
                }
                Block::ToolResult {
                    tool_use_id,
                    is_error,
                    content,
                } if record.record_type == "user" => {
                    result_order.push(is_error);
                    if is_error {
                        if let Some(tool) = tool_name_by_id.get(&tool_use_id) {
                            errors.push(ToolError {
                                tool: tool.clone(),
                                signature: preview(&content.to_text()),
                                tool_use_id,
                            });
                        }
                    }
                }
                Block::Text { text } if record.record_type == "user" => {
                    if !text.trim().is_empty() {
                        user_messages.push(text);
                    }
                }
                _ => {}
            }
        }
    }

    let ended_clean = result_order.last().map(|is_err| !is_err).unwrap_or(false);

    let had_corrective_action = errors
        .first()
        .and_then(|first_err| tools.iter().position(|t| t.id == first_err.tool_use_id))
        .map(|first_error_idx| tools.len() > first_error_idx + 1)
        .unwrap_or(false);

    Episode {
        tools,
        errors,
        user_messages,
        ended_clean,
        had_corrective_action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One assistant tool_use, then a user tool_result error, then a corrective
    // assistant tool_use, then a clean user tool_result.
    fn fix_transcript() -> String {
        [
            r#"{"type":"user","message":{"role":"user","content":"Please refactor the parser"}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo build"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"error[E0433]: failed to resolve"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/lib.rs"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}"#,
        ]
        .join("\n")
    }

    #[test]
    fn preserves_tools_and_error_evidence() {
        let ep = parse_episode(&fix_transcript());
        assert_eq!(ep.tools.len(), 2);
        assert_eq!(ep.tools[0].name, "Bash");
        assert_eq!(ep.tools[0].command.as_deref(), Some("cargo build"));
        assert_eq!(ep.tools[1].file_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(ep.errors.len(), 1);
        assert_eq!(ep.errors[0].tool, "Bash");
        assert!(ep.errors[0].signature.contains("E0433"));
    }

    #[test]
    fn detects_corrective_action_and_clean_ending() {
        let ep = parse_episode(&fix_transcript());
        assert!(ep.had_corrective_action, "Edit ran after the Bash error");
        assert!(ep.ended_clean, "last tool_result was not an error");
    }

    #[test]
    fn captures_user_messages_but_not_tool_result_carriers() {
        let ep = parse_episode(&fix_transcript());
        assert_eq!(
            ep.user_messages,
            vec!["Please refactor the parser".to_string()]
        );
    }

    #[test]
    fn no_tools_yields_empty_episode() {
        let line = r#"{"type":"user","message":{"role":"user","content":"just chatting"}}"#;
        let ep = parse_episode(line);
        assert!(ep.tools.is_empty());
        assert!(ep.errors.is_empty());
        assert!(!ep.had_corrective_action);
        assert!(!ep.ended_clean);
        assert_eq!(ep.user_messages, vec!["just chatting".to_string()]);
    }

    #[test]
    fn error_with_no_recovery_is_not_corrective() {
        let jsonl = [
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"boom"}]}}"#,
        ]
        .join("\n");
        let ep = parse_episode(&jsonl);
        assert_eq!(ep.errors.len(), 1);
        assert!(!ep.had_corrective_action, "no tool ran after the error");
        assert!(!ep.ended_clean, "last result was an error");
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let jsonl = [
            "not json",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#,
            "",
        ]
        .join("\n");
        let ep = parse_episode(&jsonl);
        assert_eq!(ep.tools.len(), 1);
        assert_eq!(ep.tools[0].name, "Read");
    }
}
