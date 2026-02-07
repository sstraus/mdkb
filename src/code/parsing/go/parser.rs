//! Go language parser implementation using tree-sitter-go 0.25.

use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::method_call::MethodCall;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
use crate::code::symbol::{ScopeContext, Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::{Node, Parser};

pub struct GoParser {
    parser: Parser,
    context: ParserContext,
}

impl std::fmt::Debug for GoParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoParser")
            .field("language", &"Go")
            .finish()
    }
}

impl GoParser {
    pub fn new() -> Result<Self, String> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_go::LANGUAGE.into())
            .map_err(|e| format!("Failed to set Go language: {e}"))?;

        Ok(Self {
            parser,
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
        let tree = match self.parser.parse(code, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut symbols = Vec::new();
        let module_path = "";
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
            "function_declaration" => {
                if let Some(symbol) =
                    self.process_function(node, code, file_id, counter, module_path)
                {
                    let fn_name = symbol.name.to_string();
                    symbols.push(symbol);

                    // Enter function scope and process children
                    self.context.enter_scope(ScopeType::Function {
                        hoisting: false,
                    });
                    if let Some(params) = node.child_by_field_name("parameters") {
                        self.process_method_parameters(
                            params, code, file_id, counter, symbols, module_path,
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
                                module_path,
                                depth + 1,
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
                    self.context.enter_scope(ScopeType::Function {
                        hoisting: false,
                    });
                    // Process receiver
                    if let Some(receiver) = node.child_by_field_name("receiver") {
                        self.process_method_receiver(
                            receiver, code, file_id, counter, symbols, module_path,
                        );
                    }
                    // Process parameters
                    if let Some(params) = node.child_by_field_name("parameters") {
                        self.process_method_parameters(
                            params, code, file_id, counter, symbols, module_path,
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
                                module_path,
                                depth + 1,
                            );
                        }
                    }
                    self.context.exit_scope();
                }
            }

            "type_declaration" => {
                self.process_type_declaration(
                    node, code, file_id, counter, symbols, module_path,
                );
            }

            "var_declaration" => {
                self.process_var_declaration(
                    node, code, file_id, counter, symbols, module_path,
                );
            }

            "const_declaration" => {
                self.process_const_declaration(
                    node, code, file_id, counter, symbols, module_path,
                );
            }

            "if_statement" | "for_statement" | "switch_statement" => {
                self.context.enter_scope(ScopeType::Block);
                // Process range clauses in for statements
                if node.kind() == "for_statement" {
                    for child in node.children(&mut node.walk()) {
                        if child.kind() == "range_clause" {
                            self.process_range_clause(
                                child, code, file_id, counter, symbols, module_path, depth,
                            );
                        }
                    }
                }
                for child in node.children(&mut node.walk()) {
                    if child.kind() != "range_clause" {
                        self.extract_symbols_from_node(
                            child, code, file_id, counter, symbols, module_path, depth + 1,
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
                        child, code, file_id, counter, symbols, module_path, depth + 1,
                    );
                }
                self.context.exit_scope();
                return; // Skip default child processing
            }

            "short_var_declaration" => {
                self.process_short_var_declaration(
                    node, code, file_id, counter, symbols, module_path,
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
                    child, code, file_id, counter, symbols, module_path, depth + 1,
                );
            }
        }
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

        let signature = self.extract_signature(node, code);
        let doc_comment = self.extract_doc_comment_impl(&node, code);
        let visibility = determine_go_visibility(name);

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

        let signature = self.extract_method_signature(node, code);
        let doc_comment = self.extract_doc_comment_impl(&node, code);
        let visibility = determine_go_visibility(name);

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
        let name_node = match node.child_by_field_name("name") {
            Some(n) => n,
            None => return,
        };
        let name = &code[name_node.byte_range()];
        let type_node = match node.child_by_field_name("type") {
            Some(n) => n,
            None => return,
        };

        match type_node.kind() {
            "struct_type" => {
                let signature = self.extract_struct_signature(node, code);
                let doc_comment = self.extract_doc_comment_impl(&node, code);
                let visibility = determine_go_visibility(name);

                let symbol = self.create_symbol(
                    counter.next_id(),
                    name.to_string(),
                    SymbolKind::Struct,
                    file_id,
                    node_range(node),
                    Some(signature),
                    doc_comment,
                    module_path,
                    visibility,
                );
                symbols.push(symbol);

                self.extract_struct_fields(
                    type_node, code, file_id, counter, symbols, module_path, name,
                );
            }
            "interface_type" => {
                let signature = self.extract_interface_signature(node, code);
                let doc_comment = self.extract_doc_comment_impl(&node, code);
                let visibility = determine_go_visibility(name);

                let symbol = self.create_symbol(
                    counter.next_id(),
                    name.to_string(),
                    SymbolKind::Interface,
                    file_id,
                    node_range(node),
                    Some(signature),
                    doc_comment,
                    module_path,
                    visibility,
                );
                symbols.push(symbol);

                self.extract_interface_methods(
                    type_node, code, file_id, counter, symbols, module_path, name,
                );
            }
            _ => {
                // Type alias
                let signature = code[node.byte_range()].to_string();
                let doc_comment = self.extract_doc_comment_impl(&node, code);
                let visibility = determine_go_visibility(name);

                let symbol = self.create_symbol(
                    counter.next_id(),
                    name.to_string(),
                    SymbolKind::TypeAlias,
                    file_id,
                    node_range(node),
                    Some(signature),
                    doc_comment,
                    module_path,
                    visibility,
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
        module_path: &str,
        struct_name: &str,
    ) {
        for child in struct_node.children(&mut struct_node.walk()) {
            if child.kind() == "field_declaration_list" {
                for field_child in child.children(&mut child.walk()) {
                    if field_child.kind() == "field_declaration" {
                        self.process_struct_field(
                            field_child, code, file_id, counter, symbols, module_path,
                            struct_name,
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
        module_path: &str,
        struct_name: &str,
    ) {
        let mut field_names = Vec::new();
        let mut field_type = None;

        for child in field_node.children(&mut field_node.walk()) {
            match child.kind() {
                "field_identifier" => {
                    field_names.push(&code[child.byte_range()]);
                }
                "type_identifier" | "pointer_type" | "array_type" | "slice_type" | "map_type"
                | "channel_type" => {
                    field_type = Some(&code[child.byte_range()]);
                }
                _ => {}
            }
        }

        for field_name in field_names {
            let visibility = determine_go_visibility(field_name);
            let signature = match field_type {
                Some(typ) => format!("{field_name} {typ}"),
                None => field_name.to_string(),
            };
            let qualified_name = format!("{struct_name}.{field_name}");

            let symbol = self.create_symbol(
                counter.next_id(),
                qualified_name,
                SymbolKind::Field,
                file_id,
                node_range(field_node),
                Some(signature),
                None,
                module_path,
                visibility,
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
        module_path: &str,
        interface_name: &str,
    ) {
        for child in interface_node.children(&mut interface_node.walk()) {
            if child.kind() == "method_elem" {
                self.process_interface_method(
                    child, code, file_id, counter, symbols, module_path, interface_name,
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
        module_path: &str,
        interface_name: &str,
    ) {
        let method_name = method_node
            .children(&mut method_node.walk())
            .find(|n| n.kind() == "field_identifier")
            .map(|n| &code[n.byte_range()]);

        if let Some(name) = method_name {
            let signature = code[method_node.byte_range()].to_string();
            let visibility = determine_go_visibility(name);
            let qualified_name = format!("{interface_name}.{name}");

            let symbol = self.create_symbol(
                counter.next_id(),
                qualified_name,
                SymbolKind::Method,
                file_id,
                node_range(method_node),
                Some(signature),
                None,
                module_path,
                visibility,
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

        for var_name in var_names {
            let visibility = determine_go_visibility(var_name);
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
                Some(signature),
                None,
                module_path,
                visibility,
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

        for const_name in const_names {
            let visibility = determine_go_visibility(const_name);
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
                Some(signature),
                None,
                module_path,
                visibility,
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
            let visibility = determine_go_visibility(var_name);
            let signature = format!("{var_name} := ...");

            let mut symbol = self.create_symbol(
                counter.next_id(),
                var_name.to_string(),
                SymbolKind::Variable,
                file_id,
                node_range(node),
                Some(signature),
                None,
                module_path,
                visibility,
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
                    let visibility = determine_go_visibility(name);
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
                        Some(signature),
                        None,
                        module_path,
                        visibility,
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
                    let visibility = determine_go_visibility(param_name);
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
                        Some(signature),
                        None,
                        module_path,
                        visibility,
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
        module_path: &str,
        depth: usize,
    ) {
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
                        child, code, file_id, counter, symbols, module_path, depth + 1,
                    );
                }
            }
        }

        for (i, var_name) in range_vars.iter().enumerate() {
            let visibility = determine_go_visibility(var_name);
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
                Some(signature),
                None,
                module_path,
                visibility,
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

    fn extract_signature(&self, node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    fn extract_method_signature(&self, node: Node, code: &str) -> String {
        let start = node.start_byte();
        let end = node
            .child_by_field_name("body")
            .map_or(node.end_byte(), |b| b.start_byte());
        code[start..end].trim().to_string()
    }

    fn extract_struct_signature(&self, node: Node, code: &str) -> String {
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

    fn extract_interface_signature(&self, node: Node, code: &str) -> String {
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

    fn extract_doc_comment_impl(&self, node: &Node, code: &str) -> Option<String> {
        // For type_spec nodes, check the parent type_declaration for comments
        let search_node = if node.kind() == "type_spec" {
            node.parent()?
        } else {
            *node
        };

        let mut doc_lines = Vec::new();
        let mut current = search_node.prev_sibling();

        while let Some(sibling) = current {
            if sibling.kind() == "comment" {
                let comment_text = &code[sibling.byte_range()];
                if comment_text.starts_with("//") {
                    let content = comment_text.trim_start_matches("//").trim();
                    doc_lines.insert(0, content.to_string());
                    current = sibling.prev_sibling();
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        if doc_lines.is_empty() {
            return None;
        }

        let filtered: Vec<String> = doc_lines.into_iter().filter(|l| !l.is_empty()).collect();
        if filtered.is_empty() {
            return None;
        }

        let joined = filtered.join("\n").trim().to_string();
        if joined.is_empty() { None } else { Some(joined) }
    }

    // ── Imports ─────────────────────────────────────────────────────────

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let tree = match self.parser.parse(code, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        self.extract_imports_from_node(tree.root_node(), code, file_id, &mut imports);
        imports
    }

    fn extract_imports_from_node(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
        if node.kind() == "import_declaration" {
            self.process_import_declaration(node, code, file_id, imports);
        } else {
            for child in node.children(&mut node.walk()) {
                self.extract_imports_from_node(child, code, file_id, imports);
            }
        }
    }

    fn process_import_declaration(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
        for child in node.children(&mut node.walk()) {
            match child.kind() {
                "import_spec" => {
                    self.process_import_spec(child, code, file_id, imports);
                }
                "import_spec_list" => {
                    for spec_child in child.children(&mut child.walk()) {
                        if spec_child.kind() == "import_spec" {
                            self.process_import_spec(spec_child, code, file_id, imports);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn process_import_spec(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        imports: &mut Vec<Import>,
    ) {
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
        &self,
        node: &Node,
        code: &'a str,
        current_function: Option<&'a str>,
        calls: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
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
                if function_node.kind() != "selector_expression" {
                    if let Some(fn_name) = extract_function_name(&function_node, code) {
                        if let Some(context) = function_context {
                            calls.push((context, fn_name, node_range(*node)));
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.extract_calls_recursive(&child, code, function_context, calls);
        }
    }

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse(code, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.extract_calls_recursive(&tree.root_node(), code, None, &mut calls);
        calls
    }

    // ── Method calls ────────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_method_calls_recursive(
        &self,
        node: &Node,
        code: &str,
        current_function: Option<&str>,
        calls: &mut Vec<MethodCall>,
    ) {
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
                if function_node.kind() == "selector_expression" {
                    if let Some((receiver, method_name, is_static)) =
                        extract_method_signature(&function_node, code)
                    {
                        if let Some(context) = function_context {
                            calls.push(MethodCall {
                                caller: context.to_string(),
                                method_name: method_name.to_string(),
                                receiver: receiver.map(|r| r.to_string()),
                                is_static,
                                range: node_range(*node),
                                caller_range: None,
                            });
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            self.extract_method_calls_recursive(&child, code, function_context, calls);
        }
    }

    fn find_method_calls_impl(&mut self, code: &str) -> Vec<MethodCall> {
        let tree = match self.parser.parse(code, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.extract_method_calls_recursive(&tree.root_node(), code, None, &mut calls);
        calls
    }

    // ── Type uses ───────────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_type_uses_recursive<'a>(
        &self,
        node: &Node,
        code: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        match node.kind() {
            "function_declaration" | "method_declaration" => {
                let context_name = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()])
                    .unwrap_or("anonymous");

                if let Some(params) = node.child_by_field_name("parameters") {
                    self.extract_go_parameter_types(params, code, context_name, uses);
                }
                if let Some(result) = node.child_by_field_name("result") {
                    self.extract_go_type_reference(&result, code, context_name, uses);
                }
            }

            "struct_type" => {
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "field_declaration_list" {
                        for field_child in child.children(&mut child.walk()) {
                            if field_child.kind() == "field_declaration" {
                                self.extract_go_field_types(&field_child, code, "struct", uses);
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
                            self.extract_go_type_reference(&child, code, var_name, uses);
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
                                    self.extract_go_type_reference(
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
            self.extract_type_uses_recursive(&child, code, uses);
        }
    }

    fn extract_go_parameter_types<'a>(
        &self,
        params_node: Node,
        code: &'a str,
        context_name: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        for param in params_node.children(&mut params_node.walk()) {
            if param.kind() == "parameter_declaration" {
                for child in param.children(&mut param.walk()) {
                    if is_go_type_kind(child.kind()) {
                        self.extract_go_type_reference(&child, code, context_name, uses);
                    }
                }
            }
        }
    }

    fn extract_go_field_types<'a>(
        &self,
        field_node: &Node,
        code: &'a str,
        context_name: &'a str,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        for child in field_node.children(&mut field_node.walk()) {
            if is_go_type_kind(child.kind()) {
                self.extract_go_type_reference(&child, code, context_name, uses);
            }
        }
    }

    fn extract_go_type_reference<'a>(
        &self,
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
        let tree = match self.parser.parse(code, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut uses = Vec::new();
        self.extract_type_uses_recursive(&tree.root_node(), code, &mut uses);
        uses
    }

    // ── Method defines ──────────────────────────────────────────────────

    #[allow(clippy::only_used_in_recursion)]
    fn extract_method_defines_recursive<'a>(
        &self,
        node: &Node,
        code: &'a str,
        defines: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
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
                    let receiver_type =
                        if let Some(receiver) = node.child_by_field_name("receiver") {
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
            self.extract_method_defines_recursive(&child, code, defines);
        }
    }

    fn find_defines_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse(code, None) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut defines = Vec::new();
        self.extract_method_defines_recursive(&tree.root_node(), code, &mut defines);
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

fn determine_go_visibility(name: &str) -> Visibility {
    if name.starts_with(|c: char| c.is_uppercase()) {
        Visibility::Public
    } else {
        Visibility::Private
    }
}

fn is_go_type_kind(kind: &str) -> bool {
    matches!(
        kind,
        "type_identifier"
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

fn extract_method_signature<'a>(
    selector_expr: &Node,
    code: &'a str,
) -> Option<(Option<&'a str>, &'a str, bool)> {
    let operand = selector_expr.child_by_field_name("operand");
    let field = selector_expr.child_by_field_name("field");

    match (operand, field) {
        (Some(obj), Some(prop)) => {
            let receiver = &code[obj.byte_range()];
            let method_name = &code[prop.byte_range()];
            Some((Some(receiver), method_name, false))
        }
        _ => None,
    }
}

fn extract_go_type_name<'a>(node: &Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "type_identifier" | "qualified_type" => Some(&code[node.byte_range()]),
        "pointer_type" => node.children(&mut node.walk()).nth(1).and_then(|child| {
            extract_go_type_name(&child, code)
        }),
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
    fn parse(
        &mut self,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
    ) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Go
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        self.extract_doc_comment_impl(node, code)
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_method_calls(&mut self, code: &str) -> Vec<MethodCall> {
        self.find_method_calls_impl(code)
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
            && s.visibility == Visibility::Private));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "MyStruct"
            && s.kind == SymbolKind::Struct
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "MyStruct.Name"
            && s.kind == SymbolKind::Field
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "MyStruct.age"
            && s.kind == SymbolKind::Field
            && s.visibility == Visibility::Private));
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

        assert!(symbols.iter().any(|s| s.name.as_ref() == "Writer"
            && s.kind == SymbolKind::Interface));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Writer.Write"
            && s.kind == SymbolKind::Method));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "FileProcessor"
            && s.kind == SymbolKind::Struct));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Write"
            && s.kind == SymbolKind::Method));
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
            && s.visibility == Visibility::Private));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "PublicVariable"
            && s.kind == SymbolKind::Variable));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "privateVariable"
            && s.kind == SymbolKind::Variable));
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
        assert!(imports
            .iter()
            .any(|i| i.path == "github.com/user/repo/utils"
                && i.alias == Some("utils".to_string())));
        assert!(imports
            .iter()
            .any(|i| i.path == "encoding/json" && i.alias == Some(".".to_string())));
        assert!(imports
            .iter()
            .any(|i| i.path == "database/sql" && i.alias == Some("_".to_string())));
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

        let public: Vec<_> = symbols
            .iter()
            .filter(|s| s.visibility == Visibility::Public)
            .collect();
        let private: Vec<_> = symbols
            .iter()
            .filter(|s| s.visibility == Visibility::Private)
            .collect();

        assert!(!public.is_empty());
        assert!(!private.is_empty());

        assert!(symbols.iter().any(|s| s.name.as_ref() == "PublicFunction"
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "privateFunction"
            && s.visibility == Visibility::Private));
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
        assert!(calls
            .iter()
            .any(|(caller, target, _)| *caller == "main" && *target == "process"));
        assert!(calls
            .iter()
            .any(|(caller, target, _)| *caller == "main" && *target == "getData"));
    }

    #[test]
    fn test_find_method_calls() {
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

        let calls = parser.find_method_calls_impl(code);
        assert!(calls
            .iter()
            .any(|c| c.caller == "main" && c.method_name == "Start"));
        assert!(calls
            .iter()
            .any(|c| c.caller == "main" && c.method_name == "Println"));
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
        assert!(defines
            .iter()
            .any(|(receiver, method, _)| *receiver == "*Server" && *method == "Start"));
        assert!(defines
            .iter()
            .any(|(receiver, method, _)| *receiver == "*Server" && *method == "Stop"));
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

        assert!(symbols.iter().any(|s| s.name.as_ref() == "Identity"
            && s.kind == SymbolKind::Function
            && s.signature.as_deref().is_some_and(|sig| sig.contains("[T any]"))));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Container"
            && s.kind == SymbolKind::Struct));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Processor"
            && s.kind == SymbolKind::Interface));
    }
}
