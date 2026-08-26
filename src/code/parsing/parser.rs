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
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol>;

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

/// Convert a tree-sitter node's span to a [`Range`].
///
/// Columns are saturated into `u16` rather than truncated with `as u16`: a
/// pathological line wider than 65_535 columns (minified/generated code) would
/// otherwise wrap to a bogus small value (DATA-C1). This is the single shared
/// implementation for all 13 language parsers.
#[inline]
pub fn node_range(node: Node) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range::new(
        start.row as u32,
        start.column.min(u16::MAX as usize) as u16,
        end.row as u32,
        end.column.min(u16::MAX as usize) as u16,
    )
}

/// Strip a `/** ... */` block doc comment to its text: drops the delimiters and
/// the leading `*` on each line, removes blank lines. Returns `None` if empty.
///
/// Shared by the C, C++, Java, Kotlin, PHP, and TypeScript doc extractors, which
/// each carried a byte-identical copy of this (SIMPLE-C duplication).
pub fn strip_block_doc_comment(text: &str) -> Option<String> {
    let inner = text
        .trim_start_matches("/**")
        .trim_end_matches("*/")
        .lines()
        .map(|l| l.trim().trim_start_matches('*').trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if inner.is_empty() { None } else { Some(inner) }
}

/// Doc comment for a C or C++ declaration, read from the comment above it.
///
/// Shared by the C and C++ extractors, which each carried a byte-identical copy.
/// Accepts `/** ... */` and `///`; any other comment yields `None`.
pub fn extract_c_family_doc(node: &Node, code: &str) -> Option<String> {
    let sibling = doc_anchor(*node).prev_sibling()?;
    if sibling.kind() != "comment" {
        return None;
    }
    let text = &code[sibling.byte_range()];
    if text.starts_with("/**") {
        strip_block_doc_comment(text)
    } else if text.starts_with("///") {
        let inner = text.trim_start_matches("///").trim();
        if inner.is_empty() {
            None
        } else {
            Some(inner.to_string())
        }
    } else {
        None
    }
}

/// The node whose previous sibling the doc comment for `node` would be.
///
/// A doc comment is written above the whole declaration, but the node carrying
/// the name often sits inside a wrapper: `/** doc */ template<typename T> T f(T)`
/// puts the comment before the `template_declaration`, so the
/// `function_definition` inside sees the parameter list as its previous sibling
/// and never finds the doc. The same happens to a function guarded by `#ifdef`.
///
/// Climbing stops at the first wrapper holding an earlier declaration: in
/// `/** doc */ #ifdef X  int f(); int g(); #endif` the comment describes `f`,
/// and `g` must not claim it too.
fn doc_anchor(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        let transparent = matches!(
            parent.kind(),
            "template_declaration" | "preproc_ifdef" | "linkage_specification"
        );
        if !transparent || !is_first_item_in(parent, current) {
            break;
        }
        current = parent;
    }
    current
}

/// Does nothing precede `child` inside `parent` except the wrapper's own syntax?
///
/// The allowed kinds are the named parts of the three wrappers above: a template
/// parameter list, the macro name after `#ifdef`, and the `"C"` of a linkage
/// specification. Anything else — a comment, an earlier declaration — means the
/// comment above `parent` was not written about `child`.
fn is_first_item_in(parent: Node, child: Node) -> bool {
    parent
        .children(&mut parent.walk())
        .take_while(|c| c.id() != child.id())
        .all(|c| {
            !c.is_named()
                || matches!(
                    c.kind(),
                    "template_parameter_list" | "identifier" | "string_literal"
                )
        })
}

/// Find a visibility/access keyword among a declaration node's modifier child
/// nodes, checking a `modifiers` container one level deep.
///
/// Matches only exact modifier *token* text — never a substring of an identifier
/// or comment (BUG-C1: `private string publicKey;` must not read as Public).
/// Grammar-agnostic: it accepts any child whose kind contains `modifier`
/// (`modifier`, `modifiers`, `visibility_modifier`, `accessibility_modifier`, …).
pub fn find_modifier_keyword<'a>(node: Node, code: &'a str, keywords: &[&str]) -> Option<&'a str> {
    for child in node.children(&mut node.walk()) {
        if !child.kind().contains("modifier") {
            continue;
        }
        let text = &code[child.byte_range()];
        if keywords.contains(&text) {
            return Some(text);
        }
        // A `modifiers` container holds the individual modifier tokens.
        for grandchild in child.children(&mut child.walk()) {
            let gtext = &code[grandchild.byte_range()];
            if keywords.contains(&gtext) {
                return Some(gtext);
            }
        }
    }
    None
}

/// Last segment of a possibly-qualified, possibly-generic type name
/// (`Outer.Inner` → `Inner`, `\Ns\Bar` → `Bar`, `List<Foo>` → `List`).
///
/// Relationships are stored and resolved by bare name, so a qualified target has
/// to be reduced to the segment a symbol is indexed under or the edge resolves to
/// nothing. Grammar-agnostic: it walks named children rather than fields, because
/// Java, C#, C++ and PHP each name the parts differently (or not at all).
///
/// Type arguments are skipped: `new List<Foo>()` constructs a `List`, and taking
/// the literal last child would name `Foo` instead.
pub fn last_name_segment<'a>(node: Node, code: &'a str) -> &'a str {
    let last = node
        .children(&mut node.walk())
        .filter(|c| c.is_named() && !c.kind().contains("argument"))
        .last();
    match last {
        Some(last) => last_name_segment(last, code),
        None => &code[node.byte_range()],
    }
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
