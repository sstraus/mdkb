//! Rust language parser implementation using tree-sitter-rust 0.24 (ABI-15).

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth, node_range};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

/// The access level a Rust `visibility_modifier` grants.
///
/// `pub` alone is the only one that leaves the crate. `pub(crate)` stops at the
/// crate and `pub(super)` and `pub(in <path>)` stop at a module, so reporting
/// all four as `Public` puts internal items in every answer about the public
/// API surface. `pub(self)` is `pub` restricted to the current module, which is
/// what no modifier at all already means.
fn rust_visibility(modifier: &str) -> Visibility {
    match modifier.trim() {
        "pub" => Visibility::Public,
        "pub(crate)" => Visibility::Crate,
        "pub(self)" => Visibility::Private,
        // `pub(super)` and `pub(in <path>)`: visible up to some module, not the
        // whole crate.
        _ => Visibility::Module,
    }
}

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
    /// Names of the `mod` blocks open around the node being visited, outermost
    /// first. A file's address comes from its path, but everything a `mod` block
    /// declares sits below that — without this, every file's `tests` module
    /// collapses onto the address of the file holding it.
    module_nesting: Vec<String>,
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
            module_nesting: Vec::new(),
        })
    }

    // ── Imports ─────────────────────────────────────────────────────────

    fn extract_imports(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut imports = Vec::new();
        Self::extract_imports_from_node(tree.root_node(), code, file_id, 0, &mut imports);
        imports
    }

    fn extract_imports_from_node(
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
                Self::extract_import_from_node(arg_node, code, file_id, imports);
            }
        } else {
            for child in node.children(&mut node.walk()) {
                Self::extract_imports_from_node(child, code, file_id, depth + 1, imports);
            }
        }
    }

    fn extract_import_from_node(
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
                        Self::extract_import_list_item(child, code, file_id, &prefix, imports);
                    }
                }
            }
            "scoped_use_list" => {
                if let Some(list_node) = node.child_by_field_name("list") {
                    Self::extract_import_from_node(list_node, code, file_id, imports);
                }
            }
            _ => {}
        }
    }

    fn extract_import_list_item(
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
        self.module_nesting.clear();
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
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
                        sym = sym.with_signature(Self::extract_signature(node, code));
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
            // A union has the same shape as a struct — a name and a field list —
            // and the index has no Union kind, so it is recorded as one.
            "struct_item" | "union_item" => {
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
                        sym = sym.with_signature(Self::extract_struct_signature(node, code));
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
                        sym = sym.with_signature(Self::extract_enum_signature(node, code));
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
                        sym = sym.with_signature(Self::extract_type_alias_signature(node, code));
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
                        sym = sym.with_signature(Self::extract_const_signature(node, code));
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
                        sym = sym.with_signature(Self::extract_trait_signature(node, code));
                        symbols.push(sym);
                    }

                    self.context.enter_scope(ScopeType::Class);
                    // The requirements below belong to the trait, not to
                    // whatever type happened to be open around it.
                    let saved_cls = self.context.current_class().map(str::to_string);
                    self.context
                        .set_current_class(Some(code[name_node.byte_range()].to_string()));
                    if let Some(body) = node.child_by_field_name("body") {
                        for child in body.children(&mut body.walk()) {
                            // A trait body holds more than methods: `type Out;`
                            // and `const N: usize;` are part of its contract and
                            // were dropped along with every mention of them.
                            let kind = match child.kind() {
                                "function_signature_item" | "function_item" => SymbolKind::Method,
                                "associated_type" => SymbolKind::TypeAlias,
                                "const_item" => SymbolKind::Constant,
                                _ => continue,
                            };
                            if let Some(mn) = child.child_by_field_name("name") {
                                if let Some(mut ms) =
                                    self.create_symbol(counter, child, mn, kind, file_id, code)
                                {
                                    ms = ms.with_signature(Self::extract_signature(child, code));
                                    symbols.push(ms);
                                }
                            }
                        }
                    }
                    self.context.exit_scope();
                    self.context.set_current_class(saved_cls);
                }
                return;
            }
            "impl_item" => {
                let impl_type = node
                    .child_by_field_name("type")
                    .and_then(|t| Self::extract_type_name(t, code));

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
                let mod_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(name_node) = node.child_by_field_name("name") {
                    // Created before the push: `mod inner` is declared *in* the
                    // enclosing module, not in itself.
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

                let nested = mod_name.is_some();
                if let Some(name) = mod_name {
                    self.module_nesting.push(name);
                }
                for child in node.children(&mut node.walk()) {
                    if child.kind() != "identifier" {
                        self.extract_symbols(child, code, file_id, symbols, counter, depth + 1);
                    }
                }
                if nested {
                    self.module_nesting.pop();
                }
                return;
            }
            // A body-less `fn` declaration inside an `extern "C"` block. The
            // trait arm handles its own signature items and returns before
            // recursing, so only foreign declarations reach here.
            "function_signature_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    if let Some(mut sym) = self.create_symbol(
                        counter,
                        node,
                        name_node,
                        SymbolKind::Function,
                        file_id,
                        code,
                    ) {
                        sym = sym.with_signature(Self::extract_signature(node, code));
                        symbols.push(sym);
                    }
                }
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
        let doc_comment = Self::extract_doc_comments_outer(&doc_node, code);

        let mut symbol = Symbol::new(id, name, kind, file_id, range);
        symbol.scope_context = Some(self.context.current_scope_context());

        // Relative to the file: COLLECT joins it onto the address the path gives.
        if !self.module_nesting.is_empty() {
            symbol = symbol.with_module_path(self.module_nesting.join("::"));
        }

        // Check visibility modifier.
        if let Some(parent) = name_node.parent() {
            for child in parent.children(&mut parent.walk()) {
                if child.kind() == "visibility_modifier" {
                    symbol = symbol.with_visibility(rust_visibility(&code[child.byte_range()]));
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

    fn extract_signature(node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    fn extract_struct_signature(node: Node, code: &str) -> String {
        Self::extract_signature(node, code)
    }

    fn extract_trait_signature(node: Node, code: &str) -> String {
        Self::extract_signature(node, code)
    }

    fn extract_enum_signature(node: Node, code: &str) -> String {
        Self::extract_signature(node, code)
    }

    fn extract_type_alias_signature(node: Node, code: &str) -> String {
        code[node.byte_range()].trim().to_string()
    }

    fn extract_const_signature(node: Node, code: &str) -> String {
        code[node.byte_range()].trim().to_string()
    }

    // ── Type name extraction ────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_type_name<'a>(node: Node, code: &'a str) -> Option<&'a str> {
        match node.kind() {
            "type_identifier" | "primitive_type" | "scoped_type_identifier" => {
                Some(&code[node.byte_range()])
            }
            "generic_type" => node
                .child_by_field_name("type")
                .and_then(|t| Self::extract_type_name(t, code)),
            _ => {
                for child in node.children(&mut node.walk()) {
                    if let Some(name) = Self::extract_type_name(child, code) {
                        return Some(name);
                    }
                }
                None
            }
        }
    }

    // ── Doc comments ────────────────────────────────────────────────────

    fn classify_doc_comment(text: &str) -> DocCommentType {
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

    fn is_outer_doc(text: &str) -> bool {
        matches!(
            Self::classify_doc_comment(text),
            DocCommentType::OuterLine | DocCommentType::OuterBlock
        )
    }

    fn is_inner_doc(text: &str) -> bool {
        matches!(
            Self::classify_doc_comment(text),
            DocCommentType::InnerLine | DocCommentType::InnerBlock
        )
    }

    fn extract_doc_comments_outer(node: &Node, code: &str) -> Option<String> {
        let mut doc_lines = Vec::new();
        let mut current = node.prev_sibling();

        while let Some(sibling) = current {
            match sibling.kind() {
                // Attributes sit between the doc comment and the item they
                // decorate (`/// doc` → `#[derive(Debug)]` → `struct`). Walk
                // past them, or every attributed item loses its documentation.
                "attribute_item" => {}
                "line_comment" | "block_comment" => {
                    if let Ok(text) = sibling.utf8_text(code.as_bytes()) {
                        if Self::is_outer_doc(text) {
                            let content = match Self::classify_doc_comment(text) {
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

    fn extract_inner_doc_comments(node: &Node, code: &str) -> Option<String> {
        let mut parts = Vec::new();
        Self::collect_inner_docs(node, code, 0, &mut parts);
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    fn collect_inner_docs<'a>(node: &Node, code: &'a str, depth: usize, parts: &mut Vec<&'a str>) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        for child in node.children(&mut node.walk()) {
            if matches!(child.kind(), "line_comment" | "block_comment") {
                if let Ok(text) = child.utf8_text(code.as_bytes()) {
                    if Self::is_inner_doc(text) {
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
                Self::collect_inner_docs(&child, code, depth + 1, parts);
            }
        }
    }

    // ── Containing function helper ──────────────────────────────────────

    // ── Calls ───────────────────────────────────────────────────────────

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        Self::find_calls_in_node(tree.root_node(), code, Some("<module>"), 0, &mut calls);
        calls
    }

    fn find_calls_in_node<'a>(
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
                    "field_expression" => fn_node
                        .child_by_field_name("field")
                        .map(|f| &code[f.byte_range()]),
                    "identifier" | "scoped_identifier" => Some(&code[fn_node.byte_range()]),
                    _ => None,
                };

                if let (Some(target), Some(caller)) = (target, current_fn) {
                    calls.push((caller, target, node_range(node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_calls_in_node(child, code, current_fn, depth + 1, calls);
        }
    }

    /// Macro invocations: `println!`, `anyhow!`, `tracing::warn!`.
    ///
    /// A separate node kind from `call_expression`, so this walk and
    /// [`find_calls_in_node`](Self::find_calls_in_node) never see each other's
    /// nodes and a name cannot land in both.
    fn find_macro_expansions_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        Self::find_macros_in_node(tree.root_node(), code, Some("<module>"), 0, &mut found);
        found
    }

    fn find_macros_in_node<'a>(
        node: Node,
        code: &'a str,
        current_fn: Option<&'a str>,
        depth: usize,
        found: &mut Vec<(&'a str, &'a str, Range)>,
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

        if node.kind() == "macro_invocation" {
            if let (Some(name_node), Some(caller)) = (Self::macro_name_node(node), current_fn) {
                found.push((caller, &code[name_node.byte_range()], node_range(node)));
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_macros_in_node(child, code, current_fn, depth + 1, found);
        }
    }

    /// The name node of a macro invocation: `println` in `println!("x")`,
    /// `tracing::warn` in `tracing::warn!("x")`.
    fn macro_name_node(node: Node) -> Option<Node> {
        node.child_by_field_name("macro")
    }

    // ── Implementations ─────────────────────────────────────────────────

    fn find_implementations_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut impls = Vec::new();
        Self::find_implementations_in_node(tree.root_node(), code, 0, &mut impls);
        impls
    }

    fn find_implementations_in_node<'a>(
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
                        Self::extract_type_name(trait_node, code),
                        Self::extract_type_name(type_node, code),
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
            Self::find_implementations_in_node(child, code, depth + 1, impls);
        }
    }

    // ── Type uses ───────────────────────────────────────────────────────

    fn find_uses_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut uses = Vec::new();
        Self::find_uses_in_node(tree.root_node(), code, 0, &mut uses);
        uses
    }

    fn find_uses_in_node<'a>(
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
                            // A named struct wraps each field in a
                            // `field_declaration` with a `type` field. A tuple
                            // struct has no names, so the types sit directly in
                            // the ordered list and there is nothing to unwrap.
                            let type_node = match child.kind() {
                                "field_declaration" => child.child_by_field_name("type"),
                                _ if body.kind() == "ordered_field_declaration_list" => {
                                    Some(child).filter(|c| c.is_named())
                                }
                                _ => None,
                            };
                            let Some(type_node) = type_node else {
                                continue;
                            };
                            if let Some(type_name) = Self::extract_type_name(type_node, code) {
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
            "function_item" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let fn_name = &code[name_node.byte_range()];
                    if let Some(params) = node.child_by_field_name("parameters") {
                        for param in params.children(&mut params.walk()) {
                            if param.kind() == "parameter" {
                                if let Some(type_node) = param.child_by_field_name("type") {
                                    if let Some(type_name) =
                                        Self::extract_type_name(type_node, code)
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
                        if let Some(type_name) = Self::extract_type_name(ret, code) {
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
            Self::find_uses_in_node(child, code, depth + 1, uses);
        }
    }

    // ── Defines ─────────────────────────────────────────────────────────

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
                    if let Some(type_name) = Self::extract_type_name(type_node, code) {
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
            Self::find_defines_in_node(child, code, depth + 1, defines);
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
        let outer = Self::extract_doc_comments_outer(node, code);
        let inner = Self::extract_inner_doc_comments(node, code);
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

    fn find_macro_expansions<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_macro_expansions_impl(code)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pub`, `pub(crate)` and `pub(super)` are three different answers to
    /// "is this part of the public API", and reporting all three as Public put
    /// every internal item in that answer.
    #[test]
    fn each_visibility_modifier_maps_to_its_own_level() {
        let mut parser = RustParser::new().unwrap();
        let code = "pub fn a() {}\n\
                    pub(crate) fn b() {}\n\
                    pub(super) fn c() {}\n\
                    pub(in crate::x) fn d() {}\n\
                    pub(self) fn e() {}\n\
                    fn f() {}\n";
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, FileId::new(1).unwrap(), &mut counter);

        let level = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} not parsed"))
                .visibility
        };
        assert_eq!(level("a"), Visibility::Public);
        assert_eq!(level("b"), Visibility::Crate);
        assert_eq!(level("c"), Visibility::Module);
        assert_eq!(level("d"), Visibility::Module);
        assert_eq!(level("e"), Visibility::Private);
        assert_eq!(level("f"), Visibility::Private);
    }

    /// The `name`/`kind` pairs the symbols of `code` carry.
    fn kinds_of(code: &str) -> Vec<(String, SymbolKind)> {
        let mut parser = RustParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        parser
            .parse_symbols(code, file_id, &mut counter)
            .iter()
            .map(|s| (s.name.as_ref().to_string(), s.kind))
            .collect()
    }

    #[test]
    fn a_union_produces_a_struct_symbol_with_its_fields() {
        let kinds = kinds_of("pub union Bits { i: i32, f: f32 }\n");
        assert!(
            kinds.contains(&("Bits".into(), SymbolKind::Struct)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("i".into(), SymbolKind::Field)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("f".into(), SymbolKind::Field)),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_trait_associated_type_produces_a_type_alias_symbol() {
        let kinds = kinds_of("pub trait T { type Out; }\n");
        assert!(
            kinds.contains(&("Out".into(), SymbolKind::TypeAlias)),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_trait_associated_const_produces_a_constant_symbol() {
        let kinds = kinds_of("pub trait T { const N: usize; }\n");
        assert!(
            kinds.contains(&("N".into(), SymbolKind::Constant)),
            "{kinds:?}"
        );
    }

    #[test]
    fn an_extern_function_declaration_produces_a_function_symbol() {
        let kinds = kinds_of("unsafe extern \"C\" { pub fn c_fn(x: i32) -> i32; }\n");
        assert!(
            kinds.contains(&("c_fn".into(), SymbolKind::Function)),
            "{kinds:?}"
        );
    }

    #[test]
    fn tuple_struct_field_types_produce_uses_relationships() {
        let mut parser = RustParser::new().unwrap();
        let uses = parser.find_uses("pub struct Wrapper(pub String, Inner);\n");
        let pairs: Vec<(&str, &str)> = uses.iter().map(|(a, b, _)| (*a, *b)).collect();
        // Exact: `pub` is a named sibling of the types in the ordered list and
        // must not become a use of its own.
        assert_eq!(pairs, vec![("Wrapper", "String"), ("Wrapper", "Inner")]);
    }

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

    /// Extract the doc comment recorded for a named symbol.
    fn doc_of(code: &str, name: &str) -> Option<String> {
        let mut parser = RustParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        symbols
            .iter()
            .find(|s| s.name.as_ref() == name)
            .and_then(|s| s.doc_comment.as_ref().map(|d| d.to_string()))
    }

    #[test]
    fn doc_comment_survives_a_derive_attribute() {
        let code = r#"
/// Documented struct.
#[derive(Debug, Clone)]
pub struct Attributed { pub a: u32 }
        "#;
        assert_eq!(
            doc_of(code, "Attributed").as_deref(),
            Some("Documented struct.")
        );
    }

    #[test]
    fn doc_comment_survives_stacked_attributes() {
        let code = r#"
/// First line.
/// Second line.
#[derive(Debug)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Stacked { pub a: u32 }
        "#;
        assert_eq!(
            doc_of(code, "Stacked").as_deref(),
            Some("First line.\nSecond line.")
        );
    }

    #[test]
    fn doc_comment_survives_an_attribute_on_a_function() {
        let code = r#"
/// Documented function.
#[inline]
pub fn attributed_fn() {}
        "#;
        assert_eq!(
            doc_of(code, "attributed_fn").as_deref(),
            Some("Documented function.")
        );
    }

    #[test]
    fn quadruple_slash_before_an_attribute_is_not_a_doc_comment() {
        let code = r#"
//// Not a doc comment.
#[inline]
fn not_documented() {}
        "#;
        assert_eq!(doc_of(code, "not_documented"), None);
    }

    #[test]
    fn a_plain_comment_between_doc_and_item_still_stops_the_walk() {
        let code = r#"
/// Real doc.
// Plain note.
#[inline]
fn interrupted() {}
        "#;
        assert_eq!(doc_of(code, "interrupted"), None);
    }

    const MACRO_CODE: &str = r#"
fn m() {
    println!("hi");
    tracing::warn!("x");
    let e = anyhow!("boom");
}
        "#;

    #[test]
    fn a_bare_macro_invocation_is_an_expansion() {
        let mut parser = RustParser::new().unwrap();
        let found = parser.find_macro_expansions_impl(MACRO_CODE);
        assert!(
            found
                .iter()
                .any(|(caller, target, _)| *caller == "m" && *target == "println"),
            "expected m -> println, got {found:?}"
        );
    }

    #[test]
    fn a_scoped_macro_invocation_keeps_its_path() {
        let mut parser = RustParser::new().unwrap();
        let found = parser.find_macro_expansions_impl(MACRO_CODE);
        assert!(
            found
                .iter()
                .any(|(caller, target, _)| *caller == "m" && *target == "tracing::warn"),
            "expected m -> tracing::warn, got {found:?}"
        );
    }

    #[test]
    fn a_macro_invocation_in_a_let_initializer_is_an_expansion() {
        let mut parser = RustParser::new().unwrap();
        let found = parser.find_macro_expansions_impl(MACRO_CODE);
        assert!(
            found
                .iter()
                .any(|(caller, target, _)| *caller == "m" && *target == "anyhow"),
            "expected m -> anyhow, got {found:?}"
        );
    }

    /// A macro must not also be reported as a call: `println` names no
    /// function, and an edge to it is an edge to nothing.
    #[test]
    fn a_macro_invocation_is_not_reported_as_a_call() {
        let mut parser = RustParser::new().unwrap();
        let calls = parser.find_calls_impl(MACRO_CODE);
        assert!(
            !calls
                .iter()
                .any(|(_, target, _)| matches!(*target, "println" | "anyhow" | "tracing::warn")),
            "macros must not appear among calls, got {calls:?}"
        );
    }

    /// A function is still a call even when a macro of the same name exists.
    /// The distinction is the invocation form, not the name.
    #[test]
    fn a_function_sharing_a_macro_name_is_still_a_call() {
        let mut parser = RustParser::new().unwrap();
        let code = "fn write() {}\nfn m() { write(); write!(f, \"x\"); }\n";

        let calls = parser.find_calls_impl(code);
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "m" && *target == "write"),
            "the function call must stay a call, got {calls:?}"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(caller, target, _)| *caller == "m" && *target == "write")
                .count(),
            1,
            "the macro must not add a second call edge, got {calls:?}"
        );
        assert!(
            parser
                .find_macro_expansions_impl(code)
                .iter()
                .any(|(caller, target, _)| *caller == "m" && *target == "write"),
            "the macro invocation must still be recorded as an expansion"
        );
    }

    const TOP_LEVEL_CODE: &str = r#"
static REG: u32 = compute();
const LIMIT: usize = derive_limit();
fn inside() { helper(); }
        "#;

    #[test]
    fn a_call_in_a_static_initializer_is_attributed_to_the_module() {
        let mut parser = RustParser::new().unwrap();
        let calls = parser.find_calls_impl(TOP_LEVEL_CODE);
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "<module>" && *target == "compute"),
            "expected <module> -> compute, got {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "<module>" && *target == "derive_limit"),
            "expected <module> -> derive_limit, got {calls:?}"
        );
    }

    #[test]
    fn a_call_inside_a_function_keeps_the_function_as_caller() {
        let mut parser = RustParser::new().unwrap();
        let calls = parser.find_calls_impl(TOP_LEVEL_CODE);
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "inside" && *target == "helper"),
            "expected inside -> helper, got {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|(caller, target, _)| *caller == "<module>" && *target == "helper"),
            "helper() must not be attributed to the module, got {calls:?}"
        );
    }
}
