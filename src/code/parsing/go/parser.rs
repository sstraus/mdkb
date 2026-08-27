//! Go language parser implementation using tree-sitter-go 0.25.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{
    LanguageParser, check_recursion_depth, node_range, receiver_call_target, unnamed_call_target,
};
use crate::code::symbol::{ScopeContext, Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct GoParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for GoParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoParser").field("language", &"Go").finish()
    }
}

impl GoParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .map_err(|e| format!("Failed to set Go language: {e}"))?;

        Ok(Self {
            parser: CachingParser::new(ts_parser),
            context: ParserContext::new(),
        })
    }

    // ── Symbol extraction ───────────────────────────────────────────────

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
        let module_path = "";
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
            "function_declaration" => {
                if let Some(symbol) =
                    self.process_function(node, code, file_id, counter, module_path)
                {
                    let fn_name = symbol.name.to_string();
                    symbols.push(symbol);

                    // Enter function scope and process children
                    self.context
                        .enter_scope(ScopeType::Function { hoisting: false });
                    if let Some(params) = node.child_by_field_name("parameters") {
                        self.process_method_parameters(
                            params,
                            code,
                            file_id,
                            counter,
                            symbols,
                            module_path,
                        );
                    }
                    for child in node.children(&mut node.walk()) {
                        if child.kind() == "block" {
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
                    let _ = fn_name;
                }
            }

            "method_declaration" => {
                if let Some(symbol) =
                    self.process_method_declaration(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);

                    // Enter function scope and process children
                    self.context
                        .enter_scope(ScopeType::Function { hoisting: false });
                    // Process receiver
                    if let Some(receiver) = node.child_by_field_name("receiver") {
                        self.process_method_receiver(
                            receiver,
                            code,
                            file_id,
                            counter,
                            symbols,
                            module_path,
                        );
                    }
                    // Process parameters
                    if let Some(params) = node.child_by_field_name("parameters") {
                        self.process_method_parameters(
                            params,
                            code,
                            file_id,
                            counter,
                            symbols,
                            module_path,
                        );
                    }
                    for child in node.children(&mut node.walk()) {
                        if child.kind() == "block" {
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
            }

            "type_declaration" => {
                self.process_type_declaration(node, code, file_id, counter, symbols, module_path);
            }

            "var_declaration" => {
                self.process_var_declaration(node, code, file_id, counter, symbols, module_path);
            }

            "const_declaration" => {
                self.process_const_declaration(node, code, file_id, counter, symbols, module_path);
            }

            "if_statement" | "for_statement" | "switch_statement" => {
                self.context.enter_scope(ScopeType::Block);
                // Process range clauses in for statements
                if node.kind() == "for_statement" {
                    for child in node.children(&mut node.walk()) {
                        if child.kind() == "range_clause" {
                            self.process_range_clause(
                                child,
                                code,
                                file_id,
                                counter,
                                symbols,
                                (module_path, depth),
                            );
                        }
                    }
                }
                for child in node.children(&mut node.walk()) {
                    if child.kind() != "range_clause" {
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
                return; // Skip default child processing
            }

            "block" => {
                self.context.enter_scope(ScopeType::Block);
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
                self.context.exit_scope();
                return; // Skip default child processing
            }

            "short_var_declaration" => {
                self.process_short_var_declaration(
                    node,
                    code,
                    file_id,
                    counter,
                    symbols,
                    module_path,
                );
            }

            _ => {}
        }

        // Default: recurse into children (unless early-returned above)
        if !matches!(
            node.kind(),
            "block" | "if_statement" | "for_statement" | "switch_statement"
        ) {
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

    // ── Symbol creation helper ──────────────────────────────────────────

    /// How far a declared name reaches.
    ///
    /// Go has one rule: a package-level name whose first letter is upper case
    /// is exported, and every other one is reachable from the whole package —
    /// there is no level below that, so nothing here is ever private by name.
    /// Inside a function body the rule does not apply at all: a local called
    /// `Total` is exported by nothing, and the parser walks into bodies.
    fn visibility_of(&self, name: &str) -> Visibility {
        if self.context.is_in_function() {
            Visibility::Private
        } else if name.starts_with(|c: char| c.is_uppercase()) {
            Visibility::Public
        } else {
            Visibility::Package
        }
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

    // ── Function / method processing ────────────────────────────────────

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

        let signature = Self::extract_signature(node, code);
        let doc_comment = Self::extract_doc_comment_impl(&node, code);
        let visibility = self.visibility_of(name);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Function,
            file_id,
            node_range(node),
            (Some(signature), doc_comment, module_path, visibility),
        ))
    }

    fn process_method_declaration(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];

        let signature = Self::extract_method_signature(node, code);
        let doc_comment = Self::extract_doc_comment_impl(&node, code);
        let visibility = self.visibility_of(name);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Method,
            file_id,
            node_range(node),
            (Some(signature), doc_comment, module_path, visibility),
        ))
    }

    // ── Type declarations ───────────────────────────────────────────────

    fn process_type_declaration(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        for child in node.children(&mut node.walk()) {
            if child.kind() == "type_spec" {
                self.process_type_spec(child, code, file_id, counter, symbols, module_path);
            }
        }
    }

    fn process_type_spec(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let Some(name_node) = node.child_by_field_name("name") else {
            return;
        };
        let name = &code[name_node.byte_range()];
        let Some(type_node) = node.child_by_field_name("type") else {
            return;
        };

        match type_node.kind() {
            "struct_type" => {
                let signature = Self::extract_struct_signature(node, code);
                let doc_comment = Self::extract_doc_comment_impl(&node, code);
                let visibility = self.visibility_of(name);

                let symbol = self.create_symbol(
                    counter.next_id(),
                    name.to_string(),
                    SymbolKind::Struct,
                    file_id,
                    node_range(node),
                    (Some(signature), doc_comment, module_path, visibility),
                );
                symbols.push(symbol);

                self.extract_struct_fields(
                    type_node,
                    code,
                    file_id,
                    counter,
                    symbols,
                    (module_path, name),
                );
            }
            "interface_type" => {
                let signature = Self::extract_interface_signature(node, code);
                let doc_comment = Self::extract_doc_comment_impl(&node, code);
                let visibility = self.visibility_of(name);

                let symbol = self.create_symbol(
                    counter.next_id(),
                    name.to_string(),
                    SymbolKind::Interface,
                    file_id,
                    node_range(node),
                    (Some(signature), doc_comment, module_path, visibility),
                );
                symbols.push(symbol);

                self.extract_interface_methods(
                    type_node,
                    code,
                    file_id,
                    counter,
                    symbols,
                    (module_path, name),
                );
            }
            _ => {
                // Type alias
                let signature = code[node.byte_range()].to_string();
                let doc_comment = Self::extract_doc_comment_impl(&node, code);
                let visibility = self.visibility_of(name);

                let symbol = self.create_symbol(
                    counter.next_id(),
                    name.to_string(),
                    SymbolKind::TypeAlias,
                    file_id,
                    node_range(node),
                    (Some(signature), doc_comment, module_path, visibility),
                );
                symbols.push(symbol);
            }
        }
    }

    // ── Struct fields ───────────────────────────────────────────────────

    fn extract_struct_fields(
        &mut self,
        struct_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, &str),
    ) {
        let (module_path, struct_name) = tail;
        for child in struct_node.children(&mut struct_node.walk()) {
            if child.kind() == "field_declaration_list" {
                for field_child in child.children(&mut child.walk()) {
                    if field_child.kind() == "field_declaration" {
                        self.process_struct_field(
                            field_child,
                            code,
                            file_id,
                            counter,
                            symbols,
                            (module_path, struct_name),
                        );
                    }
                }
            }
        }
    }

    fn process_struct_field(
        &mut self,
        field_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, &str),
    ) {
        let (module_path, struct_name) = tail;
        let mut field_names = Vec::new();
        let mut field_type = None;

        for child in field_node.children(&mut field_node.walk()) {
            match child.kind() {
                "field_identifier" => {
                    field_names.push(&code[child.byte_range()]);
                }
                kind if is_go_type_kind(kind) => {
                    field_type = Some(child);
                }
                _ => {}
            }
        }

        // A field with a type and no name of its own is embedded. Go reaches it
        // by the unqualified type name, and its signature is the declaration as
        // written, which is what says it is embedded and whether by pointer.
        let fields: Vec<(String, String)> = match (field_names.is_empty(), field_type) {
            (true, Some(typ)) => {
                let name = embedded_field_name(&code[typ.byte_range()]);
                let signature = code[field_node.start_byte()..typ.end_byte()].trim();
                vec![(name.to_string(), signature.to_string())]
            }
            (true, None) => Vec::new(),
            (false, _) => field_names
                .iter()
                .map(|name| {
                    let signature = match field_type {
                        Some(typ) => format!("{name} {}", &code[typ.byte_range()]),
                        None => (*name).to_string(),
                    };
                    ((*name).to_string(), signature)
                })
                .collect(),
        };

        for (field_name, signature) in fields {
            let visibility = self.visibility_of(&field_name);
            let qualified_name = format!("{struct_name}.{field_name}");

            let symbol = self.create_symbol(
                counter.next_id(),
                qualified_name,
                SymbolKind::Field,
                file_id,
                node_range(field_node),
                (Some(signature), None, module_path, visibility),
            );
            symbols.push(symbol);
        }
    }

    // ── Interface methods ───────────────────────────────────────────────

    fn extract_interface_methods(
        &mut self,
        interface_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, &str),
    ) {
        let (module_path, interface_name) = tail;
        for child in interface_node.children(&mut interface_node.walk()) {
            if child.kind() == "method_elem" {
                self.process_interface_method(
                    child,
                    code,
                    file_id,
                    counter,
                    symbols,
                    (module_path, interface_name),
                );
            }
        }
    }

    fn process_interface_method(
        &mut self,
        method_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, &str),
    ) {
        let (module_path, interface_name) = tail;
        let method_name = method_node
            .children(&mut method_node.walk())
            .find(|n| n.kind() == "field_identifier")
            .map(|n| &code[n.byte_range()]);

        if let Some(name) = method_name {
            let signature = code[method_node.byte_range()].to_string();
            let visibility = self.visibility_of(name);
            let qualified_name = format!("{interface_name}.{name}");

            let symbol = self.create_symbol(
                counter.next_id(),
                qualified_name,
                SymbolKind::Method,
                file_id,
                node_range(method_node),
                (Some(signature), None, module_path, visibility),
            );
            symbols.push(symbol);
        }
    }

    // ── Variable / constant declarations ────────────────────────────────

    fn process_var_declaration(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        for child in node.children(&mut node.walk()) {
            if child.kind() == "var_spec" {
                self.process_var_spec(child, code, file_id, counter, symbols, module_path);
            }
        }
    }

    fn process_var_spec(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let mut var_names = Vec::new();
        let mut var_type = None;

        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "identifier" => {
                    var_names.push(&code[child.byte_range()]);
                }
                "type_identifier" | "pointer_type" | "array_type" | "slice_type" | "map_type"
                | "channel_type" => {
                    var_type = Some(&code[child.byte_range()]);
                }
                _ => {}
            }
        }

        let doc_comment = Self::extract_doc_comment_impl(&node, code);

        for var_name in var_names {
            let visibility = self.visibility_of(var_name);
            let signature = match var_type {
                Some(typ) => format!("var {var_name} {typ}"),
                None => format!("var {var_name}"),
            };

            let symbol = self.create_symbol(
                counter.next_id(),
                var_name.to_string(),
                SymbolKind::Variable,
                file_id,
                node_range(node),
                (
                    Some(signature),
                    doc_comment.clone(),
                    module_path,
                    visibility,
                ),
            );
            symbols.push(symbol);
        }
    }

    fn process_const_declaration(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        for child in node.children(&mut node.walk()) {
            if child.kind() == "const_spec" {
                self.process_const_spec(child, code, file_id, counter, symbols, module_path);
            }
        }
    }

    fn process_const_spec(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let mut const_names = Vec::new();
        let mut const_type = None;

        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "identifier" => {
                    const_names.push(&code[child.byte_range()]);
                }
                "type_identifier" | "pointer_type" | "array_type" | "slice_type" | "map_type"
                | "channel_type" => {
                    const_type = Some(&code[child.byte_range()]);
                }
                _ => {}
            }
        }

        let doc_comment = Self::extract_doc_comment_impl(&node, code);

        for const_name in const_names {
            let visibility = self.visibility_of(const_name);
            let signature = match const_type {
                Some(typ) => format!("const {const_name} {typ}"),
                None => format!("const {const_name}"),
            };

            let symbol = self.create_symbol(
                counter.next_id(),
                const_name.to_string(),
                SymbolKind::Constant,
                file_id,
                node_range(node),
                (
                    Some(signature),
                    doc_comment.clone(),
                    module_path,
                    visibility,
                ),
            );
            symbols.push(symbol);
        }
    }

    fn process_short_var_declaration(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let mut var_names = Vec::new();

        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "expression_list" => {
                    for expr_child in child.children(&mut child.walk()) {
                        if expr_child.kind() == "identifier" {
                            var_names.push(&code[expr_child.byte_range()]);
                        }
                    }
                }
                "identifier" => {
                    var_names.push(&code[child.byte_range()]);
                }
                _ => {}
            }
        }

        for var_name in var_names {
            let visibility = self.visibility_of(var_name);
            let signature = format!("{var_name} := ...");

            let mut symbol = self.create_symbol(
                counter.next_id(),
                var_name.to_string(),
                SymbolKind::Variable,
                file_id,
                node_range(node),
                (Some(signature), None, module_path, visibility),
            );

            symbol.scope_context = Some(ScopeContext::Local {
                hoisted: false,
                parent_name: self.context.current_function().map(|s| s.into()),
                parent_kind: Some(SymbolKind::Function),
            });

            symbols.push(symbol);
        }
    }

    // ── Method receiver / parameters ────────────────────────────────────

    fn process_method_receiver(
        &mut self,
        receiver_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        for child in receiver_node.children(&mut receiver_node.walk()) {
            if child.kind() == "parameter_declaration" {
                let mut receiver_name = None;
                let mut receiver_type = None;

                for param_child in child.children(&mut child.walk()) {
                    match param_child.kind() {
                        "identifier" => {
                            receiver_name = Some(&code[param_child.byte_range()]);
                        }
                        "type_identifier" | "pointer_type" => {
                            receiver_type = Some(&code[param_child.byte_range()]);
                        }
                        _ => {}
                    }
                }

                if let Some(name) = receiver_name {
                    let visibility = self.visibility_of(name);
                    let signature = match receiver_type {
                        Some(typ) => format!("{name} {typ}"),
                        None => name.to_string(),
                    };

                    let mut symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Parameter,
                        file_id,
                        node_range(child),
                        (Some(signature), None, module_path, visibility),
                    );
                    symbol.scope_context = Some(ScopeContext::Parameter);
                    symbols.push(symbol);
                }
            }
        }
    }

    fn process_method_parameters(
        &mut self,
        params_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        for child in params_node.children(&mut params_node.walk()) {
            if child.kind() == "parameter_declaration" {
                let mut param_names = Vec::new();
                let mut param_type = None;

                for param_child in child.children(&mut child.walk()) {
                    match param_child.kind() {
                        "identifier" => {
                            param_names.push(&code[param_child.byte_range()]);
                        }
                        "type_identifier" | "pointer_type" | "array_type" | "slice_type"
                        | "map_type" | "channel_type" => {
                            param_type = Some(&code[param_child.byte_range()]);
                        }
                        _ => {}
                    }
                }

                for param_name in param_names {
                    let visibility = self.visibility_of(param_name);
                    let signature = match param_type {
                        Some(typ) => format!("{param_name} {typ}"),
                        None => param_name.to_string(),
                    };

                    let mut symbol = self.create_symbol(
                        counter.next_id(),
                        param_name.to_string(),
                        SymbolKind::Parameter,
                        file_id,
                        node_range(child),
                        (Some(signature), None, module_path, visibility),
                    );
                    symbol.scope_context = Some(ScopeContext::Parameter);
                    symbols.push(symbol);
                }
            }
        }
    }

    fn process_range_clause(
        &mut self,
        range_node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, usize),
    ) {
        let (module_path, depth) = tail;
        let mut range_vars = Vec::new();

        for child in range_node.children(&mut range_node.walk()) {
            match child.kind() {
                "expression_list" => {
                    for expr_child in child.children(&mut child.walk()) {
                        if expr_child.kind() == "identifier" {
                            range_vars.push(&code[expr_child.byte_range()]);
                        }
                    }
                }
                "identifier" => {
                    range_vars.push(&code[child.byte_range()]);
                }
                _ => {
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

        for (i, var_name) in range_vars.iter().enumerate() {
            let visibility = self.visibility_of(var_name);
            let signature = if i == 0 {
                format!("{var_name} := range (index)")
            } else {
                format!("{var_name} := range (value)")
            };

            let mut symbol = self.create_symbol(
                counter.next_id(),
                var_name.to_string(),
                SymbolKind::Variable,
                file_id,
                node_range(range_node),
                (Some(signature), None, module_path, visibility),
            );

            symbol.scope_context = Some(ScopeContext::Local {
                hoisted: false,
                parent_name: self.context.current_function().map(|s| s.into()),
                parent_kind: Some(SymbolKind::Function),
            });

            symbols.push(symbol);
        }
    }

    // ── Signatures ──────────────────────────────────────────────────────

    fn extract_signature(node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    fn extract_method_signature(node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    fn extract_struct_signature(node: Node, code: &str) -> String {
        let start = node.start_byte();
        let mut end = node.end_byte();

        if let Some(type_node) = node.child_by_field_name("type") {
            if let Some(body) = type_node
                .children(&mut type_node.walk())
                .find(|n| n.kind() == "field_declaration_list")
            {
                end = body.start_byte();
            }
        }

        code[start..end].trim().to_string()
    }

    fn extract_interface_signature(node: Node, code: &str) -> String {
        let start = node.start_byte();
        let mut end = node.end_byte();

        if let Some(type_node) = node.child_by_field_name("type") {
            if let Some(body_start) = type_node
                .children(&mut type_node.walk())
                .find(|n| n.kind() == "method_elem" || n.kind() == "type_elem")
                .map(|n| n.start_byte())
            {
                end = body_start.saturating_sub(2);
            }
        }

        code[start..end].trim().to_string()
    }

    // ── Doc comments ────────────────────────────────────────────────────

    fn extract_doc_comment_impl(node: &Node, code: &str) -> Option<String> {
        // A `type Kind int` keeps its comment above the declaration, not above
        // the spec, so a spec has to look one level up as well. Its own comment
        // comes first: inside `type (...)` each member may carry one, and the
        // one above the group speaks only for the members that do not.
        if matches!(node.kind(), "type_spec" | "var_spec" | "const_spec") {
            return Self::doc_above(*node, code).or_else(|| Self::doc_above(node.parent()?, code));
        }
        Self::doc_above(*node, code)
    }

    /// The comment block written directly above `search_node`, as Go reads it.
    fn doc_above(search_node: Node, code: &str) -> Option<String> {
        let mut doc_lines = Vec::new();
        let mut current = search_node.prev_sibling();
        // The doc is the block of comment lines *touching* the declaration.
        // Anything a blank line away belongs to whatever came before: a license
        // header, a build tag, someone else's paragraph.
        let mut documented_row = search_node.start_position().row;

        while let Some(sibling) = current {
            if sibling.kind() != "comment" {
                break;
            }
            if sibling.end_position().row + 1 != documented_row {
                break;
            }
            // A comment trailing code belongs to that line, not to the next
            // declaration: `var seed = 1 // seeded once` is not Kind's doc.
            if !is_own_line_comment(sibling, code) {
                break;
            }
            let comment_text = &code[sibling.byte_range()];
            if !comment_text.starts_with("//") {
                break;
            }
            doc_lines.insert(0, comment_text.trim_start_matches("//").trim().to_string());
            documented_row = sibling.start_position().row;
            current = sibling.prev_sibling();
        }

        if doc_lines.is_empty() {
            return None;
        }

        let filtered: Vec<String> = doc_lines
            .into_iter()
            // `//go:generate` and friends instruct the toolchain; godoc leaves
            // them out of the documentation, and so do we.
            .filter(|l| !l.is_empty() && !is_go_directive(l))
            .collect();
        if filtered.is_empty() {
            return None;
        }

        let joined = filtered.join("\n").trim().to_string();
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    }

    // ── Imports ─────────────────────────────────────────────────────────

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut imports = Vec::new();
        Self::extract_imports_from_node(tree.root_node(), code, file_id, &mut imports, 0);
        imports
    }

    fn extract_imports_from_node(
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }
        if node.kind() == "import_declaration" {
            Self::process_import_declaration(node, code, file_id, imports);
        } else {
            for child in node.children(&mut node.walk()) {
                Self::extract_imports_from_node(child, code, file_id, imports, depth + 1);
            }
        }
    }

    fn process_import_declaration(
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "import_spec" => {
                    Self::process_import_spec(child, code, file_id, imports);
                }
                "import_spec_list" => {
                    for spec_child in child.children(&mut child.walk()) {
                        if spec_child.kind() == "import_spec" {
                            Self::process_import_spec(spec_child, code, file_id, imports);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn process_import_spec(node: Node, code: &str, file_id: FileId, imports: &mut Vec<Import>) {
        let mut import_path = None;
        let mut import_alias = None;
        let mut is_dot_import = false;
        let mut is_blank_import = false;

        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "interpreted_string_literal" | "raw_string_literal" => {
                    let path_text = &code[child.byte_range()];
                    import_path =
                        Some(path_text.trim_matches(|c| c == '"' || c == '`').to_string());
                }
                "package_identifier" => {
                    import_alias = Some(code[child.byte_range()].to_string());
                }
                "dot" => {
                    is_dot_import = true;
                }
                "blank_identifier" => {
                    is_blank_import = true;
                }
                _ => {}
            }
        }

        if let Some(path) = import_path {
            imports.push(Import {
                path,
                alias: if is_dot_import {
                    Some(".".to_string())
                } else if is_blank_import {
                    Some("_".to_string())
                } else {
                    import_alias
                },
                file_id,
                is_glob: is_dot_import,
                is_type_only: false,
            });
        }
    }

    // ── Calls ───────────────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_calls_recursive<'a>(
        node: &Node,
        code: &'a str,
        current_function: Option<&'a str>,
        calls: &mut Vec<(&'a str, &'a str, Range)>,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        let function_context = if node.kind() == "function_declaration"
            || node.kind() == "method_declaration"
            || node.kind() == "func_literal"
        {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_function)
        } else {
            current_function
        };

        if node.kind() == "call_expression" {
            if let Some(function_node) = node.child_by_field_name("function") {
                let target = if function_node.kind() == "selector_expression" {
                    // `fmt.Println`, `s.Close`: kept whole so the receiver
                    // becomes the qualifier. Skipping these used to drop most
                    // of the call graph — in Go a bare call is the exception.
                    receiver_call_target(
                        function_node,
                        code,
                        "operand",
                        "field",
                        "selector_expression",
                    )
                } else {
                    extract_function_name(&function_node, code)
                }
                .or_else(|| unnamed_call_target(function_node, code, &["func_literal"]));
                if let (Some(target), Some(context)) = (target, function_context) {
                    calls.push((context, target, node_range(*node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::extract_calls_recursive(&child, code, function_context, calls, depth + 1);
        }
    }

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut calls = Vec::new();
        Self::extract_calls_recursive(&tree.root_node(), code, Some("<module>"), &mut calls, 0);
        calls
    }

    // ── Type uses ───────────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_type_uses_recursive<'a>(
        node: &Node,
        code: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        match node.kind() {
            "function_declaration" | "method_declaration" => {
                let context_name = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()])
                    .unwrap_or("anonymous");

                if let Some(params) = node.child_by_field_name("parameters") {
                    Self::extract_go_parameter_types(params, code, context_name, uses);
                }
                if let Some(result) = node.child_by_field_name("result") {
                    Self::extract_go_type_reference(&result, code, context_name, uses);
                }
            }

            "struct_type" => {
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "field_declaration_list" {
                        for field_child in child.children(&mut child.walk()) {
                            if field_child.kind() == "field_declaration" {
                                Self::extract_go_field_types(&field_child, code, "struct", uses);
                            }
                        }
                    }
                }
            }

            "var_spec" | "const_spec" => {
                if let Some(identifier) = node
                    .children(&mut node.walk())
                    .find(|n| n.kind() == "identifier")
                {
                    let var_name = &code[identifier.byte_range()];
                    for child in node.children(&mut node.walk()) {
                        if is_go_type_kind(child.kind()) {
                            Self::extract_go_type_reference(&child, code, var_name, uses);
                        }
                    }
                }
            }

            "call_expression" => {
                if let Some(function_node) = node.child_by_field_name("function") {
                    let func_name = &code[function_node.byte_range()];
                    for child in node.children(&mut node.walk()) {
                        if child.kind() == "type_arguments" {
                            for type_arg in child.children(&mut child.walk()) {
                                if is_go_type_kind(type_arg.kind()) {
                                    Self::extract_go_type_reference(
                                        &type_arg, code, func_name, uses,
                                    );
                                }
                            }
                        }
                    }
                }
            }

            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            Self::extract_type_uses_recursive(&child, code, uses, depth + 1);
        }
    }

    fn extract_go_parameter_types<'a>(
        params_node: Node,
        code: &'a str,
        context_name: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() == "parameter_declaration" {
                for child in param.children(&mut param.walk()) {
                    if is_go_type_kind(child.kind()) {
                        Self::extract_go_type_reference(&child, code, context_name, uses);
                    }
                }
            }
        }
    }

    fn extract_go_field_types<'a>(
        field_node: &Node,
        code: &'a str,
        context_name: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        for child in field_node.children(&mut field_node.walk()) {
            if is_go_type_kind(child.kind()) {
                Self::extract_go_type_reference(&child, code, context_name, uses);
            }
        }
    }

    fn extract_go_type_reference<'a>(
        type_node: &Node,
        code: &'a str,
        context_name: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if let Some(type_name) = extract_go_type_name(type_node, code) {
            uses.push((context_name, type_name, node_range(*type_node)));
        }
    }

    fn find_uses_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut uses = Vec::new();
        Self::extract_type_uses_recursive(&tree.root_node(), code, &mut uses, 0);
        uses
    }

    // ── Method defines ──────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_method_defines_recursive<'a>(
        node: &Node,
        code: &'a str,
        defines: &mut Vec<(&'a str, &'a str, Range)>,
        depth: usize,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        match node.kind() {
            "interface_type" => {
                let interface_name = "interface";
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "method_elem" {
                        if let Some(name_node) = child
                            .children(&mut child.walk())
                            .find(|n| n.kind() == "field_identifier")
                        {
                            let method_name = &code[name_node.byte_range()];
                            defines.push((interface_name, method_name, node_range(child)));
                        }
                    }
                }
            }

            "method_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let method_name = &code[name_node.byte_range()];
                    let receiver_type = if let Some(receiver) = node.child_by_field_name("receiver")
                    {
                        extract_receiver_type(receiver, code).unwrap_or("unknown")
                    } else {
                        "unknown"
                    };
                    defines.push((receiver_type, method_name, node_range(*node)));
                }
            }

            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            Self::extract_method_defines_recursive(&child, code, defines, depth + 1);
        }
    }

    fn find_defines_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut defines = Vec::new();
        Self::extract_method_defines_recursive(&tree.root_node(), code, &mut defines, 0);
        defines
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

