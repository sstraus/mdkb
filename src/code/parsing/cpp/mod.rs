//! C++ language parser implementation using tree-sitter-cpp 0.23.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct CppParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for CppParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CppParser")
            .field("language", &"C++")
            .finish()
    }
}

impl CppParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .map_err(|e| format!("Failed to set C++ language: {e}"))?;

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
        signature: Option<String>,
        doc_comment: Option<String>,
        module_path: &str,
        visibility: Visibility,
    ) -> Symbol {
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
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut symbols = Vec::new();
        self.extract_symbols_from_node(
            tree.root_node(),
            code,
            file_id,
            counter,
            &mut symbols,
            "",
            0,
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
        module_path: &str,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        match node.kind() {
            "function_definition" => {
                if let Some(symbol) =
                    self.process_function(node, code, file_id, counter, module_path)
                {
                    let fn_name = symbol.name.as_ref().to_string();
                    symbols.push(symbol);

                    self.context
                        .enter_scope(ScopeType::Function { hoisting: false });
                    self.context.set_current_function(Some(fn_name));

                    if let Some(body) = node.child_by_field_name("body") {
                        for child in body.children(&mut body.walk()) {
                            self.extract_symbols_from_node(
                                child,
                                code,
                                file_id,
                                counter,
                                symbols,
                                module_path,
                                depth + 1,
                            );
                        }
                    }

                    self.context.exit_scope();
                }
            }

            "class_specifier" | "struct_specifier" => {
                let kind_str = if node.kind() == "class_specifier" {
                    "class"
                } else {
                    "struct"
                };
                let name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(ref n) = name {
                    let doc = extract_cpp_doc(&node, code);
                    let sym_kind = if kind_str == "class" {
                        SymbolKind::Class
                    } else {
                        SymbolKind::Struct
                    };
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        n.clone(),
                        sym_kind,
                        file_id,
                        node_range(node),
                        Some(format!("{kind_str} {n}")),
                        doc,
                        module_path,
                        Visibility::Public,
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
                            module_path,
                            depth + 1,
                        );
                    }
                }

                self.context.exit_scope();
                self.context.set_current_class(saved_cls);
            }

            "namespace_definition" => {
                let ns_name = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()]);
                let new_path = match (module_path, ns_name) {
                    ("", Some(ns)) => ns.to_string(),
                    (_, Some(ns)) => format!("{module_path}::{ns}"),
                    _ => module_path.to_string(),
                };

                self.context.enter_scope(ScopeType::Namespace);
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        self.extract_symbols_from_node(
                            child,
                            code,
                            file_id,
                            counter,
                            symbols,
                            &new_path,
                            depth + 1,
                        );
                    }
                }
                self.context.exit_scope();
            }

            "enum_specifier" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_cpp_doc(&node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Enum,
                        file_id,
                        node_range(node),
                        Some(format!("enum {name}")),
                        doc,
                        module_path,
                        Visibility::Public,
                    );
                    symbols.push(symbol);
                }
            }

            "field_declaration" if self.context.is_in_class() => {
                // Method declarations inside class body
                let has_function_declarator = node
                    .children(&mut node.walk())
                    .any(|c| c.kind() == "function_declarator");

                if has_function_declarator {
                    if let Some(symbol) =
                        self.process_method_decl(node, code, file_id, counter, module_path)
                    {
                        symbols.push(symbol);
                    }
                } else {
                    // Field member
                    self.process_field(node, code, file_id, counter, symbols, module_path);
                }
            }

            "template_declaration" => {
                // Recurse into template body
                for child in node.children(&mut node.walk()) {
                    self.extract_symbols_from_node(
                        child,
                        code,
                        file_id,
                        counter,
                        symbols,
                        module_path,
                        depth + 1,
                    );
                }
            }

            "access_specifier" => {
                // public:, private:, protected: - tracked via context
            }

            _ => {
                for child in node.children(&mut node.walk()) {
                    self.extract_symbols_from_node(
                        child,
                        code,
                        file_id,
                        counter,
                        symbols,
                        module_path,
                        depth + 1,
                    );
                }
            }
        }
    }

    fn process_function(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let declarator = node.child_by_field_name("declarator")?;
        let name = extract_cpp_declarator_name(declarator, code)?;
        let doc = extract_cpp_doc(&node, code);

        let type_node = node.child_by_field_name("type");
        let sig = match type_node {
            Some(t) => format!(
                "{} {}",
                &code[t.byte_range()],
                &code[declarator.byte_range()]
            ),
            None => code[declarator.byte_range()].to_string(),
        };

        let kind = if self.context.is_in_class() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };

        let qualified_name = if let Some(cls) = self.context.current_class() {
            format!("{cls}::{name}")
        } else {
            name.to_string()
        };

        Some(self.create_symbol(
            counter.next_id(),
            qualified_name,
            kind,
            file_id,
            node_range(node),
            Some(sig),
            doc,
            module_path,
            Visibility::Public,
        ))
    }

    fn process_method_decl(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        // Find the function_declarator child
        let func_decl = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "function_declarator")?;
        let name = extract_cpp_declarator_name(func_decl, code)?;
        let doc = extract_cpp_doc(&node, code);
        let visibility = determine_cpp_member_visibility(node, code);

        let qualified_name = if let Some(cls) = self.context.current_class() {
            format!("{cls}::{name}")
        } else {
            name.to_string()
        };

        let sig = code[node.byte_range()]
            .trim_end_matches(';')
            .trim()
            .to_string();

        Some(self.create_symbol(
            counter.next_id(),
            qualified_name,
            SymbolKind::Method,
            file_id,
            node_range(node),
            Some(sig),
            doc,
            module_path,
            visibility,
        ))
    }

    fn process_field(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        for child in node.children(&mut node.walk()) {
            if child.kind() == "field_identifier" {
                let name = &code[child.byte_range()];
                let qualified_name = if let Some(cls) = self.context.current_class() {
                    format!("{cls}::{name}")
                } else {
                    name.to_string()
                };

                let symbol = self.create_symbol(
                    counter.next_id(),
                    qualified_name,
                    SymbolKind::Field,
                    file_id,
                    node_range(node),
                    Some(
                        code[node.byte_range()]
                            .trim_end_matches(';')
                            .trim()
                            .to_string(),
                    ),
                    None,
                    module_path,
                    Visibility::Private,
                );
                symbols.push(symbol);
            }
        }
    }

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.find_calls_in_node(&tree.root_node(), code, None, &mut calls);
        calls
    }

    fn find_calls_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        current_fn: Option<&'a str>,
        calls: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        let fn_ctx = if node.kind() == "function_definition" {
            node.child_by_field_name("declarator")
                .and_then(|d| extract_cpp_declarator_name(d, code))
                .or(current_fn)
        } else {
            current_fn
        };

        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                let target = &code[func.byte_range()];
                if let Some(ctx) = fn_ctx {
                    calls.push((ctx, target, node_range(*node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_calls_in_node(&child, code, fn_ctx, calls);
        }
    }

    fn find_implementations_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        results: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if matches!(node.kind(), "class_specifier" | "struct_specifier") {
            if let Some(name_node) = node.child_by_field_name("name") {
                let class_name = &code[name_node.byte_range()];
                // Look for base_class_clause
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "base_class_clause" {
                        for base in child.children(&mut child.walk()) {
                            if base.kind() == "type_identifier" {
                                results.push((
                                    class_name,
                                    &code[base.byte_range()],
                                    node_range(base),
                                ));
                            }
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_implementations_in_node(&child, code, results);
        }
    }

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        for child in tree.root_node().children(&mut tree.root_node().walk()) {
            if child.kind() == "preproc_include" {
                if let Some(path_node) = child.child_by_field_name("path") {
                    let raw = &code[path_node.byte_range()];
                    let path = raw
                        .trim_start_matches(|c| c == '"' || c == '<')
                        .trim_end_matches(|c| c == '"' || c == '>')
                        .to_string();
                    imports.push(Import {
                        path,
                        alias: None,
                        file_id,
                        is_glob: false,
                        is_type_only: false,
                    });
                }
            }
        }
        imports
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

fn node_range(node: Node) -> Range {
    Range::new(
        node.start_position().row as u32,
        node.start_position().column as u16,
        node.end_position().row as u32,
        node.end_position().column as u16,
    )
}

fn extract_cpp_declarator_name<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "identifier" | "field_identifier" | "destructor_name" => Some(&code[node.byte_range()]),
        "qualified_identifier" => {
            // Return just the rightmost name for display
            node.child_by_field_name("name")
                .and_then(|n| extract_cpp_declarator_name(n, code))
        }
        "pointer_declarator"
        | "reference_declarator"
        | "array_declarator"
        | "parenthesized_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| extract_cpp_declarator_name(d, code)),
        "function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| extract_cpp_declarator_name(d, code)),
        "template_function" => node
            .child_by_field_name("name")
            .and_then(|n| extract_cpp_declarator_name(n, code)),
        "operator_name" => Some(&code[node.byte_range()]),
        _ => None,
    }
}

fn extract_cpp_doc(node: &Node, code: &str) -> Option<String> {
    let sibling = node.prev_sibling()?;
    if sibling.kind() != "comment" {
        return None;
    }
    let text = &code[sibling.byte_range()];
    if text.starts_with("/**") {
        let inner = text
            .trim_start_matches("/**")
            .trim_end_matches("*/")
            .lines()
            .map(|l| l.trim().trim_start_matches('*').trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if inner.is_empty() { None } else { Some(inner) }
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

fn determine_cpp_member_visibility(node: Node, code: &str) -> Visibility {
    // Look at preceding siblings for access specifiers
    let mut prev = node.prev_sibling();
    while let Some(sib) = prev {
        if sib.kind() == "access_specifier" {
            let text = &code[sib.byte_range()];
            if text.contains("public") {
                return Visibility::Public;
            }
            if text.contains("protected") {
                return Visibility::Module;
            }
            if text.contains("private") {
                return Visibility::Private;
            }
        }
        prev = sib.prev_sibling();
    }
    // Default: private for class, public for struct (simplified to private)
    Visibility::Private
}

impl LanguageParser for CppParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Cpp
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_cpp_doc(node, code)
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_implementations<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut results = Vec::new();
        self.find_implementations_in_node(&tree.root_node(), code, &mut results);
        results
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
    fn test_parse_class_with_methods() {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
#include <string>

/** A rectangle class. */
class Rectangle {
public:
    int width;
    int height;

    int area() { return width * height; }
private:
    void reset() { width = 0; height = 0; }
};

void standalone() {}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Rectangle" && s.kind == SymbolKind::Class)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Rectangle::area" && s.kind == SymbolKind::Method)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "standalone" && s.kind == SymbolKind::Function)
        );
    }

    #[test]
    fn test_parse_namespace() {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
namespace mylib {
    void helper() {}
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let func = symbols
            .iter()
            .find(|s| s.name.as_ref() == "helper")
            .unwrap();
        assert_eq!(func.module_path.as_deref(), Some("mylib"));
    }

    #[test]
    fn test_find_inheritance() {
        let mut parser = CppParser::new().unwrap();

        let code = r#"
class Base {};
class Derived : public Base {};
"#;

        let impls = parser.find_implementations(code);
        assert!(
            impls
                .iter()
                .any(|(cls, base, _)| *cls == "Derived" && *base == "Base")
        );
    }

    #[test]
    fn test_find_includes() {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
#include <iostream>
#include "mylib.h"
"#;

        let imports = parser.find_imports(code, file_id);
        assert!(imports.iter().any(|i| i.path == "iostream"));
        assert!(imports.iter().any(|i| i.path == "mylib.h"));
    }
}
