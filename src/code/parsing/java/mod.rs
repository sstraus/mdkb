//! Java language parser implementation using tree-sitter-java 0.23.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::method_call::MethodCall;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth, node_range};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct JavaParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for JavaParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JavaParser")
            .field("language", &"Java")
            .finish()
    }
}

impl JavaParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_java::LANGUAGE.into())
            .map_err(|e| format!("Failed to set Java language: {e}"))?;

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
        let package_path = extract_package_path(tree.root_node(), code);
        let module_path = package_path.as_deref().unwrap_or("");

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
            "class_declaration" => {
                let class_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) = self.process_class(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(class_name);

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

            "interface_declaration" => {
                let iface_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) =
                    self.process_interface(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(iface_name);

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

            "enum_declaration" => {
                if let Some(symbol) = self.process_enum(node, code, file_id, counter, module_path) {
                    symbols.push(symbol);
                }
            }

            "method_declaration" => {
                let method_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) = self.process_method(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }

                self.context
                    .enter_scope(ScopeType::Function { hoisting: false });
                let saved_fn = self.context.current_function().map(|s| s.to_string());
                self.context.set_current_function(method_name);

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
                self.context.set_current_function(saved_fn);
            }

            "constructor_declaration" => {
                if let Some(symbol) =
                    self.process_constructor(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
            }

            "field_declaration" => {
                self.process_field(node, code, file_id, counter, symbols, module_path);
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

    fn process_class(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let visibility = determine_java_visibility(node, code);
        let signature = build_class_signature(node, code, "class");
        let doc = extract_javadoc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Class,
            file_id,
            node_range(node),
            Some(signature),
            doc,
            module_path,
            visibility,
        ))
    }

    fn process_interface(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let visibility = determine_java_visibility(node, code);
        let signature = build_class_signature(node, code, "interface");
        let doc = extract_javadoc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Interface,
            file_id,
            node_range(node),
            Some(signature),
            doc,
            module_path,
            visibility,
        ))
    }

    fn process_enum(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let visibility = determine_java_visibility(node, code);
        let doc = extract_javadoc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Enum,
            file_id,
            node_range(node),
            Some(format!("enum {name}")),
            doc,
            module_path,
            visibility,
        ))
    }

    fn process_method(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let visibility = determine_java_visibility(node, code);
        let signature = build_method_signature(node, code);
        let doc = extract_javadoc(&node, code);

        let qualified_name = if let Some(cls) = self.context.current_class() {
            format!("{cls}.{name}")
        } else {
            name.to_string()
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
            Some(signature),
            doc,
            module_path,
            visibility,
        ))
    }

    fn process_constructor(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let visibility = determine_java_visibility(node, code);
        let doc = extract_javadoc(&node, code);

        let params = node
            .child_by_field_name("parameters")
            .map(|n| &code[n.byte_range()])
            .unwrap_or("()");

        let qualified_name = if let Some(cls) = self.context.current_class() {
            format!("{cls}.{name}")
        } else {
            name.to_string()
        };

        Some(self.create_symbol(
            counter.next_id(),
            qualified_name,
            SymbolKind::Method,
            file_id,
            node_range(node),
            Some(format!("{name}{params}")),
            doc,
            module_path,
            visibility,
        ))
    }

    fn process_field(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let visibility = determine_java_visibility(node, code);
        let type_str = node
            .child_by_field_name("type")
            .map(|n| &code[n.byte_range()])
            .unwrap_or("?");

        let is_constant = is_static_final(node, code);

        // field_declaration has multiple declarators
        for child in node.children(&mut node.walk()) {
            if child.kind() == "variable_declarator" {
                if let Some(name_node) = child.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let kind = if is_constant {
                        SymbolKind::Constant
                    } else {
                        SymbolKind::Field
                    };

                    let qualified_name = if let Some(cls) = self.context.current_class() {
                        format!("{cls}.{name}")
                    } else {
                        name.to_string()
                    };

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        kind,
                        file_id,
                        node_range(child),
                        Some(format!("{type_str} {name}")),
                        None,
                        module_path,
                        visibility,
                    );
                    symbols.push(symbol);
                }
            }
        }
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
        if node.kind() == "import_declaration" {
            let text = code[node.byte_range()].trim();
            let is_glob = text.ends_with(".*;");

            // Extract the import path from the text
            let path = text
                .trim_start_matches("import ")
                .trim_start_matches("static ")
                .trim_end_matches(';')
                .trim()
                .to_string();

            imports.push(Import {
                path,
                alias: None,
                file_id,
                is_glob,
                is_type_only: false,
            });
            return;
        }

        for child in node.children(&mut node.walk()) {
            self.find_imports_in_node(child, code, file_id, imports);
        }
    }

    // ── Calls ───────────────────────────────────────────────────────────

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.find_calls_in_node(&tree.root_node(), code, Some("<module>"), &mut calls);
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
            "method_declaration" | "constructor_declaration" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        if node.kind() == "method_invocation" {
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

    // ── Method calls ────────────────────────────────────────────────────

    fn find_method_calls_impl(&mut self, code: &str) -> Vec<MethodCall> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.find_method_calls_in_node(&tree.root_node(), code, Some("<module>"), &mut calls);
        calls
    }

    fn find_method_calls_in_node(
        &self,
        node: &Node,
        code: &str,
        current_fn: Option<&str>,
        calls: &mut Vec<MethodCall>,
    ) {
        let fn_ctx = match node.kind() {
            "method_declaration" | "constructor_declaration" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        if node.kind() == "method_invocation" {
            if let Some(ctx) = fn_ctx {
                let method_name = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()]);
                let receiver = node
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

        for child in node.children(&mut node.walk()) {
            self.find_method_calls_in_node(&child, code, fn_ctx, calls);
        }
    }

    // ── Implementations (extends/implements) ────────────────────────────

    fn find_implementations_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        results: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        match node.kind() {
            "class_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let class_name = &code[name_node.byte_range()];

                    // extends
                    if let Some(superclass) = node.child_by_field_name("superclass") {
                        for child in superclass.children(&mut superclass.walk()) {
                            if child.kind() == "type_identifier" {
                                results.push((
                                    class_name,
                                    &code[child.byte_range()],
                                    node_range(child),
                                ));
                            }
                        }
                    }

                    // implements
                    if let Some(interfaces) = node.child_by_field_name("interfaces") {
                        extract_type_list(&interfaces, code, class_name, results);
                    }
                }
            }
            "interface_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let iface_name = &code[name_node.byte_range()];
                    // extends_interfaces
                    for child in node.children(&mut node.walk()) {
                        if child.kind() == "extends_interfaces" {
                            extract_type_list(&child, code, iface_name, results);
                        }
                    }
                }
            }
            _ => {}
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
        match node.kind() {
            "class_declaration" | "interface_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let type_name = &code[name_node.byte_range()];
                    let body_field = if node.kind() == "class_declaration" {
                        "body"
                    } else {
                        "body"
                    };

                    if let Some(body) = node.child_by_field_name(body_field) {
                        for child in body.children(&mut body.walk()) {
                            if matches!(
                                child.kind(),
                                "method_declaration" | "constructor_declaration"
                            ) {
                                if let Some(mn) = child.child_by_field_name("name") {
                                    let method_name = &code[mn.byte_range()];
                                    defines.push((type_name, method_name, node_range(child)));
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            self.find_defines_in_node(&child, code, defines);
        }
    }

    // ── Type uses ───────────────────────────────────────────────────────

    fn find_uses_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if node.kind() == "method_declaration" {
            let ctx = node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .unwrap_or("anonymous");

            // Return type
            if let Some(type_node) = node.child_by_field_name("type") {
                if type_node.kind() == "type_identifier" {
                    uses.push((ctx, &code[type_node.byte_range()], node_range(type_node)));
                }
            }

            // Parameter types
            if let Some(params) = node.child_by_field_name("parameters") {
                for param in params.children(&mut params.walk()) {
                    if param.kind() == "formal_parameter" {
                        if let Some(type_node) = param.child_by_field_name("type") {
                            if type_node.kind() == "type_identifier" {
                                uses.push((
                                    ctx,
                                    &code[type_node.byte_range()],
                                    node_range(type_node),
                                ));
                            }
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.find_uses_in_node(&child, code, uses);
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

/// Extract package path from the program root.
fn extract_package_path(root: Node, code: &str) -> Option<String> {
    for child in root.children(&mut root.walk()) {
        if child.kind() == "package_declaration" {
            let text = code[child.byte_range()].trim();
            let path = text
                .trim_start_matches("package ")
                .trim_end_matches(';')
                .trim();
            return Some(path.to_string());
        }
    }
    None
}

/// Determine visibility from Java modifiers.
fn determine_java_visibility(node: Node, code: &str) -> Visibility {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "modifiers" {
            let text = &code[child.byte_range()];
            if text.contains("public") {
                return Visibility::Public;
            }
            if text.contains("protected") {
                return Visibility::Module;
            }
            if text.contains("private") {
                return Visibility::Private;
            }
            // package-private (no modifier)
            return Visibility::Crate;
        }
    }
    // No modifiers = package-private
    Visibility::Crate
}

/// Check if a field is `static final` (constant).
fn is_static_final(node: Node, code: &str) -> bool {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "modifiers" {
            let text = &code[child.byte_range()];
            return text.contains("static") && text.contains("final");
        }
    }
    false
}

/// Build a class/interface signature from the declaration.
fn build_class_signature(node: Node, code: &str, keyword: &str) -> String {
    let name = node
        .child_by_field_name("name")
        .map(|n| &code[n.byte_range()])
        .unwrap_or("?");

    let type_params = node
        .child_by_field_name("type_parameters")
        .map(|n| &code[n.byte_range()]);

    match type_params {
        Some(tp) => format!("{keyword} {name}{tp}"),
        None => format!("{keyword} {name}"),
    }
}

/// Build a method signature from the declaration.
fn build_method_signature(node: Node, code: &str) -> String {
    let return_type = node
        .child_by_field_name("type")
        .map(|n| &code[n.byte_range()])
        .unwrap_or("void");

    let name = node
        .child_by_field_name("name")
        .map(|n| &code[n.byte_range()])
        .unwrap_or("?");

    let params = node
        .child_by_field_name("parameters")
        .map(|n| &code[n.byte_range()])
        .unwrap_or("()");

    format!("{return_type} {name}{params}")
}

/// Extract Javadoc comment from preceding sibling.
fn extract_javadoc(node: &Node, code: &str) -> Option<String> {
    let sibling = node.prev_sibling()?;
    if sibling.kind() != "block_comment" {
        return None;
    }
    let text = &code[sibling.byte_range()];
    if !text.starts_with("/**") {
        return None;
    }
    crate::code::parsing::parser::strip_block_doc_comment(text)
}

/// Extract type identifiers from a type list (super_interfaces, extends_interfaces).
fn extract_type_list<'a>(
    list_node: &Node,
    code: &'a str,
    owner_name: &'a str,
    results: &mut Vec<(&'a str, &'a str, Range)>,
) {
    for child in list_node.children(&mut list_node.walk()) {
        if child.kind() == "type_identifier" {
            results.push((owner_name, &code[child.byte_range()], node_range(child)));
        } else if child.kind() == "type_list" {
            extract_type_list(&child, code, owner_name, results);
        } else if child.kind() == "generic_type" {
            // Get the base type name from a generic like List<String>
            for gc in child.children(&mut child.walk()) {
                if gc.kind() == "type_identifier" {
                    results.push((owner_name, &code[gc.byte_range()], node_range(gc)));
                    break;
                }
            }
        }
    }
}

// ── LanguageParser trait impl ───────────────────────────────────────────

impl LanguageParser for JavaParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Java
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_javadoc(node, code)
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

    fn find_uses<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut uses = Vec::new();
        self.find_uses_in_node(&tree.root_node(), code, &mut uses);
        uses
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_class_and_methods() {
        let mut parser = JavaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package com.example;

/** A simple calculator. */
public class Calculator {
    private int value;

    public Calculator(int initial) {
        this.value = initial;
    }

    public int add(int x) {
        return value + x;
    }

    private void reset() {
        value = 0;
    }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator"
            && s.kind == SymbolKind::Class
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator.Calculator"
            && s.kind == SymbolKind::Method));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator.add"
            && s.kind == SymbolKind::Method
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator.reset"
            && s.kind == SymbolKind::Method
            && s.visibility == Visibility::Private));
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Calculator.value" && s.kind == SymbolKind::Field)
        );
    }

    #[test]
    fn test_parse_interface_and_enum() {
        let mut parser = JavaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
public interface Serializable {
    String serialize();
}

public enum Color {
    RED, GREEN, BLUE;
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
        let mut parser = JavaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
import java.util.List;
import java.util.Map;
import java.io.*;
import static java.lang.Math.PI;
"#;

        let imports = parser.find_imports(code, file_id);

        assert!(imports.iter().any(|i| i.path == "java.util.List"));
        assert!(imports.iter().any(|i| i.path == "java.util.Map"));
        assert!(imports.iter().any(|i| i.path == "java.io.*" && i.is_glob));
    }

    #[test]
    fn test_find_calls() {
        let mut parser = JavaParser::new().unwrap();

        let code = r#"
public class App {
    public void main() {
        process();
        System.out.println("hello");
    }

    private void process() {}
}
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
                .any(|(caller, target, _)| *caller == "main" && *target == "println")
        );
    }

    #[test]
    fn test_find_implementations() {
        let mut parser = JavaParser::new().unwrap();

        let code = r#"
interface Printable {}
interface Serializable {}
class Base {}

class Derived extends Base implements Printable, Serializable {}
"#;

        let impls = parser.find_implementations(code);
        assert!(
            impls
                .iter()
                .any(|(cls, base, _)| *cls == "Derived" && *base == "Base")
        );
        assert!(
            impls
                .iter()
                .any(|(cls, iface, _)| *cls == "Derived" && *iface == "Printable")
        );
        assert!(
            impls
                .iter()
                .any(|(cls, iface, _)| *cls == "Derived" && *iface == "Serializable")
        );
    }

    #[test]
    fn test_javadoc_extraction() {
        let mut parser = JavaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
/** Process data and return results. */
public class DataProcessor {
    /** Run the processor. */
    public void run() {}
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let cls = symbols
            .iter()
            .find(|s| s.name.as_ref() == "DataProcessor")
            .expect("should find DataProcessor");
        assert!(cls.doc_comment.as_deref().unwrap().contains("Process data"));
    }

    #[test]
    fn test_static_final_constants() {
        let mut parser = JavaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
public class Config {
    public static final int MAX_SIZE = 100;
    private String name;
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Config.MAX_SIZE" && s.kind == SymbolKind::Constant)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Config.name" && s.kind == SymbolKind::Field)
        );
    }
}
