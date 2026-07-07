//! Rust language parser implementation using tree-sitter-rust 0.24 (ABI-15).

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::method_call::MethodCall;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth, node_range};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

/// Classification for Rust doc comment types.
#[derive(Debug, Clone, Copy, PartialEq)]
enum DocCommentType {
    OuterLine,     // ///
    OuterBlock,    // /**
    InnerLine,     // //!
    InnerBlock,    // /*!
    NotDocComment, // Regular comment
}

pub struct RustParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for RustParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustParser")
            .field("language", &"Rust")
            .finish()
    }
}

impl RustParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .map_err(|e| format!("Failed to set Rust language: {e}"))?;

        Ok(Self {
            parser: CachingParser::new(ts_parser),
            context: ParserContext::new(),
        })
    }

    // ── Imports ─────────────────────────────────────────────────────────

    fn extract_imports(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        self.extract_imports_from_node(tree.root_node(), code, file_id, 0, &mut imports);
        imports
    }

    fn extract_imports_from_node(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        depth: usize,
        imports: &mut Vec<Import>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        if node.kind() == "use_declaration" {
            if let Some(arg_node) = node.child_by_field_name("argument") {
                self.extract_import_from_node(arg_node, code, file_id, imports);
            }
        } else {
            for child in node.children(&mut node.walk()) {
                self.extract_imports_from_node(child, code, file_id, depth + 1, imports);
            }
        }
    }

    fn extract_import_from_node(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
        match node.kind() {
            "identifier" | "scoped_identifier" => {
                imports.push(Import {
                    path: code[node.byte_range()].to_string(),
                    alias: None,
                    file_id,
                    is_glob: false,
                    is_type_only: false,
                });
            }
            "use_as_clause" => {
                if let Some(path_node) = node.child_by_field_name("path") {
                    let path = code[path_node.byte_range()].to_string();
                    if let Some(alias_node) = node.child_by_field_name("alias") {
                        imports.push(Import {
                            path,
                            alias: Some(code[alias_node.byte_range()].to_string()),
                            file_id,
                            is_glob: false,
                            is_type_only: false,
                        });
                    }
                }
            }
            "use_wildcard" => {
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "scoped_identifier" {
                        imports.push(Import {
                            path: code[child.byte_range()].to_string(),
                            alias: None,
                            file_id,
                            is_glob: true,
                            is_type_only: false,
                        });
                        break;
                    }
                }
            }
            "use_list" => {
                let prefix = node
                    .parent()
                    .filter(|p| p.kind() == "scoped_use_list")
                    .and_then(|p| p.child_by_field_name("path"))
                    .map(|p| code[p.byte_range()].to_string())
                    .unwrap_or_default();

                for child in node.children(&mut node.walk()) {
                    if child.kind() != "," && child.kind() != "{" && child.kind() != "}" {
                        self.extract_import_list_item(child, code, file_id, &prefix, imports);
                    }
                }
            }
            "scoped_use_list" => {
                if let Some(list_node) = node.child_by_field_name("list") {
                    self.extract_import_from_node(list_node, code, file_id, imports);
                }
            }
            _ => {}
        }
    }

    fn extract_import_list_item(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        prefix: &str,
        imports: &mut Vec<Import>,
    ) {
        match node.kind() {
            "identifier" => {
                let name = code[node.byte_range()].to_string();
                let path = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}::{name}")
                };
                imports.push(Import {
                    path,
                    alias: None,
                    file_id,
                    is_glob: false,
                    is_type_only: false,
                });
            }
            "use_as_clause" => {
                if let Some(path_node) = node.child_by_field_name("path") {
                    let name = code[path_node.byte_range()].to_string();
                    let path = if prefix.is_empty() {
                        name
                    } else {
                        format!("{prefix}::{name}")
                    };
                    if let Some(alias_node) = node.child_by_field_name("alias") {
                        imports.push(Import {
                            path,
                            alias: Some(code[alias_node.byte_range()].to_string()),
                            file_id,
                            is_glob: false,
                            is_type_only: false,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // ── Symbol extraction ───────────────────────────────────────────────

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
        self.extract_symbols(tree.root_node(), code, file_id, &mut symbols, counter, 0);
        symbols
    }

    fn extract_symbols(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        symbols: &mut Vec<Symbol>,
        counter: &mut SymbolCounter,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        match node.kind() {
            "function_item" => {
                // Determine if this is a method (inside impl block).
                let is_method = {
                    let mut p = node.parent();
                    let mut found = false;
                    while let Some(parent) = p {
                        if parent.kind() == "impl_item" {
                            found = true;
                            break;
                        }
                        p = parent.parent();
                    }
                    found
                };

                let kind = if is_method {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };

                let func_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(mut sym) =
                        self.create_symbol(counter, node, name_node, kind, file_id, code)
                    {
                        sym = sym.with_signature(self.extract_signature(node, code));
                        symbols.push(sym);
                    }
                }

                // Scope tracking for nested items.
                self.context
                    .enter_scope(ScopeType::Function { hoisting: false });
                let saved_fn = self.context.current_function().map(|s| s.to_string());
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_function(func_name);

                for child in node.children(&mut node.walk()) {
                    if child.kind() != "identifier" && child.kind() != "parameters" {
                        self.extract_symbols(child, code, file_id, symbols, counter, depth + 1);
                    }
                }

                self.context.exit_scope();
                self.context.set_current_function(saved_fn);
                self.context.set_current_class(saved_cls);
                return;
            }
            "struct_item" => {
                let struct_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(mut sym) = self.create_symbol(
                        counter,
                        node,
                        name_node,
                        SymbolKind::Struct,
                        file_id,
                        code,
                    ) {
                        sym = sym.with_signature(self.extract_struct_signature(node, code));
                        symbols.push(sym);
                    }
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_fn = self.context.current_function().map(|s| s.to_string());
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(struct_name);

                // Extract struct fields.
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        if child.kind() == "field_declaration" {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                if let Some(sym) = self.create_symbol(
                                    counter,
                                    child,
                                    name_node,
                                    SymbolKind::Field,
                                    file_id,
                                    code,
                                ) {
                                    symbols.push(sym);
                                }
                            }
                        }
                    }
                }

                for child in node.children(&mut node.walk()) {
                    if child.kind() != "identifier" && child.kind() != "field_declaration_list" {
                        self.extract_symbols(child, code, file_id, symbols, counter, depth + 1);
                    }
                }

                self.context.exit_scope();
                self.context.set_current_function(saved_fn);
                self.context.set_current_class(saved_cls);
            }
            "enum_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(mut sym) = self.create_symbol(
                        counter,
                        node,
                        name_node,
                        SymbolKind::Enum,
                        file_id,
                        code,
                    ) {
                        sym = sym.with_signature(self.extract_enum_signature(node, code));
                        symbols.push(sym);
                    }
                }

                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        if child.kind() == "enum_variant" {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                if let Some(sym) = self.create_symbol(
                                    counter,
                                    child,
                                    name_node,
                                    SymbolKind::Constant,
                                    file_id,
                                    code,
                                ) {
                                    symbols.push(sym);
                                }
                            }
                        }
                    }
                }
            }
            "type_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(mut sym) = self.create_symbol(
                        counter,
                        node,
                        name_node,
                        SymbolKind::TypeAlias,
                        file_id,
                        code,
                    ) {
                        sym = sym.with_signature(self.extract_type_alias_signature(node, code));
                        symbols.push(sym);
                    }
                }
            }
            "const_item" | "static_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(mut sym) = self.create_symbol(
                        counter,
                        node,
                        name_node,
                        SymbolKind::Constant,
                        file_id,
                        code,
                    ) {
                        sym = sym.with_signature(self.extract_const_signature(node, code));
                        symbols.push(sym);
                    }
                }
            }
            "trait_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(mut sym) = self.create_symbol(
                        counter,
                        node,
                        name_node,
                        SymbolKind::Trait,
                        file_id,
                        code,
                    ) {
                        sym = sym.with_signature(self.extract_trait_signature(node, code));
                        symbols.push(sym);
                    }

                    self.context.enter_scope(ScopeType::Class);
                    if let Some(body) = node.child_by_field_name("body") {
                        for child in body.children(&mut body.walk()) {
                            if child.kind() == "function_signature_item"
                                || child.kind() == "function_item"
                            {
                                if let Some(mn) = child.child_by_field_name("name") {
                                    if let Some(mut ms) = self.create_symbol(
                                        counter,
                                        child,
                                        mn,
                                        SymbolKind::Method,
                                        file_id,
                                        code,
                                    ) {
                                        ms = ms.with_signature(self.extract_signature(child, code));
                                        symbols.push(ms);
                                    }
                                }
                            }
                        }
                    }
                    self.context.exit_scope();
                }
                return;
            }
            "impl_item" => {
                let impl_type = node
                    .child_by_field_name("type")
                    .and_then(|t| self.extract_type_name(t, code));

                self.context.enter_scope(ScopeType::Class);
                let saved_fn = self.context.current_function().map(|s| s.to_string());
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                if let Some(tn) = impl_type {
                    self.context.set_current_class(Some(tn.to_string()));
                }

                for child in node.children(&mut node.walk()) {
                    self.extract_symbols(child, code, file_id, symbols, counter, depth + 1);
                }

                self.context.exit_scope();
                self.context.set_current_function(saved_fn);
                self.context.set_current_class(saved_cls);
                return;
            }
            "mod_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(sym) = self.create_symbol(
                        counter,
                        node,
                        name_node,
                        SymbolKind::Module,
                        file_id,
                        code,
                    ) {
                        symbols.push(sym);
                    }
                }
                for child in node.children(&mut node.walk()) {
                    if child.kind() != "identifier" {
                        self.extract_symbols(child, code, file_id, symbols, counter, depth + 1);
                    }
                }
                return;
            }
            "macro_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(sym) = self.create_symbol(
                        counter,
                        node,
                        name_node,
                        SymbolKind::Macro,
                        file_id,
                        code,
                    ) {
                        symbols.push(sym);
                    }
                }
            }
            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            self.extract_symbols(child, code, file_id, symbols, counter, depth + 1);
        }
    }

    // ── Symbol helper ───────────────────────────────────────────────────

    fn create_symbol(
        &mut self,
        counter: &mut SymbolCounter,
        full_node: Node,
        name_node: Node,
        kind: SymbolKind,
        file_id: FileId,
        code: &str,
    ) -> Option<Symbol> {
        let name = &code[name_node.byte_range()];
        let id = counter.next_id();
        let range = Range::new(
            full_node.start_position().row as u32,
            full_node.start_position().column as u16,
            full_node.end_position().row as u32,
            full_node.end_position().column as u16,
        );

        let doc_node = name_node.parent()?;
        let doc_comment = self.extract_doc_comments_outer(&doc_node, code);

        let mut symbol = Symbol::new(id, name, kind, file_id, range);
        symbol.scope_context = Some(self.context.current_scope_context());

        // Check visibility modifier.
        if let Some(parent) = name_node.parent() {
            for child in parent.children(&mut parent.walk()) {
                if child.kind() == "visibility_modifier" {
                    symbol = symbol.with_visibility(Visibility::Public);
                    break;
                }
            }
        }

        if let Some(doc) = doc_comment {
            symbol = symbol.with_doc(doc);
        }

        Some(symbol)
    }

    // ── Signatures ──────────────────────────────────────────────────────

    fn extract_signature(&self, node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    fn extract_struct_signature(&self, node: Node, code: &str) -> String {
        self.extract_signature(node, code)
    }

    fn extract_trait_signature(&self, node: Node, code: &str) -> String {
        self.extract_signature(node, code)
    }

    fn extract_enum_signature(&self, node: Node, code: &str) -> String {
        self.extract_signature(node, code)
    }

    fn extract_type_alias_signature(&self, node: Node, code: &str) -> String {
        code[node.byte_range()].trim().to_string()
    }

    fn extract_const_signature(&self, node: Node, code: &str) -> String {
        code[node.byte_range()].trim().to_string()
    }

    // ── Type name extraction ────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_type_name<'a>(&self, node: Node, code: &'a str) -> Option<&'a str> {
        match node.kind() {
            "type_identifier" | "primitive_type" | "scoped_type_identifier" => {
                Some(&code[node.byte_range()])
            }
            "generic_type" => node
                .child_by_field_name("type")
                .and_then(|t| self.extract_type_name(t, code)),
            _ => {
                for child in node.children(&mut node.walk()) {
                    if let Some(name) = self.extract_type_name(child, code) {
                        return Some(name);
                    }
                }
                None
            }
        }
    }

    /// Full type name including generic parameters (owned).
    #[allow(clippy::only_used_in_recursion)]
    fn extract_full_type_name(&self, node: Node, code: &str) -> String {
        match node.kind() {
            "type_identifier" | "primitive_type" | "scoped_type_identifier" => {
                code[node.byte_range()].to_string()
            }
            "generic_type" => {
                let mut result = String::new();
                if let Some(t) = node.child_by_field_name("type") {
                    result.push_str(&self.extract_full_type_name(t, code));
                }
                if let Some(args) = node.child_by_field_name("type_arguments") {
                    result.push('<');
                    let mut first = true;
                    for child in args.children(&mut args.walk()) {
                        if child.kind() != "," && child.kind() != "<" && child.kind() != ">" {
                            if !first {
                                result.push_str(", ");
                            }
                            result.push_str(&self.extract_full_type_name(child, code));
                            first = false;
                        }
                    }
                    result.push('>');
                }
                result
            }
            "reference_type" => {
                let mut r = String::from("&");
                if node.child_by_field_name("mutable").is_some() {
                    r.push_str("mut ");
                }
                if let Some(t) = node.child_by_field_name("type") {
                    r.push_str(&self.extract_full_type_name(t, code));
                }
                r
            }
            _ => code[node.byte_range()].to_string(),
        }
    }

    // ── Doc comments ────────────────────────────────────────────────────

    fn classify_doc_comment(&self, text: &str) -> DocCommentType {
        if text.starts_with("///") && !text.starts_with("////") {
            DocCommentType::OuterLine
        } else if text.starts_with("//!") {
            DocCommentType::InnerLine
        } else if text.starts_with("/**") && !text.starts_with("/***") && text != "/**/" {
            DocCommentType::OuterBlock
        } else if text.starts_with("/*!") && text != "/*!" {
            DocCommentType::InnerBlock
        } else {
            DocCommentType::NotDocComment
        }
    }

    fn is_outer_doc(&self, text: &str) -> bool {
        matches!(
            self.classify_doc_comment(text),
            DocCommentType::OuterLine | DocCommentType::OuterBlock
        )
    }

    fn is_inner_doc(&self, text: &str) -> bool {
        matches!(
            self.classify_doc_comment(text),
            DocCommentType::InnerLine | DocCommentType::InnerBlock
        )
    }

    fn extract_doc_comments_outer(&self, node: &Node, code: &str) -> Option<String> {
        let mut doc_lines = Vec::new();
        let mut current = node.prev_sibling();

        while let Some(sibling) = current {
            match sibling.kind() {
                "line_comment" | "block_comment" => {
                    if let Ok(text) = sibling.utf8_text(code.as_bytes()) {
                        if self.is_outer_doc(text) {
                            let content = match self.classify_doc_comment(text) {
                                DocCommentType::OuterLine => {
                                    text.trim_start_matches("///").trim().to_string()
                                }
                                DocCommentType::OuterBlock => text
                                    .trim_start_matches("/**")
                                    .trim_end_matches("*/")
                                    .trim()
                                    .to_string(),
                                _ => break,
                            };
                            doc_lines.push(content);
                        } else {
                            break;
                        }
                    }
                }
                _ => break,
            }
            current = sibling.prev_sibling();
        }

        if doc_lines.is_empty() {
            None
        } else {
            doc_lines.reverse();
            Some(doc_lines.join("\n"))
        }
    }

    fn extract_inner_doc_comments(&self, node: &Node, code: &str) -> Option<String> {
        let mut parts = Vec::new();
        self.collect_inner_docs(node, code, 0, &mut parts);
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    fn collect_inner_docs<'a>(
        &self,
        node: &Node,
        code: &'a str,
        depth: usize,
        parts: &mut Vec<&'a str>,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        for child in node.children(&mut node.walk()) {
            if matches!(child.kind(), "line_comment" | "block_comment") {
                if let Ok(text) = child.utf8_text(code.as_bytes()) {
                    if self.is_inner_doc(text) {
                        if text.starts_with("//!") {
                            let content = text.trim_start_matches("//!").trim();
                            if !content.is_empty() {
                                parts.push(content);
                            }
                        } else if text.starts_with("/*!") {
                            let content = text.trim_start_matches("/*!").trim_end_matches("*/");
                            for line in content.lines() {
                                let cleaned = line.trim().trim_start_matches('*').trim();
                                if !cleaned.is_empty() {
                                    parts.push(cleaned);
                                }
                            }
                        }
                    }
                }
            } else {
                self.collect_inner_docs(&child, code, depth + 1, parts);
            }
        }
    }

    // ── Containing function helper ──────────────────────────────────────

    // ── Calls ───────────────────────────────────────────────────────────

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.find_calls_in_node(tree.root_node(), code, None, 0, &mut calls);
        calls
    }

    fn find_calls_in_node<'a>(
        &self,
        node: Node,
        code: &'a str,
        current_fn: Option<&'a str>,
        depth: usize,
        calls: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        // Thread the innermost enclosing function name down the walk instead of
        // re-walking ancestors to the root at every node (PERF-C1: O(n) not O(n·depth)).
        let current_fn = if node.kind() == "function_item" {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn)
        } else {
            current_fn
        };

        if node.kind() == "call_expression" {
            if let Some(fn_node) = node.child_by_field_name("function") {
                let target = match fn_node.kind() {
                    "identifier" => Some(&code[fn_node.byte_range()]),
                    "field_expression" => fn_node
                        .child_by_field_name("field")
                        .map(|f| &code[f.byte_range()]),
                    "scoped_identifier" => Some(&code[fn_node.byte_range()]),
                    _ => None,
                };

                if let (Some(target), Some(caller)) = (target, current_fn) {
                    calls.push((caller, target, node_range(node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_calls_in_node(child, code, current_fn, depth + 1, calls);
        }
    }

    // ── Method calls (structured) ───────────────────────────────────────

    fn find_method_calls_impl(&mut self, code: &str) -> Vec<MethodCall> {
        let tree = match self.parser.parse_cached(code) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.find_method_calls_in_node(tree.root_node(), code, None, 0, &mut calls);
        calls
    }

    fn find_method_calls_in_node<'a>(
        &self,
        node: Node,
        code: &'a str,
        current_fn: Option<&'a str>,
        depth: usize,
        calls: &mut Vec<MethodCall>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        let current_fn = if node.kind() == "function_item" {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn)
        } else {
            current_fn
        };

        if node.kind() == "call_expression" {
            if let Some(fn_node) = node.child_by_field_name("function") {
                let range = node_range(node);

                if let Some(caller) = current_fn {
                    match fn_node.kind() {
                        "identifier" => {
                            let name = &code[fn_node.byte_range()];
                            calls.push(MethodCall::new(caller, name, range));
                        }
                        "field_expression" => {
                            if let Some(field) = fn_node.child_by_field_name("field") {
                                let method_name = &code[field.byte_range()];
                                if let Some(value) = fn_node.child_by_field_name("value") {
                                    let receiver = &code[value.byte_range()];
                                    calls.push(
                                        MethodCall::new(caller, method_name, range)
                                            .with_receiver(receiver),
                                    );
                                }
                            }
                        }
                        "scoped_identifier" => {
                            let full = &code[fn_node.byte_range()];
                            if let Some(pos) = full.rfind("::") {
                                let type_name = &full[..pos];
                                let method_name = &full[pos + 2..];
                                calls.push(
                                    MethodCall::new(caller, method_name, range)
                                        .with_receiver(type_name)
                                        .static_method(),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_method_calls_in_node(child, code, current_fn, depth + 1, calls);
        }
    }

    // ── Implementations ─────────────────────────────────────────────────

    fn find_implementations_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut impls = Vec::new();
        self.find_implementations_in_node(tree.root_node(), code, 0, &mut impls);
        impls
    }

    fn find_implementations_in_node<'a>(
        &self,
        node: Node,
        code: &'a str,
        depth: usize,
        impls: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        if node.kind() == "impl_item" {
            if let Some(trait_node) = node.child_by_field_name("trait") {
                if let Some(type_node) = node.child_by_field_name("type") {
                    if let (Some(trait_name), Some(type_name)) = (
                        self.extract_type_name(trait_node, code),
                        self.extract_type_name(type_node, code),
                    ) {
                        let range = Range::new(
                            node.start_position().row as u32,
                            node.start_position().column as u16,
                            node.end_position().row as u32,
                            node.end_position().column as u16,
                        );
                        impls.push((type_name, trait_name, range));
                    }
                }
            }
        }
        for child in node.children(&mut node.walk()) {
            self.find_implementations_in_node(child, code, depth + 1, impls);
        }
    }

    // ── Type uses ───────────────────────────────────────────────────────

    fn find_uses_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut uses = Vec::new();
        self.find_uses_in_node(tree.root_node(), code, 0, &mut uses);
        uses
    }

    fn find_uses_in_node<'a>(
        &self,
        node: Node,
        code: &'a str,
        depth: usize,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        match node.kind() {
            "struct_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let struct_name = &code[name_node.byte_range()];
                    if let Some(body) = node.child_by_field_name("body") {
                        for child in body.children(&mut body.walk()) {
                            if child.kind() == "field_declaration" {
                                if let Some(type_node) = child.child_by_field_name("type") {
                                    if let Some(type_name) = self.extract_type_name(type_node, code)
                                    {
                                        let range = Range::new(
                                            type_node.start_position().row as u32,
                                            type_node.start_position().column as u16,
                                            type_node.end_position().row as u32,
                                            type_node.end_position().column as u16,
                                        );
                                        uses.push((struct_name, type_name, range));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            "function_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let fn_name = &code[name_node.byte_range()];
                    if let Some(params) = node.child_by_field_name("parameters") {
                        for param in params.children(&mut params.walk()) {
                            if param.kind() == "parameter" {
                                if let Some(type_node) = param.child_by_field_name("type") {
                                    if let Some(type_name) = self.extract_type_name(type_node, code)
                                    {
                                        let range = Range::new(
                                            type_node.start_position().row as u32,
                                            type_node.start_position().column as u16,
                                            type_node.end_position().row as u32,
                                            type_node.end_position().column as u16,
                                        );
                                        uses.push((fn_name, type_name, range));
                                    }
                                }
                            }
                        }
                    }
                    if let Some(ret) = node.child_by_field_name("return_type") {
                        if let Some(type_name) = self.extract_type_name(ret, code) {
                            let range = Range::new(
                                ret.start_position().row as u32,
                                ret.start_position().column as u16,
                                ret.end_position().row as u32,
                                ret.end_position().column as u16,
                            );
                            uses.push((fn_name, type_name, range));
                        }
                    }
                }
            }
            _ => {}
        }
        for child in node.children(&mut node.walk()) {
            self.find_uses_in_node(child, code, depth + 1, uses);
        }
    }

    // ── Defines ─────────────────────────────────────────────────────────

    fn find_defines_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut defines = Vec::new();
        self.find_defines_in_node(tree.root_node(), code, 0, &mut defines);
        defines
    }

    fn find_defines_in_node<'a>(
        &self,
        node: Node,
        code: &'a str,
        depth: usize,
        defines: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        match node.kind() {
            "trait_item" => {
                if let Some(trait_name_node) = node.child_by_field_name("name") {
                    let trait_name = &code[trait_name_node.byte_range()];
                    if let Some(body) = node.child_by_field_name("body") {
                        for child in body.children(&mut body.walk()) {
                            if child.kind() == "function_signature_item"
                                || child.kind() == "function_item"
                            {
                                if let Some(mn) = child.child_by_field_name("name") {
                                    let range = Range::new(
                                        child.start_position().row as u32,
                                        child.start_position().column as u16,
                                        child.end_position().row as u32,
                                        child.end_position().column as u16,
                                    );
                                    defines.push((trait_name, &code[mn.byte_range()], range));
                                }
                            }
                        }
                    }
                }
            }
            "impl_item" => {
                if let Some(type_node) = node.child_by_field_name("type") {
                    if let Some(type_name) = self.extract_type_name(type_node, code) {
                        if let Some(body) = node.child_by_field_name("body") {
                            for child in body.children(&mut body.walk()) {
                                if child.kind() == "function_item" {
                                    if let Some(mn) = child.child_by_field_name("name") {
                                        let range = Range::new(
                                            child.start_position().row as u32,
                                            child.start_position().column as u16,
                                            child.end_position().row as u32,
                                            child.end_position().column as u16,
                                        );
                                        defines.push((type_name, &code[mn.byte_range()], range));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        for child in node.children(&mut node.walk()) {
            self.find_defines_in_node(child, code, depth + 1, defines);
        }
    }

    // ── Variable types ──────────────────────────────────────────────────

    fn find_variable_types_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut bindings = Vec::new();
        self.find_variable_types_in_node(tree.root_node(), code, 0, &mut bindings);
        bindings
    }

    fn find_variable_types_in_node<'a>(
        &self,
        node: Node,
        code: &'a str,
        depth: usize,
        bindings: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        if node.kind() == "let_declaration" {
            if let Some(pat) = node.child_by_field_name("pattern") {
                if pat.kind() == "identifier" {
                    let var_name = &code[pat.byte_range()];
                    if let Some(val) = node.child_by_field_name("value") {
                        if let Some(type_name) = self.extract_value_type(val, code) {
                            let range = Range::new(
                                node.start_position().row as u32,
                                node.start_position().column as u16,
                                node.end_position().row as u32,
                                node.end_position().column as u16,
                            );
                            bindings.push((var_name, type_name, range));
                        }
                    }
                }
            }
        }
        for child in node.children(&mut node.walk()) {
            self.find_variable_types_in_node(child, code, depth + 1, bindings);
        }
    }

    fn extract_value_type<'a>(&self, node: Node, code: &'a str) -> Option<&'a str> {
        match node.kind() {
            "struct_expression" => node
                .child_by_field_name("name")
                .and_then(|n| self.extract_type_name(n, code)),
            "call_expression" => node
                .child_by_field_name("function")
                .filter(|f| f.kind() == "scoped_identifier")
                .and_then(|f| {
                    let full = &code[f.byte_range()];
                    full.find("::").map(|pos| &full[..pos])
                }),
            _ => None,
        }
    }

    // ── Inherent methods ────────────────────────────────────────────────

    fn find_inherent_methods_impl(&mut self, code: &str) -> Vec<(String, String, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(t) => t,
            None => return Vec::new(),
        };
        let mut methods = Vec::new();
        self.find_inherent_methods_in_node(tree.root_node(), code, 0, &mut methods);
        methods
    }

    fn find_inherent_methods_in_node(
        &self,
        node: Node,
        code: &str,
        depth: usize,
        methods: &mut Vec<(String, String, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        if node.kind() == "impl_item" && node.child_by_field_name("trait").is_none() {
            if let Some(type_node) = node.child_by_field_name("type") {
                let type_name = self.extract_full_type_name(type_node, code);
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        if child.kind() == "function_item" {
                            if let Some(mn) = child.child_by_field_name("name") {
                                let range = Range::new(
                                    child.start_position().row as u32,
                                    child.start_position().column as u16,
                                    child.end_position().row as u32,
                                    child.end_position().column as u16,
                                );
                                methods.push((
                                    type_name.clone(),
                                    code[mn.byte_range()].to_string(),
                                    range,
                                ));
                            }
                        }
                    }
                }
            }
        }
        for child in node.children(&mut node.walk()) {
            self.find_inherent_methods_in_node(child, code, depth + 1, methods);
        }
    }
}

// ── LanguageParser trait ────────────────────────────────────────────────

impl LanguageParser for RustParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Rust
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        let outer = self.extract_doc_comments_outer(node, code);
        let inner = self.extract_inner_doc_comments(node, code);
        match (outer, inner) {
            (Some(o), Some(i)) => Some(format!("{o}\n\n{i}")),
            (Some(o), None) => Some(o),
            (None, Some(i)) => Some(i),
            (None, None) => None,
        }
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_method_calls(&mut self, code: &str) -> Vec<MethodCall> {
        self.find_method_calls_impl(code)
    }

    fn find_implementations<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_implementations_impl(code)
    }

    fn find_uses<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_uses_impl(code)
    }

    fn find_defines<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_defines_impl(code)
    }

    fn find_imports(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        self.extract_imports(code, file_id)
    }

    fn find_variable_types<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_variable_types_impl(code)
    }

    fn find_inherent_methods(&mut self, code: &str) -> Vec<(String, String, Range)> {
        self.find_inherent_methods_impl(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_functions_and_structs() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
pub fn public_function(x: i32) -> bool { true }
fn private_function() {}

pub struct MyStruct {
    pub name: String,
    age: u32,
}
        "#;

        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "public_function"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Public));
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "private_function" && s.kind == SymbolKind::Function)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "MyStruct" && s.kind == SymbolKind::Struct)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "name" && s.kind == SymbolKind::Field)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "age" && s.kind == SymbolKind::Field)
        );
    }

    #[test]
    fn test_parse_enum_and_trait() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
