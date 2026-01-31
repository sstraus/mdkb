//! Token counting using tiktoken for accurate LLM token estimation.
//!
//! Uses cl100k_base encoding (GPT-3.5/GPT-4/Claude compatible).

use std::sync::OnceLock;
use tiktoken_rs::CoreBPE;

/// Global tokenizer instance (lazy initialization).
static TOKENIZER: OnceLock<CoreBPE> = OnceLock::new();

/// Get the global tokenizer instance.
fn get_tokenizer() -> &'static CoreBPE {
    TOKENIZER.get_or_init(|| {
        tiktoken_rs::cl100k_base().expect("failed to initialize cl100k_base tokenizer")
    })
}

/// Count tokens in a string using cl100k_base encoding.
///
/// This is the standard encoding used by GPT-3.5, GPT-4, and compatible models.
/// Claude uses a similar tokenizer, so counts are approximately accurate.
pub fn count_tokens(text: &str) -> usize {
    get_tokenizer().encode_with_special_tokens(text).len()
}

/// Token counter with tracking and limits.
#[derive(Debug, Clone)]
pub struct TokenCounter {
    /// Maximum allowed tokens (0 = unlimited).
    pub max_tokens: usize,
    /// Current token count.
    pub current_tokens: usize,
}

impl TokenCounter {
    /// Create a new token counter with a maximum limit.
    pub fn new(max_tokens: usize) -> Self {
        Self {
            max_tokens,
            current_tokens: 0,
        }
    }

    /// Create an unlimited token counter.
    pub fn unlimited() -> Self {
        Self::new(0)
    }

    /// Count tokens in text and add to running total.
    pub fn add(&mut self, text: &str) -> usize {
        let tokens = count_tokens(text);
        self.current_tokens += tokens;
        tokens
    }

    /// Check if adding this text would exceed the limit.
    pub fn would_exceed(&self, text: &str) -> bool {
        if self.max_tokens == 0 {
            return false;
        }
        let tokens = count_tokens(text);
        self.current_tokens + tokens > self.max_tokens
    }

    /// Remaining tokens before limit (0 if unlimited).
    pub fn remaining(&self) -> usize {
        if self.max_tokens == 0 {
            usize::MAX
        } else {
            self.max_tokens.saturating_sub(self.current_tokens)
        }
    }

    /// Check if limit has been reached.
    pub fn is_at_limit(&self) -> bool {
        self.max_tokens > 0 && self.current_tokens >= self.max_tokens
    }

