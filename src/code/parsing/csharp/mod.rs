//! C# language parser implementation using tree-sitter-c-sharp 0.23.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{
    LanguageParser, check_recursion_depth, is_plain_path, last_name_segment, node_range,
};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct CSharpParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for CSharpParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CSharpParser")
            .field("language", &"C#")
            .finish()
    }
}

impl CSharpParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_c_sharp::LANGUAGE.into())
            .map_err(|e| format!("Failed to set C# language: {e}"))?;

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
        self.extract_symbols_from_node(
            tree.root_node(),
            code,
            file_id,
            counter,
            &mut symbols,
            ("", 0),
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
            // C#'s five type declarations differ only in the symbol they
            // produce; the scope handling below is identical for all of them.
            "class_declaration"
            | "record_declaration"
            | "interface_declaration"
            | "struct_declaration"
            | "enum_declaration" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(symbol) = self.process_type(node, code, file_id, counter, module_path) {
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(name);

                // A positional record declares its members in a parameter list,
                // which no other type has and which is not the body.
                self.process_positional_components(
                    node,
                    code,
                    file_id,
                    counter,
                    symbols,
                    module_path,
                );

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

            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                let ns_name = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()]);
                let new_path = match (module_path, ns_name) {
                    ("", Some(ns)) => ns.to_string(),
                    (_, Some(ns)) => format!("{module_path}.{ns}"),
                    _ => module_path.to_string(),
                };

                self.context.enter_scope(ScopeType::Namespace);
                if let Some(body) = node.child_by_field_name("body") {
                    for child in body.children(&mut body.walk()) {
                        self.extract_symbols_from_node(
                            child,
                            code,
                            file_id,
                            counter,
                            symbols,
                            (&new_path, depth + 1),
                        );
                    }
                } else {
                    // File-scoped namespace: everything after is in this namespace
                    for child in node.children(&mut node.walk()) {
                        self.extract_symbols_from_node(
                            child,
                            code,
                            file_id,
                            counter,
                            symbols,
                            (&new_path, depth + 1),
                        );
                    }
                }
                self.context.exit_scope();
            }

            "method_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_csharp_doc(&node, code);
                    let vis = determine_csharp_visibility(node, code);

                    let return_type = node
                        .child_by_field_name("type")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("void");
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        self.qualified(name),
                        SymbolKind::Method,
                        file_id,
                        node_range(node),
                        (
                            Some(format!("{return_type} {name}{params}")),
                            doc,
                            module_path,
                            vis,
                        ),
                    );
                    symbols.push(symbol);
                }

                // A method body can declare local functions, which are symbols
                // in their own right.
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
            }

            "constructor_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let vis = determine_csharp_visibility(node, code);
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        self.qualified(name),
                        SymbolKind::Method,
                        file_id,
                        node_range(node),
                        (Some(format!("{name}{params}")), None, module_path, vis),
                    );
                    symbols.push(symbol);
                }
            }

            "property_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let vis = determine_csharp_visibility(node, code);

                    let type_str = node
                        .child_by_field_name("type")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("?");

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        self.qualified(name),
                        SymbolKind::Field,
                        file_id,
                        node_range(node),
                        (Some(format!("{type_str} {name}")), None, module_path, vis),
                    );
                    symbols.push(symbol);
                }
            }

            "field_declaration" | "event_field_declaration" => {
                self.process_field(node, code, file_id, counter, symbols, module_path);
            }

            // Members that are named by something other than a `name` field, or
            // that are not members of the enclosing type at all.
            "enum_member_declaration"
            | "event_declaration"
            | "delegate_declaration"
            | "indexer_declaration"
            | "operator_declaration"
            | "conversion_operator_declaration"
            | "local_function_statement" => {
                if let Some(symbol) = self.process_member(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
                // A local function may hold another one, and an accessor body
                // may hold a local function too.
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

    /// Symbol for any of C#'s five type declarations.
    ///
    /// `record struct` is spelled as a `record_declaration` too, so the keyword
    /// is read back from the source rather than guessed from the node kind.
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
            "record_declaration" => (SymbolKind::Class, "record"),
            "interface_declaration" => (SymbolKind::Interface, "interface"),
            "struct_declaration" => (SymbolKind::Struct, "struct"),
            "enum_declaration" => (SymbolKind::Enum, "enum"),
            _ => return None,
        };

        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];

        // A partial type is one type written in several places, and each
        // fragment gets its own symbol because a symbol holds one file and one
        // range. Saying `partial` is what stops each fragment from reading as
        // the whole declaration.
        let is_partial = node
            .children(&mut node.walk())
            .any(|c| c.kind() == "modifier" && &code[c.byte_range()] == "partial");
        let modifier = if is_partial { "partial " } else { "" };

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            kind,
            file_id,
            node_range(node),
            (
                Some(format!("{modifier}{keyword} {name}")),
                extract_csharp_doc(&node, code),
                module_path,
                determine_csharp_visibility(node, code),
            ),
        ))
    }

    /// Fields declared by a positional record's parameter list.
    fn process_positional_components(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let Some(params) = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "parameter_list")
        else {
            return;
        };
        for param in params.children(&mut params.walk()) {
            if param.kind() != "parameter" {
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
                    // The property the compiler generates for a positional
                    // component is public.
                    Visibility::Public,
                ),
            ));
        }
    }

    /// Symbol for a member the grammar does not name with a plain `name` field.
    fn process_member(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let named = |field: &str| {
            node.child_by_field_name(field)
                .map(|n| &code[n.byte_range()])
        };
        let type_str = named("type").unwrap_or("void");
        let params = named("parameters").unwrap_or("");

        let (name, kind, signature) = match node.kind() {
            "enum_member_declaration" => {
                let name = named("name")?;
                (name.to_string(), SymbolKind::Constant, name.to_string())
            }
            "event_declaration" => {
                let name = named("name")?;
                (
                    name.to_string(),
                    SymbolKind::Field,
                    format!("event {type_str} {name}"),
                )
            }
            // A delegate declares a named function type, so it is indexed as a
            // type alias rather than as a member of the enclosing class.
            "delegate_declaration" => {
                let name = named("name")?;
                (
                    name.to_string(),
                    SymbolKind::TypeAlias,
                    format!("delegate {type_str} {name}{params}"),
                )
            }
            // An indexer has no name in the source: it is written `this[..]`.
            "indexer_declaration" => (
                "this[]".to_string(),
                SymbolKind::Field,
                format!("{type_str} this{}", named("parameters").unwrap_or("[]")),
            ),
            "operator_declaration" => {
                let op = named("operator")?;
                (
                    format!("operator {op}"),
                    SymbolKind::Method,
                    format!("{type_str} operator {op}{params}"),
                )
            }
            // A conversion operator is named by the type it converts to.
            "conversion_operator_declaration" => (
                format!("operator {type_str}"),
                SymbolKind::Method,
                format!("operator {type_str}{params}"),
            ),
            // A local function lives in a method body, not in the type, so it is
            // deliberately left unqualified.
            "local_function_statement" => {
                let name = named("name")?;
                return Some(self.create_symbol(
                    counter.next_id(),
                    name.to_string(),
                    SymbolKind::Function,
                    file_id,
                    node_range(node),
                    (
                        Some(format!("{type_str} {name}{params}")),
                        extract_csharp_doc(&node, code),
                        module_path,
                        Visibility::Private,
                    ),
                ));
            }
            _ => return None,
        };

        Some(self.create_symbol(
            counter.next_id(),
            self.qualified(&name),
            kind,
            file_id,
            node_range(node),
            (
                Some(signature),
                extract_csharp_doc(&node, code),
                module_path,
                determine_csharp_visibility(node, code),
            ),
        ))
    }

    /// Member name qualified by the type currently being walked.
    fn qualified(&self, name: &str) -> String {
        match self.context.current_class() {
            Some(cls) => format!("{cls}.{name}"),
            None => name.to_string(),
        }
    }

    fn process_field(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let vis = determine_csharp_visibility(node, code);
        let is_const = code[node.byte_range()].contains("const ");

        for child in node.children(&mut node.walk()) {
            if child.kind() == "variable_declaration" {
                for declarator in child.children(&mut child.walk()) {
                    if declarator.kind() == "variable_declarator" {
                        if let Some(name_node) = declarator.child_by_field_name("name") {
                            let name = &code[name_node.byte_range()];
                            let kind = if is_const {
                                SymbolKind::Constant
                            } else {
                                SymbolKind::Field
                            };

                            let symbol = self.create_symbol(
                                counter.next_id(),
                                self.qualified(name),
                                kind,
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
    }

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut imports = Vec::new();
        for child in tree.root_node().children(&mut tree.root_node().walk()) {
            if child.kind() == "using_directive" {
                let text = code[child.byte_range()].trim();
                let path = text
                    .trim_start_matches("using ")
                    .trim_start_matches("static ")
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
        let fn_ctx = if node.kind() == "method_declaration" {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn)
        } else {
            current_fn
        };

        let target = match node.kind() {
            "invocation_expression" => node
                .child_by_field_name("function")
                .and_then(|f| Self::call_target_name(f, code)),
            // Construction is a dependency on the constructed type. An
            // implicit `new()` names no type and is left alone.
            "object_creation_expression" => node
                .child_by_field_name("type")
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

    /// Bare member name invoked by the `function` part of a call.
    ///
    /// Relationships resolve by matching the stored target against a symbol
    /// name, so the receiver has to be dropped: `items.Select(..)` depends on
    /// `Select`, and storing `items.Select` matches nothing. Returns `None` when
    /// the callee is a computed value — `Func()()` invokes what the inner call
    /// returned and names no member of its own.
    fn call_target_name<'a>(func: Node, code: &'a str) -> Option<&'a str> {
        match func.kind() {
            "identifier" | "generic_name" => Some(last_name_segment(func, code)),
            // `Console.WriteLine` keeps its receiver when the receiver is a
            // name: without it the call matches every `WriteLine` indexed.
            "member_access_expression" => {
                let whole = &code[func.byte_range()];
                if is_plain_path(whole) {
                    Some(whole)
                } else {
                    func.child_by_field_name("name")
                        .map(|n| last_name_segment(n, code))
                }
            }
            // `a?.b` hangs the member off a trailing `member_binding_expression`;
            // the `condition` field holds the receiver.
            "conditional_access_expression" => func
                .children(&mut func.walk())
                .filter(|c| c.kind() == "member_binding_expression")
                .last()
                .and_then(|b| b.child_by_field_name("name"))
                .map(|n| last_name_segment(n, code)),
            _ => None,
        }
    }

    fn find_implementations_in_node<'a>(
        node: &Node,
        code: &'a str,
        depth: usize,
        results: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, *node) {
            return;
        }
        if matches!(
            node.kind(),
            "class_declaration" | "struct_declaration" | "interface_declaration"
        ) {
            if let Some(name_node) = node.child_by_field_name("name") {
                let type_name = &code[name_node.byte_range()];
                if let Some(bases) = node.child_by_field_name("bases") {
                    for child in bases.children(&mut bases.walk()) {
                        if child.kind() == "identifier" || child.kind() == "generic_name" {
                            results.push((type_name, &code[child.byte_range()], node_range(child)));
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_implementations_in_node(&child, code, depth + 1, results);
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

fn determine_csharp_visibility(node: Node, code: &str) -> Visibility {
    // Inspect the declaration's `modifier` AST children rather than substring-
    // matching the whole text (BUG-C1): `private string publicKey;` must not be
    // read as Public. tree-sitter-c-sharp emits each modifier as a `modifier` node.
    // Collected rather than returned on the first hit: two of C#'s six levels
    // are written as a pair, and the first word of each pair is the *wrong*
    // answer on its own. `protected internal` is the assembly OR any derived
    // type, which is wider than either half; `private protected` is a derived
    // type AND the same assembly, which is narrower than either.
    let mut protected = false;
    let mut internal = false;
    let mut private = false;
    for child in node.children(&mut node.walk()) {
        if child.kind() == "modifier" {
            match &code[child.byte_range()] {
                "public" => return Visibility::Public,
                // `file` (C# 11) is the same reach as Swift's `fileprivate`.
                "file" => return Visibility::Module,
                "protected" => protected = true,
                "internal" => internal = true,
                "private" => private = true,
                _ => {}
            }
        }
    }
    match (protected, internal, private) {
        (true, true, _) => Visibility::Package,
        (true, _, true) => Visibility::Restricted,
        (true, ..) => Visibility::Module,
        (_, true, _) => Visibility::Crate,
        // C# default is private, and so is a bare `private`.
        _ => Visibility::Private,
    }
}

fn extract_csharp_doc(node: &Node, code: &str) -> Option<String> {
    // C# uses /// XML doc comments
    let mut lines = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(sib) = prev {
        if sib.kind() == "comment" {
            let text = &code[sib.byte_range()];
            if text.starts_with("///") {
                let content = text
                    .trim_start_matches("///")
                    .trim()
                    .trim_start_matches("<summary>")
                    .trim_end_matches("</summary>")
                    .trim();
                if !content.is_empty() {
                    lines.push(content.to_string());
                }
                prev = sib.prev_sibling();
                continue;
            }
        }
        break;
    }

    if lines.is_empty() {
        None
    } else {
        lines.reverse();
        Some(lines.join("\n"))
    }
}

impl LanguageParser for CSharpParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::CSharp
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_csharp_doc(node, code)
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

    fn find_uses<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    fn find_defines<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    fn find_imports(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        self.extract_imports_impl(code, file_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two of C#'s six levels are written as a pair, and returning on the first
    /// modifier reported each pair as its first word: `protected internal` as
    /// `protected` (narrower than it is) and `private protected` as `private`
    /// (narrower still).
    #[test]
    fn a_composite_access_level_is_not_reported_as_its_first_word() {
        let mut parser = CSharpParser::new().unwrap();
        let code = r#"
class App {
    public int A;
    protected internal int B;
    private protected int C;
    protected int D;
    internal int E;
    private int F;
    int G;
}
"#;
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, FileId::new(1).unwrap(), &mut counter);
        let level = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref().ends_with(name))
                .unwrap_or_else(|| panic!("{name} not parsed"))
                .visibility
        };

        assert_eq!(level("A"), Visibility::Public);
        assert_eq!(level("B"), Visibility::Package, "protected internal");
        assert_eq!(level("C"), Visibility::Restricted, "private protected");
        assert_eq!(level("D"), Visibility::Module, "protected");
        assert_eq!(level("E"), Visibility::Crate, "internal");
        assert_eq!(level("F"), Visibility::Private);
        assert_eq!(level("G"), Visibility::Private, "C# default");
        assert_ne!(level("B"), level("D"), "the story's criterion 3");
        assert_ne!(level("C"), level("F"), "the story's criterion 4");
    }

    /// `file` (C# 11) confines a type to its source file. It is not an access
    /// modifier the collector above knows, so it used to fall through to the
    /// `private` default and become indistinguishable from a private member.
    #[test]
    fn a_file_scoped_type_is_not_reported_as_private() {
        let mut parser = CSharpParser::new().unwrap();
        let code = "file class Local {}\nclass Other {}\n";
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, FileId::new(1).unwrap(), &mut counter);
        let level = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} not parsed"))
                .visibility
        };

        assert_eq!(level("Local"), Visibility::Module);
        assert_ne!(level("Local"), level("Other"));
    }

    /// A partial class is one type written in several places. The parser emits
    /// one symbol per fragment, and a symbol carries a single file and range,
    /// so it cannot emit one symbol for the whole type. What it can do is stop
    /// each fragment from claiming to be the whole class.
    #[test]
    fn a_partial_fragment_says_it_is_one() {
        let mut parser = CSharpParser::new().unwrap();
        let code = "partial class Point { }\nclass Whole { }\npartial struct Pair { }\n";
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, FileId::new(1).unwrap(), &mut counter);
        let sig = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} not parsed"))
                .signature
                .as_deref()
                .map(str::to_string)
        };

        assert_eq!(sig("Point"), Some("partial class Point".to_string()));
        assert_eq!(sig("Whole"), Some("class Whole".to_string()));
        assert_eq!(sig("Pair"), Some("partial struct Pair".to_string()));
    }

    #[test]
    fn test_parse_class_with_methods() {
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
namespace MyApp {
    public class Calculator {
        private int value;

        public Calculator(int initial) {
            value = initial;
        }

        public int Add(int x) {
            return value + x;
        }
    }
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator"
            && s.kind == SymbolKind::Class
            && s.visibility == Visibility::Public));
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Calculator.Add" && s.kind == SymbolKind::Method)
        );
        assert!(symbols.iter().any(|s| s.name.as_ref() == "Calculator.Calculator"
            && s.kind == SymbolKind::Method));
    }

    #[test]
    fn csharp_visibility_ignores_keyword_substring_in_identifier() {
        // BUG-C1: a private member whose name contains "public" must not be read
        // as Public. The public sibling proves the modifier-node lookup works.
        let mut parser = CSharpParser::new().unwrap();
        let code = r#"
namespace N {
    class C {
        private int publicCount() { return 0; }
        public int Real() { return 1; }
    }
}
"#;
        let symbols =
            parser.parse_symbols(code, FileId::new(1).unwrap(), &mut SymbolCounter::new());
        let pc = symbols
            .iter()
            .find(|s| s.name.as_ref().ends_with("publicCount"))
            .expect("publicCount method");
        assert_eq!(
            pc.visibility,
            Visibility::Private,
            "an identifier containing 'public' must not be classified Public"
        );
        let real = symbols
            .iter()
            .find(|s| s.name.as_ref().ends_with("Real"))
            .expect("Real method");
        assert_eq!(real.visibility, Visibility::Public);
    }

    #[test]
    fn test_parse_interface_and_enum() {
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
public interface ISerializable {
    string Serialize();
}

public enum Color {
    Red,
    Green,
    Blue
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "ISerializable" && s.kind == SymbolKind::Interface)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Color" && s.kind == SymbolKind::Enum)
        );
    }

    #[test]
    fn test_find_using_directives() {
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
using System;
using System.Collections.Generic;
"#;

        let imports = parser.find_imports(code, file_id);
        assert!(imports.iter().any(|i| i.path == "System"));
        assert!(
            imports
                .iter()
                .any(|i| i.path == "System.Collections.Generic")
        );
    }

    #[test]
    fn test_namespace_as_module_path() {
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
namespace MyApp.Models {
    public class User {}
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let cls = symbols.iter().find(|s| s.name.as_ref() == "User").unwrap();
        assert_eq!(cls.module_path.as_deref(), Some("MyApp.Models"));
    }

    /// `invocation_expression` alone misses every `new`, so construction was
    /// invisible to the call graph.
    #[test]
    fn construction_produces_call_edges() {
        let mut parser = CSharpParser::new().unwrap();

        let code = r#"
class App {
    void Run() {
        var f = new Foo();
        var g = new Foo.Bar(1);
        var h = new List<Foo>();
        Helper();
    }
}
"#;

        let calls = parser.find_calls_impl(code);
        let edges: Vec<(&str, &str)> = calls.iter().map(|(c, t, _)| (*c, *t)).collect();

        assert!(edges.contains(&("Run", "Foo")), "new Foo(): {edges:?}");
        assert!(edges.contains(&("Run", "Bar")), "new Foo.Bar(): {edges:?}");
        assert!(
            edges.contains(&("Run", "List")),
            "type arguments must not shadow the constructed type: {edges:?}"
        );
        assert!(edges.contains(&("Run", "Helper")), "plain call: {edges:?}");
    }

    /// A written receiver is kept so the call can be narrowed to one owner;
    /// anything that is not a plain name — a conditional access, a type argument
    /// list, a call — leaves only the member name, because there is no name to
    /// qualify with.
    #[test]
    fn qualified_call_targets_keep_a_receiver_that_is_a_name() {
        let mut parser = CSharpParser::new().unwrap();

        let code = r#"
class App {
    void Run() {
        other?.Bar();
        MyEvent?.Invoke(this);
        items.Select(x => x);
        Ns.Static.Deep.Call();
        obj.Generic<int>();
        a?.b?.c();
        Helper();
        Func()();
    }
}
"#;

        let calls = parser.find_calls_impl(code);
        let edges: Vec<(&str, &str)> = calls.iter().map(|(c, t, _)| (*c, *t)).collect();

        for target in [
            // A receiver that is a name is kept.
            "items.Select",
            "Ns.Static.Deep.Call",
            // `?.` is not part of a name, `<int>` is not part of one either, and
            // `Func()` is a call: all four keep only the member.
            "Bar",
            "Invoke",
            "Generic",
            "c",
            "Helper",
            "Func",
        ] {
            assert!(
                edges.contains(&("Run", target)),
                "missing target {target}: {edges:?}"
            );
        }
        assert!(
            !edges.iter().any(|(_, t)| t.contains('<')),
            "no target may carry its type arguments: {edges:?}"
        );
        // `Func()()` invokes the value the inner call returned. There is no name
        // to point at, and the inner call is already recorded on its own.
        assert_eq!(
            edges.iter().filter(|(_, t)| *t == "Func").count(),
            1,
            "invoking a returned delegate must not duplicate the target: {edges:?}"
        );
    }

    /// Six member forms fell through to generic recursion and produced nothing,
    /// so an event, a delegate, an indexer, an operator, a local function and
    /// every enum member were invisible.
    #[test]
    fn events_delegates_indexers_operators_and_local_functions_produce_symbols() {
        let mut parser = CSharpParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
public record Vec(double X, double Y) {
    public event EventHandler MyEvent;
    public event EventHandler Other { add {} remove {} }
    public delegate void MyDelegate(int a);
    public int this[int index] { get => 0; }
    public static Vec operator +(Vec a, Vec b) => a;
    public static implicit operator string(Vec v) => "";
    void Run() {
        void Local() { }
    }
}

public enum Color { Red = 1, Green }
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let found: Vec<(&str, SymbolKind)> =
            symbols.iter().map(|s| (s.name.as_ref(), s.kind)).collect();
        let has = |name: &str, kind: SymbolKind| found.contains(&(name, kind));

        assert!(has("Vec", SymbolKind::Class), "record type: {found:?}");
        assert!(
            has("Vec.X", SymbolKind::Field),
            "positional component: {found:?}"
        );
        assert!(
            has("Vec.Y", SymbolKind::Field),
            "positional component: {found:?}"
        );

        // A field-style event and an accessor-style event are the same member
        // written two ways.
        assert!(
            has("Vec.MyEvent", SymbolKind::Field),
            "event field: {found:?}"
        );
        assert!(
            has("Vec.Other", SymbolKind::Field),
            "event accessors: {found:?}"
        );

        assert!(
            has("Vec.MyDelegate", SymbolKind::TypeAlias),
            "delegate: {found:?}"
        );
        assert!(has("Vec.this[]", SymbolKind::Field), "indexer: {found:?}");
        assert!(
            has("Vec.operator +", SymbolKind::Method),
            "operator: {found:?}"
        );
        assert!(
            has("Vec.operator string", SymbolKind::Method),
            "conversion operator: {found:?}"
        );

        // A local function belongs to the method body, not to the type, so it is
        // not qualified by the class.
        assert!(
            has("Local", SymbolKind::Function),
            "local function: {found:?}"
        );

        assert!(has("Color", SymbolKind::Enum), "enum type: {found:?}");
        assert!(
            has("Color.Red", SymbolKind::Constant),
            "enum member: {found:?}"
        );
        assert!(
            has("Color.Green", SymbolKind::Constant),
            "enum member: {found:?}"
        );
    }
}
