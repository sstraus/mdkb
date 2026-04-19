//! Markdown + YAML frontmatter serialization for [`MemoryEntry`].
//!
//! Used by `mdkb memory export` and `mdkb memory import <dir>` to
//! round-trip memory entries through per-entry `.md` files.
//!
//! The YAML header is hand-written with a stable key order and
//! conservative quoting so the output is deterministic and diffable.
//! Parsing delegates to `gray_matter` (already used elsewhere in the
//! codebase) and maps the raw JSON value onto [`MemoryFileMeta`].

use std::borrow::Cow;

use gray_matter::{Matter, engine::YAML};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ErrorKind, Result};
use crate::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType};

/// Parsed + typed frontmatter for a memory entry file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFileMeta {
    pub id: String,
    pub title: String,
    pub entry_type: EntryType,
    #[serde(default)]
    pub source_type: SourceType,
    #[serde(default)]
    pub status: EntryStatus,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub access_count: Option<u64>,
    #[serde(default)]
    pub last_accessed: Option<i64>,
    #[serde(default)]
    pub confirmations: Option<u32>,
    #[serde(default)]
    pub last_confirmed_at: Option<i64>,
    #[serde(default)]
    pub source_path: Option<String>,
    #[serde(default)]
    pub superseded_by: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub due_at: Option<i64>,
}

/// A memory entry as it lives on disk: frontmatter + markdown body.
#[derive(Debug, Clone)]
pub struct MemoryFile {
    pub meta: MemoryFileMeta,
    pub content: String,
}

impl MemoryFile {
    /// Convert this file back into a full [`MemoryEntry`], preserving any
    /// derived counters that happened to be in the frontmatter.
    ///
    /// Callers performing a user-facing import should prefer
    /// [`Self::into_fresh_entry`]: the DB owns counter truth, and imported
    /// entries must start fresh to avoid forging `access_count` from disk.
    pub fn into_entry(self) -> MemoryEntry {
        let now = chrono::Utc::now().timestamp();
        let Self { meta, content } = self;
        MemoryEntry {
            id: meta.id,
            title: meta.title,
            content,
            entry_type: meta.entry_type,
            tags: meta.tags,
            status: meta.status,
            created_at: meta.created_at.unwrap_or(now),
            updated_at: meta.updated_at.unwrap_or(now),
            superseded_by: meta.superseded_by,
            access_count: meta.access_count.unwrap_or(0),
            last_accessed: meta.last_accessed,
            source_path: meta.source_path,
            confirmations: meta.confirmations.unwrap_or(0),
            last_confirmed_at: meta.last_confirmed_at,
            source_type: meta.source_type,
            expires_at: meta.expires_at,
            due_at: meta.due_at,
        }
    }

    /// Convert into a [`MemoryEntry`] with derived counters reset.
    ///
    /// Used by `mdkb memory import <dir>` — counters on disk are a snapshot,
    /// not a source of truth, so a re-imported entry always starts with
    /// `access_count = 0`, no `last_accessed`, `confirmations = 0`.
    pub fn into_fresh_entry(self) -> MemoryEntry {
        MemoryEntry {
            access_count: 0,
            last_accessed: None,
            confirmations: 0,
            last_confirmed_at: None,
            ..self.into_entry()
        }
    }
}

/// Serialize a [`MemoryEntry`] to a markdown file with YAML frontmatter.
///
/// Output layout:
/// ```text
/// ---
/// id: …
/// title: …
/// entry_type: …
/// source_type: …
/// status: …
/// tags: [a, b]
/// created_at: 1700000000
/// …
/// ---
///
/// <content>
/// ```
pub fn to_markdown(entry: &MemoryEntry) -> String {
    let mut out = String::with_capacity(entry.content.len() + 256);
    out.push_str("---\n");
    write_scalar(&mut out, "id", &entry.id);
    write_scalar(&mut out, "title", &entry.title);
    write_scalar(&mut out, "entry_type", &entry.entry_type.to_string());
    write_scalar(&mut out, "source_type", &entry.source_type.to_string());
    write_scalar(&mut out, "status", &entry.status.to_string());
    write_string_array(&mut out, "tags", &entry.tags);
    write_i64(&mut out, "created_at", entry.created_at);
    write_i64(&mut out, "updated_at", entry.updated_at);
    write_u64(&mut out, "access_count", entry.access_count);
    write_opt_i64(&mut out, "last_accessed", entry.last_accessed);
    write_u64(&mut out, "confirmations", u64::from(entry.confirmations));
    write_opt_i64(&mut out, "last_confirmed_at", entry.last_confirmed_at);
    write_opt_scalar(&mut out, "source_path", entry.source_path.as_deref());
    write_opt_scalar(&mut out, "superseded_by", entry.superseded_by.as_deref());
    write_opt_i64(&mut out, "expires_at", entry.expires_at);
    write_opt_i64(&mut out, "due_at", entry.due_at);
    out.push_str("---\n\n");
    out.push_str(&entry.content);
    if !entry.content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Parse a memory file (YAML frontmatter + markdown body).
///
/// Missing optional fields take their defaults. Required fields are
/// `id`, `title`, and `entry_type`; any absence or unknown enum value
/// returns an `InvalidQuery` error.
pub fn from_markdown(text: &str) -> Result<MemoryFile> {
    let matter = Matter::<YAML>::new();
    let parsed = matter.parse(text);
    let data = parsed
        .data
        .ok_or_else(|| ErrorKind::InvalidQuery("missing YAML frontmatter".to_string()))?;
    let value: Value = data
        .deserialize()
        .map_err(|e| ErrorKind::InvalidQuery(format!("frontmatter is not an object: {e}")))?;
    let meta: MemoryFileMeta = serde_json::from_value(value)
        .map_err(|e| ErrorKind::InvalidQuery(format!("invalid frontmatter: {e}")))?;
    let mut content = parsed.content.trim_start_matches('\n').to_string();
    if content.ends_with('\n') {
        content.pop();
    }
    Ok(MemoryFile { meta, content })
}

// ---------- hand-rolled YAML scalar writers ----------

fn write_scalar(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&quote_if_needed(value));
    out.push('\n');
}

