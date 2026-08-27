//! Java language parser implementation using tree-sitter-java 0.23.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{
    LanguageParser, check_recursion_depth, last_name_segment, node_range, receiver_call_target,
};
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
            // Java's five type declarations differ only in the symbol they
            // produce; the scope handling below is identical for all of them.
            "class_declaration"
            | "interface_declaration"
            | "record_declaration"
            | "annotation_type_declaration"
            | "enum_declaration" => {
                let type_name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) = self.process_type(node, code, file_id, counter, module_path) {
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(type_name);

                // A record declares its components in the parameter list, which
                // is where its fields live; every other type has only a body.
                if node.kind() == "record_declaration" {
                    self.process_record_components(
                        node,
                        code,
                        file_id,
                        counter,
                        symbols,
                        module_path,
                    );
                }

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

            "enum_constant" => {
                if let Some(symbol) =
                    self.process_enum_constant(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
            }

            // An annotation element is a method in the language, and the type it
            // declares is its return type.
            "annotation_type_element_declaration" | "method_declaration" => {
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
                            (module_path, depth + 1),
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
                        (module_path, depth + 1),
                    );
                }
            }
        }
    }

    // ── Symbol processors ───────────────────────────────────────────────

    /// Symbol for any of Java's five type declarations.
    ///
    /// An annotation type is an interface in the language, so it is indexed as
    /// one. A record carries data like a class and is indexed as a class; its
    /// components become fields, handled separately because they sit in the
    /// parameter list rather than the body.
    fn process_type(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let (kind, keyword) = match node.kind() {
            "class_declaration" => (SymbolKind::Class, "class"),
            "interface_declaration" => (SymbolKind::Interface, "interface"),
            "record_declaration" => (SymbolKind::Class, "record"),
            "annotation_type_declaration" => (SymbolKind::Interface, "@interface"),
            "enum_declaration" => (SymbolKind::Enum, "enum"),
            _ => return None,
        };

        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let visibility = determine_java_visibility(node, code);
        let signature = build_class_signature(node, code, keyword);
        let doc = extract_javadoc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            kind,
            file_id,
            node_range(node),
            (Some(signature), doc, module_path, visibility),
        ))
    }

    /// Fields declared by a record's component list.
    fn process_record_components(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let Some(params) = node.child_by_field_name("parameters") else {
            return;
        };
        for param in params.children(&mut params.walk()) {
            if param.kind() != "formal_parameter" {
                continue;
            }
            let (Some(name_node), Some(type_node)) = (
                param.child_by_field_name("name"),
                param.child_by_field_name("type"),
            ) else {
                continue;
            };
            let name = &code[name_node.byte_range()];
            let type_str = &code[type_node.byte_range()];

            symbols.push(self.create_symbol(
                counter.next_id(),
                self.qualified(name),
                SymbolKind::Field,
                file_id,
                node_range(param),
                (
                    Some(format!("{type_str} {name}")),
                    None,
                    module_path,
                    // A record component is always public: the accessor the
                    // compiler generates for it is.
                    Visibility::Public,
                ),
            ));
        }
    }

    /// Symbol for one enum constant. Each is an immutable instance of its enum.
    fn process_enum_constant(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let doc = extract_javadoc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            self.qualified(name),
            SymbolKind::Constant,
            file_id,
            node_range(node),
            (Some(name.to_string()), doc, module_path, Visibility::Public),
        ))
    }

    /// Member name qualified by the type currently being walked.
    fn qualified(&self, name: &str) -> String {
        match self.context.current_class() {
            Some(cls) => format!("{cls}.{name}"),
            None => name.to_string(),
        }
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

        let kind = if self.context.is_in_class() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };

        Some(self.create_symbol(
            counter.next_id(),
            self.qualified(name),
            kind,
            file_id,
            node_range(node),
            (Some(signature), doc, module_path, visibility),
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

        Some(self.create_symbol(
            counter.next_id(),
            self.qualified(name),
            SymbolKind::Method,
            file_id,
            node_range(node),
            (
                Some(format!("{name}{params}")),
                doc,
                module_path,
                visibility,
            ),
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

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        self.qualified(name),
                        kind,
                        file_id,
                        node_range(child),
                        (
                            Some(format!("{type_str} {name}")),
                            None,
                            module_path,
                            visibility,
                        ),
                    );
                    symbols.push(symbol);
                }
            }
        }
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
            "method_declaration" | "constructor_declaration" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        let target = match node.kind() {
            // `System.out.println` keeps its receiver: the qualifier is what
            // tells the resolver this is not some local `println`.
            "method_invocation" => {
                receiver_call_target(*node, code, "object", "name", "field_access")
            }
            // Construction is a dependency on the constructed type.
            "object_creation_expression" => node
                .child_by_field_name("type")
                .map(|n| last_name_segment(n, code)),
            // `Foo::bar` names the method it defers to.
            "method_reference" => node
                .children(&mut node.walk())
                .filter(|c| c.is_named())
                .last()
                .map(|n| &code[n.byte_range()]),
            // `this(..)` delegates to a constructor of the same class and
            // `super(..)` to one of the superclass; both are indexed under the
            // class name, so name the class rather than the keyword.
            "explicit_constructor_invocation" => node
                .child_by_field_name("constructor")
                .and_then(|kw| Self::delegation_target(*node, &code[kw.byte_range()], code)),
            _ => None,
        };

        if let (Some(target), Some(ctx)) = (target, fn_ctx) {
            calls.push((ctx, target, node_range(*node)));
        }

        for child in node.children(&mut node.walk()) {
            Self::find_calls_in_node(&child, code, fn_ctx, depth + 1, calls);
        }
    }

    /// Class named by a `this`/`super` constructor delegation.
    ///
    /// Walks up to the enclosing `class_declaration` rather than threading class
    /// context through the whole traversal: delegation is rare, so paying for it
    /// only where it occurs is cheaper than carrying two more parameters.
    fn delegation_target<'a>(node: Node, keyword: &str, code: &'a str) -> Option<&'a str> {
        let mut current = node.parent();
        while let Some(parent) = current {
            if parent.kind() == "class_declaration" {
                let field = if keyword == "super" {
                    "superclass"
                } else {
                    "name"
                };
                return parent
                    .child_by_field_name(field)
                    .map(|n| last_name_segment(n, code));
            }
            current = parent.parent();
        }
        None
    }

    // ── Implementations (extends/implements) ────────────────────────────

    fn find_implementations_in_node<'a>(
        node: &Node,
        code: &'a str,
        depth: usize,
        results: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }

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

        match node.kind() {
            "class_declaration" | "interface_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let type_name = &code[name_node.byte_range()];
                    let body_field = "body";

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
            Self::find_uses_in_node(&child, code, depth + 1, uses);
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
            return Visibility::Package;
        }
    }
    // No modifiers = package-private
    Visibility::Package
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

    /// Java's default access is the package, not the whole compilation unit.
    /// While the enum had no `Package` level it landed on `Crate`, which is what
    /// Kotlin's `internal` and C#'s `internal` mean: a whole module. The two are
    /// different reaches and an answer about the API surface of a package cannot
    /// tell them apart while they share a level.
    #[test]
    fn package_private_is_not_the_same_reach_as_a_whole_module() {
        let mut parser = JavaParser::new().unwrap();
        let mut counter = SymbolCounter::new();
        let code = r"
class Hidden {
    void helper() {}
    static void util() {}
    public void exposed() {}
}
";
        let symbols = parser.parse_symbols(code, FileId::new(1).unwrap(), &mut counter);
        let level = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} not parsed"))
                .visibility
        };

        assert_eq!(level("Hidden"), Visibility::Package);
        assert_eq!(level("Hidden.helper"), Visibility::Package);
        // `static` is a modifier but not an access one, so this reaches the
        // decision through the other branch and must land on the same level.
        assert_eq!(level("Hidden.util"), Visibility::Package);
        assert_eq!(level("Hidden.exposed"), Visibility::Public);
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

    /// The receiver is only kept when it is a name. `list.get(0).trim()` has a
    /// call in it, so `trim` is recorded on its own rather than qualified by an
    /// expression no symbol is named after.
    #[test]
    fn a_call_on_a_computed_receiver_keeps_only_the_method_name() {
        let mut parser = JavaParser::new().unwrap();

        let code = r#"
public class App {
    public void main(java.util.List<String> list) {
        list.get(0).trim();
    }
}
"#;

        let targets: Vec<&str> = parser
            .find_calls_impl(code)
            .iter()
            .map(|(_, t, _)| *t)
            .collect();
        assert!(
            targets.contains(&"trim"),
            "expected a bare trim: {targets:?}"
        );
        assert!(
            targets.contains(&"list.get"),
            "the named receiver of the inner call is kept: {targets:?}"
        );
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
        // The receiver is kept: `System.out` is what tells this `println` apart
        // from any other one the index holds.
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "System.out.println")
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

    /// Construction is a dependency: an index that only records `method_invocation`
    /// hides every `new`, every method reference and every constructor delegation.
    #[test]
    fn construction_and_delegation_produce_call_edges() {
        let mut parser = JavaParser::new().unwrap();

        let code = r#"
class App extends Base {
    App() { super(); }
    App(int x) { this(); }
    void run() {
        Foo f = new Foo();
        Outer.Inner i = new Outer.Inner(1);
        java.util.List<String> l = new java.util.ArrayList<String>();
        Runnable r = Foo::bar;
        helper();
    }
}
"#;

        let calls = parser.find_calls_impl(code);
        let edges: Vec<(&str, &str)> = calls.iter().map(|(c, t, _)| (*c, *t)).collect();

        assert!(edges.contains(&("run", "Foo")), "new Foo(): {edges:?}");
        assert!(
            edges.contains(&("run", "Inner")),
            "new Outer.Inner(): {edges:?}"
        );
        assert!(
            edges.contains(&("run", "ArrayList")),
            "type arguments must not shadow the constructed type: {edges:?}"
        );
        assert!(
            edges.contains(&("run", "bar")),
            "method reference: {edges:?}"
        );
        // `this()` targets a constructor of the same class, `super()` one of the
        // superclass, so both resolve to a symbol that is actually indexed.
        assert!(edges.contains(&("App", "App")), "this(): {edges:?}");
        assert!(edges.contains(&("App", "Base")), "super(): {edges:?}");
        // Existing behaviour is untouched.
        assert!(edges.contains(&("run", "helper")), "plain call: {edges:?}");
    }

    /// Records, annotation types and everything inside an enum body fell through
    /// to generic recursion, so a record produced not even its own type symbol.
    #[test]
    fn records_annotation_types_and_enum_bodies_produce_symbols() {
        let mut parser = JavaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
public record Point(int x, String label) implements Shape {
    public double area() { return 0; }
}

@interface MyAnnotation {
    String value() default "";
    int count();
}

public enum Color {
    RED("r"), GREEN("g");
    private final String code;
    Color(String c) { this.code = c; }
    public String get() { return code; }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let found: Vec<(&str, SymbolKind)> =
            symbols.iter().map(|s| (s.name.as_ref(), s.kind)).collect();

        let has = |name: &str, kind: SymbolKind| found.contains(&(name, kind));

        assert!(has("Point", SymbolKind::Class), "record type: {found:?}");
        assert!(
            has("Point.x", SymbolKind::Field),
            "record component: {found:?}"
        );
        assert!(
            has("Point.label", SymbolKind::Field),
            "record component: {found:?}"
        );
        assert!(
            has("Point.area", SymbolKind::Method),
            "record method: {found:?}"
        );

        assert!(
            has("MyAnnotation", SymbolKind::Interface),
            "annotation type: {found:?}"
        );
        assert!(
            has("MyAnnotation.value", SymbolKind::Method),
            "annotation element: {found:?}"
        );

        assert!(has("Color", SymbolKind::Enum), "enum type: {found:?}");
        assert!(
            has("Color.RED", SymbolKind::Constant),
            "enum constant: {found:?}"
        );
        assert!(
            has("Color.GREEN", SymbolKind::Constant),
            "enum constant: {found:?}"
        );
        assert!(
            has("Color.code", SymbolKind::Field),
            "enum field: {found:?}"
        );
        assert!(
            has("Color.Color", SymbolKind::Method),
            "enum constructor: {found:?}"
        );
        assert!(
            has("Color.get", SymbolKind::Method),
            "enum method: {found:?}"
        );
    }
}
