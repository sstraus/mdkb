//! Language parser trait definition.
//!
//! All language parsers implement [`LanguageParser`] to provide symbol
//! extraction, call graph detection, and relationship finding via
//! tree-sitter AST traversal.

use super::import::Import;
use super::language::Language;
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

    /// Find macro invocations: (invoker_name, macro_name, range).
    ///
    /// Empty by default, because most languages cannot tell one at parse time.
    /// C and C++ are the clear case: `MAX(a, b)` and `max(a, b)` are the same
    /// `call_expression` until the preprocessor has run, so their parsers index
    /// `#define`s as symbols and still report a use of one as a call.
    ///
    /// Where the grammar does distinguish it — Rust's `macro_invocation` — the
    /// invocation belongs here and not in
    /// [`find_calls`](LanguageParser::find_calls): a macro is not a function,
    /// and reporting `assert!` as a call to a missing `assert` is 4 921 wrong
    /// edges in this repository's own index.
    fn find_macro_expansions<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let _ = code;
        Vec::new()
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

/// Is `text` a plain dotted path — `a.b.c`, `System.out.println` — with nothing
/// computed in it?
///
/// The node-based [`receiver_call_target`] is preferable and is used wherever
/// the grammar names its fields. Kotlin's `navigation_expression` and C#'s
/// `member_access_expression` chains do not, so their receivers are judged on
/// the text they span: anything holding a call, an index or an operator is not
/// a name and cannot be a qualifier.
pub fn is_plain_path(text: &str) -> bool {
    !text.is_empty()
        && !text.starts_with('.')
        && !text.ends_with('.')
        && !text.contains("..")
        && text
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '$')
}

/// The call target for a call made through a receiver: `fmt.Println`,
/// `this.helper`, `Namespace.other`.
///
/// Returns the whole path when the receiver is a plain identifier chain, so
/// [`split_call_target`] can keep it as the qualifier. When the receiver is
/// computed — `build().Close()`, `items[0].Close()` — there is no name to
/// qualify with, so only the member is returned and the call resolves as a bare
/// name. Either way an edge is produced: a call the index cannot place is still
/// a call that happened, and dropping it is how Go and TypeScript came to
/// record no method calls at all.
///
/// `receiver_field` and `member_field` are the grammar's field names — Go says
/// `operand`/`field`, TypeScript says `object`/`property` — and `chain_kind` is
/// the node kind that nests, so `a.b.c` is recognised as one path.
pub fn receiver_call_target<'a>(
    node: Node,
    code: &'a str,
    receiver_field: &str,
    member_field: &str,
    chain_kind: &str,
) -> Option<&'a str> {
    let member = node.child_by_field_name(member_field)?;
    if is_identifier_path(node, receiver_field, chain_kind) {
        // From the start of the receiver to the end of the member, not the end
        // of `node`: Java's `method_invocation` and PHP's call expressions span
        // the argument list too, and `System.out.println(helper())` is not a
        // name.
        Some(&code[node.start_byte()..member.end_byte()])
    } else {
        Some(&code[member.byte_range()])
    }
}

/// How a callee with no name is written, when writing it down is worth an edge.
///
/// `handlers[key]()` and `(*fn)()` call something no name can be resolved to,
/// but the call is there, and the expression is how the source refers to it.
/// Recording it keeps "what does this function call" honest — dynamic dispatch
/// shows up instead of vanishing — and it matches no symbol, so it cannot
/// invent an edge to a map or a variable that is not a function.
///
/// Two callees give nothing back. One written over more than one line would put
/// a paragraph where a name goes. One that *is* the function — an IIFE, a
/// lambda literal — calls nothing a reader could look up, and `literal_kinds`
/// names those per grammar.
pub fn unnamed_call_target<'a>(
    node: Node,
    code: &'a str,
    literal_kinds: &[&str],
) -> Option<&'a str> {
    if node.start_position().row != node.end_position().row {
        return None;
    }
    let inner = match node.kind() {
        "parenthesized_expression" => node.named_child(0).unwrap_or(node),
        _ => node,
    };
    if literal_kinds.contains(&inner.kind()) {
        return None;
    }
    Some(code[node.byte_range()].trim())
}

/// Is everything left of the last separator a chain of plain names?
fn is_identifier_path(node: Node, receiver_field: &str, chain_kind: &str) -> bool {
    let Some(receiver) = node.child_by_field_name(receiver_field) else {
        return false;
    };
    match receiver.kind() {
        // One kind per grammar that means "a plain name": PHP writes `name`
        // and `qualified_name`, Java `identifier`, TypeScript `this`.
        "identifier" | "type_identifier" | "field_identifier" | "name" | "qualified_name"
        | "this" | "super" => true,
        kind if kind == chain_kind => is_identifier_path(receiver, receiver_field, chain_kind),
        _ => false,
    }
}