/// Extract the receiver type from a Go method's parameter_list node.
///
/// The receiver is a `parameter_list` containing a `parameter_declaration`
/// which itself contains the type (e.g. `*Server` or `Server`).
fn extract_receiver_type<'a>(receiver: Node, code: &'a str) -> Option<&'a str> {
    for child in receiver.children(&mut receiver.walk()) {
        if child.kind() == "parameter_declaration" {
            for param_child in child.children(&mut child.walk()) {
                if matches!(param_child.kind(), "type_identifier" | "pointer_type") {
                    return Some(&code[param_child.byte_range()]);
                }
            }
        }
    }
    None
}

/// Whether the comment stands on a line of its own.
///
/// A comment sharing a line with code documents that line, and has nothing to
/// say about the declaration that happens to follow it.
fn is_own_line_comment(comment: Node, code: &str) -> bool {
    let start = comment.start_byte();
    let line_start = code[..start].rfind('\n').map_or(0, |newline| newline + 1);
    code[line_start..start].trim().is_empty()
}

/// Whether the comment body is an instruction to the Go toolchain rather than
/// prose — `//go:generate`, `//go:build`, and the older `// +build`.
fn is_go_directive(content: &str) -> bool {
    content.starts_with("go:") || content.starts_with("+build")
}

/// The name an embedded field is reached by.
///
/// Go promotes an embedded field under its unqualified type name, without the
/// package and without the pointer: `*sync.Mutex` is reached as `Mutex`.
fn embedded_field_name(field_type: &str) -> &str {
    field_type
        .trim_start_matches('*')
        .rsplit('.')
        .next()
        .unwrap_or(field_type)
        .trim()
}

