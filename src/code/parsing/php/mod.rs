//! PHP language parser implementation using tree-sitter-php 0.24.

use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;
use crate::code::parsing::caching_parser::CachingParser;

pub struct PhpParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for PhpParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhpParser")
            .field("language", &"PHP")
            .finish()
    }
}

impl PhpParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|e| format!("Failed to set PHP language: {e}"))?;

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
        let ns = extract_php_namespace(tree.root_node(), code);
        let module_path = ns.as_deref().unwrap_or("");

        self.extract_symbols_from_node(
            tree.root_node(),
            code,
            file_id,
            counter,
            &mut symbols,
            module_path,
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
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_phpdoc(&node, code);
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Function,
                        file_id,
                        node_range(node),
                        Some(format!("function {name}{params}")),
                        doc,
                        module_path,
                        Visibility::Public,
                    );
                    symbols.push(symbol);
                }
            }

            "class_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(ref n) = name {
                    let doc = extract_phpdoc(&node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        n.clone(),
                        SymbolKind::Class,
                        file_id,
                        node_range(node),
                        Some(format!("class {n}")),
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
                            child, code, file_id, counter, symbols, module_path, depth + 1,
                        );
                    }
                }

                self.context.exit_scope();
                self.context.set_current_class(saved_cls);
            }

            "interface_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_phpdoc(&node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Interface,
                        file_id,
                        node_range(node),
                        Some(format!("interface {name}")),
                        doc,
                        module_path,
                        Visibility::Public,
                    );
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        self.extract_symbols_from_node(
                            child, code, file_id, counter, symbols, module_path, depth + 1,
                        );
                    }
                }
                self.context.exit_scope();
            }

            "trait_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_phpdoc(&node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Trait,
                        file_id,
                        node_range(node),
                        Some(format!("trait {name}")),
                        doc,
                        module_path,
                        Visibility::Public,
                    );
                    symbols.push(symbol);
                }
            }

            "enum_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Enum,
                        file_id,
                        node_range(node),
                        Some(format!("enum {name}")),
                        None,
                        module_path,
                        Visibility::Public,
                    );
                    symbols.push(symbol);
                }
            }

            "method_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let vis = determine_php_visibility(node, code);
                    let doc = extract_phpdoc(&node, code);
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    let qualified_name = if let Some(cls) = self.context.current_class() {
                        format!("{cls}::{name}")
                    } else {
                        name.to_string()
                    };

                    let kind = if name == "__construct" {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Method
                    };

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        kind,
                        file_id,
                        node_range(node),
                        Some(format!("function {name}{params}")),
                        doc,
                        module_path,
                        vis,
                    );
                    symbols.push(symbol);
                }
            }

            "property_declaration" => {
                self.process_property(node, code, file_id, counter, symbols, module_path);
            }

            "const_declaration" => {
                self.process_const(node, code, file_id, counter, symbols, module_path);
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

    fn process_property(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let vis = determine_php_visibility(node, code);
        for child in node.children(&mut node.walk()) {
            if child.kind() == "property_element" {
                for gc in child.children(&mut child.walk()) {
                    if gc.kind() == "variable_name" {
                        let name = &code[gc.byte_range()];
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

    fn process_const(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        for child in node.children(&mut node.walk()) {
            if child.kind() == "const_element" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let qualified_name = if let Some(cls) = self.context.current_class() {
                        format!("{cls}::{name}")
                    } else {
                        name.to_string()
                    };

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        SymbolKind::Constant,
                        file_id,
                        node_range(node),
                        None,
                        None,
                        module_path,
                        Visibility::Public,
                    );
                    symbols.push(symbol);
                }
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
        let fn_ctx = match node.kind() {
            "function_definition" | "method_declaration" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        if node.kind() == "function_call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                let target = &code[func.byte_range()];
                if let Some(ctx) = fn_ctx {
                    calls.push((ctx, target, node_range(*node)));
                }
            }
        } else if node.kind() == "member_call_expression" {
            if let Some(name_node) = node.child_by_field_name("name") {
                let target = &code[name_node.byte_range()];
                if let Some(ctx) = fn_ctx {
                    calls.push((ctx, target, node_range(*node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_calls_in_node(&child, code, fn_ctx, calls);
        }
    }

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        for child in tree.root_node().children(&mut tree.root_node().walk()) {
            if child.kind() == "namespace_use_declaration" {
                let text = code[child.byte_range()].trim();
                let path = text
                    .trim_start_matches("use ")
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

fn extract_php_namespace(root: Node, code: &str) -> Option<String> {
    for child in root.children(&mut root.walk()) {
        if child.kind() == "namespace_definition" {
            if let Some(name_node) = child.child_by_field_name("name") {
                return Some(code[name_node.byte_range()].to_string());
            }
        }
    }
    None
}

fn determine_php_visibility(node: Node, code: &str) -> Visibility {
    let text = &code[node.byte_range()];
    let first_line = text.lines().next().unwrap_or("");
    if first_line.contains("public") {
        Visibility::Public
    } else if first_line.contains("protected") {
        Visibility::Module
    } else if first_line.contains("private") {
        Visibility::Private
    } else {
        Visibility::Public // PHP default is public
    }
}

fn extract_phpdoc(node: &Node, code: &str) -> Option<String> {
    let sibling = node.prev_sibling()?;
    if sibling.kind() != "comment" {
        return None;
    }
    let text = &code[sibling.byte_range()];
    if !text.starts_with("/**") {
        return None;
    }
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

impl LanguageParser for PhpParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Php
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_phpdoc(node, code)
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
    fn test_parse_class_and_methods() {
        let mut parser = PhpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"<?php

namespace App\Models;

/** A user model. */
class User {
    private string $name;

    public function __construct(string $name) {
        $this->name = $name;
    }

    public function getName(): string {
        return $this->name;
    }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols
            .iter()
            .any(|s| s.name.as_ref() == "User" && s.kind == SymbolKind::Class));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "User::__construct"
            && s.kind == SymbolKind::Method));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "User::getName"
            && s.kind == SymbolKind::Method));
    }

    #[test]
    fn test_parse_interface_and_trait() {
        let mut parser = PhpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"<?php

interface Serializable {
    public function serialize(): string;
}

trait Timestampable {
    public function getCreatedAt(): string {}
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "Serializable"
            && s.kind == SymbolKind::Interface));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Timestampable"
            && s.kind == SymbolKind::Trait));
    }

    #[test]
    fn test_parse_function() {
        let mut parser = PhpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"<?php

function helper(int $x): int {
    return $x * 2;
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        assert!(symbols
            .iter()
            .any(|s| s.name.as_ref() == "helper" && s.kind == SymbolKind::Function));
    }
}