    /// Reset the counter.
    pub fn reset(&mut self) {
        self.current_tokens = 0;
    }
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// Truncate text to fit within a token limit.
///
/// Returns the truncated text and whether truncation occurred.
pub fn truncate_to_tokens(text: &str, max_tokens: usize) -> (String, bool) {
    let tokenizer = get_tokenizer();
    let tokens = tokenizer.encode_with_special_tokens(text);

    if tokens.len() <= max_tokens {
        return (text.to_string(), false);
    }

    // Decode only the first max_tokens
    let truncated_tokens = &tokens[..max_tokens];
    let truncated = tokenizer
        .decode(truncated_tokens.to_vec())
        .unwrap_or_else(|_| text.chars().take(max_tokens * 4).collect());

    (truncated, true)
}

/// Truncate text with an ellipsis indicator.
pub fn truncate_with_ellipsis(text: &str, max_tokens: usize) -> String {
    const ELLIPSIS: &str = "\n\n... [truncated]";
    let ellipsis_tokens = count_tokens(ELLIPSIS);

    if max_tokens <= ellipsis_tokens {
        return ELLIPSIS.to_string();
    }

    let (truncated, was_truncated) = truncate_to_tokens(text, max_tokens - ellipsis_tokens);
    if was_truncated {
        format!("{truncated}{ELLIPSIS}")
    } else {
        truncated
    }
}

/// Truncation result with continuation information.
#[derive(Debug, Clone)]
pub struct TruncationResult {
    /// The truncated content.
    pub content: String,
    /// Whether truncation occurred.
    pub truncated: bool,
    /// Last line included (1-indexed), if truncation occurred.
    pub last_line: Option<usize>,
    /// Total lines in the original content.
    pub total_lines: usize,
}

/// Truncate text with line-based continuation guidance.
///
/// When truncation occurs, includes a message telling the LLM how to
/// retrieve the remaining content using line ranges.
pub fn truncate_with_continuation(text: &str, max_tokens: usize, doc_id: i64) -> TruncationResult {
    let total_lines = text.lines().count();

    // Reserve tokens for the continuation message
    // Example: "\n\n[Truncated at line 150 of 500. To continue: mdkb_get(id=123, lines=\"151:250\")]"
    let continuation_template = format!(
        "\n\n[Truncated at line 9999 of {}. To continue: mdkb_get(id={}, lines=\"9999:9999\")]",
        total_lines, doc_id
    );
    let continuation_tokens = count_tokens(&continuation_template) + 5; // buffer

    if max_tokens <= continuation_tokens {
        return TruncationResult {
            content: format!(
                "[Content too large. Use: mdkb_get(id={}, lines=\"1:100\")]",
                doc_id
            ),
            truncated: true,
            last_line: Some(0),
            total_lines,
        };
    }

    let (truncated_content, was_truncated) =
        truncate_to_tokens(text, max_tokens - continuation_tokens);

    if !was_truncated {
        return TruncationResult {
            content: truncated_content,
            truncated: false,
            last_line: None,
            total_lines,
        };
    }

    // Count lines in truncated content
    let last_line = truncated_content.lines().count();

    // Calculate a reasonable next chunk size (aim for ~100 lines or remaining)
    let remaining = total_lines.saturating_sub(last_line);
    let chunk_size = remaining.min(100);
    let next_end = last_line + chunk_size;

    let continuation_msg = format!(
        "\n\n[Truncated at line {} of {}. To continue: mdkb_get(id={}, lines=\"{}:{}\")]",
        last_line,
        total_lines,
        doc_id,
        last_line + 1,
        next_end
    );

    TruncationResult {
        content: format!("{truncated_content}{continuation_msg}"),
        truncated: true,
        last_line: Some(last_line),
        total_lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_tokens_empty() {
        assert_eq!(count_tokens(""), 0);
    }

    #[test]
    fn test_count_tokens_simple() {
        // "hello world" is typically 2 tokens
        let count = count_tokens("hello world");
        assert!(count >= 1 && count <= 4);
    }

    #[test]
    fn test_count_tokens_code() {
        // Code tends to have more tokens
        let code = "fn main() { println!(\"Hello, world!\"); }";
        let count = count_tokens(code);
        assert!(count > 5);
    }

    #[test]
    fn test_token_counter_add() {
        let mut counter = TokenCounter::new(100);
        let tokens = counter.add("hello world");
        assert!(tokens > 0);
        assert_eq!(counter.current_tokens, tokens);
    }

    #[test]
    fn test_token_counter_limit() {
        let mut counter = TokenCounter::new(5);
        counter.add("hello");

        // Long text should exceed limit
        assert!(counter.would_exceed(
            "This is a very long text that should definitely exceed the token limit we set earlier."
        ));
    }

    #[test]
    fn test_token_counter_unlimited() {
        let counter = TokenCounter::unlimited();
        assert!(!counter.would_exceed("any text"));
        assert_eq!(counter.remaining(), usize::MAX);
    }

    #[test]
    fn test_truncate_to_tokens_short() {
        let (result, truncated) = truncate_to_tokens("hello", 100);
        assert_eq!(result, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_to_tokens_long() {
        let long_text = "word ".repeat(1000);
        let (result, truncated) = truncate_to_tokens(&long_text, 10);
        assert!(truncated);
        assert!(count_tokens(&result) <= 10);
    }

    #[test]
    fn test_truncate_with_ellipsis() {
        let long_text = "word ".repeat(1000);
        let result = truncate_with_ellipsis(&long_text, 20);
        assert!(result.ends_with("... [truncated]"));
        assert!(count_tokens(&result) <= 20);
    }

    #[test]
    fn test_counter_reset() {
        let mut counter = TokenCounter::new(100);
        counter.add("hello world");
        assert!(counter.current_tokens > 0);

        counter.reset();
        assert_eq!(counter.current_tokens, 0);
    }

    #[test]
    fn test_truncate_with_continuation_no_truncation() {
        let text = "line 1\nline 2\nline 3";
        let result = truncate_with_continuation(text, 1000, 42);
        assert!(!result.truncated);
        assert!(result.last_line.is_none());
        assert_eq!(result.total_lines, 3);
        assert_eq!(result.content, text);
    }

    #[test]
    fn test_truncate_with_continuation_truncated() {
        let lines: Vec<String> = (1..=500)
            .map(|i| format!("This is line number {}", i))
            .collect();
        let text = lines.join("\n");
        let result = truncate_with_continuation(&text, 100, 123);

        assert!(result.truncated);
        assert!(result.last_line.is_some());
        assert_eq!(result.total_lines, 500);
        // Should contain continuation message with doc ID
        assert!(result.content.contains("mdkb_get"));
        assert!(result.content.contains("id=123"));
        assert!(result.content.contains("lines="));
    }

    #[test]
    fn test_truncate_with_continuation_tiny_limit() {
        let text = "line 1\nline 2\nline 3";
        let result = truncate_with_continuation(text, 10, 999);
        assert!(result.truncated);
        // Should still provide guidance
        assert!(result.content.contains("mdkb_get"));
    }
}
