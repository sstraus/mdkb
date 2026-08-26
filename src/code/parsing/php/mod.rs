//! PHP language parser implementation using tree-sitter-php 0.24.

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
        let ns = extract_php_namespace(tree.root_node(), code);
        let module_path = ns.as_deref().unwrap_or("");

        self.extract_symbols_from_node(
            tree.root_node(),
            code,
            file_id,
            counter,
            &mut symbols,
            (module_path, 0),
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
                        (
                            Some(format!("function {name}{params}")),
                            doc,
                            module_path,
                            Visibility::Public,
                        ),
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
                        (
                            Some(format!("class {n}")),
                            doc,
                            module_path,
                            Visibility::Public,
                        ),
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
                        (
                            Some(format!("interface {name}")),
                            doc,
                            module_path,
                            Visibility::Public,
                        ),
                    );
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
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
                        (
                            Some(format!("trait {name}")),
                            doc,
                            module_path,
                            Visibility::Public,
                        ),
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
                        (
                            Some(format!("enum {name}")),
                            None,
                            module_path,
                            Visibility::Public,
                        ),
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

                    let kind = SymbolKind::Method;

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        kind,
                        file_id,
                        node_range(node),
                        (
                            Some(format!("function {name}{params}")),
                            doc,
                            module_path,
                            vis,
                        ),
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
                            (None, None, module_path, vis),
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
                        (None, None, module_path, Visibility::Public),
                    );
                    symbols.push(symbol);
                }
            }
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
        let fn_ctx = match node.kind() {
            "function_definition" | "method_declaration" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        let target = match node.kind() {
            "function_call_expression" => node
                .child_by_field_name("function")
                .map(|n| &code[n.byte_range()]),
            // `$o->method()` and `Foo::bar()` are both named by their `name` field.
            "member_call_expression" | "scoped_call_expression" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()]),
            // Construction is a dependency on the constructed class. `new $cls()`
            // picks its class at run time; naming the variable would invent an edge.
            "object_creation_expression" => node
                .children(&mut node.walk())
                .find(|c| matches!(c.kind(), "name" | "qualified_name"))
                .map(|n| last_name_segment(n, code)),
            _ => None,
        };

        if let (Some(target), Some(ctx)) = (target, fn_ctx) {
            calls.push((ctx, target, node_range(*node)));
        }

        for child in node.children(&mut node.walk()) {
            Self::find_calls_in_node(&child, code, fn_ctx, depth + 1, calls);
        }
    }

    /// Inheritance and interface conformance, which share one traversal because
    /// the grammar puts both clauses on the same declaration node.
    fn find_inheritance_impl<'a>(
        &mut self,
        code: &'a str,
        clause: &str,
    ) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        Self::find_inheritance_in_node(tree.root_node(), code, clause, 0, &mut found);
        found
    }

    fn find_inheritance_in_node<'a>(
        node: Node,
        code: &'a str,
        clause: &str,
        depth: usize,
        found: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        if matches!(
            node.kind(),
            "class_declaration" | "interface_declaration" | "enum_declaration"
        ) {
            if let Some(name) = node.child_by_field_name("name") {
                let derived = &code[name.byte_range()];
                for c in node.children(&mut node.walk()) {
                    if c.kind() != clause {
                        continue;
                    }
                    // Both clauses list their bases as bare `name` children;
                    // `implements` may list several.
                    for base in c.children(&mut c.walk()) {
                        if matches!(base.kind(), "name" | "qualified_name") {
                            found.push((derived, last_name_segment(base, code), node_range(base)));
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_inheritance_in_node(child, code, clause, depth + 1, found);
        }
    }

    fn find_uses_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut uses = Vec::new();
        Self::find_uses_in_node(tree.root_node(), code, "<module>", 0, &mut uses);
        uses
    }

    fn find_uses_in_node<'a>(
        node: Node,
        code: &'a str,
        context: &'a str,
        depth: usize,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        // The innermost named thing owns the types written inside it: a property
        // belongs to its class, a parameter and a return type to their function.
        let context = match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition"
            | "method_declaration" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .unwrap_or(context),
            _ => context,
        };

        match node.kind() {
            "property_declaration" | "simple_parameter" | "property_promotion_parameter" => {
                push_php_types(node.child_by_field_name("type"), context, code, uses);
            }
            "function_definition" | "method_declaration" => {
                push_php_types(node.child_by_field_name("return_type"), context, code, uses);
            }
            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            Self::find_uses_in_node(child, code, context, depth + 1, uses);
        }
    }

    fn find_defines_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut defines = Vec::new();
        Self::find_defines_in_node(tree.root_node(), code, 0, &mut defines);
        defines
    }

    fn find_defines_in_node<'a>(
        node: Node,
        code: &'a str,
        depth: usize,
        defines: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        if matches!(
            node.kind(),
            "class_declaration" | "interface_declaration" | "trait_declaration"
        ) {
            if let (Some(name), Some(body)) = (
                node.child_by_field_name("name"),
                node.child_by_field_name("body"),
            ) {
                let owner = &code[name.byte_range()];
                for member in body.children(&mut body.walk()) {
                    if member.kind() != "method_declaration" {
                        continue;
                    }
                    if let Some(mn) = member.child_by_field_name("name") {
                        defines.push((owner, &code[mn.byte_range()], node_range(member)));
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_defines_in_node(child, code, depth + 1, defines);
        }
    }

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
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

/// Record every class named by a type annotation as a use.
///
/// A declared type is a tree, not a token: `?Cache` wraps a `named_type`, and
/// `Foo|Bar` holds two of them. Only `named_type` nodes are collected, so
/// `int`, `string` and the other primitives — which are built into the language
/// and never indexed as symbols — drop out without a keyword list.
fn push_php_types<'a>(
    type_node: Option<Node>,
    context: &'a str,
    code: &'a str,
    uses: &mut Vec<(&'a str, &'a str, Range)>,
) {
    let Some(type_node) = type_node else {
        return;
    };
    if type_node.kind() == "named_type" {
        uses.push((
            context,
            last_name_segment(type_node, code),
            node_range(type_node),
        ));
        return;
    }
    for child in type_node.children(&mut type_node.walk()) {
        push_php_types(Some(child), context, code, uses);
    }
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
    // Inspect modifier AST nodes, not substrings of the declaration text (BUG-C1).
    match find_modifier_keyword(node, code, &["public", "protected", "private"]) {
        Some("protected") => Visibility::Module,
        Some("private") => Visibility::Private,
        _ => Visibility::Public, // PHP default is public
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
    crate::code::parsing::parser::strip_block_doc_comment(text)
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

    fn find_implementations<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_inheritance_impl(code, "class_interface_clause")
    }

    fn find_extends<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_inheritance_impl(code, "base_clause")
    }

    fn find_uses<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_uses_impl(code)
    }

    fn find_defines<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_defines_impl(code)
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

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "User" && s.kind == SymbolKind::Class)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "User::__construct" && s.kind == SymbolKind::Method)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "User::getName" && s.kind == SymbolKind::Method)
        );
    }

    #[test]
    fn php_visibility_ignores_keyword_substring_in_identifier() {
        // BUG-C1: a private method whose name contains "public" must stay Private.
        let mut parser = PhpParser::new().unwrap();
        let code = r#"<?php
class C {
    private function publicHelper() {}
    public function real() {}
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

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Serializable" && s.kind == SymbolKind::Interface)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Timestampable" && s.kind == SymbolKind::Trait)
        );
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
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "helper" && s.kind == SymbolKind::Function)
        );
    }

    /// `new` and `::` were both invisible: only free calls and `->` calls were
    /// recorded, so static and constructed dependencies vanished.
    #[test]
    fn construction_and_static_calls_produce_call_edges() {
        let mut parser = PhpParser::new().unwrap();

        let code = r#"<?php
class App extends Base {
    function run($cls) {
        $f = new Foo();
        $g = new \Ns\Bar(1);
        $dynamic = new $cls();
        Baz::make();
        parent::__construct();
        helper();
        $this->method();
    }
}
"#;

        let calls = parser.find_calls_impl(code);
        let edges: Vec<(&str, &str)> = calls.iter().map(|(c, t, _)| (*c, *t)).collect();

        assert!(edges.contains(&("run", "Foo")), "new Foo(): {edges:?}");
        assert!(
            edges.contains(&("run", "Bar")),
            "new \\Ns\\Bar(): {edges:?}"
        );
        assert!(edges.contains(&("run", "make")), "static call: {edges:?}");
        assert!(
            edges.contains(&("run", "__construct")),
            "parent call: {edges:?}"
        );
        // `new $cls()` names a class only at run time; guessing a target here
        // would invent an edge to the variable.
        assert!(
            !edges.iter().any(|(_, t)| *t == "cls"),
            "dynamic construction must not invent a target: {edges:?}"
        );
        // Existing behaviour is untouched.
        assert!(edges.contains(&("run", "helper")), "plain call: {edges:?}");
        assert!(edges.contains(&("run", "method")), "member call: {edges:?}");
    }

    /// An interface states a contract and a class states its members; both are
    /// `Defines` edges from the owner to the method.
    #[test]
    fn interface_and_class_methods_are_recorded_as_defines() {
        let mut parser = PhpParser::new().unwrap();

        let code = "<?php\n\
                    interface Countable { public function count(): int; }\n\
                    trait Loggable { public function log(string $m) {} }\n\
                    class Repo implements Countable { public function count(): int { return 0; } }\n";

        let defines: Vec<(&str, &str)> = parser
            .find_defines(code)
            .iter()
            .map(|(c, t, _)| (*c, *t))
            .collect();

        assert!(
            defines.contains(&("Countable", "count")),
            "interface: {defines:?}"
        );
        assert!(defines.contains(&("Loggable", "log")), "trait: {defines:?}");
        assert!(defines.contains(&("Repo", "count")), "class: {defines:?}");
    }

    /// A declared type is a tree: `?Cache` wraps its class and `Foo|Bar` holds
    /// two. Reading only the outer node would miss both.
    #[test]
    fn nullable_and_union_property_types_are_all_recorded() {
        let mut parser = PhpParser::new().unwrap();

        let code = "<?php\n\
                    class Box {\n\
                        public ?Cache $cache = null;\n\
                        private Foo|Bar $either;\n\
                        public function set(int|Baz $v): void {}\n\
                    }\n";

        let uses: Vec<(&str, &str)> = parser
            .find_uses(code)
            .iter()
            .map(|(c, t, _)| (*c, *t))
            .collect();

        assert!(uses.contains(&("Box", "Cache")), "nullable: {uses:?}");
        assert!(
            uses.contains(&("Box", "Foo")) && uses.contains(&("Box", "Bar")),
            "union: {uses:?}"
        );
        assert!(uses.contains(&("set", "Baz")), "union parameter: {uses:?}");
        assert!(
            !uses.iter().any(|(_, t)| matches!(*t, "int" | "void")),
            "a primitive is not a used class: {uses:?}"
        );
    }
}
