//! Python language parser implementation using tree-sitter-python 0.25.

use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::method_call::MethodCall;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;
use crate::code::parsing::caching_parser::CachingParser;

pub struct PythonParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for PythonParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PythonParser")
            .field("language", &"Python")
            .finish()
    }
}

impl PythonParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_python::LANGUAGE.into())
            .map_err(|e| format!("Failed to set Python language: {e}"))?;

        Ok(Self {
            parser: CachingParser::new(ts_parser),
            context: ParserContext::new(),
        })
    }

    // ── Symbol creation helper ──────────────────────────────────────────

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

    // ── Main parse ──────────────────────────────────────────────────────

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
                let func_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) =
                    self.process_function(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }

                self.context
                    .enter_scope(ScopeType::Function { hoisting: false });
                let saved_fn = self.context.current_function().map(|s| s.to_string());
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_function(func_name);

                if let Some(body) = node.child_by_field_name("body") {
                    self.extract_symbols_from_node(
                        body, code, file_id, counter, symbols, module_path, depth + 1,
                    );
                }

                self.context.exit_scope();
                self.context.set_current_function(saved_fn);
                self.context.set_current_class(saved_cls);
            }

            "class_definition" => {
                let class_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) =
                    self.process_class(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_fn = self.context.current_function().map(|s| s.to_string());
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(class_name);

                if let Some(body) = node.child_by_field_name("body") {
                    self.extract_symbols_from_node(
                        body, code, file_id, counter, symbols, module_path, depth + 1,
                    );
                }

                self.context.exit_scope();
                self.context.set_current_function(saved_fn);
                self.context.set_current_class(saved_cls);
            }

            "expression_statement" => {
                // Process children (may contain assignments)
                for child in node.children(&mut node.walk()) {
                    self.extract_symbols_from_node(
                        child, code, file_id, counter, symbols, module_path, depth + 1,
                    );
                }
            }

            "assignment" => {
                self.process_assignment(node, code, file_id, counter, symbols, module_path);
            }

            "type_alias_statement" => {
                if let Some(symbol) =
                    self.process_type_alias(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
            }

            "decorated_definition" => {
                for child in node.children(&mut node.walk()) {
                    self.extract_symbols_from_node(
                        child, code, file_id, counter, symbols, module_path, depth + 1,
                    );
                }
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

    // ── Symbol processors ───────────────────────────────────────────────

    fn process_function(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];

        let is_method = self.context.is_in_class();
        let kind = if is_method {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };

        let is_async = is_async_function(node);
        let signature = build_function_signature(node, code, is_async);
        let docstring = extract_function_docstring(node, code);

        // Qualify method names with class name
        let qualified_name = if is_method {
            if let Some(cls) = self.context.current_class() {
                format!("{cls}.{name}")
            } else {
                name.to_string()
            }
        } else {
            name.to_string()
        };

        let visibility = determine_python_visibility(name);

        Some(self.create_symbol(
            counter.next_id(),
            qualified_name,
            kind,
            file_id,
            node_range(node),
            Some(signature),
            docstring,
            module_path,
            visibility,
        ))
    }

    fn process_class(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];

        let signature = extract_class_signature(node, code);
        let docstring = extract_class_docstring(node, code);
        let visibility = determine_python_visibility(name);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Class,
            file_id,
            node_range(node),
            Some(signature),
            docstring,
            module_path,
            visibility,
        ))
    }

    fn process_assignment(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        // Only handle simple identifier assignments
        let left = match node.child_by_field_name("left") {
            Some(n) if n.kind() == "identifier" => n,
            _ => return,
        };
        let name = &code[left.byte_range()];

        let kind = if is_python_constant(name) {
            SymbolKind::Constant
        } else {
            SymbolKind::Variable
        };

        let signature = code[node.byte_range()].to_string();
        let visibility = determine_python_visibility(name);

        let symbol = self.create_symbol(
            counter.next_id(),
            name.to_string(),
            kind,
            file_id,
            node_range(node),
            Some(signature),
            None,
            module_path,
            visibility,
        );
        symbols.push(symbol);
    }

    fn process_type_alias(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let value = node
            .child_by_field_name("value")
            .map(|n| &code[n.byte_range()]);

        let signature = match value {
            Some(v) => format!("{name} = {v}"),
            None => name.to_string(),
        };
        let visibility = determine_python_visibility(name);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::TypeAlias,
            file_id,
            node_range(node),
            Some(signature),
            None,
            module_path,
            visibility,
        ))
    }

    // ── Imports ─────────────────────────────────────────────────────────

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        self.find_imports_in_node(tree.root_node(), code, file_id, &mut imports);
        imports
    }

    fn find_imports_in_node(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
        match node.kind() {
            "import_statement" => {
                self.process_import_statement(node, code, file_id, imports);
            }
            "import_from_statement" => {
                self.process_from_import(node, code, file_id, imports);
            }
            _ => {
                for child in node.children(&mut node.walk()) {
                    self.find_imports_in_node(child, code, file_id, imports);
                }
            }
        }
    }

    fn process_import_statement(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "dotted_name" | "identifier" => {
                    let path = code[child.byte_range()].to_string();
                    imports.push(Import {
                        path,
                        alias: None,
                        file_id,
                        is_glob: false,
                        is_type_only: false,
                    });
                }
                "aliased_import" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let path = code[name_node.byte_range()].to_string();
                        let alias = child
                            .child_by_field_name("alias")
                            .map(|a| code[a.byte_range()].to_string());
                        imports.push(Import {
                            path,
                            alias,
                            file_id,
                            is_glob: false,
                            is_type_only: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn process_from_import(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
        // Extract the module path
        let module_path = extract_from_module_path(node, code);

        // Check for wildcard import (from x import *)
        if has_wildcard_import(node) {
            imports.push(Import {
                path: module_path,
                alias: None,
                file_id,
                is_glob: true,
                is_type_only: false,
            });
            return;
        }

        // Extract named imports
        let mut found_import = false;
        for child in node.children(&mut node.walk()) {
            // Skip until after the "import" keyword
            if child.kind() == "import" {
                found_import = true;
                continue;
            }
            if !found_import {
                continue;
            }

            match child.kind() {
                "dotted_name" | "identifier" => {
                    let name = code[child.byte_range()].to_string();
                    let full_path = if module_path.is_empty() {
                        name
                    } else {
                        format!("{module_path}.{name}")
                    };
                    imports.push(Import {
                        path: full_path,
                        alias: None,
                        file_id,
                        is_glob: false,
                        is_type_only: false,
                    });
                }
                "aliased_import" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = code[name_node.byte_range()].to_string();
                        let full_path = if module_path.is_empty() {
                            name
                        } else {
                            format!("{module_path}.{name}")
                        };
                        let alias = child
                            .child_by_field_name("alias")
                            .map(|a| code[a.byte_range()].to_string());
                        imports.push(Import {
                            path: full_path,
                            alias,
                            file_id,
                            is_glob: false,
                            is_type_only: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // ── Calls ───────────────────────────────────────────────────────────

    fn find_calls_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        current_fn: Option<&'a str>,
        calls: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        let fn_ctx = match node.kind() {
            "function_definition" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        if node.kind() == "call" {
            if let Some(function_node) = node.child_by_field_name("function") {
                if let Some(target) = extract_call_target(&function_node, code) {
                    if let Some(ctx) = fn_ctx {
                        calls.push((ctx, target, node_range(*node)));
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_calls_in_node(&child, code, fn_ctx, calls);
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

    // ── Method calls ────────────────────────────────────────────────────

    fn find_method_calls_in_node(
        &self,
        node: &Node,
        code: &str,
        current_fn: Option<&str>,
        calls: &mut Vec<MethodCall>,
    ) {
        let fn_ctx = match node.kind() {
            "function_definition" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        if node.kind() == "call" {
            if let Some(function_node) = node.child_by_field_name("function") {
                if function_node.kind() == "attribute" {
                    if let Some(ctx) = fn_ctx {
                        let method_name = function_node
                            .child_by_field_name("attribute")
                            .map(|n| &code[n.byte_range()]);
                        let receiver = function_node
                            .child_by_field_name("object")
                            .map(|n| &code[n.byte_range()]);

                        if let Some(method) = method_name {
                            calls.push(MethodCall {
                                caller: ctx.to_string(),
                                method_name: method.to_string(),
                                receiver: receiver.map(|r| r.to_string()),
                                is_static: false,
                                range: node_range(*node),
                                caller_range: None,
                            });
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_method_calls_in_node(&child, code, fn_ctx, calls);
        }
    }

    fn find_method_calls_impl(&mut self, code: &str) -> Vec<MethodCall> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.find_method_calls_in_node(&tree.root_node(), code, None, &mut calls);
        calls
    }

    // ── Implementations (inheritance) ───────────────────────────────────

    fn find_implementations_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        results: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if node.kind() == "class_definition" {
            if let Some(name_node) = node.child_by_field_name("name") {
                let class_name = &code[name_node.byte_range()];
                if let Some(superclasses) = node.child_by_field_name("superclasses") {
                    extract_base_class_names(&superclasses, code, class_name, results);
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_implementations_in_node(&child, code, results);
        }
    }

    // ── Method defines ──────────────────────────────────────────────────

    fn find_defines_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        defines: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if node.kind() == "class_definition" {
            if let Some(name_node) = node.child_by_field_name("name") {
                let class_name = &code[name_node.byte_range()];
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        if child.kind() == "function_definition" {
                            if let Some(method_name_node) = child.child_by_field_name("name") {
                                let method_name = &code[method_name_node.byte_range()];
                                defines.push((class_name, method_name, node_range(child)));
                            }
                        }
                        // Handle decorated methods
                        if child.kind() == "decorated_definition" {
                            for grandchild in child.children(&mut child.walk()) {
                                if grandchild.kind() == "function_definition" {
                                    if let Some(mn) = grandchild.child_by_field_name("name") {
                                        let method_name = &code[mn.byte_range()];
                                        defines.push((
                                            class_name,
                                            method_name,
                                            node_range(grandchild),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_defines_in_node(&child, code, defines);
        }
    }

    // ── Variable types ──────────────────────────────────────────────────

    fn find_variable_types_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        types: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if node.kind() == "assignment" {
            if let Some(type_node) = node.child_by_field_name("type") {
                let var_name = node
                    .child_by_field_name("left")
                    .and_then(|n| extract_variable_name(n, code));
                if let Some(name) = var_name {
                    let type_str = &code[type_node.byte_range()];
                    types.push((name, type_str, node_range(type_node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_variable_types_in_node(&child, code, types);
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

/// Python visibility: underscore prefix = private, dunder = private.
fn determine_python_visibility(name: &str) -> Visibility {
    if name.starts_with('_') {
        Visibility::Private
    } else {
        Visibility::Public
    }
}

/// Check if name is a Python constant (ALL_UPPERCASE).
fn is_python_constant(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        && name.chars().any(|c| c.is_ascii_alphabetic())
}

/// Check if a function definition is async.
fn is_async_function(node: Node) -> bool {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "async" {
            return true;
        }
        // Stop looking after "def" keyword
        if child.kind() == "def" {
            break;
        }
    }
    false
}

/// Build function signature from AST.
fn build_function_signature(node: Node, code: &str, is_async: bool) -> String {
    let name = node
        .child_by_field_name("name")
        .map(|n| &code[n.byte_range()])
        .unwrap_or("?");

    let params = node
        .child_by_field_name("parameters")
        .map(|n| &code[n.byte_range()])
        .unwrap_or("()");

    let return_type = node
        .child_by_field_name("return_type")
        .map(|n| &code[n.byte_range()]);

    let prefix = if is_async { "async def" } else { "def" };

    match return_type {
        Some(ret) => format!("{prefix} {name}{params} -> {ret}"),
        None => format!("{prefix} {name}{params}"),
    }
}

/// Extract class signature including base classes.
fn extract_class_signature(node: Node, code: &str) -> String {
    let name = node
        .child_by_field_name("name")
        .map(|n| &code[n.byte_range()])
        .unwrap_or("?");

    let bases = node
        .child_by_field_name("superclasses")
        .map(|n| &code[n.byte_range()]);

    match bases {
        Some(b) => format!("class {name}{b}"),
        None => format!("class {name}"),
    }
}

/// Extract docstring from function body.
fn extract_function_docstring(node: Node, code: &str) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    extract_docstring_from_body(body, code)
}

/// Extract docstring from class body.
fn extract_class_docstring(node: Node, code: &str) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    extract_docstring_from_body(body, code)
}

/// Extract docstring from a body node (first expression_statement containing a string).
fn extract_docstring_from_body(body: Node, code: &str) -> Option<String> {
    // If body is a block, get first child
    let first_stmt = if body.kind() == "block" {
        body.children(&mut body.walk()).next()
    } else {
        Some(body)
    };

    let stmt = first_stmt?;
    if stmt.kind() != "expression_statement" {
        return None;
    }

    let string_node = stmt
        .children(&mut stmt.walk())
        .find(|n| n.kind() == "string")?;

    let raw = &code[string_node.byte_range()];
    normalize_docstring(raw)
}

/// Normalize a Python docstring by removing quotes and whitespace.
fn normalize_docstring(raw: &str) -> Option<String> {
    let inner = if raw.starts_with("\"\"\"") || raw.starts_with("'''") {
        &raw[3..raw.len().saturating_sub(3)]
    } else if raw.starts_with('"') || raw.starts_with('\'') {
        &raw[1..raw.len().saturating_sub(1)]
    } else {
        raw
    };

    let trimmed = inner.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Extract module path from `from X import ...` statement.
fn extract_from_module_path(node: Node, code: &str) -> String {
    for child in node.children(&mut node.walk()) {
        if matches!(child.kind(), "dotted_name" | "relative_import") {
            return code[child.byte_range()].to_string();
        }
    }
    String::new()
}

/// Check for wildcard import (from x import *).
fn has_wildcard_import(node: Node) -> bool {
    node.children(&mut node.walk())
        .any(|c| c.kind() == "wildcard_import")
}

/// Extract function name or attribute path for call targets.
fn extract_call_target<'a>(node: &Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "identifier" => Some(&code[node.byte_range()]),
        "attribute" => Some(&code[node.byte_range()]),
        _ => None,
    }
}

/// Extract variable name from assignment left side.
fn extract_variable_name<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "identifier" => Some(&code[node.byte_range()]),
        "attribute" => node
            .child_by_field_name("attribute")
            .map(|n| &code[n.byte_range()]),
        _ => None,
    }
}

/// Extract base class names from superclasses argument list.
fn extract_base_class_names<'a>(
    superclasses: &Node,
    code: &'a str,
    class_name: &'a str,
    results: &mut Vec<(&'a str, &'a str, Range)>,
) {
    for child in superclasses.children(&mut superclasses.walk()) {
        match child.kind() {
            "identifier" => {
                let base = &code[child.byte_range()];
                results.push((class_name, base, node_range(child)));
            }
            "attribute" => {
                let base = &code[child.byte_range()];
                results.push((class_name, base, node_range(child)));
            }
            "argument_list" => {
                extract_base_class_names(&child, code, class_name, results);
            }
            _ => {}
        }
    }
}

// ── LanguageParser trait impl ───────────────────────────────────────────

impl LanguageParser for PythonParser {
    fn parse(
        &mut self,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
    ) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Python
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        match node.kind() {
            "function_definition" => extract_function_docstring(*node, code),
            "class_definition" => extract_class_docstring(*node, code),
            _ => None,
        }
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_method_calls(&mut self, code: &str) -> Vec<MethodCall> {
        self.find_method_calls_impl(code)
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

    fn find_defines<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut defines = Vec::new();
        self.find_defines_in_node(&tree.root_node(), code, &mut defines);
        defines
    }

    fn find_imports(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        self.extract_imports_impl(code, file_id)
    }

    fn find_variable_types<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut types = Vec::new();
        self.find_variable_types_in_node(&tree.root_node(), code, &mut types);
        types
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_functions_and_classes() {
        let mut parser = PythonParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
def public_function(x: int) -> bool:
    return True

def _private_function():
    pass

class MyClass:
    def __init__(self, name: str):
        self.name = name

    def greet(self) -> str:
        return f"Hello {self.name}"
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "public_function"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "_private_function"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Private));
        assert!(symbols
            .iter()
            .any(|s| s.name.as_ref() == "MyClass" && s.kind == SymbolKind::Class));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "MyClass.__init__"
            && s.kind == SymbolKind::Method));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "MyClass.greet"
            && s.kind == SymbolKind::Method));
    }

    #[test]
    fn test_parse_constants_and_variables() {
        let mut parser = PythonParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
MAX_SIZE = 100
API_KEY = "secret"
regular_var = 42
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "MAX_SIZE"
            && s.kind == SymbolKind::Constant));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "API_KEY"
            && s.kind == SymbolKind::Constant));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "regular_var"
            && s.kind == SymbolKind::Variable));
    }

    #[test]
    fn test_find_imports() {
        let mut parser = PythonParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
import os
import sys
from typing import List, Dict
from collections import OrderedDict as OD
from pathlib import *
"#;

        let imports = parser.find_imports(code, file_id);

        assert!(imports.iter().any(|i| i.path == "os"));
        assert!(imports.iter().any(|i| i.path == "sys"));
        assert!(imports.iter().any(|i| i.path == "typing.List"));
        assert!(imports.iter().any(|i| i.path == "typing.Dict"));
        assert!(imports.iter().any(|i| i.path == "collections.OrderedDict"
            && i.alias == Some("OD".to_string())));
        assert!(imports.iter().any(|i| i.path == "pathlib" && i.is_glob));
    }

    #[test]
    fn test_find_calls() {
        let mut parser = PythonParser::new().unwrap();

        let code = r#"
def main():
    process()
    data = get_data()
    print(data)

def process():
    pass

def get_data():
    return []
"#;

        let calls = parser.find_calls_impl(code);
        assert!(calls
            .iter()
            .any(|(caller, target, _)| *caller == "main" && *target == "process"));
        assert!(calls
            .iter()
            .any(|(caller, target, _)| *caller == "main" && *target == "get_data"));
        assert!(calls
            .iter()
            .any(|(caller, target, _)| *caller == "main" && *target == "print"));
    }

    #[test]
    fn test_find_method_calls() {
        let mut parser = PythonParser::new().unwrap();

        let code = r#"
class Server:
    def start(self):
        pass

def main():
    s = Server()
    s.start()
    print("hello")
"#;

        let calls = parser.find_method_calls_impl(code);
        assert!(calls
            .iter()
            .any(|c| c.caller == "main" && c.method_name == "start"));
    }

    #[test]
    fn test_find_inheritance() {
        let mut parser = PythonParser::new().unwrap();

        let code = r#"
class Animal:
    pass

class Dog(Animal):
    pass

class GuideDog(Dog, Trainable):
    pass
"#;

        let impls = parser.find_implementations(code);
        assert!(impls
            .iter()
            .any(|(cls, base, _)| *cls == "Dog" && *base == "Animal"));
        assert!(impls
            .iter()
            .any(|(cls, base, _)| *cls == "GuideDog" && *base == "Dog"));
        assert!(impls
            .iter()
            .any(|(cls, base, _)| *cls == "GuideDog" && *base == "Trainable"));
    }

    #[test]
    fn test_find_defines() {
        let mut parser = PythonParser::new().unwrap();

        let code = r#"
class Calculator:
    def add(self, a, b):
        return a + b

    def subtract(self, a, b):
        return a - b
"#;

        let tree = parser.parser.parse_cached(code).unwrap();
        let mut defines = Vec::new();
        parser.find_defines_in_node(&tree.root_node(), code, &mut defines);

        assert!(defines
            .iter()
            .any(|(cls, method, _)| *cls == "Calculator" && *method == "add"));
        assert!(defines
            .iter()
            .any(|(cls, method, _)| *cls == "Calculator" && *method == "subtract"));
    }

    #[test]
    fn test_docstring_extraction() {
        let mut parser = PythonParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
def process_data(data):
    """Process the given data and return results."""
    return data

class DataProcessor:
    """A class for processing data."""

    def run(self):
        """Run the data processor."""
        pass
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let func = symbols
            .iter()
            .find(|s| s.name.as_ref() == "process_data")
            .expect("should find process_data");
        assert!(func.doc_comment.is_some());
        assert!(func
            .doc_comment
            .as_deref()
            .unwrap()
            .contains("Process the given data"));

        let cls = symbols
            .iter()
            .find(|s| s.name.as_ref() == "DataProcessor")
            .expect("should find DataProcessor");
        assert!(cls.doc_comment.is_some());
        assert!(cls
            .doc_comment
            .as_deref()
            .unwrap()
            .contains("processing data"));
    }

    #[test]
    fn test_async_functions() {
        let mut parser = PythonParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
async def fetch_data(url: str) -> dict:
    pass

def sync_function():
    pass
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let async_fn = symbols
            .iter()
            .find(|s| s.name.as_ref() == "fetch_data")
            .expect("should find fetch_data");
        assert!(async_fn
            .signature
            .as_deref()
            .unwrap()
            .starts_with("async def"));

        let sync_fn = symbols
            .iter()
            .find(|s| s.name.as_ref() == "sync_function")
            .expect("should find sync_function");
        assert!(sync_fn
            .signature
            .as_deref()
            .unwrap()
            .starts_with("def "));
    }

    #[test]
    fn test_python_visibility() {
        let mut parser = PythonParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
def public_func():
    pass

def _private_func():
    pass

def __dunder_func():
    pass
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "public_func"
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "_private_func"
            && s.visibility == Visibility::Private));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "__dunder_func"
            && s.visibility == Visibility::Private));
    }
}
