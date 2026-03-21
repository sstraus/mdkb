//! Language parser trait definition.
//!
//! All language parsers implement [`LanguageParser`] to provide symbol
//! extraction, call graph detection, and relationship finding via
//! tree-sitter AST traversal.

use super::import::Import;
use super::language::Language;
use super::method_call::MethodCall;
use crate::code::symbol::Symbol;
use crate::code::types::{FileId, Range, SymbolCounter};

use tree_sitter::Node;

/// Common interface for all language parsers.
///
/// Each language (Rust, Go, TypeScript, Python, ...) implements this
/// trait to extract symbols and relationships from source code.
pub trait LanguageParser: Send {
    /// Parse source code and extract symbols.
    fn parse(
        &mut self,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
    ) -> Vec<Symbol>;

    /// Which language this parser handles.
    fn language(&self) -> Language;

    /// Extract documentation comment for an AST node.
    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String>;

    /// Find function/method calls: (caller_name, callee_name, range).
    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)>;

    /// Find method calls with structured receiver information.
    ///
    /// Default converts from [`find_calls`](LanguageParser::find_calls).
    fn find_method_calls(&mut self, code: &str) -> Vec<MethodCall> {
        self.find_calls(code)
            .into_iter()
            .map(|(caller, target, range)| MethodCall::new(caller, target, range))
            .collect()
    }

    /// Find trait/interface implementations: (type_name, trait_name, range).
    fn find_implementations<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)>;

    /// Find inheritance (extends): (derived, base, range).
    fn find_extends<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    /// Find type usage (fields, params, returns): (context_name, used_type, range).
    fn find_uses<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)>;

    /// Find method definitions (in traits/types): (definer_name, method_name, range).
    fn find_defines<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)>;

    /// Find import statements.
    fn find_imports(&mut self, code: &str, file_id: FileId) -> Vec<Import>;

    /// Extract variable-to-type bindings: (var_name, type_name, range).
    fn find_variable_types<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    /// Find methods defined directly on types (not via traits):
    /// (type_name, method_name, range).
    fn find_inherent_methods(&mut self, _code: &str) -> Vec<(String, String, Range)> {
        Vec::new()
    }
}

/// Maximum recursion depth for AST traversal to prevent stack overflow.
pub const MAX_AST_DEPTH: usize = 500;

/// Check if recursion depth exceeds safe limits.
///
/// Returns `true` if safe to continue, `false` if limit exceeded.
#[inline]
pub fn check_recursion_depth(depth: usize, node: Node) -> bool {
    if depth > MAX_AST_DEPTH {
        tracing::warn!(
            "[parser] max AST depth ({MAX_AST_DEPTH}) exceeded at {}:{}",
            node.start_position().row + 1,
            node.start_position().column + 1,
        );
        return false;
    }
    true
}

/// Safely truncate a UTF-8 string at a character boundary.
#[inline]
pub fn safe_truncate_str(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !s.is_char_boundary(boundary) {
        boundary -= 1;
    }
    &s[..boundary]
}

/// Truncate with ellipsis for display.
#[inline]
pub fn truncate_for_display(s: &str, max_bytes: usize) -> String {
    let truncated = safe_truncate_str(s, max_bytes);
    if truncated.len() < s.len() {
        format!("{truncated}...")
    } else {
        truncated.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_truncate_ascii() {
        assert_eq!(safe_truncate_str("Hello, World!", 7), "Hello, ");
        assert_eq!(safe_truncate_str("Short", 10), "Short");
    }

    #[test]
    fn test_safe_truncate_emoji() {
        // Emoji is 4 bytes - truncating at byte 10 must not split it
        let text = "Status: \u{1f50d} Active";
        let result = safe_truncate_str(text, 10);
        assert!(result.len() <= 10);
        assert_eq!(result, "Status: ");
    }

    #[test]
    fn test_truncate_for_display() {
        assert_eq!(truncate_for_display("This is long", 7), "This is...");
        assert_eq!(truncate_for_display("Short", 10), "Short");
    }
}
