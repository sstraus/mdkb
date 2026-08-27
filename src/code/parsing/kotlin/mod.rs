//! Kotlin language parser implementation using tree-sitter-kotlin-codanna 0.3.
//!
//! Supports classes, objects, interfaces, enums, functions, properties,
//! companion objects, constructors, KDoc extraction, imports, and call graphs.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::method_call::MethodCall;
use crate::code::parsing::parser::{
    LanguageParser, check_recursion_depth, is_plain_path, node_range,
};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct KotlinParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for KotlinParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KotlinParser")
            .field("language", &"Kotlin")
            .finish()
    }
}

impl KotlinParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_kotlin_codanna::language())
            .map_err(|e| format!("Failed to set Kotlin language: {e}"))?;

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

    // ── Main parse ──────────────────────────────────────────────────────

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
        let package_path = extract_package_path(tree.root_node(), code);
        let module_path = package_path.as_deref().unwrap_or("");

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
            "class_declaration" => {
                self.process_class_declaration(
                    node,
                    code,
                    file_id,
                    counter,
                    symbols,
                    (module_path, depth),
                );
            }

            "object_declaration" => {
                let obj_name = find_type_identifier(node, code);
                if let Some(symbol) = self.process_object(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(obj_name);

                if let Some(body) = find_child_by_kind(node, "class_body") {
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

            "companion_object" => {
                // Companion objects define static-like members on the enclosing class
                if let Some(body) = find_child_by_kind(node, "class_body") {
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
            }

            "function_declaration" => {
                let fn_name = find_simple_identifier(node, code);

                if let Some(symbol) =
                    self.process_function(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }

                self.context
                    .enter_scope(ScopeType::Function { hoisting: false });
                let saved_fn = self.context.current_function().map(|s| s.to_string());
                self.context.set_current_function(fn_name);

                if let Some(body) = find_child_by_kind(node, "function_body") {
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
                self.context.set_current_function(saved_fn);
            }

            "secondary_constructor" => {
                symbols.push(self.process_secondary_constructor(
                    node,
                    code,
                    file_id,
                    counter,
                    module_path,
                ));
            }

            "property_declaration" => {
                self.process_property(node, code, file_id, counter, symbols, module_path);
            }

            "type_alias" => {
                if let Some(symbol) =
                    self.process_type_alias(node, code, file_id, counter, module_path)
                {
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

    // ── Class processing (handles class, interface, enum) ───────────────

    fn process_class_declaration(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, usize),
    ) {
        let (module_path, depth) = tail;
        let class_name = find_type_identifier(node, code);
        let is_enum = child_has_kind(node, "enum");
        let is_interface = child_has_kind(node, "interface");

        let kind = if is_enum {
            SymbolKind::Enum
        } else if is_interface {
            SymbolKind::Interface
        } else {
            SymbolKind::Class
        };

        let visibility = determine_kotlin_visibility(node, code);
        let signature = build_class_signature(node, code, kind);
        let doc = extract_kdoc(&node, code);

        let name = class_name.clone().unwrap_or_default();
        if !name.is_empty() {
            symbols.push(self.create_symbol(
                counter.next_id(),
                name,
                kind,
                file_id,
                node_range(node),
                (Some(signature), doc, module_path, visibility),
            ));
        }

        // Enter class scope before processing constructor params and body
        self.context.enter_scope(ScopeType::Class);
        let saved_cls = self.context.current_class().map(|s| s.to_string());
        self.context.set_current_class(class_name);

        // Primary constructor parameters (val/var promote to fields)
        if let Some(ctor) = find_child_by_kind(node, "primary_constructor") {
            self.process_primary_constructor_params(
                ctor,
                code,
                file_id,
                counter,
                symbols,
                module_path,
            );
        }

        let body_kind = if is_enum {
            "enum_class_body"
        } else {
            "class_body"
        };

        if let Some(body) = find_child_by_kind(node, body_kind) {
            for child in body.children(&mut body.walk()) {
                if child.kind() == "enum_entry" {
                    // Enum entries are constants
                    if let Some(name_node) = find_child_by_kind(child, "simple_identifier") {
                        let entry_name = &code[name_node.byte_range()];
                        let qualified = if let Some(cls) = self.context.current_class() {
                            format!("{cls}.{entry_name}")
                        } else {
                            entry_name.to_string()
                        };
                        symbols.push(self.create_symbol(
                            counter.next_id(),
                            qualified,
                            SymbolKind::Constant,
                            file_id,
                            node_range(child),
                            (None, None, module_path, Visibility::Public),
                        ));
                    }
                } else {
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

        self.context.exit_scope();
        self.context.set_current_class(saved_cls);
    }

    // ── Primary constructor params (val/var promote to fields) ──────────

    fn process_primary_constructor_params(
        &mut self,
        ctor_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        for child in ctor_node.children(&mut ctor_node.walk()) {
            if child.kind() == "class_parameter" {
                let has_binding = find_child_by_kind(child, "binding_pattern_kind").is_some();
                if !has_binding {
                    continue;
                }

                if let Some(name_node) = find_child_by_kind(child, "simple_identifier") {
                    let name = &code[name_node.byte_range()];
                    let visibility = determine_kotlin_visibility(child, code);
                    let type_str = extract_type_annotation(child, code);

                    let qualified = if let Some(cls) = self.context.current_class() {
                        format!("{cls}.{name}")
                    } else {
                        name.to_string()
                    };

                    let sig = type_str.map(|t| format!("{name}: {t}"));

                    symbols.push(self.create_symbol(
                        counter.next_id(),
                        qualified,
                        SymbolKind::Field,
                        file_id,
                        node_range(child),
                        (sig, None, module_path, visibility),
                    ));
                }
            }
        }
    }

    // ── Symbol processors ───────────────────────────────────────────────

    fn process_object(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name = find_type_identifier(node, code)?;
        let visibility = determine_kotlin_visibility(node, code);
        let doc = extract_kdoc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name,
            SymbolKind::Class,
            file_id,
            node_range(node),
            (
                Some(format!(
                    "object {}",
                    find_type_identifier(node, code).unwrap_or_default()
                )),
                doc,
                module_path,
                visibility,
            ),
        ))
    }

    fn process_function(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name = find_simple_identifier(node, code)?;
        let visibility = determine_kotlin_visibility(node, code);
        let signature = build_function_signature(node, code);
        let doc = extract_kdoc(&node, code);

        let qualified_name = if let Some(cls) = self.context.current_class() {
            format!("{cls}.{name}")
        } else {
            name
        };

        let kind = if self.context.is_in_class() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };

        Some(self.create_symbol(
            counter.next_id(),
            qualified_name,
            kind,
            file_id,
            node_range(node),
            (Some(signature), doc, module_path, visibility),
        ))
    }

    fn process_secondary_constructor(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Symbol {
        let visibility = determine_kotlin_visibility(node, code);
        let params = find_child_by_kind(node, "function_value_parameters")
            .map(|n| &code[n.byte_range()])
            .unwrap_or("()");

        let name = if let Some(cls) = self.context.current_class() {
            format!("{cls}.constructor")
        } else {
            "constructor".to_string()
        };

        self.create_symbol(
            counter.next_id(),
            name.clone(),
            SymbolKind::Method,
            file_id,
            node_range(node),
            (
                Some(format!("constructor{params}")),
                extract_kdoc(&node, code),
                module_path,
                visibility,
            ),
        )
    }

    fn process_property(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let visibility = determine_kotlin_visibility(node, code);
        let is_const = has_modifier_keyword(node, code, "const");

        // Property can have a variable_declaration child or directly a simple_identifier
        let prop_name = find_child_by_kind(node, "variable_declaration")
            .and_then(|vd| find_child_by_kind(vd, "simple_identifier"))
            .or_else(|| find_child_by_kind(node, "simple_identifier"))
            .map(|n| &code[n.byte_range()]);

        if let Some(name) = prop_name {
            let kind = if is_const {
                SymbolKind::Constant
            } else if self.context.is_in_class() {
                SymbolKind::Field
            } else {
                SymbolKind::Variable
            };

            let qualified = if let Some(cls) = self.context.current_class() {
                format!("{cls}.{name}")
            } else {
                name.to_string()
            };

            let type_str = extract_property_type(node, code);
            let sig = type_str.map(|t| format!("{name}: {t}"));

            symbols.push(self.create_symbol(
                counter.next_id(),
                qualified,
                kind,
                file_id,
                node_range(node),
                (sig, extract_kdoc(&node, code), module_path, visibility),
            ));
        }
    }

    fn process_type_alias(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name = find_type_identifier(node, code)?;
        let visibility = determine_kotlin_visibility(node, code);

        Some(
            self.create_symbol(
                counter.next_id(),
                name.clone(),
                SymbolKind::TypeAlias,
                file_id,
                node_range(node),
                (
                    Some(
                        code[node.byte_range()]
                            .lines()
                            .next()
                            .unwrap_or("")
                            .trim()
                            .to_string(),
                    ),
                    extract_kdoc(&node, code),
                    module_path,
                    visibility,
                ),
            ),
        )
    }

    // ── Imports ─────────────────────────────────────────────────────────

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut imports = Vec::new();
        Self::find_imports_in_node(tree.root_node(), code, file_id, 0, &mut imports);
        imports
    }

    fn find_imports_in_node(
        node: Node,
        code: &str,
        file_id: FileId,
        depth: usize,
        imports: &mut Vec<Import>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        if node.kind() == "import_header" {
            let text = code[node.byte_range()].trim();
            let is_glob = find_child_by_kind(node, "wildcard_import").is_some();

            // Extract path from the identifier child
            let path = find_child_by_kind(node, "identifier")
                .map(|n| code[n.byte_range()].to_string())
                .unwrap_or_else(|| text.trim_start_matches("import ").trim().to_string());

            let alias = find_child_by_kind(node, "import_alias")
                .and_then(|n| {
                    find_child_by_kind(n, "type_identifier")
                        .or_else(|| find_child_by_kind(n, "simple_identifier"))
                })
                .map(|n| code[n.byte_range()].to_string());

            imports.push(Import {
                path,
                alias,
                file_id,
                is_glob,
                is_type_only: false,
            });
            return;
        }

        for child in node.children(&mut node.walk()) {
            Self::find_imports_in_node(child, code, file_id, depth + 1, imports);
        }
    }

    // ── Calls ───────────────────────────────────────────────────────────

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
            "function_declaration" => find_simple_identifier_ref(*node, code).or(current_fn),
            "secondary_constructor" => Some("constructor"),
            _ => current_fn,
        };

        if node.kind() == "call_expression" {
            // The callee is the first child (simple_identifier or navigation_expression)
            if let Some(callee_node) = node.child(0) {
                let target = match callee_node.kind() {
                    "simple_identifier" => Some(&code[callee_node.byte_range()]),
                    // `Other.stat` keeps its receiver so the call resolves to
                    // that type; `build().stat` has no name to keep, so only
                    // the method is recorded.
                    "navigation_expression" => {
                        let whole = &code[callee_node.byte_range()];
                        if is_plain_path(whole) {
                            Some(whole)
                        } else {
                            extract_nav_method_name(callee_node, code)
                        }
                    }
                    _ => None,
                };
                if let (Some(ctx), Some(target)) = (fn_ctx, target) {
                    calls.push((ctx, target, node_range(*node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_calls_in_node(&child, code, fn_ctx, depth + 1, calls);
        }
    }

    // ── Method calls ────────────────────────────────────────────────────

    fn find_method_calls_impl(&mut self, code: &str) -> Vec<MethodCall> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        Self::find_method_calls_in_node(&tree.root_node(), code, Some("<module>"), 0, &mut calls);
        calls
    }

    fn find_method_calls_in_node(
        node: &Node,
        code: &str,
        current_fn: Option<&str>,
        depth: usize,
        calls: &mut Vec<MethodCall>,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }

        let fn_ctx = match node.kind() {
            "function_declaration" => find_simple_identifier_ref(*node, code).or(current_fn),
            "secondary_constructor" => Some("constructor"),
            _ => current_fn,
        };

        if node.kind() == "call_expression" {
            if let Some(ctx) = fn_ctx {
                if let Some(callee_node) = node.child(0) {
                    match callee_node.kind() {
                        "simple_identifier" => {
                            let method = &code[callee_node.byte_range()];
                            calls.push(MethodCall {
                                caller: ctx.to_string(),
                                method_name: method.to_string(),
                                receiver: None,
                                is_static: false,
                                range: node_range(*node),
                                caller_range: None,
                            });
                        }
                        "navigation_expression" => {
                            let receiver = extract_nav_receiver(callee_node, code);
                            let method = extract_nav_method_name(callee_node, code);
                            if let Some(method) = method {
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
                        _ => {}
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_method_calls_in_node(&child, code, fn_ctx, depth + 1, calls);
        }
    }

    // ── Implementations (: interface) ───────────────────────────────────

    fn find_implementations_in_node<'a>(
        node: &Node,
        code: &'a str,
        depth: usize,
        results: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }

        if node.kind() == "class_declaration" || node.kind() == "object_declaration" {
            let name_opt = find_type_identifier_ref(*node, code);

            if let Some(class_name) = name_opt {
                // delegation_specifier children contain supertypes
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "delegation_specifier" {
                        extract_delegation_type(child, code, class_name, results);
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_implementations_in_node(&child, code, depth + 1, results);
        }
    }

    // ── Method defines ──────────────────────────────────────────────────

    fn find_defines_in_node<'a>(
        node: &Node,
        code: &'a str,
        depth: usize,
        defines: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }

        if node.kind() == "class_declaration" || node.kind() == "object_declaration" {
            let type_name = find_type_identifier_ref(*node, code);

            if let Some(type_name) = type_name {
                let body = find_child_by_kind(*node, "class_body")
                    .or_else(|| find_child_by_kind(*node, "enum_class_body"));

                if let Some(body) = body {
                    for child in body.children(&mut body.walk()) {
                        if child.kind() == "function_declaration" {
                            if let Some(fn_name) = find_simple_identifier_ref(child, code) {
                                defines.push((type_name, fn_name, node_range(child)));
                            }
                        } else if child.kind() == "secondary_constructor" {
                            defines.push((type_name, "constructor", node_range(child)));
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_defines_in_node(&child, code, depth + 1, defines);
        }
    }

    // ── Type uses ───────────────────────────────────────────────────────

    fn find_uses_in_node<'a>(
        node: &Node,
        code: &'a str,
        depth: usize,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }

        if node.kind() == "function_declaration" {
            let ctx = find_simple_identifier_ref(*node, code).unwrap_or("anonymous");

            // Return type (user_type children)
            for child in node.children(&mut node.walk()) {
                if child.kind() == "user_type" {
                    if let Some(type_id) = find_child_by_kind(child, "type_identifier") {
                        uses.push((ctx, &code[type_id.byte_range()], node_range(type_id)));
                    }
                }
            }

            // Parameter types
            if let Some(params) = find_child_by_kind(*node, "function_value_parameters") {
                for param in params.children(&mut params.walk()) {
                    if param.kind() == "parameter" {
                        Self::extract_type_uses_from_param(param, code, ctx, uses);
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_uses_in_node(&child, code, depth + 1, uses);
        }
    }

    fn extract_type_uses_from_param<'a>(
        node: Node,
        code: &'a str,
        ctx: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        for child in node.children(&mut node.walk()) {
            if child.kind() == "user_type" {
                if let Some(type_id) = find_child_by_kind(child, "type_identifier") {
                    uses.push((ctx, &code[type_id.byte_range()], node_range(type_id)));
                }
            }
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

/// Find the first `type_identifier` child of a node (used for class/object names).
fn find_type_identifier(node: Node, code: &str) -> Option<String> {
    find_type_identifier_ref(node, code).map(|s| s.to_string())
}

/// Find the first `type_identifier` child, returning a reference into `code`.
fn find_type_identifier_ref<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "type_identifier" {
            return Some(&code[child.byte_range()]);
        }
    }
    None
}

/// Find the first `simple_identifier` child (used for function/property names).
fn find_simple_identifier(node: Node, code: &str) -> Option<String> {
    find_simple_identifier_ref(node, code).map(|s| s.to_string())
}

/// Find the first `simple_identifier` child, returning a reference into `code`.
fn find_simple_identifier_ref<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "simple_identifier" {
            return Some(&code[child.byte_range()]);
        }
    }
    None
}

/// Find the first child of a given kind.
fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    node.children(&mut node.walk())
        .find(|&child| child.kind() == kind)
}

/// Check if a node has a direct child token of the given kind (e.g., "val", "var").
fn child_has_kind(node: Node, kind: &str) -> bool {
    for child in node.children(&mut node.walk()) {
        if child.kind() == kind {
            return true;
        }
    }
    false
}

/// Extract package path from the source_file root.
fn extract_package_path(root: Node, code: &str) -> Option<String> {
    for child in root.children(&mut root.walk()) {
        if child.kind() == "package_header" {
            if let Some(id_node) = find_child_by_kind(child, "identifier") {
                return Some(code[id_node.byte_range()].to_string());
            }
        }
    }
    None
}

/// Check if the modifiers contain a specific keyword (e.g., "enum", "interface", "data", "const").
fn has_modifier_keyword(node: Node, code: &str, keyword: &str) -> bool {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "modifiers" {
            let text = &code[child.byte_range()];
            if text.split_whitespace().any(|w| w == keyword) {
                return true;
            }
        }
    }
    false
}

/// Determine visibility from Kotlin modifiers.
fn determine_kotlin_visibility(node: Node, code: &str) -> Visibility {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "modifiers" {
            for mod_child in child.children(&mut child.walk()) {
                if mod_child.kind() == "visibility_modifier" {
                    let text = &code[mod_child.byte_range()];
                    return match text {
                        "private" => Visibility::Private,
                        "protected" => Visibility::Module,
                        "internal" => Visibility::Crate,
                        _ => Visibility::Public,
                    };
                }
            }
        }
    }
    // Kotlin default is public
    Visibility::Public
}

/// Build a class/interface/enum signature.
fn build_class_signature(node: Node, code: &str, kind: SymbolKind) -> String {
    let keyword = match kind {
        SymbolKind::Enum => "enum class",
        SymbolKind::Interface => "interface",
        _ => "class",
    };

    let name = find_type_identifier(node, code).unwrap_or_else(|| "?".to_string());
    let type_params =
        find_child_by_kind(node, "type_parameters").map(|n| code[n.byte_range()].to_string());

    match type_params {
        Some(tp) => format!("{keyword} {name}{tp}"),
        None => format!("{keyword} {name}"),
    }
}

/// Build a function signature.
fn build_function_signature(node: Node, code: &str) -> String {
    let name = find_simple_identifier(node, code).unwrap_or_else(|| "?".to_string());

    let params = find_child_by_kind(node, "function_value_parameters")
        .map(|n| code[n.byte_range()].to_string())
        .unwrap_or_else(|| "()".to_string());

    // Return type: look for user_type or nullable_type after the parameters
    let return_type = extract_return_type(node, code);

    match return_type {
        Some(rt) => format!("fun {name}{params}: {rt}"),
        None => format!("fun {name}{params}"),
    }
}

/// Extract the return type from a function_declaration.
fn extract_return_type(node: Node, code: &str) -> Option<String> {
    let mut found_params = false;
    for child in node.children(&mut node.walk()) {
        if child.kind() == "function_value_parameters" {
            found_params = true;
            continue;
        }
        if found_params {
            match child.kind() {
                "user_type" | "nullable_type" | "function_type" | "not_nullable_type"
                | "parenthesized_type" => {
                    return Some(code[child.byte_range()].to_string());
                }
                _ => {}
            }
        }
    }
    None
}

/// Extract type annotation from a class_parameter or variable_declaration.
fn extract_type_annotation(node: Node, code: &str) -> Option<String> {
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "user_type" | "nullable_type" | "function_type" | "not_nullable_type"
            | "parenthesized_type" => {
                return Some(code[child.byte_range()].to_string());
            }
            _ => {}
        }
    }
    None
}

/// Extract type from a property_declaration via its variable_declaration child.
fn extract_property_type(node: Node, code: &str) -> Option<String> {
    // Check variable_declaration child first
    if let Some(vd) = find_child_by_kind(node, "variable_declaration") {
        return extract_type_annotation(vd, code);
    }
    // Fallback: direct type children on property_declaration itself
    extract_type_annotation(node, code)
}

/// Extract KDoc comment from preceding sibling.
fn extract_kdoc(node: &Node, code: &str) -> Option<String> {
    let sibling = node.prev_sibling()?;
    if sibling.kind() != "multiline_comment" {
        return None;
    }
    let text = &code[sibling.byte_range()];
    if !text.starts_with("/**") {
        return None;
    }
    crate::code::parsing::parser::strip_block_doc_comment(text)
}

/// Extract the method name from a navigation_expression (last simple_identifier in nav suffix).
fn extract_nav_method_name<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    // navigation_expression children: expression, navigation_suffix
    // navigation_suffix contains: simple_identifier
    for child in node.children(&mut node.walk()) {
        if child.kind() == "navigation_suffix" {
            return find_simple_identifier_ref(child, code);
        }
    }
    None
}

/// Extract the receiver from a navigation_expression (the left side).
fn extract_nav_receiver<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    // First child of navigation_expression is the receiver
    let first = node.child(0)?;
    match first.kind() {
        "simple_identifier" => Some(&code[first.byte_range()]),
        "this_expression" => Some("this"),
        "super_expression" => Some("super"),
        _ => None,
    }
}

/// Extract type names from a delegation_specifier.
fn extract_delegation_type<'a>(
    node: Node,
    code: &'a str,
    owner_name: &'a str,
    results: &mut Vec<(&'a str, &'a str, Range)>,
) {
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "user_type" => {
                if let Some(type_id) = find_child_by_kind(child, "type_identifier") {
                    results.push((owner_name, &code[type_id.byte_range()], node_range(type_id)));
                }
            }
            "constructor_invocation" => {
                // constructor_invocation contains user_type + value_arguments
                if let Some(user_type) = find_child_by_kind(child, "user_type") {
                    if let Some(type_id) = find_child_by_kind(user_type, "type_identifier") {
                        results.push((
                            owner_name,
                            &code[type_id.byte_range()],
                            node_range(type_id),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

// ── LanguageParser trait impl ───────────────────────────────────────────

impl LanguageParser for KotlinParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Kotlin
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_kdoc(node, code)
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_method_calls(&mut self, code: &str) -> Vec<MethodCall> {
        self.find_method_calls_impl(code)
    }

    fn find_implementations<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut results = Vec::new();
        Self::find_implementations_in_node(&tree.root_node(), code, 0, &mut results);
        results
    }

    fn find_uses<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut uses = Vec::new();
        Self::find_uses_in_node(&tree.root_node(), code, 0, &mut uses);
        uses
    }

    fn find_defines<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut defines = Vec::new();
        Self::find_defines_in_node(&tree.root_node(), code, 0, &mut defines);
        defines
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
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package com.example

/** A simple calculator. */
class Calculator(val initial: Int) {
    private var value: Int = initial

    fun add(x: Int): Int {
        return value + x
    }

    private fun reset() {
        value = 0
    }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols.iter().any(|s| s.name.as_ref() == "Calculator"
                && s.kind == SymbolKind::Class
                && s.visibility == Visibility::Public),
            "expected Calculator class, got: {:?}",
            symbols
                .iter()
                .map(|s| (&s.name, &s.kind))
                .collect::<Vec<_>>()
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Calculator.initial" && s.kind == SymbolKind::Field),
            "expected Calculator.initial field"
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Calculator.add" && s.kind == SymbolKind::Method),
            "expected Calculator.add method"
        );
        assert!(
            symbols.iter().any(|s| s.name.as_ref() == "Calculator.reset"
                && s.kind == SymbolKind::Method
                && s.visibility == Visibility::Private),
            "expected Calculator.reset private method"
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Calculator.value" && s.kind == SymbolKind::Field),
            "expected Calculator.value field"
        );
    }

    #[test]
    fn test_parse_interface_and_enum() {
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
interface Serializable {
    fun serialize(): String
}

enum class Color {
    RED, GREEN, BLUE
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Serializable" && s.kind == SymbolKind::Interface),
            "expected Serializable interface, got: {:?}",
            symbols
                .iter()
                .map(|s| (&s.name, &s.kind))
                .collect::<Vec<_>>()
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Color" && s.kind == SymbolKind::Enum),
            "expected Color enum"
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Color.RED" && s.kind == SymbolKind::Constant),
            "expected Color.RED constant"
        );
    }

    #[test]
    fn test_parse_object_and_companion() {
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
object Singleton {
    fun instance(): Singleton = this
}

class MyClass {
    companion object {
        fun create(): MyClass = MyClass()
    }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Singleton" && s.kind == SymbolKind::Class),
            "expected Singleton object"
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Singleton.instance" && s.kind == SymbolKind::Method),
            "expected Singleton.instance method"
        );
        // companion object method should be scoped under the class
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "MyClass.create" && s.kind == SymbolKind::Method),
            "expected MyClass.create from companion, got: {:?}",
            symbols
                .iter()
                .map(|s| (&s.name, &s.kind))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_find_imports() {
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
import kotlin.collections.List
import kotlin.io.*
import java.util.Map as JavaMap
"#;

        let imports = parser.find_imports(code, file_id);

        assert!(
            imports
                .iter()
                .any(|i| i.path.contains("kotlin.collections.List"))
        );
        assert!(imports.iter().any(|i| i.is_glob));
        assert!(
            imports
                .iter()
                .any(|i| i.alias.as_deref() == Some("JavaMap")),
            "expected alias JavaMap, got: {:?}",
            imports
                .iter()
                .map(|i| (&i.path, &i.alias))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_find_calls() {
        let mut parser = KotlinParser::new().unwrap();

        let code = r#"
class App {
    fun main() {
        process()
        println("hello")
    }

    fun process() {}
}
"#;

        let calls = parser.find_calls_impl(code);
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "process"),
            "expected main->process call, got: {:?}",
            calls
        );
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "println"),
            "expected main->println call"
        );
    }

    #[test]
    fn a_call_through_a_receiver_keeps_the_receiver() {
        let mut parser = KotlinParser::new().unwrap();

        let code = r#"
class App {
    fun main() {
        Other.stat()
        build().stat()
    }
}
"#;

        let targets: Vec<&str> = parser
            .find_calls_impl(code)
            .iter()
            .map(|(_, t, _)| *t)
            .collect();
        assert!(
            targets.contains(&"Other.stat"),
            "a named receiver must be kept: {targets:?}"
        );
        // `build()` is a call, not a name: there is nothing to qualify with.
        assert!(
            targets.contains(&"stat"),
            "a computed receiver leaves only the method: {targets:?}"
        );
    }

    #[test]
    fn test_find_implementations() {
        let mut parser = KotlinParser::new().unwrap();

        let code = r#"
interface Printable
interface Serializable
open class Base

class Derived : Base(), Printable, Serializable
"#;

        let impls = parser.find_implementations(code);
        assert!(
            impls
                .iter()
                .any(|(cls, base, _)| *cls == "Derived" && *base == "Base"),
            "expected Derived extends Base, got: {:?}",
            impls
        );
        assert!(
            impls
                .iter()
                .any(|(cls, iface, _)| *cls == "Derived" && *iface == "Printable"),
            "expected Derived implements Printable"
        );
        assert!(
            impls
                .iter()
                .any(|(cls, iface, _)| *cls == "Derived" && *iface == "Serializable"),
            "expected Derived implements Serializable"
        );
    }

    #[test]
    fn test_kdoc_extraction() {
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
/** Process data and return results. */
class DataProcessor {
    /** Run the processor. */
    fun run() {}
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let cls = symbols
            .iter()
            .find(|s| s.name.as_ref() == "DataProcessor")
            .expect("should find DataProcessor");
        assert!(
            cls.doc_comment.as_deref().unwrap().contains("Process data"),
            "expected KDoc on class"
        );
    }

    #[test]
    fn test_top_level_function() {
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
fun topLevel(x: Int): String {
    return x.toString()
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "topLevel" && s.kind == SymbolKind::Function),
            "expected topLevel function, got: {:?}",
            symbols
                .iter()
                .map(|s| (&s.name, &s.kind))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_data_class() {
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
data class Point(val x: Int, val y: Int)
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Point" && s.kind == SymbolKind::Class),
            "expected Point class"
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Point.x" && s.kind == SymbolKind::Field),
            "expected Point.x field"
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Point.y" && s.kind == SymbolKind::Field),
            "expected Point.y field"
        );
    }

    #[test]
    fn test_const_val() {
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
class Config {
    companion object {
        const val MAX_SIZE: Int = 100
    }
    var name: String = ""
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Config.MAX_SIZE" && s.kind == SymbolKind::Constant),
            "expected Config.MAX_SIZE constant, got: {:?}",
            symbols
                .iter()
                .map(|s| (&s.name, &s.kind))
                .collect::<Vec<_>>()
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Config.name" && s.kind == SymbolKind::Field),
            "expected Config.name field"
        );
    }

    #[test]
    fn test_package_as_module_path() {
        let mut parser = KotlinParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package com.example.app

fun greet() {}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let greet = symbols
            .iter()
            .find(|s| s.name.as_ref() == "greet")
            .expect("should find greet");
        assert_eq!(
            greet.module_path.as_deref(),
            Some("com.example.app"),
            "expected module_path from package header"
        );
    }
}
