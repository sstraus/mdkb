//! Document chunking for embedding generation.
//!
//! Splits documents into smaller chunks for more precise vector search.
//! Supports markdown-aware splitting (on headings) and fixed-size windows.

use crate::config::ChunkingConfig;
use crate::metrics::tokens::count_tokens;

/// A chunk of a document with its context.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Zero-based index within the parent document.
    pub index: usize,
    /// Chunk text content.
    pub content: String,
    /// Heading path for context (e.g., "# API > ## Authentication").
    pub heading_path: Option<String>,
}

/// Split document content into chunks using the configured strategy.
pub fn split(content: &str, config: &ChunkingConfig) -> Vec<Chunk> {
    if content.is_empty() {
        return vec![];
    }

    // If document fits in a single chunk, return it as-is
    let token_count = count_tokens(content);
    if token_count <= config.max_tokens {
        return vec![Chunk {
            index: 0,
            content: content.to_string(),
            heading_path: None,
        }];
    }

    match config.strategy.as_str() {
        "markdown" => split_markdown(content, config),
        "fixed" => split_fixed(content, config),
        other => {
            tracing::warn!("Unknown chunking strategy '{other}', falling back to fixed");
            split_fixed(content, config)
        }
    }
}

/// Split on markdown headings, preserving heading hierarchy as context.
fn split_markdown(content: &str, config: &ChunkingConfig) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut current_text = String::new();
    let mut heading_stack: Vec<(usize, String)> = Vec::new(); // (level, text)
    let mut chunk_index = 0;

    for line in content.lines() {
        let heading_level = heading_level(line);

        if heading_level > 0 {
            // Flush current chunk if non-empty
            if !current_text.trim().is_empty() {
                let heading_path = format_heading_path(&heading_stack, config.include_header_path);
                chunks.push(Chunk {
                    index: chunk_index,
                    content: current_text.trim().to_string(),
                    heading_path,
                });
                chunk_index += 1;
                current_text.clear();
            }

            // Update heading stack: pop headings at same or deeper level
            while heading_stack
                .last()
                .is_some_and(|(lvl, _)| *lvl >= heading_level)
            {
                heading_stack.pop();
            }
            let heading_text = line.trim_start_matches('#').trim().to_string();
            heading_stack.push((heading_level, heading_text));
        }

        current_text.push_str(line);
        current_text.push('\n');

        // If current chunk exceeds max_tokens, flush it
        if count_tokens(&current_text) > config.max_tokens {
            let heading_path = format_heading_path(&heading_stack, config.include_header_path);
            chunks.push(Chunk {
                index: chunk_index,
                content: current_text.trim().to_string(),
                heading_path,
            });
            chunk_index += 1;
            current_text.clear();
        }
    }

    // Flush remaining text
    if !current_text.trim().is_empty() {
        let heading_path = format_heading_path(&heading_stack, config.include_header_path);
        chunks.push(Chunk {
            index: chunk_index,
            content: current_text.trim().to_string(),
            heading_path,
        });
    }

    // Merge very small trailing chunks into the previous one
    merge_small_chunks(&mut chunks, config.max_tokens / 4);

    chunks
}

/// Split into fixed-size token windows with overlap.
fn split_fixed(content: &str, config: &ChunkingConfig) -> Vec<Chunk> {
    let lines: Vec<&str> = content.lines().collect();
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut chunk_index = 0;

    while start < lines.len() {
        let mut end = start;
        let mut text = String::new();

        // Add lines until we hit max_tokens
        while end < lines.len() {
            let candidate = if text.is_empty() {
                lines[end].to_string()
            } else {
                format!("{}\n{}", text, lines[end])
            };

            if count_tokens(&candidate) > config.max_tokens && !text.is_empty() {
                break;
            }
            text = candidate;
            end += 1;
        }

        if !text.is_empty() {
            chunks.push(Chunk {
                index: chunk_index,
                content: text,
                heading_path: None,
            });
            chunk_index += 1;
        }

        // Move start forward, accounting for overlap
        let overlap_lines = find_overlap_start(&lines, end, config.overlap_tokens);
        start = if overlap_lines > start {
            overlap_lines
        } else {
            end
        };

        // Safety: prevent infinite loop
        if start == end && end < lines.len() {
            start = end + 1;
        }
    }

    chunks
}

/// Find the line index where the overlap window starts.
fn find_overlap_start(lines: &[&str], end: usize, overlap_tokens: usize) -> usize {
    if overlap_tokens == 0 || end == 0 {
        return end;
    }

    let mut tokens = 0;
    let mut overlap_start = end;
    for i in (0..end).rev() {
        tokens += count_tokens(lines[i]);
        if tokens >= overlap_tokens {
            break;
        }
        overlap_start = i;
    }
    overlap_start
}

/// Detect markdown heading level (1-6), or 0 if not a heading.
fn heading_level(line: &str) -> usize {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return 0;
    }
    let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
    if hashes > 6 {
        return 0;
    }
    // Must be followed by space or end of line
    if trimmed.len() > hashes && !trimmed.as_bytes()[hashes].is_ascii_whitespace() {
        return 0;
    }
    hashes
}

/// Format heading stack into a path string.
fn format_heading_path(stack: &[(usize, String)], include: bool) -> Option<String> {
    if !include || stack.is_empty() {
        return None;
    }
    Some(
        stack
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" > "),
    )
}

