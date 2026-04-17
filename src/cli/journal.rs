//! Journal parsing and import functionality.
//!
//! Parses Claude Code journal entries (markdown) and converts them to memory entries.

use std::path::Path;

use crate::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType};

/// Parsed sections from a journal entry.
#[derive(Debug, Default)]
pub struct ParsedJournal {
    /// Session ID or title
    pub title: String,
    /// Date string from the journal
    pub date: Option<String>,
    /// Summary section content
    pub summary: Option<String>,
    /// Key findings/insights
    pub key_findings: Vec<String>,
    /// Decisions made
    pub decisions: Vec<String>,
    /// Problems encountered and solutions
    pub problems: Vec<String>,
    /// Recommendations
    pub recommendations: Vec<String>,
    /// Raw sections that didn't match known patterns
    pub other_sections: Vec<(String, String)>,
}

/// Parse a journal markdown file into structured sections.
pub fn parse_journal(content: &str) -> ParsedJournal {
    let mut journal = ParsedJournal::default();

    let mut current_section: Option<String> = None;
    let mut current_content = String::new();
    let mut in_code_block = false;

    for line in content.lines() {
        // Track code blocks to avoid parsing headers inside them
        if line.starts_with("```") {
            in_code_block = !in_code_block;
            current_content.push_str(line);
            current_content.push('\n');
            continue;
        }

        if in_code_block {
            current_content.push_str(line);
            current_content.push('\n');
            continue;
        }

        // Parse title (H1)
        if line.starts_with("# ") && journal.title.is_empty() {
            journal.title = line[2..].trim().to_string();
            continue;
        }

        // Parse date line
        if line.starts_with("**Date:**") || line.starts_with("Date:") {
            let date = line
                .split(':')
                .skip(1)
                .collect::<Vec<_>>()
                .join(":")
                .trim()
                .to_string();
            journal.date = Some(date.trim_matches('*').trim().to_string());
            continue;
        }

        // Parse section headers (H2 or H3)
        if line.starts_with("## ") || line.starts_with("### ") {
            // Save previous section
            if let Some(ref section) = current_section {
                save_section(&mut journal, section, &current_content);
            }

            // Start new section
            let header = if line.starts_with("## ") {
                &line[3..]
            } else {
                &line[4..]
            };
            current_section = Some(header.trim().to_string());
            current_content.clear();
            continue;
        }

        // Skip separator lines
        if line.trim() == "---" {
            continue;
        }

        // Accumulate content
        current_content.push_str(line);
        current_content.push('\n');
    }

    // Save last section
    if let Some(ref section) = current_section {
        save_section(&mut journal, section, &current_content);
    }

    journal
}

fn save_section(journal: &mut ParsedJournal, section_name: &str, content: &str) {
    let content = content.trim();
    if content.is_empty() {
        return;
    }

    let lower = section_name.to_lowercase();

    if lower.contains("summary") {
        journal.summary = Some(content.to_string());
    } else if lower.contains("key") && (lower.contains("finding") || lower.contains("insight")) {
        journal.key_findings = extract_bullet_points(content);
    } else if lower.contains("decision") {
        journal.decisions = extract_bullet_points(content);
    } else if lower.contains("problem") || lower.contains("issue") {
        journal.problems = extract_bullet_points(content);
    } else if lower.contains("recommend") {
        journal.recommendations = extract_bullet_points(content);
    } else {
        journal
            .other_sections
            .push((section_name.to_string(), content.to_string()));
    }
}

fn extract_bullet_points(content: &str) -> Vec<String> {
    let mut points = Vec::new();
    let mut current_point = String::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // New bullet point
        if trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("• ")
            || (trimmed.len() > 2
                && trimmed.chars().next().unwrap().is_ascii_digit()
                && trimmed.contains(". "))
        {
            // Save previous point
            if !current_point.is_empty() {
                points.push(current_point.trim().to_string());
            }
            // Start new point (remove bullet)
            let text = if trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || trimmed.starts_with("• ")
            {
                &trimmed[2..]
            } else {
                // Numbered list: skip "N. "
                trimmed.splitn(2, ". ").nth(1).unwrap_or(trimmed)
            };
            current_point = text.to_string();
        } else if !trimmed.is_empty() && !current_point.is_empty() {
            // Continuation of previous point
            current_point.push(' ');
            current_point.push_str(trimmed);
        } else if !trimmed.is_empty() {
            // Non-bulleted content, treat as single point
            points.push(trimmed.to_string());
        }
    }

    // Save last point
    if !current_point.is_empty() {
        points.push(current_point.trim().to_string());
    }

    points
}

/// Result of a journal import operation.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct JournalImportResult {
    /// IDs of created memory entries
    pub created: Vec<String>,
    /// Skipped sections (reason, section name)
    pub skipped: Vec<(String, String)>,
    /// Source path
    pub source_path: String,
}

