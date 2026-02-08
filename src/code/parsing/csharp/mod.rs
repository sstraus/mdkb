//! C# language parser implementation using tree-sitter-c-sharp 0.23.

use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::{Node, Parser};

pub struct CSharpParser {
    parser: Parser,
    context: ParserContext,
}

impl std::fmt::Debug for CSharpParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CSharpParser")
            .field("language", &"C#")
            .finish()
    }
}

impl CSharpParser {
    pub fn new() -> Result<Self, String> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .map_err(|e| format!("Failed to set C# language: {e}"))?;

        Ok(Self {
            parser,
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
        let tree = match self.parser.parse(code, None) {
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
            "class_declaration" | "record_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(ref n) = name {
                    let doc = extract_csharp_doc(&node, code);
                    let vis = determine_csharp_visibility(node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        n.clone(),
                        SymbolKind::Class,
                        file_id,
                        node_range(node),
                        Some(format!("class {n}")),
                        doc,
                        module_path,
                        vis,
                    );
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(name);

                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        self.extract_symbols_from_node(
                            child, code, file_id, counter, symbols, module_path, depth + 1,
                        );
                    }
                }

                self.context.exit_scope();
                self.context.set_current_class(saved_cls);
            }

            "interface_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(ref n) = name {
                    let doc = extract_csharp_doc(&node, code);
                    let vis = determine_csharp_visibility(node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        n.clone(),
                        SymbolKind::Interface,
                        file_id,
                        node_range(node),
                        Some(format!("interface {n}")),
                        doc,
                        module_path,
                        vis,
                    );
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(name);

                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        self.extract_symbols_from_node(
                            child, code, file_id, counter, symbols, module_path, depth + 1,
                        );
                    }
                }

                self.context.exit_scope();
                self.context.set_current_class(saved_cls);
            }

            "struct_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_csharp_doc(&node, code);
                    let vis = determine_csharp_visibility(node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Struct,
                        file_id,
                        node_range(node),
                        Some(format!("struct {name}")),
                        doc,
                        module_path,
                        vis,
                    );
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                for child in node.children(&mut node.walk()) {
                    self.extract_symbols_from_node(
                        child, code, file_id, counter, symbols, module_path, depth + 1,
                    );
                }
                self.context.exit_scope();
            }

            "enum_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_csharp_doc(&node, code);
                    let vis = determine_csharp_visibility(node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Enum,
                        file_id,
                        node_range(node),
                        Some(format!("enum {name}")),
                        doc,
                        module_path,
                        vis,
                    );
                    symbols.push(symbol);
                }
            }

            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                let ns_name = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()]);
                let new_path = match (module_path, ns_name) {
                    ("", Some(ns)) => ns.to_string(),
                    (_, Some(ns)) => format!("{module_path}.{ns}"),
                    _ => module_path.to_string(),
                };

                self.context.enter_scope(ScopeType::Namespace);
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        self.extract_symbols_from_node(
                            child, code, file_id, counter, symbols, &new_path, depth + 1,
                        );
                    }
                } else {
                    // File-scoped namespace: everything after is in this namespace
                    for child in node.children(&mut node.walk()) {
                        self.extract_symbols_from_node(
                            child, code, file_id, counter, symbols, &new_path, depth + 1,
                        );
                    }
                }
                self.context.exit_scope();
            }

            "method_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_csharp_doc(&node, code);
                    let vis = determine_csharp_visibility(node, code);

                    let qualified_name = if let Some(cls) = self.context.current_class() {
                        format!("{cls}.{name}")
                    } else {
                        name.to_string()
                    };

                    let return_type = node
                        .child_by_field_name("type")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("void");
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        SymbolKind::Method,
                        file_id,
                        node_range(node),
                        Some(format!("{return_type} {name}{params}")),
                        doc,
                        module_path,
                        vis,
                    );
                    symbols.push(symbol);
                }
            }

            "constructor_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let vis = determine_csharp_visibility(node, code);
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    let qualified_name = if let Some(cls) = self.context.current_class() {
                        format!("{cls}.{name}")
                    } else {
                        name.to_string()
                    };

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        SymbolKind::Method,
                        file_id,
                        node_range(node),
                        Some(format!("{name}{params}")),
                        None,
                        module_path,
                        vis,
                    );
                    symbols.push(symbol);
                }
            }

            "property_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let vis = determine_csharp_visibility(node, code);

                    let qualified_name = if let Some(cls) = self.context.current_class() {
                        format!("{cls}.{name}")
                    } else {
                        name.to_string()
                    };

                    let type_str = node
                        .child_by_field_name("type")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("?");

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        SymbolKind::Field,
                        file_id,
                        node_range(node),
                        Some(format!("{type_str} {name}")),
                        None,
                        module_path,
                        vis,
                    );
                    symbols.push(symbol);
                }
            }

            "field_declaration" => {
                self.process_field(node, code, file_id, counter, symbols, module_path);
            }

            _ => {
                for child in node.children(&mut node.walk()) {
                    self.extract_symbols_from_node(
                        child, code, file_id, counter, symbols, module_path, depth + 1,
                    );
                }
            }
        }
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
        let vis = determine_csharp_visibility(node, code);
        let is_const = code[node.byte_range()].contains("const ");

        for child in node.children(&mut node.walk()) {
            if child.kind() == "variable_declaration" {
                for declarator in child.children(&mut child.walk()) {
                    if declarator.kind() == "variable_declarator" {
                        if let Some(name_node) = declarator.child_by_field_name("name") {
                            let name = &code[name_node.byte_range()];
                            let kind = if is_const {
                                SymbolKind::Constant
                            } else {
                                SymbolKind::Field
                            };

                            let qualified_name =
                                if let Some(cls) = self.context.current_class() {
                                    format!("{cls}.{name}")
                                } else {
                                    name.to_string()
                                };

                            let symbol = self.create_symbol(
                                counter.next_id(),
                                qualified_name,
                                kind,
                                file_id,
                                node_range(node),
                                None,
                                None,
                                module_path,
                                vis,
                            );
                            symbols.push(symbol);
                        }
                    }
                }
            }
        }
    }

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let tree = match self.parser.parse(code, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        for child in tree.root_node().children(&mut tree.root_node().walk()) {
            if child.kind() == "using_directive" {
                let text = code[child.byte_range()].trim();
                let path = text
                    .trim_start_matches("using ")
                    .trim_start_matches("static ")
                    .trim_end_matches(';')
                    .trim()
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
        imports
    }

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse(code, None) {
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
        let fn_ctx = if node.kind() == "method_declaration" {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn)
        } else {
            current_fn
        };

        if node.kind() == "invocation_expression" {
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
        if matches!(
            node.kind(),
            "class_declaration" | "struct_declaration" | "interface_declaration"
        ) {
            if let Some(name_node) = node.child_by_field_name("name") {
                let type_name = &code[name_node.byte_range()];
                if let Some(bases) = node.child_by_field_name("bases") {
                    for child in bases.children(&mut bases.walk()) {
                        if child.kind() == "identifier" || child.kind() == "generic_name" {
                            results.push((
                                type_name,
                                &code[child.byte_range()],
                                node_range(child),
                            ));
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_implementations_in_node(&child, code, results);
        }
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

fn determine_csharp_visibility(node: Node, code: &str) -> Visibility {
    let text = &code[node.byte_range()];
    let first_line = text.lines().next().unwrap_or("");
    if first_line.contains("public") {
        Visibility::Public
    } else if first_line.contains("internal") {
        Visibility::Crate
    } else if first_line.contains("protected") {
        Visibility::Module
    } else if first_line.contains("private") {
        Visibility::Private
    } else {
        Visibility::Private // C# default is private
    }
}

fn extract_csharp_doc(node: &Node, code: &str) -> Option<String> {
    // C# uses /// XML doc comments
    let mut lines = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(sib) = prev {
        if sib.kind() == "comment" {
            let text = &code[sib.byte_range()];
            if text.starts_with("///") {
                let content = text
                    .trim_start_matches("///")
                    .trim()
                    .trim_start_matches("<summary>")
                    .trim_end_matches("</summary>")
                    .trim();
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

impl LanguageParser for CSharpParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::CSharp
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_csharp_doc(node, code)
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_implementations<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse(code, None) {
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
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
namespace MyApp {
    public class Calculator {
        private int value;

        public Calculator(int initial) {
            value = initial;
        }

        public int Add(int x) {
            return value + x;
        }
    }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator"
            && s.kind == SymbolKind::Class
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator.Add"
            && s.kind == SymbolKind::Method));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator.Calculator"
            && s.kind == SymbolKind::Method));
    }

    #[test]
    fn test_parse_interface_and_enum() {
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
public interface ISerializable {
    string Serialize();
}

public enum Color {
    Red,
    Green,
    Blue
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "ISerializable"
            && s.kind == SymbolKind::Interface));
        assert!(symbols
            .iter()
            .any(|s| s.name.as_ref() == "Color" && s.kind == SymbolKind::Enum));
    }

    #[test]
    fn test_find_using_directives() {
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
using System;
using System.Collections.Generic;
"#;

        let imports = parser.find_imports(code, file_id);
        assert!(imports.iter().any(|i| i.path == "System"));
        assert!(imports
            .iter()
            .any(|i| i.path == "System.Collections.Generic"));
    }

    #[test]
    fn test_namespace_as_module_path() {
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
namespace MyApp.Models {
    public class User {}
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let cls = symbols.iter().find(|s| s.name.as_ref() == "User").unwrap();
        assert_eq!(cls.module_path.as_deref(), Some("MyApp.Models"));
    }
}