/// Omit the key entirely when `value` is `None` — cleaner YAML than
/// `key: null`, and parsing defaults back to `None` anyway.
fn write_opt_scalar(out: &mut String, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        write_scalar(out, key, v);
    }
}

fn write_i64(out: &mut String, key: &str, value: i64) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&value.to_string());
    out.push('\n');
}

fn write_u64(out: &mut String, key: &str, value: u64) {
    out.push_str(key);
    out.push_str(": ");
    out.push_str(&value.to_string());
    out.push('\n');
}

fn write_opt_i64(out: &mut String, key: &str, value: Option<i64>) {
    if let Some(v) = value {
        write_i64(out, key, v);
    }
}

fn write_string_array(out: &mut String, key: &str, values: &[String]) {
    out.push_str(key);
    if values.is_empty() {
        out.push_str(": []\n");
        return;
    }
    out.push_str(": [");
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&quote_if_needed(v));
    }
    out.push_str("]\n");
}

/// Quote a YAML scalar when it contains characters that would confuse a
/// YAML parser or when it would be mistaken for a non-string type.
///
/// Empty strings, reserved booleans/nulls, leading/trailing whitespace,
/// and strings containing `:` `#` `,` `[` `]` `{` `}` `"` `'` `\` `\n`
/// or leading `-`/`?`/`!`/`&`/`*`/`>`/`|` get double-quoted with JSON
/// string escaping.
fn quote_if_needed(value: &str) -> Cow<'_, str> {
    if needs_quoting(value) {
        Cow::Owned(quote_double(value))
    } else {
        Cow::Borrowed(value)
    }
}

fn needs_quoting(value: &str) -> bool {
    let Some(first) = value.chars().next() else {
        return true; // empty → must quote
    };
    // Reserved word → must quote to preserve string type.
    const RESERVED: &[&str] = &[
        "true", "false", "null", "yes", "no", "on", "off", "True", "False", "Null", "YES", "NO",
        "ON", "OFF", "~",
    ];
    if RESERVED.contains(&value) {
        return true;
    }
    // Numeric-looking → quote to keep it a string.
    if value.parse::<f64>().is_ok() {
        return true;
    }
    if matches!(
        first,
        '-' | '?' | '!' | '&' | '*' | '>' | '|' | '%' | '@' | '`'
    ) {
        return true;
    }
    if first.is_whitespace() || value.ends_with(char::is_whitespace) {
        return true;
    }
    value.chars().any(|c| {
        matches!(
            c,
            ':' | '#' | ',' | '[' | ']' | '{' | '}' | '"' | '\'' | '\\' | '\n' | '\r' | '\t'
        ) || c.is_control()
    })
}

