//! Swift language parser implementation using tree-sitter-swift 0.7.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{
    LanguageParser, check_recursion_depth, find_modifier_keyword, last_name_segment, node_range,
};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct SwiftParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for SwiftParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwiftParser")
            .field("language", &"Swift")
            .finish()
    }
}

impl SwiftParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_swift::LANGUAGE.into())
            .map_err(|e| format!("Failed to set Swift language: {e}"))?;

        Ok(Self {
            parser: CachingParser::new(ts_parser),
            context: ParserContext::new(),
        })
    }

    fn create_symbol(
        &self,
        id: crate::code::types::SymbolId,
        name: String,
        kind: SymbolKind,
        file_id: FileId,
        range: Range,
        tail: (Option<String>, Option<String>, &str, Visibility),
    ) -> Symbol {
        let (signature, doc_comment, module_path, visibility) = tail;
        let mut sym = Symbol::new(id, name, kind, file_id, range);
        sym.visibility = visibility;
        sym.scope_context = Some(self.context.current_scope_context());
        if let Some(sig) = signature {
            sym = sym.with_signature(sig);
        }
        if let Some(doc) = doc_comment {
            sym = sym.with_doc(doc);
        }
        if !module_path.is_empty() {
            sym = sym.with_module_path(module_path);
        }
        sym
    }

    fn parse_symbols(
        &mut self,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
    ) -> Vec<Symbol> {
        self.context = ParserContext::new();
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut symbols = Vec::new();
        self.extract_symbols_from_node(
            tree.root_node(),
            code,
            file_id,
            counter,
            &mut symbols,
            ("", 0),
        );
        symbols
    }

    fn extract_symbols_from_node(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, usize),
    ) {
        let (module_path, depth) = tail;
        if !check_recursion_depth(depth, node) {
            return;
        }

        match node.kind() {
            // tree-sitter-swift uses class_declaration for class, struct, enum
            // and extension alike; the `declaration_kind` field holds the
            // keyword that tells them apart.
            "class_declaration" => {
                let keyword = node
                    .child_by_field_name("declaration_kind")
                    .map(|n| &code[n.byte_range()])
                    .unwrap_or("class");
                let kind = match keyword {
                    "struct" => SymbolKind::Struct,
                    "enum" => SymbolKind::Enum,
                    _ => SymbolKind::Class,
                };

                // An extension names the type it extends, which may be written
                // as a `user_type` rather than a bare identifier.
                let name = node
                    .child_by_field_name("name")
                    .map(|n| last_name_segment(n, code).to_string());

                // An extension adds members to a type declared elsewhere; it
                // declares no type itself. Emitting one would put a phantom
                // Class next to the real symbol and split name resolution.
                if let Some(n) = name.as_ref().filter(|_| keyword != "extension") {
                    let doc = extract_swift_doc(&node, code);
                    let vis = determine_swift_visibility(node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        n.clone(),
                        kind,
                        file_id,
                        node_range(node),
                        (Some(format!("{keyword} {n}")), doc, module_path, vis),
                    );
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(name);

                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        self.extract_symbols_from_node(
                            child,
                            code,
                            file_id,
                            counter,
                            symbols,
                            (module_path, depth + 1),
                        );
                    }
                }

                self.context.exit_scope();
                self.context.set_current_class(saved_cls);
            }

            "protocol_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(ref n) = name {
                    let doc = extract_swift_doc(&node, code);
                    let vis = determine_swift_visibility(node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        n.clone(),
                        SymbolKind::Interface,
                        file_id,
                        node_range(node),
                        (Some(format!("protocol {n}")), doc, module_path, vis),
                    );
                    symbols.push(symbol);
                }

                // The requirements a protocol states are what makes it answer
                // "who implements what"; without walking the body they are lost.
                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(name);

                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        self.extract_symbols_from_node(
                            child,
                            code,
                            file_id,
                            counter,
                            symbols,
                            (module_path, depth + 1),
                        );
                    }
                }

                self.context.exit_scope();
                self.context.set_current_class(saved_cls);
            }

            // Members the grammar leaves unnamed, or names through a nested
            // node rather than a plain identifier.
            "protocol_function_declaration"
            | "protocol_property_declaration"
            | "associatedtype_declaration"
            | "subscript_declaration"
            | "deinit_declaration"
            | "typealias_declaration"
            | "enum_entry" => {
                self.process_member(node, code, file_id, counter, symbols, module_path);
            }

            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_swift_doc(&node, code);
                    let vis = determine_swift_visibility(node, code);

                    let kind = if self.context.is_in_class() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };

                    let sig = first_line(node, code);

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        self.qualified(name),
                        kind,
                        file_id,
                        node_range(node),
                        (Some(sig), doc, module_path, vis),
                    );
                    symbols.push(symbol);
                }
            }

            "init_declaration" => {
                if let Some(cls) = self.context.current_class() {
                    let vis = determine_swift_visibility(node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        format!("{cls}.init"),
                        SymbolKind::Method,
                        file_id,
                        node_range(node),
                        (None, None, module_path, vis),
                    );
                    symbols.push(symbol);
                }
            }

            "property_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let vis = determine_swift_visibility(node, code);

                    let is_let = code[node.byte_range()].starts_with("let ")
                        || code[node.byte_range()].contains(" let ");

                    let kind = if is_let {
                        SymbolKind::Constant
                    } else {
                        SymbolKind::Variable
                    };

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        self.qualified(name),
                        kind,
                        file_id,
                        node_range(node),
                        (None, None, module_path, vis),
                    );
                    symbols.push(symbol);
                }
            }

            _ => {
                for child in node.children(&mut node.walk()) {
                    self.extract_symbols_from_node(
                        child,
                        code,
                        file_id,
                        counter,
                        symbols,
                        (module_path, depth + 1),
                    );
                }
            }
        }
    }

    /// Symbols for members that carry no plain `name` field.
    ///
    /// `subscript` and `deinit` have no name at all in the source and are named
    /// after the keyword that declares them. An enum case may declare several
    /// names in one `case`, so every `name` field is read, not just the first.
    fn process_member(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let vis = determine_swift_visibility(node, code);
        let doc = extract_swift_doc(&node, code);
        let mut push = |name: String, kind: SymbolKind, signature: String| {
            symbols.push(self.create_symbol(
                counter.next_id(),
                self.qualified(&name),
                kind,
                file_id,
                node_range(node),
                (Some(signature), doc.clone(), module_path, vis),
            ));
        };

        let named = |field: &str| {
            node.child_by_field_name(field)
                .map(|n| code[n.byte_range()].to_string())
        };

        match node.kind() {
            "protocol_function_declaration" => {
                if let Some(name) = named("name") {
                    push(name, SymbolKind::Method, first_line(node, code));
                }
            }
            // The requirement is named through a `pattern`, which holds the
            // identifier in its `bound_identifier` field.
            "protocol_property_declaration" => {
                if let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|p| p.child_by_field_name("bound_identifier"))
                    .map(|n| code[n.byte_range()].to_string())
                {
                    push(name, SymbolKind::Field, first_line(node, code));
                }
            }
            "associatedtype_declaration" | "typealias_declaration" => {
                if let Some(name) = named("name") {
                    push(name, SymbolKind::TypeAlias, first_line(node, code));
                }
            }
            "subscript_declaration" => push(
                "subscript".to_string(),
                SymbolKind::Method,
                first_line(node, code),
            ),
            "deinit_declaration" => push("deinit".to_string(), SymbolKind::Method, "deinit".into()),
            "enum_entry" => {
                for name_node in node.children_by_field_name("name", &mut node.walk()) {
                    let name = code[name_node.byte_range()].to_string();
                    push(name.clone(), SymbolKind::Constant, format!("case {name}"));
                }
            }
            _ => {}
        }
    }

    /// Member name qualified by the type currently being walked.
    fn qualified(&self, name: &str) -> String {
        match self.context.current_class() {
            Some(cls) => format!("{cls}.{name}"),
            None => name.to_string(),
        }
    }

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        Self::find_calls_in_node(&tree.root_node(), code, Some("<module>"), 0, &mut calls);
        calls
    }

    fn find_calls_in_node<'a>(
        node: &Node,
        code: &'a str,
        current_fn: Option<&'a str>,
        depth: usize,
        calls: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        let fn_ctx = if node.kind() == "function_declaration" {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn)
        } else {
            current_fn
        };

        if node.kind() == "call_expression" {
            if let Some(func) = node.children(&mut node.walk()).next() {
                let target = &code[func.byte_range()];
                if let Some(ctx) = fn_ctx {
                    calls.push((ctx, target, node_range(*node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_calls_in_node(&child, code, fn_ctx, depth + 1, calls);
        }
    }

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut imports = Vec::new();
        for child in tree.root_node().children(&mut tree.root_node().walk()) {
            if child.kind() == "import_declaration" {
                let text = code[child.byte_range()].trim();
                let path = text.trim_start_matches("import ").trim().to_string();
                imports.push(Import {
                    path,
                    alias: None,
                    file_id,
                    is_glob: false,
                    is_type_only: false,
                });
            }
        }
        imports
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

fn determine_swift_visibility(node: Node, code: &str) -> Visibility {
    // Inspect modifier AST nodes, not substrings of the declaration text (BUG-C1).
    match find_modifier_keyword(
        node,
        code,
        &["public", "open", "internal", "fileprivate", "private"],
    ) {
        Some("public" | "open") => Visibility::Public,
        Some("fileprivate") => Visibility::Module,
        Some("private") => Visibility::Private,
        _ => Visibility::Crate, // Swift default is internal
    }
}

/// First line of a declaration, trimmed of its opening brace.
///
/// Swift signatures are written on one line and the grammar has no field that
/// spans just them, so the source line is the signature.
fn first_line(node: Node, code: &str) -> String {
    code[node.byte_range()]
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('{')
        .trim()
        .to_string()
}

fn extract_swift_doc(node: &Node, code: &str) -> Option<String> {
    // Swift uses /// doc comments
    let mut lines = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(sib) = prev {
        if sib.kind() == "comment" {
            let text = &code[sib.byte_range()];
            if text.starts_with("///") {
                let content = text.trim_start_matches("///").trim();
                if !content.is_empty() {
                    lines.push(content.to_string());
                }
                prev = sib.prev_sibling();
                continue;
            }
        }
        break;
    }

    if lines.is_empty() {
        None
    } else {
        lines.reverse();
        Some(lines.join("\n"))
    }
}

impl LanguageParser for SwiftParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Swift
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_swift_doc(node, code)
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_implementations<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    fn find_uses<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    fn find_defines<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    fn find_imports(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        self.extract_imports_impl(code, file_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_class_and_struct() {
        let mut parser = SwiftParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
import Foundation

/// A point in 2D space.
public struct Point {
    let x: Double
    let y: Double
}

public class Calculator {
    var value: Int = 0

    public func add(_ x: Int) -> Int {
        value += x
        return value
    }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Point" && s.kind == SymbolKind::Struct)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Calculator" && s.kind == SymbolKind::Class)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Calculator.add" && s.kind == SymbolKind::Method)
        );
    }

    #[test]
    fn swift_visibility_ignores_keyword_substring_in_identifier() {
        // BUG-C1: a private member whose name contains "public" must stay Private.
        let mut parser = SwiftParser::new().unwrap();
        let code = r#"
class C {
    private func publicHelper() {}
    public func real() {}
}
"#;
        let symbols =
            parser.parse_symbols(code, FileId::new(1).unwrap(), &mut SymbolCounter::new());
        let ph = symbols
            .iter()
            .find(|s| s.name.as_ref().ends_with("publicHelper"))
            .expect("publicHelper method");
        assert_eq!(ph.visibility, Visibility::Private);
        let real = symbols
            .iter()
            .find(|s| s.name.as_ref().ends_with("real"))
            .expect("real method");
        assert_eq!(real.visibility, Visibility::Public);
    }

    #[test]
    fn test_parse_protocol_and_enum() {
        let mut parser = SwiftParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
protocol Serializable {
    func serialize() -> String
}

enum Color {
    case red, green, blue
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Serializable" && s.kind == SymbolKind::Interface)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Color" && s.kind == SymbolKind::Enum)
        );
    }

    #[test]
    fn test_find_imports() {
        let mut parser = SwiftParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
import Foundation
import UIKit
"#;

        let imports = parser.find_imports(code, file_id);
        assert!(imports.iter().any(|i| i.path == "Foundation"));
        assert!(imports.iter().any(|i| i.path == "UIKit"));
    }

    /// tree-sitter-swift parses `extension Foo` as a class_declaration, the same
    /// node kind as a real class. An extension adds members to a type declared
    /// elsewhere; it declares no type of its own, so emitting one puts a ghost
    /// next to the real symbol and makes name resolution pick between them.
    #[test]
    fn an_extension_declares_no_type_of_its_own() {
        let mut parser = SwiftParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
protocol Greetable {
}

extension Greetable {
    func greet() -> String { return "hi" }
}

class Real {
    func run() {}
}

struct Point {}
enum Shape {}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let found: Vec<(&str, SymbolKind)> =
            symbols.iter().map(|s| (s.name.as_ref(), s.kind)).collect();

        assert_eq!(
            found.iter().filter(|(n, _)| *n == "Greetable").count(),
            1,
            "the protocol and its extension must yield one symbol: {found:?}"
        );
        assert!(
            found.contains(&("Greetable", SymbolKind::Interface)),
            "the surviving symbol is the protocol: {found:?}"
        );
        assert!(
            found.contains(&("Greetable.greet", SymbolKind::Method)),
            "extension members are still attributed to the extended type: {found:?}"
        );

        // Every real declaration still emits its own symbol.
        assert!(
            found.contains(&("Real", SymbolKind::Class)),
            "class: {found:?}"
        );
        assert!(
            found.contains(&("Point", SymbolKind::Struct)),
            "struct: {found:?}"
        );
        assert!(
            found.contains(&("Shape", SymbolKind::Enum)),
            "enum: {found:?}"
        );
    }

    /// A protocol whose requirements are invisible cannot answer who implements
    /// what, and four more member forms produced nothing at all.
    #[test]
    fn protocol_requirements_enum_cases_and_unnamed_members_produce_symbols() {
        let mut parser = SwiftParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
protocol Greetable {
    func greet() -> String
    var name: String { get }
    associatedtype Item
}

class Real {
    subscript(i: Int) -> Int { return i }
    deinit { }
}

typealias Handler = (Int) -> Void

enum Shape {
    case circle
    case rect(w: Int, h: Int)
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let found: Vec<(&str, SymbolKind)> =
            symbols.iter().map(|s| (s.name.as_ref(), s.kind)).collect();
        let has = |name: &str, kind: SymbolKind| found.contains(&(name, kind));

        assert!(
            has("Greetable.greet", SymbolKind::Method),
            "protocol method requirement: {found:?}"
        );
        assert!(
            has("Greetable.name", SymbolKind::Field),
            "protocol property requirement: {found:?}"
        );
        assert!(
            has("Greetable.Item", SymbolKind::TypeAlias),
            "associated type: {found:?}"
        );

        // A subscript and a deinitialiser have no name in the source; they are
        // named after the keyword that declares them.
        assert!(
            has("Real.subscript", SymbolKind::Method),
            "subscript: {found:?}"
        );
        assert!(has("Real.deinit", SymbolKind::Method), "deinit: {found:?}");

        assert!(
            has("Handler", SymbolKind::TypeAlias),
            "typealias: {found:?}"
        );

        assert!(
            has("Shape.circle", SymbolKind::Constant),
            "enum case: {found:?}"
        );
        assert!(
            has("Shape.rect", SymbolKind::Constant),
            "enum case with associated values: {found:?}"
        );
    }
}
