//! Claude Code session JSONL parser and document converter.
//!
//! Extracts user and assistant text from Claude Code session files,
//! filtering out tool use/result blocks, progress events, and other
//! non-conversation records. Converts sessions into indexable documents
//! with optional chunking for long conversations.

use std::io::BufRead;
use std::path::Path;

use serde::Deserialize;

/// A parsed conversation record from a session file.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// "user" or "assistant".
    pub role: String,
    /// Concatenated text content (tool blocks excluded).
    pub text: String,
    /// ISO 8601 timestamp from the record.
    pub timestamp: Option<String>,
}

/// Parse a single JSONL line into a SessionRecord, if it's a user or assistant
/// message with extractable text.
///
/// Returns `None` for progress, system, file-history-snapshot, queue-operation,
/// summary records, and for user/assistant records with no text content.
pub fn parse_record(line: &str) -> Option<SessionRecord> {
    let raw: RawRecord = serde_json::from_str(line).ok()?;

    match raw.record_type.as_str() {
        "user" | "assistant" => {}
        _ => return None,
    }

    let message = raw.message?;
    let text = extract_text(&message.content);

    if text.is_empty() {
        return None;
    }

    Some(SessionRecord {
        role: raw.record_type,
        text,
        timestamp: raw.timestamp,
    })
}

/// Extract text from message content, which can be a string or array of blocks.
fn extract_text(content: &Content) -> String {
    match content {
        Content::Text(s) => s.clone(),
        Content::Blocks(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Block::Text { text } = block {
                    parts.push(text.as_str());
                }
            }
            parts.join("\n\n")
        }
    }
}

// =============================================================================
// Project path encoding
// =============================================================================

/// Encode a project root path using Claude Code's directory encoding.
///
/// Replaces `/` and `.` with `-` to match the encoding CC uses for
/// `~/.claude/projects/{encoded-path}/`.
pub fn encode_project_path(path: &str) -> String {
    path.replace(['/', '.'], "-")
}

/// Find the Claude Code session directory for a project root.
///
/// Looks for `{sessions_base}/{encoded_path}` on disk.
/// Returns `None` if the directory doesn't exist.
pub fn find_session_dir(sessions_base: &Path, project_root: &str) -> Option<std::path::PathBuf> {
    let encoded = encode_project_path(project_root);
    let dir = sessions_base.join(encoded);
    if dir.is_dir() { Some(dir) } else { None }
}

// =============================================================================
// Session-to-document conversion
// =============================================================================

/// Configuration for session parsing and chunking.
#[derive(Debug, Clone)]
pub struct SessionParseConfig {
    /// Minimum user messages required to index a session.
    pub min_turns: usize,
    /// Maximum turns per chunk.
    pub chunk_size: usize,
    /// Overlap turns between consecutive chunks.
    pub chunk_overlap: usize,
}

impl Default for SessionParseConfig {
    fn default() -> Self {
        Self {
            min_turns: 3,
            chunk_size: 10,
            chunk_overlap: 2,
        }
    }
}

/// Metadata extracted from session JSONL records.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub session_id: String,
    pub git_branch: Option<String>,
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
    pub turn_count: usize,
}

/// An indexable document produced from a session or session chunk.
#[derive(Debug, Clone)]
pub struct SessionDocument {
    /// Relative path for indexing (e.g. "abc-123" or "abc-123-chunk-001").
    pub relative_path: String,
    /// Formatted markdown content.
    pub content: String,
    /// Session metadata.
    pub metadata: SessionMetadata,
}

/// Parse a session JSONL file into indexable documents.
///
/// Extracts conversation turns, filters short sessions, and chunks long ones.
/// Returns empty vec for sessions below `min_turns` threshold.
pub fn parse_session_file(
    path: &Path,
    config: &SessionParseConfig,
) -> std::io::Result<Vec<SessionDocument>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);

    let mut records = Vec::new();
    let mut session_id = None;
    let mut git_branch = None;

    for line in reader.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }

        // Try to extract session metadata from any record
        if session_id.is_none() || git_branch.is_none() {
            if let Ok(raw) = serde_json::from_str::<RawRecordMeta>(&line) {
                if session_id.is_none() {
                    if let Some(id) = raw.session_id {
                        session_id = Some(id);
                    }
                }
                if git_branch.is_none() {
                    if let Some(branch) = raw.git_branch {
                        if !branch.is_empty() {
                            git_branch = Some(branch);
                        }
                    }
                }
            }
        }

        if let Some(record) = parse_record(&line) {
            records.push(record);
        }
    }

    // Count user turns
    let user_turn_count = records.iter().filter(|r| r.role == "user").count();
    if user_turn_count < config.min_turns {
        return Ok(Vec::new());
    }

    let sid = session_id.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string()
    });

    let first_ts = records.first().and_then(|r| r.timestamp.clone());
    let last_ts = records.last().and_then(|r| r.timestamp.clone());

    let metadata = SessionMetadata {
        session_id: sid.clone(),
        git_branch,
        first_timestamp: first_ts,
        last_timestamp: last_ts,
        turn_count: user_turn_count,
    };

    if records.len() <= config.chunk_size {
        let content = format_records(&records);
        Ok(vec![SessionDocument {
            relative_path: sid,
            content,
            metadata,
        }])
    } else {
        let mut docs = Vec::new();
        let mut start = 0;
        let mut chunk_idx = 0u32;

        while start < records.len() {
            let end = (start + config.chunk_size).min(records.len());
            let chunk = &records[start..end];
            let content = format_records(chunk);

            docs.push(SessionDocument {
                relative_path: format!("{}-chunk-{:03}", sid, chunk_idx),
                content,
                metadata: metadata.clone(),
            });

            chunk_idx += 1;
            let advance = config.chunk_size.saturating_sub(config.chunk_overlap);
            if advance == 0 {
                break;
            }
            start += advance;
        }

        Ok(docs)
    }
}

