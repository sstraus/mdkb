//! Wiki-link parsing and normalization.

use regex::Regex;

/// A parsed wiki-link from markdown content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiLink {
    /// The link target (file path or name).
    pub target: String,

    /// Optional display text (after |).
    pub display_text: Option<String>,

    /// Line number where the link appears (1-indexed).
    pub line_number: usize,
}

/// Extract all wiki-links from markdown content.
pub fn extract_wiki_links(content: &str) -> Vec<WikiLink> {
    let mut links = Vec::new();

    // Regex for wiki-links: [[target]] or [[target|display]]
    let link_regex = Regex::new(r"\[\[([^\]\|]+)(?:\|([^\]]+))?\]\]").unwrap();

    // Track code blocks and inline code to skip them
    let mut in_code_block = false;

    for (line_idx, line) in content.lines().enumerate() {
        let line_number = line_idx + 1;

        // Check for code block start/end
        if line.trim().starts_with("```") {
            in_code_block = !in_code_block;
            continue;
        }

        if in_code_block {
            continue;
        }

        // Find all wiki-links in this line, excluding those in inline code
        let clean_line = remove_inline_code(line);

        for caps in link_regex.captures_iter(&clean_line) {
            let target = caps.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            let display_text = caps.get(2).map(|m| m.as_str().to_string());

            links.push(WikiLink {
                target,
                display_text,
                line_number,
            });
        }
    }

    links
}

/// Remove inline code (backtick-wrapped) from a line.
fn remove_inline_code(line: &str) -> String {
    let inline_code_regex = Regex::new(r"`[^`]+`").unwrap();
    inline_code_regex.replace_all(line, "").to_string()
}

/// Normalize a wiki-link target to a relative path.
///
/// Handles various formats:
/// - `[[Note]]` -> `Note.md`
/// - `[[folder/Note]]` -> `folder/Note.md`
/// - `[[Note.md]]` -> `Note.md`
/// - `[[Note#heading]]` -> `Note.md` (strips anchor)
pub fn normalize_link_target(target: &str) -> String {
    // Strip anchor if present
    let path = target.split('#').next().unwrap_or(target);

    // Add .md extension if not present
    if path.ends_with(".md") {
        path.to_string()
    } else {
        format!("{path}.md")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_simple_link() {
        let content = "Check out [[MyNote]] for details.";
        let links = extract_wiki_links(content);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "MyNote");
        assert_eq!(links[0].display_text, None);
        assert_eq!(links[0].line_number, 1);
    }

    #[test]
    fn test_extract_link_with_display_text() {
        let content = "See [[MyNote|this note]] for more.";
        let links = extract_wiki_links(content);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "MyNote");
        assert_eq!(links[0].display_text, Some("this note".to_string()));
    }

    #[test]
    fn test_extract_multiple_links() {
        let content = "Links to [[First]] and [[Second]] and [[Third]].";
        let links = extract_wiki_links(content);

        assert_eq!(links.len(), 3);
        assert_eq!(links[0].target, "First");
        assert_eq!(links[1].target, "Second");
        assert_eq!(links[2].target, "Third");
    }

    #[test]
    fn test_extract_links_multiline() {
        let content = "Line 1\n[[Link1]]\nLine 3\n[[Link2]]";
        let links = extract_wiki_links(content);

        assert_eq!(links.len(), 2);
        assert_eq!(links[0].line_number, 2);
        assert_eq!(links[1].line_number, 4);
    }

    #[test]
    fn test_extract_link_with_path() {
        let content = "See [[folder/subfolder/Note]] for details.";
        let links = extract_wiki_links(content);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "folder/subfolder/Note");
    }

    #[test]
    fn test_extract_link_with_extension() {
        let content = "Check [[readme.md]] out.";
        let links = extract_wiki_links(content);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "readme.md");
    }

    #[test]
    fn test_extract_link_with_anchor() {
        let content = "See [[Note#section]] for that part.";
        let links = extract_wiki_links(content);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "Note#section");
    }

    #[test]
    fn test_extract_no_links() {
        let content = "No wiki links here, just [regular](link.md).";
        let links = extract_wiki_links(content);

        assert!(links.is_empty());
    }

    #[test]
    fn test_extract_ignore_code_blocks() {
        let content = "Before\n```\n[[CodeLink]]\n```\nAfter [[RealLink]]";
        let links = extract_wiki_links(content);

        // Should only find RealLink, not CodeLink
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "RealLink");
    }

    #[test]
    fn test_extract_ignore_inline_code() {
        let content = "Use `[[NotALink]]` syntax but [[RealLink]] works.";
        let links = extract_wiki_links(content);

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target, "RealLink");
    }

    // Normalization tests

    #[test]
    fn test_normalize_simple() {
        assert_eq!(normalize_link_target("Note"), "Note.md");
    }

    #[test]
    fn test_normalize_with_extension() {
        assert_eq!(normalize_link_target("Note.md"), "Note.md");
    }

    #[test]
    fn test_normalize_with_path() {
        assert_eq!(normalize_link_target("folder/Note"), "folder/Note.md");
    }

    #[test]
    fn test_normalize_strip_anchor() {
        assert_eq!(normalize_link_target("Note#section"), "Note.md");
    }

    #[test]
    fn test_normalize_path_with_anchor() {
        assert_eq!(
            normalize_link_target("folder/Note#heading"),
            "folder/Note.md"
        );
    }

    #[test]
    fn test_normalize_already_has_extension_and_anchor() {
        assert_eq!(
            normalize_link_target("docs/readme.md#install"),
            "docs/readme.md"
        );
    }
}