pub enum Color { Red, Green, Blue }

pub trait Drawable {
    fn draw(&self);
    fn area(&self) -> f64;
}
        "#;

        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Color" && s.kind == SymbolKind::Enum)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Red" && s.kind == SymbolKind::Constant)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Drawable" && s.kind == SymbolKind::Trait)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "draw" && s.kind == SymbolKind::Method)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "area" && s.kind == SymbolKind::Method)
        );
    }

    #[test]
    fn test_parse_impl_methods() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
struct Point { x: f64, y: f64 }

impl Point {
    pub fn new(x: f64, y: f64) -> Self { Self { x, y } }
    fn distance(&self) -> f64 { 0.0 }
}
        "#;

        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "new" && s.kind == SymbolKind::Method)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "distance" && s.kind == SymbolKind::Method)
        );
    }

    #[test]
    fn test_find_calls() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
fn main() {
    process();
    String::new();
    data.push(42);
}
fn process() {}
        "#;

        let calls = parser.find_calls_impl(code);
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "process")
        );
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "String::new")
        );
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "push")
        );
    }

    #[test]
    fn test_find_implementations() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
trait Display { fn fmt(&self); }
struct Point { x: f64, y: f64 }
impl Display for Point { fn fmt(&self) {} }
        "#;

        let impls = parser.find_implementations_impl(code);
        assert!(impls
            .iter()
            .any(|(type_name, trait_name, _)| *type_name == "Point" && *trait_name == "Display"));
    }

    #[test]
    fn test_find_imports() {
        let mut parser = RustParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let code = r#"
use std::collections::HashMap;
use std::io::{self, Read, Write};
use crate::config::Settings as Config;
use std::path::*;
        "#;

        let imports = parser.extract_imports(code, file_id);
        assert!(
            imports
                .iter()
                .any(|i| i.path == "std::collections::HashMap")
        );
        assert!(imports.iter().any(|i| i.path == "std::io::Read"));
        assert!(imports.iter().any(|i| i.path == "std::io::Write"));
        assert!(
            imports.iter().any(
                |i| i.path == "crate::config::Settings" && i.alias.as_deref() == Some("Config")
            )
        );
        assert!(imports.iter().any(|i| i.is_glob));
    }

    #[test]
    fn test_find_uses() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