/// Split a call target into the bare name it is indexed under and the qualifier
/// the call site wrote.
///
/// `Store::open` gives `("open", Some("Store"))`, `std::fs::write` gives
/// `("write", Some("std::fs"))`, `open` gives `("open", None)`. The qualifier is
/// what lets a call be narrowed to one owner instead of matching every symbol of
/// that name — and, when it names nothing indexed, what tells an external target
/// apart from a local one. Dropping it to widen the match is measurably wrong:
/// 3041 edges in this repository would gain a target they never called, starting
/// with `std::fs::write` pointing at this crate's own `write`.
///
/// The three separators are taken together rather than per language because a
/// call target only ever holds one of them, and PHP writes both (`\App\Util` and
/// `Util::run` in the same expression).
///
/// A receiver pronoun is not a qualifier: `self.helper()` names no type, so
/// keeping `self` would class every Python method call as external. Stripping it
/// leaves the call unqualified, which enters the cascade at "declared in the
/// calling file" — where the method it means almost always is. The ancestor
/// pronouns — `super`, C#'s `base`, PHP's `parent` — are stripped for the same
/// reason: they name a type the call site never wrote, and the method they reach
/// carries the name that is written, so the name is all there is to match on.
pub fn split_call_target(target: &str) -> (&str, Option<&str>) {
    let split = ["::", "\\", ".", ":"]
        .iter()
        .filter_map(|sep| target.rfind(sep).map(|at| (at, sep.len())))
        // A lone `:` is Lua's method call, `obj:meth`. Inside a `::` it is half
        // of a path separator and splitting there would leave the qualifier
        // ending in a colon.
        .filter(|&(at, len)| len > 1 || !is_inside_double_colon(target, at))
        .max_by_key(|(at, _)| *at);

    let Some((at, sep_len)) = split else {
        return (target, None);
    };
    let (qualifier, name) = (&target[..at], &target[at + sep_len..]);
    if name.is_empty() {
        return (target, None);
    }
    match qualifier {
        "" | "self" | "this" | "Self" | "cls" | "super" | "base" | "parent" => (name, None),
        _ => (name, Some(qualifier)),
    }
}

/// Is the byte at `at` one half of a `::`?
fn is_inside_double_colon(target: &str, at: usize) -> bool {
    let bytes = target.as_bytes();
    bytes.get(at) == Some(&b':')
        && (bytes.get(at + 1) == Some(&b':') || (at > 0 && bytes[at - 1] == b':'))
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
    #[test]
    fn a_qualified_call_keeps_the_owner_the_call_site_named() {
        assert_eq!(split_call_target("Store::open"), ("open", Some("Store")));
        assert_eq!(
            split_call_target("std::fs::write"),
            ("write", Some("std::fs"))
        );
        assert_eq!(split_call_target("os.path.join"), ("join", Some("os.path")));
        assert_eq!(
            split_call_target("\\App\\Util::run"),
            ("run", Some("\\App\\Util"))
        );
    }

    /// Lua writes a method call with a colon. It must not disturb `::`, where
    /// splitting on the second colon would leave `std::fs:` as the qualifier.
    #[test]
    fn a_lua_method_call_splits_on_its_colon() {
        assert_eq!(split_call_target("obj:meth"), ("meth", Some("obj")));
        assert_eq!(
            split_call_target("std::fs::write"),
            ("write", Some("std::fs"))
        );
        assert_eq!(split_call_target("Store::open"), ("open", Some("Store")));
    }

    #[test]
    fn a_bare_call_has_no_qualifier() {
        assert_eq!(split_call_target("open"), ("open", None));
    }

    /// A receiver pronoun names no type. Keeping it would make every method call
    /// on `self` a call to something outside the index.
    #[test]
    fn a_receiver_pronoun_is_not_a_qualifier() {
        assert_eq!(split_call_target("self.helper"), ("helper", None));
        assert_eq!(split_call_target("this.helper"), ("helper", None));
        assert_eq!(split_call_target("Self::helper"), ("helper", None));
        assert_eq!(split_call_target("cls.helper"), ("helper", None));
    }

    /// An ancestor pronoun names a type the call site never wrote, so it cannot
    /// narrow the target the way a written owner does.
    #[test]
    fn an_ancestor_pronoun_is_not_a_qualifier_either() {
        assert_eq!(split_call_target("super._ready"), ("_ready", None));
        assert_eq!(split_call_target("base.Dispose"), ("Dispose", None));
        assert_eq!(split_call_target("parent::run"), ("run", None));
    }

    /// Nothing a parser can emit should come back as an empty name: an edge with
    /// no target name matches every symbol or none, and both are wrong.
    #[test]
    fn a_target_that_is_only_a_separator_is_left_whole() {
        assert_eq!(split_call_target("foo::"), ("foo::", None));
        assert_eq!(split_call_target("."), (".", None));
        assert_eq!(split_call_target(""), ("", None));
    }
}