fn quote_double(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry() -> MemoryEntry {
        MemoryEntry {
            id: "auth-oauth2".to_string(),
            title: "OAuth2 PKCE Flow".to_string(),
            content: "Always use PKCE for public clients.".to_string(),
            entry_type: EntryType::Topic,
            tags: vec!["auth".to_string(), "security".to_string()],
            status: EntryStatus::Active,
            created_at: 1_700_000_000,
            updated_at: 1_700_000_100,
            superseded_by: None,
            access_count: 3,
            last_accessed: Some(1_700_000_200),
            source_path: Some("docs/auth.md".to_string()),
            confirmations: 1,
            last_confirmed_at: Some(1_700_000_150),
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        }
    }

    #[test]
    fn round_trip_topic_preserves_authored_fields() {
        let entry = sample_entry();
        let text = to_markdown(&entry);
        let parsed = from_markdown(&text).expect("parse");
        let back = parsed.into_entry();

        assert_eq!(back.id, entry.id);
        assert_eq!(back.title, entry.title);
        assert_eq!(back.content, entry.content);
        assert_eq!(back.entry_type, entry.entry_type);
        assert_eq!(back.tags, entry.tags);
        assert_eq!(back.status, entry.status);
        assert_eq!(back.created_at, entry.created_at);
        assert_eq!(back.updated_at, entry.updated_at);
        assert_eq!(back.source_type, entry.source_type);
        assert_eq!(back.source_path, entry.source_path);
        assert_eq!(back.access_count, entry.access_count);
        assert_eq!(back.last_accessed, entry.last_accessed);
        assert_eq!(back.confirmations, entry.confirmations);
        assert_eq!(back.last_confirmed_at, entry.last_confirmed_at);
        assert_eq!(back.expires_at, entry.expires_at);
        assert_eq!(back.due_at, entry.due_at);
    }

    #[test]
    fn empty_tags_render_as_empty_array() {
        let mut entry = sample_entry();
        entry.tags.clear();
        let text = to_markdown(&entry);
        assert!(text.contains("tags: []"));
        let back = from_markdown(&text).unwrap().into_entry();
        assert!(back.tags.is_empty());
    }

    #[test]
    fn title_with_colon_is_quoted_and_round_trips() {
        let mut entry = sample_entry();
        entry.title = "Fix: crash on startup".to_string();
        let text = to_markdown(&entry);
        assert!(text.contains("title: \"Fix: crash on startup\""));
        let back = from_markdown(&text).unwrap().into_entry();
        assert_eq!(back.title, entry.title);
    }

    #[test]
    fn title_with_quotes_and_backslash_is_escaped() {
        let mut entry = sample_entry();
        entry.title = r#"He said "hi" \ back"#.to_string();
        let text = to_markdown(&entry);
        let back = from_markdown(&text).unwrap().into_entry();
        assert_eq!(back.title, entry.title);
    }

    #[test]
    fn tag_containing_comma_is_quoted() {
        let mut entry = sample_entry();
        entry.tags = vec!["one,two".to_string(), "plain".to_string()];
        let text = to_markdown(&entry);
        let back = from_markdown(&text).unwrap().into_entry();
        assert_eq!(back.tags, entry.tags);
    }

    #[test]
    fn reminder_with_due_at_round_trips() {
        let mut entry = sample_entry();
        entry.entry_type = EntryType::Reminder;
        entry.due_at = Some(1_700_999_999);
        let text = to_markdown(&entry);
        assert!(text.contains("due_at: 1700999999"));
        let back = from_markdown(&text).unwrap().into_entry();
        assert_eq!(back.entry_type, EntryType::Reminder);
        assert_eq!(back.due_at, Some(1_700_999_999));
    }

    #[test]
    fn expired_entry_with_expires_at_round_trips() {
        let mut entry = sample_entry();
        entry.expires_at = Some(1_600_000_000);
        let text = to_markdown(&entry);
        assert!(text.contains("expires_at: 1600000000"));
        let back = from_markdown(&text).unwrap().into_entry();
        assert_eq!(back.expires_at, Some(1_600_000_000));
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let minimal = "---\nid: abc\ntitle: My Entry\nentry_type: topic\n---\n\nHello.\n";
        let parsed = from_markdown(minimal).expect("parse");
        assert_eq!(parsed.meta.id, "abc");
        assert_eq!(parsed.meta.title, "My Entry");
        assert_eq!(parsed.meta.entry_type, EntryType::Topic);
        assert_eq!(parsed.meta.source_type, SourceType::UserStatement);
        assert_eq!(parsed.meta.status, EntryStatus::Active);
        assert!(parsed.meta.tags.is_empty());
        assert_eq!(parsed.meta.created_at, None);
        assert_eq!(parsed.meta.access_count, None);
    }

    #[test]
    fn body_without_trailing_newline_is_padded() {
        let mut entry = sample_entry();
        entry.content = "no newline".to_string();
        let text = to_markdown(&entry);
        assert!(text.ends_with("no newline\n"));
    }

    #[test]
    fn numeric_looking_title_gets_quoted() {
        let mut entry = sample_entry();
        entry.title = "42".to_string();
        let text = to_markdown(&entry);
        assert!(text.contains("title: \"42\""));
        let back = from_markdown(&text).unwrap().into_entry();
        assert_eq!(back.title, "42");
    }

    #[test]
    fn reserved_title_gets_quoted() {
        let mut entry = sample_entry();
        entry.title = "true".to_string();
        let text = to_markdown(&entry);
        assert!(text.contains("title: \"true\""));
        let back = from_markdown(&text).unwrap().into_entry();
        assert_eq!(back.title, "true");
    }

    #[test]
    fn missing_frontmatter_errors() {
        let bad = "just content, no frontmatter\n";
        let err = from_markdown(bad).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("frontmatter") || msg.contains("object"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn unknown_entry_type_errors() {
        let bad = "---\nid: abc\ntitle: X\nentry_type: bogus\n---\n\nBody.\n";
        let err = from_markdown(bad).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("entry_type") || msg.contains("bogus"),
            "unexpected error: {msg}"
        );
    }
}