struct Container { items: Vec<Item>, name: String }
fn process(input: Config) -> Result {}
        "#;

        let uses = parser.find_uses_impl(code);
        assert!(
            uses.iter()
                .any(|(ctx, typ, _)| *ctx == "Container" && *typ == "Vec")
        );
        assert!(
            uses.iter()
                .any(|(ctx, typ, _)| *ctx == "process" && *typ == "Config")
        );
    }

    #[test]
    fn test_find_defines() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
trait Handler {
    fn handle(&self);
    fn process(&self);
}
struct Server;
impl Server {
    fn start(&self) {}
}
        "#;

        let defines = parser.find_defines_impl(code);
        assert!(
            defines
                .iter()
                .any(|(definer, method, _)| *definer == "Handler" && *method == "handle")
        );
        assert!(
            defines
                .iter()
                .any(|(definer, method, _)| *definer == "Server" && *method == "start")
        );
    }

    #[test]
    fn test_doc_comments() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
/// This is documented.
/// Multiple lines.
pub fn documented() {}

//// Not a doc comment.
fn not_documented() {}
        "#;

        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let doc_fn = symbols
            .iter()
            .find(|s| s.name.as_ref() == "documented")
            .unwrap();
        assert!(doc_fn.doc_comment.is_some());
        let doc = doc_fn.doc_comment.as_ref().unwrap();
        assert!(doc.contains("This is documented"));
        assert!(doc.contains("Multiple lines"));

        let no_doc = symbols
            .iter()
            .find(|s| s.name.as_ref() == "not_documented")
            .unwrap();
        assert!(no_doc.doc_comment.is_none());
    }

    #[test]
    fn test_method_calls_structured() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