/// Merge chunks smaller than min_tokens into the previous chunk.
fn merge_small_chunks(chunks: &mut Vec<Chunk>, min_tokens: usize) {
    if chunks.len() <= 1 {
        return;
    }

    let mut i = 1;
    while i < chunks.len() {
        if count_tokens(&chunks[i].content) < min_tokens {
            let small = chunks.remove(i);
            chunks[i - 1].content.push('\n');
            chunks[i - 1].content.push_str(&small.content);
            // Don't increment i — check the merged chunk again
        } else {
            i += 1;
        }
    }

    // Re-index
    for (idx, chunk) in chunks.iter_mut().enumerate() {
        chunk.index = idx;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> ChunkingConfig {
        ChunkingConfig {
            strategy: "markdown".to_string(),
            max_tokens: 100,
            overlap_tokens: 10,
            include_header_path: true,
        }
    }

    #[test]
    fn test_empty_content_returns_empty() {
        let chunks = split("", &default_config());
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_small_content_single_chunk() {
        let chunks = split("Hello world", &default_config());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "Hello world");
        assert_eq!(chunks[0].index, 0);
    }

    #[test]
    fn test_markdown_split_on_headings() {
        let content = "\
# Introduction

This is the intro paragraph with some text about the project. It contains multiple sentences to ensure it has enough tokens to force chunking. The project is designed to be a knowledge base for AI assistants.

## Getting Started

Here is how you get started with the project. First you need to install dependencies. Then configure the database connection. Finally run the initialization command to set up the schema.

## Advanced Usage

Here is the advanced section with more details about configuration options. You can customize search weights, chunking strategies, and embedding models. Each option has sensible defaults.";

        let mut config = default_config();
        config.max_tokens = 50; // Force splits

        let chunks = split(content, &config);

        assert!(
            chunks.len() >= 2,
            "Should split into multiple chunks, got {}",
            chunks.len()
        );

        // Each chunk should have heading context
        let has_heading_paths = chunks.iter().filter(|c| c.heading_path.is_some()).count();
        assert!(
            has_heading_paths > 0,
            "Some chunks should have heading paths"
        );
    }

    #[test]
    fn test_heading_path_hierarchy() {
        let content = "\
# Top

Some content.

## Sub

More content.

### Deep

Deep content.";

        let mut config = default_config();
        config.max_tokens = 30;

        let chunks = split(content, &config);

        // Find a chunk under "Sub > Deep"
        let deep_chunk = chunks
            .iter()
            .find(|c| c.heading_path.as_ref().is_some_and(|p| p.contains("Deep")));
        if let Some(chunk) = deep_chunk {
            let path = chunk.heading_path.as_ref().unwrap();
            assert!(
                path.contains("Top"),
                "Should include parent heading, got: {path}"
            );
            assert!(
                path.contains("Sub"),
                "Should include parent heading, got: {path}"
            );
        }
    }

    #[test]
    fn test_fixed_strategy() {
        let content = (0..50)
            .map(|i| format!("Line number {} with some text content.", i))
            .collect::<Vec<_>>()
            .join("\n");

        let config = ChunkingConfig {
            strategy: "fixed".to_string(),
            max_tokens: 100,
            overlap_tokens: 20,
            include_header_path: false,
        };

        let chunks = split(&content, &config);

        assert!(
            chunks.len() >= 2,
            "Should split into multiple chunks, got {}",
            chunks.len()
        );

        // Chunks should be indexed sequentially
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i);
        }

        // No heading paths for fixed strategy
        for chunk in &chunks {
            assert!(chunk.heading_path.is_none());
        }
    }

    #[test]
    fn test_fixed_strategy_with_overlap() {
        let lines: Vec<String> = (0..20).map(|i| format!("Unique line {i}")).collect();
        let content = lines.join("\n");

        let config = ChunkingConfig {
            strategy: "fixed".to_string(),
            max_tokens: 30,
            overlap_tokens: 10,
            include_header_path: false,
        };

        let chunks = split(&content, &config);

        // With overlap, some content should appear in adjacent chunks
        if chunks.len() >= 2 {
            let chunk0_lines: Vec<&str> = chunks[0].content.lines().collect();
            let chunk1_lines: Vec<&str> = chunks[1].content.lines().collect();
            let last_of_0 = chunk0_lines.last().unwrap();
            // The overlap means chunk1 should start with lines from near the end of chunk0
            let overlap = chunk1_lines.iter().any(|l| chunk0_lines.contains(l));
            assert!(
                overlap,
                "Adjacent chunks should overlap. Chunk0 ends with: {last_of_0}, Chunk1 starts with: {}",
                chunk1_lines[0]
            );
        }
    }

    #[test]
    fn test_heading_level_detection() {
        assert_eq!(heading_level("# Title"), 1);
        assert_eq!(heading_level("## Sub"), 2);
        assert_eq!(heading_level("### Deep"), 3);
        assert_eq!(heading_level("###### Max"), 6);
        assert_eq!(heading_level("Not a heading"), 0);
        assert_eq!(heading_level("#No space"), 0);
        assert_eq!(heading_level("####### Too many"), 0);
        assert_eq!(heading_level(""), 0);
    }

    #[test]
    fn test_chunks_cover_all_content() {
        let content = "\
# Section A

Content A here.

# Section B

Content B here.

# Section C

Content C here.";

        let mut config = default_config();
        config.max_tokens = 30;

        let chunks = split(content, &config);
        let all_text: String = chunks
            .iter()
            .map(|c| c.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(all_text.contains("Content A"), "Should contain Content A");
        assert!(all_text.contains("Content B"), "Should contain Content B");
        assert!(all_text.contains("Content C"), "Should contain Content C");
    }
}