/// Format records into markdown content.
fn format_records(records: &[SessionRecord]) -> String {
    let mut content = String::new();
    for record in records {
        let heading = match record.role.as_str() {
            "user" => "User",
            "assistant" => "Assistant",
            other => other,
        };
        content.push_str(&format!("## {}\n{}\n\n", heading, record.text));
    }
    content
}

// =============================================================================
// Internal deserialization types
// =============================================================================

#[derive(Deserialize)]
struct RawRecord {
    #[serde(rename = "type")]
    record_type: String,
    message: Option<RawMessage>,
    timestamp: Option<String>,
}

/// Lightweight struct to extract metadata without full parsing.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecordMeta {
    session_id: Option<String>,
    git_branch: Option<String>,
}

#[derive(Deserialize)]
struct RawMessage {
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
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_user_text_message() {
        let line = r#"{"type":"user","message":{"role":"user","content":"Hello world"},"timestamp":"2026-01-09T08:43:52.235Z"}"#;
        let record = parse_record(line).expect("should parse user text");
        assert_eq!(record.role, "user");
        assert_eq!(record.text, "Hello world");
        assert_eq!(
            record.timestamp.as_deref(),
            Some("2026-01-09T08:43:52.235Z")
        );
    }

    #[test]
    fn test_parse_assistant_text_blocks() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Here is the answer."}]},"timestamp":"2026-01-09T08:44:00.000Z"}"#;
        let record = parse_record(line).expect("should parse assistant text");
        assert_eq!(record.role, "assistant");
        assert_eq!(record.text, "Here is the answer.");
    }

    #[test]
    fn test_parse_assistant_multiple_text_blocks() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"First part."},{"type":"tool_use","id":"t1","name":"read","input":{}},{"type":"text","text":"Second part."}]}}"#;
        let record = parse_record(line).expect("should parse multiple text blocks");
        assert_eq!(record.text, "First part.\n\nSecond part.");
    }

    #[test]
    fn test_parse_tool_use_only_returns_none() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}}]}}"#;
        assert!(
            parse_record(line).is_none(),
            "tool-only messages should return None"
        );
    }

    #[test]
    fn test_parse_tool_result_user_returns_none() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file contents"}]}}"#;
        assert!(
            parse_record(line).is_none(),
            "tool_result messages should return None"
        );
    }

    #[test]
    fn test_parse_progress_returns_none() {
        let line = r#"{"type":"progress"}"#;
        assert!(parse_record(line).is_none());
    }

    #[test]
    fn test_parse_system_returns_none() {
        let line = r#"{"type":"system","message":{"content":"system prompt"}}"#;
        assert!(parse_record(line).is_none());
    }

    #[test]
    fn test_parse_file_history_snapshot_returns_none() {
        let line = r#"{"type":"file-history-snapshot","messageId":"abc","snapshot":{}}"#;
        assert!(parse_record(line).is_none());
    }

    #[test]
    fn test_parse_queue_operation_returns_none() {
        let line = r#"{"type":"queue-operation","data":{}}"#;
        assert!(parse_record(line).is_none());
    }

    #[test]
    fn test_parse_summary_returns_none() {
        let line = r#"{"type":"summary","message":{"content":"Summary text"}}"#;
        assert!(parse_record(line).is_none());
    }

    #[test]
    fn test_parse_thinking_block_discarded() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"deep thoughts"},{"type":"text","text":"Visible answer."}]}}"#;
        let record = parse_record(line).expect("should parse with thinking");
        assert_eq!(record.text, "Visible answer.");
        assert!(!record.text.contains("deep thoughts"));
    }

    #[test]
    fn test_parse_invalid_json_returns_none() {
        assert!(parse_record("not json at all").is_none());
        assert!(parse_record("").is_none());
        assert!(parse_record("{}").is_none());
    }

    #[test]
    fn test_parse_no_timestamp() {
        let line = r#"{"type":"user","message":{"role":"user","content":"Hello"}}"#;
        let record = parse_record(line).expect("should parse");
        assert!(record.timestamp.is_none());
    }

    // ==================== Path Encoding Tests ====================

    #[test]
    fn test_encode_project_path() {
        assert_eq!(
            encode_project_path("/Users/stefano.straus/Gits/personal/mdkb"),
            "-Users-stefano-straus-Gits-personal-mdkb"
        );
    }

    #[test]
    fn test_encode_project_path_no_dots() {
        assert_eq!(
            encode_project_path("/home/user/project"),
            "-home-user-project"
        );
    }

    #[test]
    fn test_find_session_dir_exists() {
        let dir = tempfile::tempdir().unwrap();
        let encoded = "-Users-test-project";
        std::fs::create_dir(dir.path().join(encoded)).unwrap();

        let result = find_session_dir(dir.path(), "/Users/test/project");
        assert!(result.is_some());
        assert!(result.unwrap().ends_with(encoded));
    }

    #[test]
    fn test_find_session_dir_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = find_session_dir(dir.path(), "/nonexistent/path");
        assert!(result.is_none());
    }

    // ==================== Session-to-Document Converter Tests ====================

    fn write_session_file(dir: &Path, id: &str, turns: usize) -> std::path::PathBuf {
        let path = dir.join(format!("{}.jsonl", id));
        let mut content = String::new();
        for i in 0..turns {
            content.push_str(&format!(
                r#"{{"type":"user","sessionId":"{}","gitBranch":"main","message":{{"role":"user","content":"Question {}"}},"timestamp":"2026-01-09T08:{:02}:00.000Z"}}"#,
                id, i, i * 2
            ));
            content.push('\n');
            content.push_str(&format!(
                r#"{{"type":"assistant","sessionId":"{}","message":{{"role":"assistant","content":[{{"type":"text","text":"Answer {}"}}]}},"timestamp":"2026-01-09T08:{:02}:00.000Z"}}"#,
                id, i, i * 2 + 1
            ));
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_parse_session_file_below_min_turns() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session_file(dir.path(), "sess-short", 2);

        let config = SessionParseConfig::default();
        let docs = parse_session_file(&path, &config).unwrap();
        assert!(
            docs.is_empty(),
            "sessions below min_turns should return empty"
        );
    }

    #[test]
    fn test_parse_session_file_single_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session_file(dir.path(), "sess-normal", 5);

        let config = SessionParseConfig::default();
        let docs = parse_session_file(&path, &config).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].relative_path, "sess-normal");
        assert!(docs[0].content.contains("## User\nQuestion 0"));
        assert!(docs[0].content.contains("## Assistant\nAnswer 4"));
        assert_eq!(docs[0].metadata.session_id, "sess-normal");
        assert_eq!(docs[0].metadata.git_branch.as_deref(), Some("main"));
        assert_eq!(docs[0].metadata.turn_count, 5);
    }

    #[test]
    fn test_parse_session_file_chunked() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session_file(dir.path(), "sess-long", 8);

        let config = SessionParseConfig {
            min_turns: 1,
            chunk_size: 10,
            chunk_overlap: 2,
        };
        let docs = parse_session_file(&path, &config).unwrap();
        assert!(
            docs.len() >= 2,
            "long session should produce multiple chunks"
        );
        assert_eq!(docs[0].relative_path, "sess-long-chunk-000");
        assert_eq!(docs[1].relative_path, "sess-long-chunk-001");
    }

    #[test]
    fn test_parse_session_file_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session_file(dir.path(), "sess-meta", 4);

        let config = SessionParseConfig {
            min_turns: 1,
            ..Default::default()
        };
        let docs = parse_session_file(&path, &config).unwrap();
        assert_eq!(docs[0].metadata.session_id, "sess-meta");
        assert!(docs[0].metadata.first_timestamp.is_some());
        assert!(docs[0].metadata.last_timestamp.is_some());
    }

    #[test]
    fn test_parse_session_file_filters_non_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");
        let mut content = String::new();
        content.push_str("{\"type\":\"progress\"}\n");
        content
            .push_str("{\"type\":\"file-history-snapshot\",\"messageId\":\"x\",\"snapshot\":{}}\n");
        for i in 0..3 {
            content.push_str(&format!(
                r#"{{"type":"user","sessionId":"mixed","message":{{"role":"user","content":"Q{}"}},"timestamp":"2026-01-01T00:0{}:00Z"}}"#,
                i, i
            ));
            content.push('\n');
            content.push_str(&format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"A{}"}}]}},"timestamp":"2026-01-01T00:0{}:30Z"}}"#,
                i, i
            ));
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();

        let config = SessionParseConfig::default();
        let docs = parse_session_file(&path, &config).unwrap();
        assert_eq!(docs.len(), 1);
        assert!(!docs[0].content.contains("progress"));
        assert!(!docs[0].content.contains("snapshot"));
    }

    #[test]
    fn test_session_parse_config_defaults() {
        let config = SessionParseConfig::default();
        assert_eq!(config.min_turns, 3);
        assert_eq!(config.chunk_size, 10);
        assert_eq!(config.chunk_overlap, 2);
    }

    #[test]
    fn test_format_records() {
        let records = vec![
            SessionRecord {
                role: "user".into(),
                text: "Hello".into(),
                timestamp: None,
            },
            SessionRecord {
                role: "assistant".into(),
                text: "Hi there".into(),
                timestamp: None,
            },
        ];
        let content = format_records(&records);
        assert_eq!(content, "## User\nHello\n\n## Assistant\nHi there\n\n");
    }
}