/// Convert a parsed journal to memory entries.
pub fn journal_to_memory_entries(
    journal: &ParsedJournal,
    source_path: &Path,
    base_id: &str,
) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();
    let now = chrono::Utc::now().timestamp();
    let source = source_path.to_string_lossy().to_string();

    // Create entry for summary + key findings (most valuable)
    if journal.summary.is_some() || !journal.key_findings.is_empty() {
        let mut content = String::new();

        if let Some(ref summary) = journal.summary {
            content.push_str(summary);
            content.push_str("\n\n");
        }

        if !journal.key_findings.is_empty() {
            content.push_str("## Key Findings\n\n");
            for finding in &journal.key_findings {
                content.push_str("- ");
                content.push_str(finding);
                content.push('\n');
            }
        }

        if !content.trim().is_empty() {
            entries.push(MemoryEntry {
                id: format!("{}-insights", base_id),
                title: format!("{} - Insights", journal.title),
                content: content.trim().to_string(),
                entry_type: EntryType::Topic,
                tags: vec!["journal".to_string(), "insights".to_string()],
                status: EntryStatus::Active,
                created_at: now,
                updated_at: now,
                superseded_by: None,
                access_count: 0,
                last_accessed: None,
                source_path: Some(source.clone()),
                confirmations: 0,

                last_confirmed_at: None,
                source_type: SourceType::UserStatement,
                expires_at: None,
                due_at: None,
            });
        }
    }

    // Create entry for decisions
    if !journal.decisions.is_empty() {
        let mut content = String::new();
        for decision in &journal.decisions {
            content.push_str("- ");
            content.push_str(decision);
            content.push('\n');
        }

        entries.push(MemoryEntry {
            id: format!("{}-decisions", base_id),
            title: format!("{} - Decisions", journal.title),
            content: content.trim().to_string(),
            entry_type: EntryType::Decision,
            tags: vec!["journal".to_string(), "decisions".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: Some(source.clone()),
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        });
    }

    // Create entry for problems/solutions
    if !journal.problems.is_empty() {
        let mut content = String::new();
        for problem in &journal.problems {
            content.push_str("- ");
            content.push_str(problem);
            content.push('\n');
        }

        entries.push(MemoryEntry {
            id: format!("{}-problems", base_id),
            title: format!("{} - Problems & Solutions", journal.title),
            content: content.trim().to_string(),
            entry_type: EntryType::Problem,
            tags: vec!["journal".to_string(), "problems".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: Some(source.clone()),
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        });
    }

    // Create entries for significant other sections
    for (section_name, content) in &journal.other_sections {
        // Skip auto-generated footer
        if section_name.to_lowercase().contains("auto-generated") {
            continue;
        }
        // Skip small sections
        if content.len() < 100 {
            continue;
        }

        let slug = section_name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect::<String>();

        entries.push(MemoryEntry {
            id: format!("{}-{}", base_id, slug),
            title: format!("{} - {}", journal.title, section_name),
            content: content.clone(),
            entry_type: EntryType::Topic,
            tags: vec!["journal".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: Some(source.clone()),
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        });
    }

    entries
}

/// Generate a slug-safe base ID from a file path.
pub fn path_to_base_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("journal")
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_journal() {
        let content = r#"# Session: test-session

**Date:** 2026-01-31

## Summary

This was a productive session.

## Key Findings

- Found a bug in the auth module
- Discovered caching improves performance by 50%

## Decisions Made

- Use Redis for caching
- Implement retry logic

---

*Auto-generated by SessionEnd hook*
"#;

        let journal = parse_journal(content);

        assert_eq!(journal.title, "Session: test-session");
        assert_eq!(journal.date, Some("2026-01-31".to_string()));
        assert_eq!(
            journal.summary,
            Some("This was a productive session.".to_string())
        );
        assert_eq!(journal.key_findings.len(), 2);
        assert!(journal.key_findings[0].contains("bug"));
        assert_eq!(journal.decisions.len(), 2);
    }

    #[test]
    fn test_parse_journal_with_code_blocks() {
        let content = r#"# Research Notes

## Key Findings

- Found this pattern:

```rust
fn example() {
    ## This is not a header
}
```

- Another finding
"#;

        let journal = parse_journal(content);

        // Should have 2 findings, not split by the code block
        assert_eq!(journal.key_findings.len(), 2);
    }

    #[test]
    fn test_journal_to_memory_entries() {
        let content = r#"# Test Session

## Summary

Test summary.

## Key Findings

- Finding 1
- Finding 2
"#;

        let journal = parse_journal(content);
        let entries = journal_to_memory_entries(&journal, Path::new("/test/journal.md"), "test");

        assert_eq!(entries.len(), 1); // Only insights entry
        assert_eq!(entries[0].id, "test-insights");
        assert_eq!(entries[0].entry_type, EntryType::Topic);
        assert!(entries[0].source_path.is_some());
    }

    #[test]
    fn test_path_to_base_id() {
        assert_eq!(
            path_to_base_id(Path::new(
                "/home/user/.claude/journal/2026-01-31-session-1.md"
            )),
            "2026-01-31-session-1"
        );
        assert_eq!(
            path_to_base_id(Path::new("some file with spaces.md")),
            "some-file-with-spaces"
        );
    }
}