fn is_go_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        "type_identifier"
            // A qualified type is a type. Leaving it out made `sync.Mutex`
            // invisible wherever a type is looked for.
            | "qualified_type"
            | "pointer_type"
            | "array_type"
            | "slice_type"
            | "map_type"
            | "channel_type"
    )
}

fn extract_function_name<'a>(node: &Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "identifier" | "selector_expression" => Some(&code[node.byte_range()]),
        _ => None,
    }
}

fn extract_go_type_name<'a>(node: &Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "type_identifier" | "qualified_type" => Some(&code[node.byte_range()]),
        "pointer_type" => node
            .children(&mut node.walk())
            .nth(1)
            .and_then(|child| extract_go_type_name(&child, code)),
        "array_type" | "slice_type" => node
            .child_by_field_name("element")
            .and_then(|el| extract_go_type_name(&el, code)),
        "map_type" => node
            .child_by_field_name("value")
            .and_then(|v| extract_go_type_name(&v, code)),
        "channel_type" => node
            .child_by_field_name("element")
            .and_then(|el| extract_go_type_name(&el, code)),
        _ => None,
    }
}

// ── LanguageParser trait impl ───────────────────────────────────────────

impl LanguageParser for GoParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Go
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        Self::extract_doc_comment_impl(node, code)
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    /// Go uses implicit interface implementation (duck typing).
    /// Cannot be detected through AST parsing alone.
    fn find_implementations<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    /// Go has no class inheritance.
    fn find_extends<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
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
    fn a_call_with_no_name_is_still_a_call() {
        // Go dispatches through maps and function values. Accepting only an
        // identifier or a selector meant those call sites left no trace at all,
        // so a function reading as "calls nothing" was really calling three.
        let mut parser = GoParser::new().unwrap();
        let code =
            "package main\nfunc f() {\n\thandlers[k]()\n\t(*fn)()\n\tfunc() { run() }()\n}\n";

        let calls: Vec<(&str, &str)> = parser
            .find_calls(code)
            .into_iter()
            .map(|(caller, callee, _)| (caller, callee))
            .collect();

        assert!(
            calls.contains(&("f", "handlers[k]")),
            "the indexed call must be recorded, got {calls:?}"
        );
        assert!(
            calls.contains(&("f", "(*fn)")),
            "the call through a function value must be recorded, got {calls:?}"
        );
        // The literal calls nobody but itself; what it does call is recorded.
        assert!(
            !calls.iter().any(|(_, callee)| callee.starts_with("func(")),
            "an immediately invoked literal is not a call to anything, got {calls:?}"
        );
        assert!(
            calls.contains(&("f", "run")),
            "the call inside the literal must still be there, got {calls:?}"
        );
    }

    /// Every call through a receiver used to be dropped on the floor. In Go
    /// that is most calls there are: `fmt.Println`, every package function,
    /// every method. The receiver is kept as the qualifier, which is what lets
    /// `fmt.Println` resolve to the `fmt` package rather than to a local
    /// `Println`.
    #[test]
    fn a_call_through_a_receiver_keeps_the_receiver() {
        let mut parser = GoParser::new().unwrap();
        let code =
            "package main\nimport \"fmt\"\nfunc m() {\n  fmt.Println(helper())\n  s.Close()\n}\n";

        let calls: Vec<(&str, &str)> = parser
            .find_calls(code)
            .into_iter()
            .map(|(caller, callee, _)| (caller, callee))
            .collect();

        assert!(
            calls.contains(&("m", "fmt.Println")),
            "expected the package call, got {calls:?}"
        );
        assert!(
            calls.contains(&("m", "s.Close")),
            "expected the method call, got {calls:?}"
        );
        assert!(
            calls.contains(&("m", "helper")),
            "the bare call must still be there, got {calls:?}"
        );
    }

    /// A receiver that is itself a call has no name to qualify with. The member
    /// is still recorded — a call the index cannot place is not a call that did
    /// not happen.
    #[test]
    fn a_call_on_a_computed_receiver_keeps_only_the_method_name() {
        let mut parser = GoParser::new().unwrap();
        let code = "package main\nfunc m() {\n  build().Close()\n  items[0].Close()\n}\n";

        let calls: Vec<&str> = parser
            .find_calls(code)
            .into_iter()
            .map(|(_, callee, _)| callee)
            .collect();

        assert_eq!(
            calls.iter().filter(|c| **c == "Close").count(),
            2,
            "both computed receivers give a bare method name, got {calls:?}"
        );
    }

    /// One edge per call site. The receiver form is extracted twice in this
    /// file — once here and once for `find_method_calls` — and if both reached
    /// the pipeline every method call would be counted double.
    #[test]
    fn a_receiver_call_is_recorded_once() {
        let mut parser = GoParser::new().unwrap();
        let code = "package main\nfunc m() {\n  fmt.Println(1)\n}\n";

        assert_eq!(parser.find_calls(code).len(), 1);
    }

    #[test]
    fn test_parse_functions_and_structs() {
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package main

// PublicFunction does something publicly.
func PublicFunction(x int) bool {
    return true
}

func privateFunction() {}

type MyStruct struct {
    Name   string
    age    int
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "PublicFunction"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "privateFunction"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Package));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "MyStruct"
            && s.kind == SymbolKind::Struct
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "MyStruct.Name"
            && s.kind == SymbolKind::Field
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "MyStruct.age"
            && s.kind == SymbolKind::Field
            && s.visibility == Visibility::Package));
    }

    #[test]
    fn test_parse_interface_and_methods() {
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package main

type Writer interface {
    Write(data []byte) (int, error)
}

type FileProcessor struct {
    filename string
}

func (f *FileProcessor) Write(data []byte) (int, error) {
    return len(data), nil
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Writer" && s.kind == SymbolKind::Interface)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Writer.Write" && s.kind == SymbolKind::Method)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "FileProcessor" && s.kind == SymbolKind::Struct)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Write" && s.kind == SymbolKind::Method)
        );
    }

    #[test]
    fn test_parse_constants_and_variables() {
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package main

const PublicConstant = "public"
const privateConstant = "private"

var PublicVariable int
var privateVariable = 42
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "PublicConstant"
            && s.kind == SymbolKind::Constant
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "privateConstant"
            && s.kind == SymbolKind::Constant
            && s.visibility == Visibility::Package));
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "PublicVariable" && s.kind == SymbolKind::Variable)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "privateVariable" && s.kind == SymbolKind::Variable)
        );
    }

    #[test]
    fn test_find_imports() {
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
package main

import (
    "fmt"
    "strings"
    "net/http"
    utils "github.com/user/repo/utils"
    . "encoding/json"
    _ "database/sql"
)
"#;

        let imports = parser.find_imports(code, file_id);

        assert_eq!(imports.len(), 6);
        assert!(imports.iter().any(|i| i.path == "fmt" && i.alias.is_none()));
        assert!(imports.iter().any(
            |i| i.path == "github.com/user/repo/utils" && i.alias == Some("utils".to_string())
        ));
        assert!(
            imports
                .iter()
                .any(|i| i.path == "encoding/json" && i.alias == Some(".".to_string()))
        );
        assert!(
            imports
                .iter()
                .any(|i| i.path == "database/sql" && i.alias == Some("_".to_string()))
        );
    }

    #[test]
    fn test_go_visibility() {
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package main

func PublicFunction() string { return "" }
func privateFunction() string { return "" }

type PublicStruct struct {
    PublicField  string
    privateField int
}

type privateStruct struct {
    field string
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let exported: Vec<_> = symbols
            .iter()
            .filter(|s| s.visibility == Visibility::Public)
            .collect();
        let unexported: Vec<_> = symbols
            .iter()
            .filter(|s| s.visibility == Visibility::Package)
            .collect();

        assert!(!exported.is_empty());
        assert!(!unexported.is_empty());

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "PublicFunction" && s.visibility == Visibility::Public)
        );
        assert!(
            symbols.iter().any(
                |s| s.name.as_ref() == "privateFunction" && s.visibility == Visibility::Package
            )
        );
    }

    #[test]
    fn an_unexported_package_level_name_is_reachable_from_its_whole_package() {
        // Go has no private level. An unexported name is reachable from every
        // file of its package, which is exactly `Package`. Calling it private
        // makes a legitimate call from a sibling file look like a reach into
        // something's inside.
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package store

const limit = 10

var registry map[string]int

type config struct {
    name string
}

func (c *config) load() error { return nil }

func helper() {}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        for name in [
            "limit",
            "registry",
            "config",
            "config.name",
            "load",
            "helper",
        ] {
            let symbol = symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("no symbol named {name}"));
            assert_eq!(
                symbol.visibility,
                Visibility::Package,
                "{name} is unexported, not private"
            );
        }
    }

    #[test]
    fn an_embedded_field_is_indexed_under_the_name_it_is_reached_by() {
        // Embedding is how Go composes. An embedded field has no name of its
        // own, so a parser that only collects `field_identifier` drops it
        // entirely — and with it the answer to "what does Server hold?".
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package store

type Base struct{ ID int }

type Server struct {
    Base
    *sync.Mutex
    io.Reader
    mu sync.Locker
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let field = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name && s.kind == SymbolKind::Field)
                .unwrap_or_else(|| panic!("no field named {name}"))
                .signature
                .as_deref()
                .unwrap_or("")
                .to_string()
        };

        // Go promotes an embedded field under its unqualified type name, so
        // that is the name a caller writes, and the name to index it under.
        assert_eq!(field("Server.Base"), "Base");
        assert_eq!(field("Server.Mutex"), "*sync.Mutex");
        assert_eq!(field("Server.Reader"), "io.Reader");
        // A qualified type is a type: it belongs in a named field's signature.
        assert_eq!(field("Server.mu"), "mu sync.Locker");
    }

    #[test]
    fn the_export_rule_does_not_reach_inside_a_function_body() {
        // The case of a name says something only where the name is package
        // level. A local called `Total` is not exported by anything, and the
        // parser walks into function bodies, so it must not ask.
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package store

func Run(Input string) int {
    Total := 0
    var Buffer []byte
    const Step = 2
    for Index := range Input {
        Total += Index + Step + len(Buffer)
    }
    return Total
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        for name in ["Input", "Total", "Buffer", "Step", "Index"] {
            let symbol = symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("no symbol named {name}"));
            assert_eq!(
                symbol.visibility,
                Visibility::Private,
                "{name} is local to Run, whatever its first letter"
            );
        }

        let run = symbols.iter().find(|s| s.name.as_ref() == "Run").unwrap();
        assert_eq!(run.visibility, Visibility::Public);
    }

    #[test]
    fn test_go_implicit_interfaces() {
        let mut parser = GoParser::new().unwrap();

        let code = r#"
package main

type Writer interface {
    Write([]byte) (int, error)
}

type MyWriter struct{}

func (w *MyWriter) Write(data []byte) (int, error) {
    return len(data), nil
}
"#;

        // Go uses implicit interface implementation - no explicit declarations
        let implementations = parser.find_implementations(code);
        assert_eq!(implementations.len(), 0);

        let extends = parser.find_extends(code);
        assert_eq!(extends.len(), 0);
    }

    #[test]
    fn test_find_calls() {
        let mut parser = GoParser::new().unwrap();

        let code = r#"
package main

import "fmt"

func main() {
    process()
    fmt.Println("hello")
    data := getData()
    _ = data
}

func process() {}
func getData() string { return "" }
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

    /// The same calls through the surviving API: `find_method_calls` held a
    /// second, parallel representation of what `find_calls` already records.
    #[test]
    fn a_method_call_and_a_package_call_are_both_recorded() {
        let mut parser = GoParser::new().unwrap();

        let code = r#"
package main

import "fmt"

type Server struct{}

func (s *Server) Start() {}

func main() {
    s := Server{}
    s.Start()
    fmt.Println("hello")
}
"#;

        let calls = parser.find_calls_impl(code);
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "s.Start"),
            "got {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "fmt.Println"),
            "got {calls:?}"
        );
    }

    #[test]
    fn test_find_defines() {
        let mut parser = GoParser::new().unwrap();

        let code = r#"
package main

type Server struct{}

func (s *Server) Start() {}
func (s *Server) Stop() {}
"#;

        let defines = parser.find_defines_impl(code);
        assert!(
            defines
                .iter()
                .any(|(receiver, method, _)| *receiver == "*Server" && *method == "Start")
        );
        assert!(
            defines
                .iter()
                .any(|(receiver, method, _)| *receiver == "*Server" && *method == "Stop")
        );
    }

    #[test]
    fn test_doc_comments() {
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package main

// ProcessData handles data processing.
// It takes a byte slice and returns an error.
func ProcessData(data []byte) error {
    return nil
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        let process_fn = symbols
            .iter()
            .find(|s| s.name.as_ref() == "ProcessData")
            .expect("should find ProcessData");

        assert!(process_fn.doc_comment.is_some());
        let doc = process_fn.doc_comment.as_deref().unwrap();
        assert!(doc.contains("ProcessData handles data processing"));
        assert!(doc.contains("byte slice"));
    }

    #[test]
    fn a_doc_comment_stops_where_the_documentation_stops() {
        // Go's rule: the doc is the comment block touching the declaration.
        // A toolchain directive, a blank line, and a comment trailing someone
        // else's code are all outside it.
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"//go:build linux

// Copyright 2026 nobody.
// SPDX-License-Identifier: MIT

package store

var seed = 1 // seeded once
// Kind is a kind.
type Kind int

//go:generate stringer -type=Level

// Level is a level.
type Level int
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let doc = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("no symbol named {name}"))
                .doc_comment
                .as_deref()
                .unwrap_or("")
                .to_string()
        };

        assert_eq!(doc("Kind"), "Kind is a kind.");
        assert_eq!(doc("Level"), "Level is a level.");
    }

    #[test]
    fn each_member_of_a_group_keeps_its_own_doc_comment() {
        // The comment above `type (` documents the group, so it is the best
        // answer for a member that says nothing about itself — but it must not
        // silence the one a member does carry.
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package store

// Group doc.
type (
    // Reader reads.
    Reader interface{}
    Writer interface{}
)

const (
    // Limit is the cap.
    Limit = 10
    Floor = 0
)

// Registry holds them.
var Registry map[string]int

// Kind is a kind.
type Kind int
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let symbol = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("no symbol named {name}"))
        };

        assert_eq!(
            symbol("Reader").doc_comment.as_deref(),
            Some("Reader reads.")
        );
        // Writer says nothing about itself, so what the group says stands.
        assert_eq!(symbol("Writer").doc_comment.as_deref(), Some("Group doc."));
        // A constant and a variable are documented like anything else.
        assert_eq!(
            symbol("Limit").doc_comment.as_deref(),
            Some("Limit is the cap.")
        );
        assert_eq!(symbol("Floor").doc_comment.as_deref(), None);
        assert_eq!(
            symbol("Registry").doc_comment.as_deref(),
            Some("Registry holds them.")
        );
        // An ungrouped spec still inherits from its declaration, which is
        // where its comment sits.
        assert_eq!(
            symbol("Kind").doc_comment.as_deref(),
            Some("Kind is a kind.")
        );
    }

    #[test]
    fn test_generic_types() {
        let mut parser = GoParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
package main

func Identity[T any](value T) T {
    return value
}

type Container[T any] struct {
    items []T
}

type Processor[T any] interface {
    Process(T) error
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| {
            s.name.as_ref() == "Identity"
                && s.kind == SymbolKind::Function
                && s.signature
                    .as_deref()
                    .is_some_and(|sig| sig.contains("[T any]"))
        }));
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Container" && s.kind == SymbolKind::Struct)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Processor" && s.kind == SymbolKind::Interface)
        );
    }
}
