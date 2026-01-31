//! Frontmatter (YAML) parsing from markdown files.

use gray_matter::{Matter, engine::YAML};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An evolution reference from frontmatter (supersedes/updates).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionRef {
    /// Path to the target document (relative to collection root).
    pub path: String,
    /// Optional scope for updates (e.g., "## Token Handling").
    pub scope: Option<String>,
    /// Optional reason for the evolution.
    pub reason: Option<String>,
}

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

    /// Documents this supersedes (complete replacement).
    pub supersedes: Vec<EvolutionRef>,

    /// Documents this updates (partial modification).
    pub updates: Vec<EvolutionRef>,

    /// Documents this corrects (error fixes).
    pub corrects: Vec<EvolutionRef>,

    /// Documents this extends (additive content).
    pub extends: Vec<EvolutionRef>,
}

/// Parse frontmatter from markdown content.
pub fn parse_frontmatter(content: &str) -> ParsedDocument {
    let matter = Matter::<YAML>::new();
    let result = matter.parse(content);

    let frontmatter: Option<Value> = result.data.and_then(|d| d.deserialize().ok());
    let body = result.content.to_string();

    let title = extract_title(frontmatter.as_ref(), &body);
    let tags = extract_tags(frontmatter.as_ref());
    let supersedes = extract_evolution_refs(frontmatter.as_ref(), "supersedes");
    let updates = extract_evolution_refs(frontmatter.as_ref(), "updates");
    let corrects = extract_evolution_refs(frontmatter.as_ref(), "corrects");
    let extends = extract_evolution_refs(frontmatter.as_ref(), "extends");

    ParsedDocument {
        frontmatter,
        body,
        title,
        tags,
        supersedes,
        updates,
        corrects,
        extends,
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

/// Extract evolution references from frontmatter (supersedes, updates, etc.).
///
/// Supports two formats:
/// 1. Array of objects: `[{path: "...", reason: "..."}]`
/// 2. Array of strings: `["path1", "path2"]`
/// 3. Single string: `"path"`
pub fn extract_evolution_refs(frontmatter: Option<&Value>, field: &str) -> Vec<EvolutionRef> {
    let Some(fm) = frontmatter else {
        return Vec::new();
    };

    let Some(value) = fm.get(field) else {
        return Vec::new();
    };

    match value {
        // Array of objects or strings
        Value::Array(arr) => arr.iter().filter_map(|v| parse_evolution_ref(v)).collect(),
        // Single object
        Value::Object(_) => parse_evolution_ref(value).into_iter().collect(),
        // Single string path
        Value::String(s) => vec![EvolutionRef {
            path: s.clone(),
            scope: None,
            reason: None,
        }],
        _ => Vec::new(),
    }
}

/// Parse a single evolution reference from a JSON value.
fn parse_evolution_ref(value: &Value) -> Option<EvolutionRef> {
    match value {
        // Object with path field
        Value::Object(obj) => {
            let path = obj.get("path")?.as_str()?.to_string();
            let scope = obj.get("scope").and_then(|v| v.as_str()).map(String::from);
            let reason = obj.get("reason").and_then(|v| v.as_str()).map(String::from);
            Some(EvolutionRef {
                path,
                scope,
                reason,
            })
        }
        // Simple string path
        Value::String(s) => Some(EvolutionRef {
            path: s.clone(),
            scope: None,
            reason: None,
        }),
        _ => None,
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

    // ==================== Evolution Extraction Tests ====================

    #[test]
    fn test_extract_supersedes_object_array() {
        let content = r#"---
title: Auth API v2
supersedes:
  - path: "docs/auth-api-v1.md"
    reason: "Complete redesign with OAuth2"
  - path: "docs/auth-legacy.md"
    reason: "Deprecated"
---

Content here.
"#;
        let parsed = parse_frontmatter(content);

        assert_eq!(parsed.supersedes.len(), 2);
        assert_eq!(parsed.supersedes[0].path, "docs/auth-api-v1.md");
        assert_eq!(
            parsed.supersedes[0].reason,
            Some("Complete redesign with OAuth2".to_string())
        );
        assert_eq!(parsed.supersedes[1].path, "docs/auth-legacy.md");
    }

    #[test]
    fn test_extract_supersedes_string_array() {
        let content = r#"---
title: New Doc
supersedes:
  - "old-doc.md"
  - "another-old.md"
---

Content.
"#;
        let parsed = parse_frontmatter(content);

        assert_eq!(parsed.supersedes.len(), 2);
        assert_eq!(parsed.supersedes[0].path, "old-doc.md");
        assert!(parsed.supersedes[0].reason.is_none());
    }

    #[test]
    fn test_extract_supersedes_single_string() {
        let content = r#"---
title: New Doc
supersedes: "old-doc.md"
---

Content.
"#;
        let parsed = parse_frontmatter(content);

        assert_eq!(parsed.supersedes.len(), 1);
        assert_eq!(parsed.supersedes[0].path, "old-doc.md");
    }

    #[test]
    fn test_extract_updates_with_scope() {
        let content = "---
title: Security Update
updates:
  - path: \"docs/security.md\"
    scope: \"Token Handling Section\"
    reason: \"Added JWT support\"
---

Content.
";
        let parsed = parse_frontmatter(content);

        assert_eq!(parsed.updates.len(), 1);
        assert_eq!(parsed.updates[0].path, "docs/security.md");
        assert_eq!(
            parsed.updates[0].scope,
            Some("Token Handling Section".to_string())
        );
        assert_eq!(
            parsed.updates[0].reason,
            Some("Added JWT support".to_string())
        );
    }

    #[test]
    fn test_extract_corrects() {
        let content = r#"---
title: Correction
corrects:
  - path: "docs/api.md"
    reason: "Fixed incorrect endpoint documentation"
---

Content.
"#;
        let parsed = parse_frontmatter(content);

        assert_eq!(parsed.corrects.len(), 1);
        assert_eq!(parsed.corrects[0].path, "docs/api.md");
    }

    #[test]
    fn test_extract_extends() {
        let content = r#"---
title: Extended Guide
extends:
  - path: "docs/intro.md"
    reason: "Adding advanced topics"
---

Content.
"#;
        let parsed = parse_frontmatter(content);

        assert_eq!(parsed.extends.len(), 1);
        assert_eq!(parsed.extends[0].path, "docs/intro.md");
    }

    #[test]
    fn test_extract_evolution_missing() {
        let content = r#"---
title: Plain Doc
---

Content.
"#;
        let parsed = parse_frontmatter(content);

        assert!(parsed.supersedes.is_empty());
        assert!(parsed.updates.is_empty());
        assert!(parsed.corrects.is_empty());
        assert!(parsed.extends.is_empty());
    }

    #[test]
    fn test_extract_evolution_no_frontmatter() {
        let content = "# Just a heading\n\nContent.";
        let parsed = parse_frontmatter(content);

        assert!(parsed.supersedes.is_empty());
        assert!(parsed.updates.is_empty());
    }

    #[test]
    fn test_extract_evolution_multiple_types() {
        let content = "---
title: Comprehensive Update
supersedes:
  - \"old-main.md\"
updates:
  - path: \"docs/related.md\"
    scope: \"API Section\"
extends:
  - \"base-doc.md\"
---

Content.
";
        let parsed = parse_frontmatter(content);

        assert_eq!(parsed.supersedes.len(), 1);
        assert_eq!(parsed.updates.len(), 1);
        assert_eq!(parsed.extends.len(), 1);
        assert!(parsed.corrects.is_empty());
    }
}