fn main() {
    let data = Vec::new();
    data.push(42);
    self.validate();
}
        "#;

        let calls = parser.find_method_calls_impl(code);
        assert!(
            calls
                .iter()
                .any(|c| c.caller == "main" && c.method_name == "new" && c.is_static)
        );
        assert!(calls.iter().any(|c| c.caller == "main"
            && c.method_name == "push"
            && c.receiver.as_deref() == Some("data")));
        assert!(calls.iter().any(|c| c.caller == "main"
            && c.method_name == "validate"
            && c.receiver.as_deref() == Some("self")));
    }

    #[test]
    fn test_variable_types() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
fn main() {
    let config = Config::new();
    let server = Server { port: 8080 };
    let name = "test";
}
        "#;

        let bindings = parser.find_variable_types_impl(code);
        assert!(
            bindings
                .iter()
                .any(|(var, typ, _)| *var == "config" && *typ == "Config")
        );
        assert!(
            bindings
                .iter()
                .any(|(var, typ, _)| *var == "server" && *typ == "Server")
        );
        // Literals don't produce type bindings.
        assert!(!bindings.iter().any(|(var, _, _)| *var == "name"));
    }

    #[test]
    fn test_inherent_methods() {
        let mut parser = RustParser::new().unwrap();
        let code = r#"
struct MyType;
impl MyType {
    fn method_a(&self) {}
    fn method_b() {}
}
trait Foo { fn foo(&self); }
impl Foo for MyType { fn foo(&self) {} }
        "#;

        let methods = parser.find_inherent_methods_impl(code);
        assert!(
            methods
                .iter()
                .any(|(t, m, _)| t == "MyType" && m == "method_a")
        );
        assert!(
            methods
                .iter()
                .any(|(t, m, _)| t == "MyType" && m == "method_b")
        );
        // Trait impl methods should NOT appear in inherent methods.
        assert!(!methods.iter().any(|(_, m, _)| m == "foo"));
    }
}
