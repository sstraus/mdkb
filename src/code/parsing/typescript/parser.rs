//! TypeScript language parser implementation using tree-sitter-typescript (TSX grammar).
//!
//! Uses the TSX grammar to handle both TypeScript and TSX/JSX files.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::method_call::MethodCall;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
use crate::code::symbol::{ScopeContext, Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct TypeScriptParser {
    parser: CachingParser,
    context: ParserContext,
    /// Symbols marked as default exported (export default <name>).
    default_exported: std::collections::HashSet<String>,
    /// Symbols marked as named exported (export { Name }).
    named_exported: std::collections::HashSet<String>,
}

impl std::fmt::Debug for TypeScriptParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypeScriptParser")
            .field("language", &"TypeScript")
            .finish()
    }
}

impl TypeScriptParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        // Use the TSX grammar so TSX/JSX syntax parses correctly.
        let language: tree_sitter::Language = tree_sitter_typescript::LANGUAGE_TSX.into();
        ts_parser
            .set_language(&language)
            .map_err(|e| format!("Failed to set TypeScript language: {e}"))?;

        Ok(Self {
            parser: CachingParser::new(ts_parser),
            context: ParserContext::new(),
            default_exported: std::collections::HashSet::new(),
            named_exported: std::collections::HashSet::new(),
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
        self.default_exported.clear();
        self.named_exported.clear();

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

        // Post-process: mark default/named exported symbols as Public
        for symbol in &mut symbols {
            if self.default_exported.contains(symbol.name.as_ref())
                || self.named_exported.contains(symbol.name.as_ref())
            {
                symbol.visibility = Visibility::Public;
            }
        }

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
            "function_declaration" | "generator_function_declaration" => {
                let func_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) =
                    self.process_function(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }

                self.context
                    .enter_scope(ScopeType::Function { hoisting: true });
                let saved_fn = self.context.current_function().map(|s| s.to_string());
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_function(func_name);

                if let Some(body) = node.child_by_field_name("body") {
                    self.extract_symbols_from_node(
                        body,
                        code,
                        file_id,
                        counter,
                        symbols,
                        module_path,
                        depth + 1,
                    );
                }

                self.context.exit_scope();
                self.context.set_current_function(saved_fn);
                self.context.set_current_class(saved_cls);
            }

            "class_declaration" | "abstract_class_declaration" => {
                let class_name = node
                    .children(&mut node.walk())
                    .find(|n| n.kind() == "type_identifier")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) = self.process_class(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                    self.context.enter_scope(ScopeType::Class);
                    let saved_fn = self.context.current_function().map(|s| s.to_string());
                    let saved_cls = self.context.current_class().map(|s| s.to_string());
                    self.context.set_current_class(class_name);

                    self.extract_class_members(
                        node,
                        code,
                        file_id,
                        counter,
                        symbols,
                        module_path,
                        depth + 1,
                    );

                    self.context.exit_scope();
                    self.context.set_current_function(saved_fn);
                    self.context.set_current_class(saved_cls);
                }
            }

            "interface_declaration" => {
                if let Some(symbol) =
                    self.process_interface(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
            }

            "type_alias_declaration" => {
                if let Some(symbol) =
                    self.process_type_alias(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
            }

            "enum_declaration" => {
                if let Some(symbol) = self.process_enum(node, code, file_id, counter, module_path) {
                    symbols.push(symbol);
                }
            }

            "lexical_declaration" | "variable_declaration" => {
                self.process_variable_declaration(
                    node,
                    code,
                    file_id,
                    counter,
                    symbols,
                    module_path,
                    depth + 1,
                );
            }

            "export_statement" => {
                let children: Vec<Node> = node.children(&mut node.walk()).collect();

                // Track default exports
                for (i, child) in children.iter().enumerate() {
                    if child.kind() == "default" {
                        if let Some(next) = children.get(i + 1) {
                            if next.kind() == "identifier" {
                                let name = code[next.byte_range()].to_string();
                                self.default_exported.insert(name);
                            }
                        }
                    }
                }

                // Track named export lists: export { Foo, Bar }
                for child in &children {
                    if child.kind() == "export_clause" {
                        for spec in child.children(&mut child.walk()) {
                            if spec.kind() == "export_specifier" {
                                if let Some(name_node) = spec.child_by_field_name("name") {
                                    let name = code[name_node.byte_range()].to_string();
                                    self.named_exported.insert(name);
                                }
                            }
                        }
                    }
                }

                // Process children for nested declarations (export function foo())
                let is_default = children.iter().any(|c| c.kind() == "default");
                if !is_default {
                    for child in children {
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
        let signature = self.extract_signature(node, code);
        let doc_comment = extract_jsdoc(&node, code);
        let visibility = determine_ts_visibility(node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Function,
            file_id,
            node_range(node),
            Some(signature),
            doc_comment,
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
        let name = node
            .children(&mut node.walk())
            .find(|n| n.kind() == "type_identifier")
            .map(|n| &code[n.byte_range()])?;

        let signature = self.extract_class_signature(node, code);
        let doc_comment = extract_jsdoc(&node, code);
        let visibility = determine_ts_visibility(node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Class,
            file_id,
            node_range(node),
            Some(signature),
            doc_comment,
            module_path,
            visibility,
        ))
    }

    fn extract_class_members(
        &mut self,
        class_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
        depth: usize,
    ) {
        if let Some(body) = class_node.child_by_field_name("body") {
            for child in body.children(&mut body.walk()) {
                match child.kind() {
                    "method_definition" => {
                        let method_name = child
                            .child_by_field_name("name")
                            .map(|n| code[n.byte_range()].to_string());

                        if let Some(symbol) =
                            self.process_method(child, code, file_id, counter, module_path)
                        {
                            symbols.push(symbol);
                        }

                        if let Some(body) = child.child_by_field_name("body") {
                            self.context
                                .enter_scope(ScopeType::Function { hoisting: false });
                            let saved_fn = self.context.current_function().map(|s| s.to_string());
                            self.context.set_current_function(method_name);

                            self.extract_symbols_from_node(
                                body,
                                code,
                                file_id,
                                counter,
                                symbols,
                                module_path,
                                depth + 1,
                            );

                            self.context.exit_scope();
                            self.context.set_current_function(saved_fn);
                        }
                    }
                    "public_field_definition" | "property_declaration" => {
                        if let Some(symbol) =
                            self.process_property(child, code, file_id, counter, module_path)
                        {
                            symbols.push(symbol);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn process_interface(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let signature = self.extract_interface_signature(node, code);
        let doc_comment = extract_jsdoc(&node, code);
        let visibility = determine_ts_visibility(node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Interface,
            file_id,
            node_range(node),
            Some(signature),
            doc_comment,
            module_path,
            visibility,
        ))
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
        let signature = code[node.byte_range()].to_string();
        let doc_comment = extract_jsdoc(&node, code);
        let visibility = determine_ts_visibility(node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::TypeAlias,
            file_id,
            node_range(node),
            Some(signature),
            doc_comment,
            module_path,
            visibility,
        ))
    }

    fn process_enum(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let signature = code[node.byte_range()].to_string();
        let doc_comment = extract_jsdoc(&node, code);
        let visibility = determine_ts_visibility(node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Enum,
            file_id,
            node_range(node),
            Some(signature),
            doc_comment,
            module_path,
            visibility,
        ))
    }

    fn process_method(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let signature = self.extract_signature(node, code);
        let doc_comment = extract_jsdoc(&node, code);
        let visibility = determine_member_visibility(node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Method,
            file_id,
            node_range(node),
            Some(signature),
            doc_comment,
            module_path,
            visibility,
        ))
    }

    fn process_property(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let doc_comment = extract_jsdoc(&node, code);
        let visibility = determine_member_visibility(node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Field,
            file_id,
            node_range(node),
            None,
            doc_comment,
            module_path,
            visibility,
        ))
    }

    fn process_variable_declaration(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
        depth: usize,
    ) {
        for child in node.children(&mut node.walk()) {
            if child.kind() != "variable_declarator" {
                continue;
            }
            let name_node = match child.child_by_field_name("name") {
                Some(n) if n.kind() == "identifier" => n,
                _ => continue,
            };
            let name = &code[name_node.byte_range()];

            let is_arrow_fn = child
                .child_by_field_name("value")
                .is_some_and(|v| v.kind() == "arrow_function");

            let kind = if is_arrow_fn {
                SymbolKind::Function
            } else if code[node.byte_range()].starts_with("const") {
                SymbolKind::Constant
            } else {
                SymbolKind::Variable
            };

            let visibility = determine_ts_visibility(node, code);
            let doc_comment = extract_jsdoc(&node, code);

            let mut symbol = self.create_symbol(
                counter.next_id(),
                name.to_string(),
                kind,
                file_id,
                node_range(child),
                None,
                doc_comment,
                module_path,
                visibility,
            );

            // Arrow functions are never hoisted
            if is_arrow_fn {
                let (parent_name, parent_kind) =
                    if let Some(fn_name) = self.context.current_function() {
                        (Some(fn_name.into()), Some(SymbolKind::Function))
                    } else if let Some(cls_name) = self.context.current_class() {
                        (Some(cls_name.into()), Some(SymbolKind::Class))
                    } else {
                        (None, None)
                    };
                symbol.scope_context = Some(ScopeContext::Local {
                    hoisted: false,
                    parent_name,
                    parent_kind,
                });
            }

            symbols.push(symbol);

            // Process arrow function body for nested symbols
            if is_arrow_fn {
                if let Some(value_node) = child.child_by_field_name("value") {
                    if let Some(body) = value_node.child_by_field_name("body") {
                        let saved_fn = self.context.current_function().map(|s| s.to_string());
                        let saved_cls = self.context.current_class().map(|s| s.to_string());
                        self.context
                            .enter_scope(ScopeType::Function { hoisting: false });
                        self.context.set_current_function(Some(name.to_string()));

                        self.extract_symbols_from_node(
                            body,
                            code,
                            file_id,
                            counter,
                            symbols,
                            module_path,
                            depth + 1,
                        );

                        self.context.exit_scope();
                        self.context.set_current_function(saved_fn);
                        self.context.set_current_class(saved_cls);
                    }
                }
            }
        }
    }

    // ── Signatures ──────────────────────────────────────────────────────

    fn extract_signature(&self, node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    fn extract_class_signature(&self, node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    fn extract_interface_signature(&self, node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    // ── Imports ─────────────────────────────────────────────────────────

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        self.collect_imports(tree.root_node(), code, file_id, &mut imports);
        imports
    }

    fn collect_imports(&self, node: Node, code: &str, file_id: FileId, imports: &mut Vec<Import>) {
        match node.kind() {
            "import_statement" => {
                self.process_import_statement(node, code, file_id, imports);
            }
            "export_statement" => {
                if node.child_by_field_name("source").is_some() {
                    self.process_export_reexport(node, code, file_id, imports);
                }
            }
            _ => {
                for child in node.children(&mut node.walk()) {
                    self.collect_imports(child, code, file_id, imports);
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
        // Check for type-only import
        let mut is_type_only = false;
        for (i, child) in node.children(&mut node.walk()).enumerate() {
            if child.kind() == "type" && i == 1 {
                is_type_only = true;
            }
        }

        let source_node = match node.child_by_field_name("source") {
            Some(n) => n,
            None => return,
        };
        let source_path = code[source_node.byte_range()]
            .trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .to_string();

        let import_clause = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "import_clause");

        if let Some(clause) = import_clause {
            let mut has_default = false;
            let mut has_named = false;
            let mut has_namespace = false;
            let mut default_name = None;
            let mut namespace_name = None;

            for child in clause.children(&mut clause.walk()) {
                match child.kind() {
                    "identifier" => {
                        has_default = true;
                        default_name = Some(code[child.byte_range()].to_string());
                    }
                    "named_imports" => {
                        has_named = true;
                        for spec in child.children(&mut child.walk()) {
                            if spec.kind() == "import_specifier" {
                                let local = spec
                                    .children(&mut spec.walk())
                                    .filter(|p| p.kind() == "identifier")
                                    .last()
                                    .map(|p| code[p.byte_range()].to_string());
                                imports.push(Import {
                                    path: source_path.clone(),
                                    alias: local,
                                    file_id,
                                    is_glob: false,
                                    is_type_only,
                                });
                            }
                        }
                    }
                    "namespace_import" => {
                        has_namespace = true;
                        let children: Vec<_> = child.children(&mut child.walk()).collect();
                        if let Some(ident) = children.iter().rfind(|n| n.kind() == "identifier") {
                            namespace_name = Some(code[ident.byte_range()].to_string());
                        }
                    }
                    _ => {}
                }
            }

            if has_namespace {
                imports.push(Import {
                    path: source_path,
                    alias: namespace_name,
                    file_id,
                    is_glob: true,
                    is_type_only,
                });
            } else if has_default {
                imports.push(Import {
                    path: source_path,
                    alias: default_name,
                    file_id,
                    is_glob: false,
                    is_type_only,
                });
            } else if !has_named {
                // Side-effect import
                imports.push(Import {
                    path: source_path,
                    alias: None,
                    file_id,
                    is_glob: false,
                    is_type_only: false,
                });
            }
        } else {
            // Side-effect import
            imports.push(Import {
                path: source_path,
                alias: None,
                file_id,
                is_glob: false,
                is_type_only: false,
            });
        }
    }

    fn process_export_reexport(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
        let source_node = match node.child_by_field_name("source") {
            Some(n) => n,
            None => return,
        };
        let source_path = code[source_node.byte_range()]
            .trim_matches(|c| c == '"' || c == '\'' || c == '`')
            .to_string();

        let node_text = &code[node.byte_range()];
        let is_type_only = node_text.starts_with("export type");
        let is_glob = node_text.contains("* from");

        imports.push(Import {
            path: source_path,
            alias: None,
            file_id,
            is_glob,
            is_type_only,
        });
    }

    // ── Calls ───────────────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_calls_recursive<'a>(
        &self,
        node: &Node,
        code: &'a str,
        current_fn: Option<&'a str>,
        calls: &mut Vec<(&'a str, &'a str, Range)>,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        let fn_ctx = match node.kind() {
            "function_declaration" | "generator_function_declaration" | "method_declaration" => {
                node.child_by_field_name("name")
                    .map(|n| &code[n.byte_range()])
                    .or(current_fn)
            }
            "arrow_function" => {
                // Check parent for variable name
                node.parent()
                    .filter(|p| p.kind() == "variable_declarator")
                    .and_then(|p| p.child_by_field_name("name"))
                    .map(|n| &code[n.byte_range()])
                    .or(current_fn)
            }
            _ => current_fn,
        };

        if node.kind() == "call_expression" {
            if let Some(function_node) = node.child_by_field_name("function") {
                if function_node.kind() != "member_expression" {
                    if let Some(fn_name) = extract_ts_function_name(&function_node, code) {
                        if let Some(ctx) = fn_ctx {
                            calls.push((ctx, fn_name, node_range(*node)));
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.extract_calls_recursive(&child, code, fn_ctx, calls, depth + 1);
        }
    }

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.extract_calls_recursive(&tree.root_node(), code, Some("<module>"), &mut calls, 0);
        calls
    }

    // ── Method calls ────────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_method_calls_recursive(
        &self,
        node: &Node,
        code: &str,
        current_fn: Option<&str>,
        calls: &mut Vec<MethodCall>,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        let fn_ctx = match node.kind() {
            "function_declaration" | "generator_function_declaration" | "method_declaration" => {
                node.child_by_field_name("name")
                    .map(|n| &code[n.byte_range()])
                    .or(current_fn)
            }
            "arrow_function" => node
                .parent()
                .filter(|p| p.kind() == "variable_declarator")
                .and_then(|p| p.child_by_field_name("name"))
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        if node.kind() == "call_expression" {
            if let Some(function_node) = node.child_by_field_name("function") {
                if function_node.kind() == "member_expression" {
                    if let Some((receiver, method_name)) =
                        extract_ts_method_parts(&function_node, code)
                    {
                        if let Some(ctx) = fn_ctx {
                            calls.push(MethodCall {
                                caller: ctx.to_string(),
                                method_name: method_name.to_string(),
                                receiver: Some(receiver.to_string()),
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
            self.extract_method_calls_recursive(&child, code, fn_ctx, calls, depth + 1);
        }
    }

    fn find_method_calls_impl(&mut self, code: &str) -> Vec<MethodCall> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.extract_method_calls_recursive(
            &tree.root_node(),
            code,
            Some("<module>"),
            &mut calls,
            0,
        );
        calls
    }

    // ── Implementations / extends ───────────────────────────────────────

    fn find_implementations_in_node<'a>(
        &self,
        node: Node,
        code: &'a str,
        results: &mut Vec<(&'a str, &'a str, Range)>,
        extends_only: bool,
    ) {
        match node.kind() {
            "class_declaration" | "abstract_class_declaration" => {
                let class_name = node
                    .children(&mut node.walk())
                    .find(|n| n.kind() == "type_identifier")
                    .map(|n| &code[n.byte_range()]);

                if let Some(class_name) = class_name {
                    for child in node.children(&mut node.walk()) {
                        if child.kind() == "class_heritage" {
                            self.process_heritage(child, code, class_name, results, extends_only);
                        }
                    }
                }
            }
            "interface_declaration" => {
                if extends_only {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let iface_name = &code[name_node.byte_range()];
                        for child in node.children(&mut node.walk()) {
                            if child.kind() == "extends_type_clause" {
                                if let Some(type_node) = child.child_by_field_name("type") {
                                    if let Some(base) = extract_ts_type_name(type_node, code) {
                                        results.push((iface_name, base, node_range(type_node)));
                                    }
                                } else {
                                    self.process_extends_clause(child, code, iface_name, results);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            self.find_implementations_in_node(child, code, results, extends_only);
        }
    }

    fn process_heritage<'a>(
        &self,
        heritage_node: Node,
        code: &'a str,
        class_name: &'a str,
        results: &mut Vec<(&'a str, &'a str, Range)>,
        extends_only: bool,
    ) {
        for child in heritage_node.children(&mut heritage_node.walk()) {
            match child.kind() {
                "extends_clause" if extends_only => {
                    for ext_child in child.children(&mut child.walk()) {
                        if matches!(
                            ext_child.kind(),
                            "type_identifier"
                                | "identifier"
                                | "nested_type_identifier"
                                | "generic_type"
                        ) {
                            if let Some(base) = extract_ts_type_name(ext_child, code) {
                                results.push((class_name, base, node_range(ext_child)));
                            }
                        }
                    }
                }
                "implements_clause" if !extends_only => {
                    for impl_child in child.children(&mut child.walk()) {
                        if matches!(
                            impl_child.kind(),
                            "type_identifier"
                                | "identifier"
                                | "nested_type_identifier"
                                | "generic_type"
                        ) {
                            if let Some(iface) = extract_ts_type_name(impl_child, code) {
                                results.push((class_name, iface, node_range(impl_child)));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn process_extends_clause<'a>(
        &self,
        node: Node,
        code: &'a str,
        iface_name: &'a str,
        results: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        for child in node.children(&mut node.walk()) {
            if matches!(child.kind(), "type_identifier" | "nested_type_identifier") {
                if let Some(base) = extract_ts_type_name(child, code) {
                    results.push((iface_name, base, node_range(child)));
                }
            }
        }
    }

    // ── Type uses ───────────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_type_uses_recursive<'a>(
        &self,
        node: &Node,
        code: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        match node.kind() {
            "function_declaration" | "method_definition" => {
                let ctx = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()])
                    .unwrap_or("anonymous");

                if let Some(params) = node.child_by_field_name("parameters") {
                    self.extract_param_types(params, code, ctx, uses);
                }
                if let Some(ret) = node.child_by_field_name("return_type") {
                    extract_ts_type_from_annotation(&ret, code, ctx, uses);
                }
            }
            "class_declaration" | "abstract_class_declaration" => {
                let class_name = node
                    .children(&mut node.walk())
                    .find(|n| n.kind() == "type_identifier")
                    .map(|n| &code[n.byte_range()])
                    .unwrap_or("anonymous");

                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        if matches!(
                            child.kind(),
                            "public_field_definition" | "property_declaration"
                        ) {
                            if let Some(type_ann) = child.child_by_field_name("type") {
                                extract_ts_type_from_annotation(&type_ann, code, class_name, uses);
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            self.extract_type_uses_recursive(&child, code, uses, depth + 1);
        }
    }

    fn extract_param_types<'a>(
        &self,
        params_node: Node,
        code: &'a str,
        ctx: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        for param in params_node.children(&mut params_node.walk()) {
            if matches!(
                param.kind(),
                "required_parameter" | "optional_parameter" | "rest_parameter"
            ) {
                if let Some(type_ann) = param.child_by_field_name("type") {
                    extract_ts_type_from_annotation(&type_ann, code, ctx, uses);
                }
            }
        }
    }

    fn find_uses_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut uses = Vec::new();
        self.extract_type_uses_recursive(&tree.root_node(), code, &mut uses, 0);
        uses
    }

    // ── Method defines ──────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_method_defines_recursive<'a>(
        &self,
        node: &Node,
        code: &'a str,
        defines: &mut Vec<(&'a str, &'a str, Range)>,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        match node.kind() {
            "interface_declaration" => {
                let iface_name = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()])
                    .unwrap_or("anonymous");
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        if child.kind() == "method_signature" {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                let method = &code[name_node.byte_range()];
                                defines.push((iface_name, method, node_range(child)));
                            }
                        }
                    }
                }
            }
            "class_declaration" | "abstract_class_declaration" => {
                let class_name = node
                    .children(&mut node.walk())
                    .find(|n| n.kind() == "type_identifier")
                    .map(|n| &code[n.byte_range()])
                    .unwrap_or("anonymous");
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        if matches!(
                            child.kind(),
                            "method_definition" | "abstract_method_signature"
                        ) {
                            if let Some(name_node) = child.child_by_field_name("name") {
                                let method = &code[name_node.byte_range()];
                                defines.push((class_name, method, node_range(child)));
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            self.extract_method_defines_recursive(&child, code, defines, depth + 1);
        }
    }

    fn find_defines_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut defines = Vec::new();
        self.extract_method_defines_recursive(&tree.root_node(), code, &mut defines, 0);
        defines
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

/// Determine export-based visibility for top-level declarations.
fn determine_ts_visibility(node: Node, _code: &str) -> Visibility {
    // Walk up to 3 ancestors looking for export_statement
    let mut anc = node.parent();
    for _ in 0..3 {
        if let Some(a) = anc {
            if a.kind() == "export_statement" {
                return Visibility::Public;
            }
            anc = a.parent();
        } else {
            break;
        }
    }
    Visibility::Private
}

/// Determine visibility for class members (private/protected/public).
fn determine_member_visibility(node: Node, code: &str) -> Visibility {
    let sig = &code[node.byte_range()];
    if sig.contains("private ") || sig.starts_with('#') {
        Visibility::Private
    } else if sig.contains("protected ") {
        Visibility::Module
    } else {
        Visibility::Public
    }
}

/// Extract JSDoc comment (/** ... */) from preceding sibling.
fn extract_jsdoc(node: &Node, code: &str) -> Option<String> {
    // For exported declarations, check the parent's previous sibling
    let search_node = if let Some(parent) = node.parent() {
        if parent.kind() == "export_statement" {
            parent
        } else {
            *node
        }
    } else {
        *node
    };

    let sibling = search_node.prev_sibling()?;
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

fn extract_ts_function_name<'a>(node: &Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "identifier" => Some(&code[node.byte_range()]),
        "member_expression" => Some(&code[node.byte_range()]),
        _ => None,
    }
}

fn extract_ts_method_parts<'a>(node: &Node, code: &'a str) -> Option<(&'a str, &'a str)> {
    let obj = node.child_by_field_name("object")?;
    let prop = node.child_by_field_name("property")?;
    Some((&code[obj.byte_range()], &code[prop.byte_range()]))
}

fn extract_ts_type_name<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "type_identifier" | "identifier" | "nested_type_identifier" => {
            Some(&code[node.byte_range()])
        }
        "generic_type" => node
            .child_by_field_name("name")
            .and_then(|n| extract_ts_type_name(n, code)),
        _ => None,
    }
}

/// TS primitive types that should be filtered from type uses.
const TS_PRIMITIVES: &[&str] = &[
    "string",
    "number",
    "boolean",
    "void",
    "any",
    "never",
    "unknown",
    "null",
    "undefined",
    "object",
    "symbol",
    "bigint",
];

fn extract_ts_type_from_annotation<'a>(
    type_node: &Node,
    code: &'a str,
    ctx: &'a str,
    uses: &mut Vec<(&'a str, &'a str, Range)>,
) {
    if let Some(type_name) = extract_ts_type_name(*type_node, code) {
        if !TS_PRIMITIVES.contains(&type_name) {
            uses.push((ctx, type_name, node_range(*type_node)));
        }
    }
    // Recurse into children for nested types
    for child in type_node.children(&mut type_node.walk()) {
        if matches!(
            child.kind(),
            "type_identifier" | "generic_type" | "nested_type_identifier"
        ) {
            extract_ts_type_from_annotation(&child, code, ctx, uses);
        }
    }
}

// ── LanguageParser trait impl ───────────────────────────────────────────

impl LanguageParser for TypeScriptParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_jsdoc(node, code)
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
        self.find_implementations_in_node(tree.root_node(), code, &mut results, false);
        results
    }

    fn find_extends<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut results = Vec::new();
        self.find_implementations_in_node(tree.root_node(), code, &mut results, true);
        results
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
    fn test_parse_functions_and_classes() {
        let mut parser = TypeScriptParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
export function publicFunction(x: number): boolean {
    return true;
}

function privateFunction(): void {}

export class MyClass {
    private name: string;
    public age: number;

    constructor(name: string) {
        this.name = name;
    }

    public greet(): string {
        return `Hello ${this.name}`;
    }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "publicFunction"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Public));
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "privateFunction" && s.kind == SymbolKind::Function)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "MyClass" && s.kind == SymbolKind::Class)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "greet" && s.kind == SymbolKind::Method)
        );
    }

    #[test]
    fn test_parse_interfaces_and_types() {
        let mut parser = TypeScriptParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
export interface Serializable {
    serialize(): string;
}

export type UserId = string | number;

export enum Color {
    Red,
    Green,
    Blue,
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
                .any(|s| s.name.as_ref() == "UserId" && s.kind == SymbolKind::TypeAlias)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Color" && s.kind == SymbolKind::Enum)
        );
    }

    #[test]
    fn test_find_imports() {
        let mut parser = TypeScriptParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
import React from 'react';
import { useState, useEffect } from 'react';
import * as path from 'path';
import type { Config } from './config';
import './styles.css';
"#;

        let imports = parser.find_imports(code, file_id);

        assert!(
            imports
                .iter()
                .any(|i| i.path == "react" && i.alias == Some("React".to_string()))
        );
        assert!(
            imports
                .iter()
                .any(|i| i.path == "react" && i.alias == Some("useState".to_string()))
        );
        assert!(imports.iter().any(|i| i.path == "path" && i.is_glob));
        assert!(
            imports
                .iter()
                .any(|i| i.path == "./config" && i.is_type_only)
        );
        assert!(imports.iter().any(|i| i.path == "./styles.css"));
    }

    #[test]
    fn test_find_calls() {
        let mut parser = TypeScriptParser::new().unwrap();

        let code = r#"
function main(): void {
    process();
    console.log("hello");
    const data = getData();
}

function process(): void {}
function getData(): string { return ""; }
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
                .any(|(caller, target, _)| *caller == "main" && *target == "getData")
        );
    }

    #[test]
    fn test_find_calls_top_level() {
        let mut parser = TypeScriptParser::new().unwrap();

        // CommonJS pattern: require + top-level call (e.g., hook entry points)
        let code = r#"
const { setupHookBoilerplate } = require('./lib/auto-memory-utils');

function getStories() { return []; }

if (require.main === module) {
    setupHookBoilerplate('compact-guard');
    getStories();
}
"#;

        let calls = parser.find_calls_impl(code);
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "<module>"
                    && *target == "setupHookBoilerplate"),
            "Top-level call to setupHookBoilerplate should have <module> as caller: {:?}",
            calls
        );
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "<module>" && *target == "getStories"),
            "Top-level call to getStories should have <module> as caller: {:?}",
            calls
        );
    }

    #[test]
    fn test_find_method_calls() {
        let mut parser = TypeScriptParser::new().unwrap();

        let code = r#"
class Server {
    start(): void {}
}

function main(): void {
    const s = new Server();
    s.start();
    console.log("hello");
}
"#;

        let calls = parser.find_method_calls_impl(code);
        assert!(
            calls
                .iter()
                .any(|c| c.caller == "main" && c.method_name == "start")
        );
        assert!(
            calls
                .iter()
                .any(|c| c.caller == "main" && c.method_name == "log")
        );
    }

    #[test]
    fn test_extends_and_implements() {
        let mut parser = TypeScriptParser::new().unwrap();

        let code = r#"
interface Serializable {
    serialize(): string;
}

interface Printable {
    print(): void;
}

class Base {
    id: number;
}

class Derived extends Base implements Serializable, Printable {
    serialize(): string { return ""; }
    print(): void {}
}
"#;

        let impls = parser.find_implementations(code);
        assert!(
            impls
                .iter()
                .any(|(cls, iface, _)| *cls == "Derived" && *iface == "Serializable")
        );
        assert!(
            impls
                .iter()
                .any(|(cls, iface, _)| *cls == "Derived" && *iface == "Printable")
        );

        let extends = parser.find_extends(code);
        assert!(
            extends
                .iter()
                .any(|(cls, base, _)| *cls == "Derived" && *base == "Base")
        );
    }

    #[test]
    fn test_export_visibility() {
        let mut parser = TypeScriptParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
export function exported(): void {}
function notExported(): void {}
export const EXPORTED_CONST = 42;
const PRIVATE_CONST = 0;
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "exported" && s.visibility == Visibility::Public)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "notExported" && s.visibility == Visibility::Private)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "EXPORTED_CONST" && s.visibility == Visibility::Public)
        );
    }

    #[test]
    fn test_arrow_functions() {
        let mut parser = TypeScriptParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
const greet = (name: string): string => {
    return `Hello ${name}`;
};

export const process = () => {
    console.log("processing");
};
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "greet" && s.kind == SymbolKind::Function)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "process" && s.kind == SymbolKind::Function)
        );
    }

    #[test]
    fn test_jsdoc_extraction() {
        let mut parser = TypeScriptParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
/** Process data and return results. */
function processData(data: string[]): string[] {
    return data;
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let func = symbols
            .iter()
            .find(|s| s.name.as_ref() == "processData")
            .expect("should find processData");
        assert!(func.doc_comment.is_some());
        assert!(
            func.doc_comment
                .as_deref()
                .unwrap()
                .contains("Process data")
        );
    }
}
