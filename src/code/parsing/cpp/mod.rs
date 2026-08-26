//! C++ language parser implementation using tree-sitter-cpp 0.23.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{
    LanguageParser, check_recursion_depth, extract_c_family_doc, node_range,
};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct CppParser {
    parser: CachingParser,
    context: ParserContext,
    /// Access section currently in force inside a class body. C++ access is
    /// stateful: `private:` applies to every member after it until the next
    /// specifier, so it cannot be read off an individual member node.
    current_access: Visibility,
}

impl std::fmt::Debug for CppParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CppParser")
            .field("language", &"C++")
            .finish()
    }
}

impl CppParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .map_err(|e| format!("Failed to set C++ language: {e}"))?;

        Ok(Self {
            parser: CachingParser::new(ts_parser),
            context: ParserContext::new(),
            current_access: Visibility::Public,
        })
    }

    /// Visibility for the member being extracted. Inside a class body that is
    /// the access section in force; at namespace scope a free function is
    /// public.
    fn member_visibility(&self) -> Visibility {
        if self.context.is_in_class() {
            self.current_access
        } else {
            Visibility::Public
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
            "function_definition" => {
                if let Some(symbol) =
                    self.process_function(node, code, file_id, counter, module_path)
                {
                    let fn_name = symbol.name.as_ref().to_string();
                    symbols.push(symbol);

                    self.context
                        .enter_scope(ScopeType::Function { hoisting: false });
                    self.context.set_current_function(Some(fn_name));

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
                }
            }

            "class_specifier" | "struct_specifier" | "union_specifier" => {
                let kind_str = match node.kind() {
                    "class_specifier" => "class",
                    "union_specifier" => "union",
                    _ => "struct",
                };
                let name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(ref n) = name {
                    let doc = extract_c_family_doc(&node, code);
                    let sym_kind = if kind_str == "class" {
                        SymbolKind::Class
                    } else {
                        // A union is a record type; there is no Union kind, and
                        // Struct is the closest thing the index models.
                        SymbolKind::Struct
                    };
                    // Read before `set_current_class` below, so a nested type is
                    // qualified by its enclosing class and takes the access
                    // section it was declared in — the same rule as a method.
                    let qualified = match self.context.current_class() {
                        Some(cls) => format!("{cls}::{n}"),
                        None => n.clone(),
                    };
                    let visibility = self.member_visibility();
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified,
                        sym_kind,
                        file_id,
                        node_range(node),
                        (
                            Some(format!("{kind_str} {n}")),
                            doc,
                            module_path,
                            visibility,
                        ),
                    );
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(name);
                // A class starts private, a struct public. Saved and restored so
                // a nested type cannot leak its section to the enclosing body.
                let saved_access = self.current_access;
                self.current_access = if kind_str == "class" {
                    Visibility::Private
                } else {
                    Visibility::Public
                };

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
                self.current_access = saved_access;
            }

            "namespace_definition" => {
                let ns_name = node
                    .child_by_field_name("name")
                    .map(|n| &code[n.byte_range()]);
                let new_path = match (module_path, ns_name) {
                    ("", Some(ns)) => ns.to_string(),
                    (_, Some(ns)) => format!("{module_path}::{ns}"),
                    _ => module_path.to_string(),
                };

                // The namespace is a symbol in its own right, not just a prefix
                // for the things inside it. Named by its full path, so `inner`
                // in two different outers stays two symbols.
                if ns_name.is_some() {
                    let doc = extract_c_family_doc(&node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        new_path.clone(),
                        SymbolKind::Module,
                        file_id,
                        node_range(node),
                        (
                            Some(format!("namespace {new_path}")),
                            doc,
                            module_path,
                            Visibility::Public,
                        ),
                    );
                    symbols.push(symbol);
                }

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
                }
                self.context.exit_scope();
            }

            "enum_specifier" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_c_family_doc(&node, code);
                    let qualified = match self.context.current_class() {
                        Some(cls) => format!("{cls}::{name}"),
                        None => name.to_string(),
                    };
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified,
                        SymbolKind::Enum,
                        file_id,
                        node_range(node),
                        (
                            Some(format!("enum {name}")),
                            doc,
                            module_path,
                            self.member_visibility(),
                        ),
                    );
                    symbols.push(symbol);
                }
                self.process_enumerators(node, code, file_id, counter, symbols, module_path);
            }

            "alias_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_c_family_doc(&node, code);
                    let qualified = match self.context.current_class() {
                        Some(cls) => format!("{cls}::{name}"),
                        None => name.to_string(),
                    };
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified,
                        SymbolKind::TypeAlias,
                        file_id,
                        node_range(node),
                        (
                            Some(
                                code[node.byte_range()]
                                    .trim_end_matches(';')
                                    .trim()
                                    .to_string(),
                            ),
                            doc,
                            module_path,
                            self.member_visibility(),
                        ),
                    );
                    symbols.push(symbol);
                }
            }

            "preproc_def" | "preproc_function_def" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let doc = extract_c_family_doc(&node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        code[name_node.byte_range()].to_string(),
                        SymbolKind::Macro,
                        file_id,
                        node_range(node),
                        (
                            Some(code[node.byte_range()].trim_end().to_string()),
                            doc,
                            module_path,
                            Visibility::Public,
                        ),
                    );
                    symbols.push(symbol);
                }
            }

            "field_declaration" if self.context.is_in_class() => {
                // A type declared inside a class body is a field declaration
                // wrapping its specifier. Walk it, or neither the nested type
                // nor any of its members is ever seen. A specifier with no body
                // is a reference to a type declared elsewhere (`struct Point p;`)
                // and must not be walked, or it invents a duplicate symbol.
                if let Some(nested) = node.children(&mut node.walk()).find(|c| {
                    matches!(
                        c.kind(),
                        "class_specifier"
                            | "struct_specifier"
                            | "union_specifier"
                            | "enum_specifier"
                    ) && c.child_by_field_name("body").is_some()
                }) {
                    self.extract_symbols_from_node(
                        nested,
                        code,
                        file_id,
                        counter,
                        symbols,
                        (module_path, depth + 1),
                    );
                }

                // Method declarations inside class body
                let has_function_declarator = node
                    .children(&mut node.walk())
                    .any(|c| c.kind() == "function_declarator");

                if has_function_declarator {
                    if let Some(symbol) =
                        self.process_method_decl(node, code, file_id, counter, module_path)
                    {
                        symbols.push(symbol);
                    }
                } else {
                    // Field member
                    self.process_field(node, code, file_id, counter, symbols, module_path);
                }
            }

            "template_declaration" => {
                // Recurse into template body
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

            "access_specifier" => {
                let text = &code[node.byte_range()];
                self.current_access = if text.contains("public") {
                    Visibility::Public
                } else if text.contains("protected") {
                    Visibility::Module
                } else {
                    Visibility::Private
                };
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

    fn process_function(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let declarator = node.child_by_field_name("declarator")?;
        let name = extract_cpp_declarator_name(declarator, code)?;
        let doc = extract_c_family_doc(&node, code);

        let type_node = node.child_by_field_name("type");
        let sig = match type_node {
            Some(t) => format!(
                "{} {}",
                &code[t.byte_range()],
                &code[declarator.byte_range()]
            ),
            None => code[declarator.byte_range()].to_string(),
        };

        let kind = if self.context.is_in_class() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };

        let qualified_name = if let Some(cls) = self.context.current_class() {
            format!("{cls}::{name}")
        } else {
            name.to_string()
        };

        Some(self.create_symbol(
            counter.next_id(),
            qualified_name,
            kind,
            file_id,
            node_range(node),
            (Some(sig), doc, module_path, self.member_visibility()),
        ))
    }

    fn process_method_decl(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        // Find the function_declarator child
        let func_decl = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "function_declarator")?;
        let name = extract_cpp_declarator_name(func_decl, code)?;
        let doc = extract_c_family_doc(&node, code);
        let visibility = self.member_visibility();

        let qualified_name = if let Some(cls) = self.context.current_class() {
            format!("{cls}::{name}")
        } else {
            name.to_string()
        };

        let sig = code[node.byte_range()]
            .trim_end_matches(';')
            .trim()
            .to_string();

        Some(self.create_symbol(
            counter.next_id(),
            qualified_name,
            SymbolKind::Method,
            file_id,
            node_range(node),
            (Some(sig), doc, module_path, visibility),
        ))
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
        for child in node.children(&mut node.walk()) {
            if child.kind() == "field_identifier" {
                let name = &code[child.byte_range()];
                let qualified_name = if let Some(cls) = self.context.current_class() {
                    format!("{cls}::{name}")
                } else {
                    name.to_string()
                };

                let symbol = self.create_symbol(
                    counter.next_id(),
                    qualified_name,
                    SymbolKind::Field,
                    file_id,
                    node_range(node),
                    (
                        Some(
                            code[node.byte_range()]
                                .trim_end_matches(';')
                                .trim()
                                .to_string(),
                        ),
                        None,
                        module_path,
                        self.member_visibility(),
                    ),
                );
                symbols.push(symbol);
            }
        }
    }

    /// Enumerators, named `Enum::member`. Valid for a scoped enum and, since
    /// C++11, for an unscoped one too, so it is always a spelling source can use
    /// — and unlike a bare name it tells two enums' `None` apart. Qualified by
    /// the immediate enum only, matching how members of a nested type are named.
    fn process_enumerators(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let (Some(name_node), Some(body)) = (
            node.child_by_field_name("name"),
            node.child_by_field_name("body"),
        ) else {
            return;
        };
        let enum_name = &code[name_node.byte_range()];
        let visibility = self.member_visibility();

        for member in body.children(&mut body.walk()) {
            if member.kind() != "enumerator" {
                continue;
            }
            let Some(member_name) = member.child_by_field_name("name") else {
                continue;
            };
            let symbol = self.create_symbol(
                counter.next_id(),
                format!("{enum_name}::{}", &code[member_name.byte_range()]),
                SymbolKind::Constant,
                file_id,
                node_range(member),
                (
                    Some(code[member.byte_range()].to_string()),
                    None,
                    module_path,
                    visibility,
                ),
            );
            symbols.push(symbol);
        }
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
        let fn_ctx = if node.kind() == "function_definition" {
            node.child_by_field_name("declarator")
                .and_then(|d| extract_cpp_declarator_name(d, code))
                .or(current_fn)
        } else {
            current_fn
        };

        if node.kind() == "call_expression" {
            if let Some(func) = node.child_by_field_name("function") {
                let target = &code[func.byte_range()];
                if let Some(ctx) = fn_ctx {
                    calls.push((ctx, target, node_range(*node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_calls_in_node(&child, code, fn_ctx, depth + 1, calls);
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
        if matches!(node.kind(), "class_specifier" | "struct_specifier") {
            if let Some(name_node) = node.child_by_field_name("name") {
                let class_name = &code[name_node.byte_range()];
                // Look for base_class_clause
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "base_class_clause" {
                        for base in child.children(&mut child.walk()) {
                            if base.kind() == "type_identifier" {
                                results.push((
                                    class_name,
                                    &code[base.byte_range()],
                                    node_range(base),
                                ));
                            }
                        }
                    }
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_implementations_in_node(&child, code, depth + 1, results);
        }
    }

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut imports = Vec::new();
        for child in tree.root_node().children(&mut tree.root_node().walk()) {
            if child.kind() == "preproc_include" {
                if let Some(path_node) = child.child_by_field_name("path") {
                    let raw = &code[path_node.byte_range()];
                    let path = raw
                        .trim_start_matches(['"', '<'])
                        .trim_end_matches(['"', '>'])
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
        }
        imports
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

fn extract_cpp_declarator_name<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "qualified_identifier" => {
            // Return just the rightmost name for display
            node.child_by_field_name("name")
                .and_then(|n| extract_cpp_declarator_name(n, code))
        }
        "pointer_declarator"
        | "reference_declarator"
        | "array_declarator"
        | "parenthesized_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| extract_cpp_declarator_name(d, code)),
        "function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| extract_cpp_declarator_name(d, code)),
        "template_function" => node
            .child_by_field_name("name")
            .and_then(|n| extract_cpp_declarator_name(n, code)),
        "identifier" | "field_identifier" | "destructor_name" | "operator_name" => {
            Some(&code[node.byte_range()])
        }
        _ => None,
    }
}

impl LanguageParser for CppParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Cpp
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_c_family_doc(node, code)
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

    /// Visibility recorded for a symbol, by name.
    fn visibility_of(code: &str, name: &str) -> Visibility {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        symbols
            .iter()
            .find(|s| s.name.as_ref() == name)
            .unwrap_or_else(|| {
                panic!(
                    "no symbol {name}, got {:?}",
                    symbols.iter().map(|s| s.as_name()).collect::<Vec<_>>()
                )
            })
            .visibility
    }

    const SECTIONS: &str = r#"
class Widget {
    void implicitlyPrivate() {}
public:
    int pubField;
    void pubMethod() {}
    void pubProto();
protected:
    int protField;
    void protMethod() {}
    void protProto();
private:
    int privField;
    void privMethod() {}
};
"#;

    #[test]
    fn a_method_with_a_body_takes_its_access_section() {
        assert_eq!(
            visibility_of(SECTIONS, "Widget::privMethod"),
            Visibility::Private
        );
        assert_eq!(
            visibility_of(SECTIONS, "Widget::pubMethod"),
            Visibility::Public
        );
    }

    #[test]
    fn a_defined_method_and_a_declared_one_agree_on_visibility() {
        assert_eq!(
            visibility_of(SECTIONS, "Widget::protMethod"),
            visibility_of(SECTIONS, "Widget::protProto"),
        );
        assert_eq!(
            visibility_of(SECTIONS, "Widget::pubMethod"),
            visibility_of(SECTIONS, "Widget::pubProto"),
        );
    }

    #[test]
    fn a_field_takes_its_access_section() {
        assert_eq!(
            visibility_of(SECTIONS, "Widget::pubField"),
            Visibility::Public
        );
        assert_eq!(
            visibility_of(SECTIONS, "Widget::protField"),
            visibility_of(SECTIONS, "Widget::protMethod"),
        );
        assert_eq!(
            visibility_of(SECTIONS, "Widget::privField"),
            Visibility::Private
        );
    }

    #[test]
    fn a_class_member_before_any_specifier_is_private() {
        assert_eq!(
            visibility_of(SECTIONS, "Widget::implicitlyPrivate"),
            Visibility::Private
        );
    }

    #[test]
    fn a_struct_member_before_any_specifier_is_public() {
        let code = r#"
struct Point {
    int x;
    void translate() {}
private:
    int hidden;
};
"#;
        assert_eq!(visibility_of(code, "Point::x"), Visibility::Public);
        assert_eq!(visibility_of(code, "Point::translate"), Visibility::Public);
        assert_eq!(visibility_of(code, "Point::hidden"), Visibility::Private);
    }

    /// Names of the symbols `code` yields.
    fn names_of(code: &str) -> Vec<String> {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        parser
            .parse_symbols(code, file_id, &mut counter)
            .iter()
            .map(|s| s.as_name().to_string())
            .collect()
    }

    const NESTED: &str = r#"
struct Outer {
    struct Inner {
    private:
        int innerHidden;
    };
private:
    class Guard {
        int guarded;
    };
    int outerHidden;
};
"#;

    /// The `name`/`kind` pairs the symbols of `code` carry.
    fn kinds_of(code: &str) -> Vec<(String, SymbolKind)> {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        parser
            .parse_symbols(code, file_id, &mut counter)
            .iter()
            .map(|s| (s.as_name().to_string(), s.kind))
            .collect()
    }

    /// The doc comment recorded for the symbol named `name`.
    fn doc_of(code: &str, name: &str) -> Option<String> {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        symbols
            .iter()
            .find(|s| s.name.as_ref() == name)
            .unwrap_or_else(|| {
                panic!(
                    "no symbol {name}, got {:?}",
                    symbols.iter().map(|s| s.as_name()).collect::<Vec<_>>()
                )
            })
            .doc_comment
            .as_ref()
            .map(|d| d.to_string())
    }

    #[test]
    fn a_documented_template_function_keeps_its_doc() {
        let doc = doc_of(
            "/** Identity. */\ntemplate<typename T> T identity(T x) { return x; }\n",
            "identity",
        );
        assert_eq!(doc.as_deref(), Some("Identity."));
    }

    #[test]
    fn a_documented_template_class_keeps_its_doc() {
        let doc = doc_of(
            "/** A box. */\ntemplate<typename T> class Box { T v; };\n",
            "Box",
        );
        assert_eq!(doc.as_deref(), Some("A box."));
    }

    #[test]
    fn a_non_doc_comment_above_a_template_yields_no_doc() {
        let doc = doc_of(
            "// just a note\ntemplate<typename T> T identity(T x) { return x; }\n",
            "identity",
        );
        assert_eq!(doc, None);
    }

    #[test]
    fn a_namespace_produces_a_module_symbol() {
        let kinds = kinds_of("namespace outer { namespace inner { int x; } }\n");
        assert!(
            kinds.contains(&("outer".into(), SymbolKind::Module)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("outer::inner".into(), SymbolKind::Module)),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_using_alias_produces_a_type_alias_symbol() {
        let kinds = kinds_of("using MyAlias = int;\n");
        assert!(
            kinds.contains(&("MyAlias".into(), SymbolKind::TypeAlias)),
            "{kinds:?}"
        );
    }

    #[test]
    fn enum_members_are_qualified_by_their_enum() {
        // `Level::High` is valid for a scoped enum and, since C++11, for an
        // unscoped one too — so qualifying is always a spelling source can use.
        let kinds = kinds_of("enum class Level { Low, High = 3 };\nenum Color { RED };\n");
        assert!(
            kinds.contains(&("Level::High".into(), SymbolKind::Constant)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("Color::RED".into(), SymbolKind::Constant)),
            "{kinds:?}"
        );
    }

    #[test]
    fn both_define_forms_produce_macro_symbols() {
        let kinds = kinds_of("#define MAX 10\n#define SQ(x) ((x)*(x))\n");
        assert!(
            kinds.contains(&("MAX".into(), SymbolKind::Macro)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("SQ".into(), SymbolKind::Macro)),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_type_nested_in_a_class_body_is_extracted() {
        let names = names_of(NESTED);
        assert!(names.iter().any(|n| n == "Outer::Inner"), "{names:?}");
        assert!(names.iter().any(|n| n == "Outer::Guard"), "{names:?}");
    }

    #[test]
    fn members_of_a_nested_type_are_qualified_by_it() {
        let names = names_of(NESTED);
        assert!(names.iter().any(|n| n == "Inner::innerHidden"), "{names:?}");
        assert!(names.iter().any(|n| n == "Guard::guarded"), "{names:?}");
    }

    #[test]
    fn a_nested_type_applies_its_own_access_default_to_its_members() {
        // `Guard` is a class, so `guarded` is private even though the enclosing
        // struct section it sits in is not.
        assert_eq!(visibility_of(NESTED, "Guard::guarded"), Visibility::Private);
        assert_eq!(
            visibility_of(NESTED, "Inner::innerHidden"),
            Visibility::Private
        );
    }

    #[test]
    fn the_enclosing_class_resumes_its_own_section_after_a_nested_type() {
        // `Inner` sits in Outer's default public section; `Guard` and
        // `outerHidden` follow a `private:` that the nested bodies must not reset.
        assert_eq!(visibility_of(NESTED, "Outer::Inner"), Visibility::Public);
        assert_eq!(visibility_of(NESTED, "Outer::Guard"), Visibility::Private);
        assert_eq!(
            visibility_of(NESTED, "Outer::outerHidden"),
            Visibility::Private
        );
    }

    #[test]
    fn a_nested_enum_or_union_produces_a_symbol() {
        let names = names_of("struct Outer { enum Tag { A }; union Bits { int i; }; };\n");
        assert!(names.iter().any(|n| n == "Outer::Tag"), "{names:?}");
        assert!(names.iter().any(|n| n == "Outer::Bits"), "{names:?}");
    }

    #[test]
    fn a_field_whose_type_is_a_named_struct_is_still_a_field() {
        // `struct Point p;` names no new type — recursing into the specifier
        // would invent a duplicate `Point` and lose the field.
        let names = names_of("struct Outer { struct Point p; };\n");
        assert!(names.iter().any(|n| n == "Outer::p"), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("::Point")), "{names:?}");
    }

    #[test]
    fn a_nested_type_declared_with_a_variable_yields_both() {
        let names = names_of("struct Outer { struct Inner { int q; } inner; };\n");
        assert!(names.iter().any(|n| n == "Outer::Inner"), "{names:?}");
        assert!(names.iter().any(|n| n == "Outer::inner"), "{names:?}");
    }

    #[test]
    fn a_free_function_stays_public() {
        assert_eq!(
            visibility_of("void standalone() {}\n", "standalone"),
            Visibility::Public
        );
    }

    #[test]
    fn test_parse_class_with_methods() {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
#include <string>

/** A rectangle class. */
class Rectangle {
public:
    int width;
    int height;

    int area() { return width * height; }
private:
    void reset() { width = 0; height = 0; }
};

void standalone() {}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Rectangle" && s.kind == SymbolKind::Class)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Rectangle::area" && s.kind == SymbolKind::Method)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "standalone" && s.kind == SymbolKind::Function)
        );
    }

    #[test]
    fn test_parse_namespace() {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
namespace mylib {
    void helper() {}
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let func = symbols
            .iter()
            .find(|s| s.name.as_ref() == "helper")
            .unwrap();
        assert_eq!(func.module_path.as_deref(), Some("mylib"));
    }

    #[test]
    fn test_find_inheritance() {
        let mut parser = CppParser::new().unwrap();

        let code = r#"
class Base {};
class Derived : public Base {};
"#;

        let impls = parser.find_implementations(code);
        assert!(
            impls
                .iter()
                .any(|(cls, base, _)| *cls == "Derived" && *base == "Base")
        );
    }

    #[test]
    fn test_find_includes() {
        let mut parser = CppParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
#include <iostream>
#include "mylib.h"
"#;

        let imports = parser.find_imports(code, file_id);
        assert!(imports.iter().any(|i| i.path == "iostream"));
        assert!(imports.iter().any(|i| i.path == "mylib.h"));
    }
}
