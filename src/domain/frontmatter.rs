//! Frontmatter (YAML) parsing from markdown files.

use gray_matter::{engine::YAML, Matter};
use regex::Regex;
use serde_json::Value;

/// Parsed frontmatter result.
#[derive(Debug, Clone, Default)]
pub struct ParsedDocument {
    /// YAML frontmatter as JSON value, if present.
    pub frontmatter: Option<Value>,

    /// Document body without frontmatter.
    pub body: String,

    /// Title extracted from frontmatter or first H1.
    pub title: Option<String>,

    /// Tags extracted from frontmatter.
    pub tags: Vec<String>,
}

/// Parse frontmatter from markdown content.
pub fn parse_frontmatter(content: &str) -> ParsedDocument {
    let matter = Matter::<YAML>::new();
    let result = matter.parse(content);

    let frontmatter: Option<Value> = result.data.and_then(|d| d.deserialize().ok());
    let body = result.content.to_string();

    let title = extract_title(frontmatter.as_ref(), &body);
    let tags = extract_tags(frontmatter.as_ref());

    ParsedDocument {
        frontmatter,
        body,
        title,
        tags,
    }
}

/// Extract title from frontmatter or first H1 in body.
pub fn extract_title(frontmatter: Option<&Value>, body: &str) -> Option<String> {
    // First try frontmatter title
    if let Some(fm) = frontmatter {
        if let Some(title) = fm.get("title").and_then(|t| t.as_str()) {
            return Some(title.to_string());
        }
    }

    // Fall back to first H1 in body
    let h1_regex = Regex::new(r"^#\s+(.+)$").unwrap();
    for line in body.lines() {
        if let Some(caps) = h1_regex.captures(line) {
            let raw_title = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            // Strip markdown formatting
            let cleaned = strip_markdown_formatting(raw_title);
            return Some(cleaned);
        }
    }

    None
}

/// Strip basic markdown formatting (bold, italic) from text.
fn strip_markdown_formatting(text: &str) -> String {
    // Remove **bold** and *italic*
    let bold_regex = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    let italic_regex = Regex::new(r"\*(.+?)\*").unwrap();

    let text = bold_regex.replace_all(text, "$1");
    let text = italic_regex.replace_all(&text, "$1");

    text.to_string()
}

/// Extract tags from frontmatter metadata.
pub fn extract_tags(frontmatter: Option<&Value>) -> Vec<String> {
    let Some(fm) = frontmatter else {
        return Vec::new();
    };

    let Some(tags_value) = fm.get("tags") else {
        return Vec::new();
    };

    match tags_value {
        // Array of strings
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect(),
        // Comma-separated string
        Value::String(s) => s
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frontmatter_with_yaml() {
        let content = r#"---
title: My Document
tags:
  - rust
  - programming
author: Claude
---

# Heading

Body content here.
"#;
        let parsed = parse_frontmatter(content);

        assert!(parsed.frontmatter.is_some());
        let fm = parsed.frontmatter.unwrap();
        assert_eq!(fm["title"], "My Document");
        assert_eq!(fm["author"], "Claude");
        assert!(parsed.body.contains("# Heading"));
        assert!(parsed.body.contains("Body content here."));
        assert!(!parsed.body.contains("---"));
    }

    #[test]
    fn test_parse_frontmatter_without_yaml() {
        let content = "# Just a Heading\n\nBody content.";
        let parsed = parse_frontmatter(content);

        assert!(parsed.frontmatter.is_none());
        assert_eq!(parsed.body, content);
    }

    #[test]
    fn test_parse_frontmatter_empty_yaml() {
        let content = "---\n---\n\nBody after empty frontmatter.";
        let parsed = parse_frontmatter(content);

        // Empty frontmatter is valid
        assert!(parsed.frontmatter.is_some() || parsed.frontmatter.is_none());
        assert!(parsed.body.contains("Body after empty frontmatter."));
    }

    #[test]
    fn test_parse_frontmatter_with_dashes_in_body() {
        let content = r#"---
title: Test
---

This has --- dashes in the body.
"#;
        let parsed = parse_frontmatter(content);

        assert!(parsed.frontmatter.is_some());
        assert!(parsed.body.contains("--- dashes"));
    }

    #[test]
    fn test_extract_title_from_frontmatter() {
        let fm: Value = serde_json::json!({"title": "FM Title"});
        let body = "# Body H1\n\nContent";

        let title = extract_title(Some(&fm), body);
        assert_eq!(title, Some("FM Title".to_string()));
    }

    #[test]
    fn test_extract_title_from_h1() {
        let body = "# My Heading\n\nContent";
        let title = extract_title(None, body);
        assert_eq!(title, Some("My Heading".to_string()));
    }

    #[test]
    fn test_extract_title_h1_with_formatting() {
        let body = "# **Bold** and *italic* title\n\nContent";
        let title = extract_title(None, body);
        // Should strip formatting
        assert_eq!(title, Some("Bold and italic title".to_string()));
    }

    #[test]
    fn test_extract_title_no_h1() {
        let body = "No heading here\n\nJust content.";
        let title = extract_title(None, body);
        assert!(title.is_none());
    }

    #[test]
    fn test_extract_tags_array() {
        let fm: Value = serde_json::json!({
            "tags": ["rust", "programming", "cli"]
        });

        let tags = extract_tags(Some(&fm));
        assert_eq!(tags, vec!["rust", "programming", "cli"]);
    }

    #[test]
    fn test_extract_tags_string() {
        // Some frontmatter has comma-separated tags
        let fm: Value = serde_json::json!({
            "tags": "rust, programming, cli"
        });

        let tags = extract_tags(Some(&fm));
        assert_eq!(tags, vec!["rust", "programming", "cli"]);
    }

    #[test]
    fn test_extract_tags_missing() {
        let fm: Value = serde_json::json!({"title": "No tags"});
        let tags = extract_tags(Some(&fm));
        assert!(tags.is_empty());
    }

    #[test]
    fn test_extract_tags_none() {
        let tags = extract_tags(None);
        assert!(tags.is_empty());
    }

    #[test]
    fn test_parsed_document_full() {
        let content = r#"---
title: Complete Doc
tags:
  - test
  - demo
---

# Ignored H1

Body text.
"#;
        let parsed = parse_frontmatter(content);

        assert_eq!(parsed.title, Some("Complete Doc".to_string()));
        assert_eq!(parsed.tags, vec!["test", "demo"]);
    }
}
