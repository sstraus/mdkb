//! Lua language parser implementation using tree-sitter-lua 0.4.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::ParserContext;
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth, node_range};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct LuaParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for LuaParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaParser")
            .field("language", &"Lua")
            .finish()
    }
}

impl LuaParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_lua::LANGUAGE.into())
            .map_err(|e| format!("Failed to set Lua language: {e}"))?;

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
            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_lua_doc(&node, code);
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    // Method-style: M:foo() or M.foo()
                    let (kind, qualified_name) = if name.contains(':') || name.contains('.') {
                        (SymbolKind::Method, name.to_string())
                    } else {
                        (SymbolKind::Function, name.to_string())
                    };

                    // `local function f()` is a plain function_declaration
                    // carrying a `local` token — the grammar has no separate
                    // node kind for it, so lexical scope is the only signal
                    // that the function is private.
                    let is_local = has_child_of_kind(node, "local");
                    let visibility = if is_local || name.starts_with('_') {
                        Visibility::Private
                    } else {
                        Visibility::Public
                    };
                    let prefix = if is_local { "local function" } else { "function" };

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        kind,
                        file_id,
                        node_range(node),
                        (
                            Some(format!("{prefix} {name}{params}")),
                            doc,
                            module_path,
                            visibility,
                        ),
                    );
                    symbols.push(symbol);
                }
            }

            "variable_declaration" | "assignment_statement" => {
                // `local x = 1` is a variable_declaration wrapping an
                // assignment_statement; a bare `y = 1` is that assignment on its
                // own. Either way the names live in a variable_list one level
                // below the assignment, never as its direct children.
                let is_local = node.kind() == "variable_declaration";
                let owner = if is_local {
                    child_of_kind(node, "assignment_statement").unwrap_or(node)
                } else {
                    node
                };
                self.extract_assigned_symbols(
                    owner,
                    code,
                    file_id,
                    counter,
                    symbols,
                    (module_path, is_local),
                );
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

    /// Emit a Variable symbol per assigned name, plus a symbol for any function
    /// stored in a table literal on the right-hand side.
    ///
    /// Lua assigns positionally (`a, b = f, g`), so name *i* pairs with value
    /// *i*.
    fn extract_assigned_symbols(
        &mut self,
        owner: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, bool),
    ) {
        let (module_path, is_local) = tail;
        let names = children_of_kind(child_of_kind(owner, "variable_list"), "identifier");
        let values = child_of_kind(owner, "expression_list")
            .map(|list| list.named_children(&mut list.walk()).collect::<Vec<_>>())
            .unwrap_or_default();

        for (i, name_node) in names.iter().enumerate() {
            let name = &code[name_node.byte_range()];
            let visibility = if is_local || name.starts_with('_') {
                Visibility::Private
            } else {
                Visibility::Public
            };

            symbols.push(self.create_symbol(
                counter.next_id(),
                name.to_string(),
                SymbolKind::Variable,
                file_id,
                node_range(owner),
                (None, None, module_path, visibility),
            ));

            if let Some(value) = values.get(i) {
                self.extract_table_functions(
                    *value,
                    code,
                    file_id,
                    counter,
                    symbols,
                    (name, module_path, visibility),
                );
            }
        }
    }

    /// Emit a symbol for every function held in a table literal, named
    /// `<table>.<field>` so it reads like the `M.foo` form already recorded as a
    /// method.
    fn extract_table_functions(
        &mut self,
        value: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, &str, Visibility),
    ) {
        let (table, module_path, visibility) = tail;
        if value.kind() != "table_constructor" {
            return;
        }
        for field in value.children(&mut value.walk()) {
            if field.kind() != "field" {
                continue;
            }
            let (Some(name_node), Some(function)) = (
                field.child_by_field_name("name"),
                child_of_kind(field, "function_definition"),
            ) else {
                continue;
            };
            let params = function
                .child_by_field_name("parameters")
                .map(|n| &code[n.byte_range()])
                .unwrap_or("()");
            let name = format!("{table}.{}", &code[name_node.byte_range()]);

            symbols.push(self.create_symbol(
                counter.next_id(),
                name,
                SymbolKind::Method,
                file_id,
                node_range(field),
                (
                    Some(format!("function{params}")),
                    extract_lua_doc(&field, code),
                    module_path,
                    visibility,
                ),
            ));
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
        let fn_ctx = match node.kind() {
            "function_declaration" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn),
            _ => current_fn,
        };

        if node.kind() == "function_call" {
            if let Some(name_node) = node.child_by_field_name("name") {
                let target = &code[name_node.byte_range()];
                if let Some(ctx) = fn_ctx {
                    calls.push((ctx, target, node_range(*node)));
                }
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_calls_in_node(&child, code, fn_ctx, depth + 1, calls);
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

/// The first child of `node` with the given kind.
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    node.children(&mut node.walk()).find(|c| c.kind() == kind)
}

/// Whether `node` has a direct child of the given kind.
fn has_child_of_kind(node: Node, kind: &str) -> bool {
    child_of_kind(node, kind).is_some()
}

/// Every child of `parent` with the given kind, or an empty list when `parent`
/// is absent.
fn children_of_kind<'a>(parent: Option<Node<'a>>, kind: &str) -> Vec<Node<'a>> {
    parent
        .map(|p| {
            p.children(&mut p.walk())
                .filter(|c| c.kind() == kind)
                .collect()
        })
        .unwrap_or_default()
}

fn extract_lua_doc(node: &Node, code: &str) -> Option<String> {
    // Lua uses --- doc comments (LDoc/EmmyLua style)
    let mut lines = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(sib) = prev {
        if sib.kind() == "comment" {
            let text = &code[sib.byte_range()];
            if text.starts_with("---") {
                let content = text.trim_start_matches("---").trim();
                if !content.is_empty() && !content.starts_with('@') {
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

impl LanguageParser for LuaParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Lua
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_lua_doc(node, code)
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_implementations<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    fn find_uses<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    fn find_defines<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }

    fn find_imports(&mut self, _code: &str, _file_id: FileId) -> Vec<Import> {
        Vec::new() // Lua uses require() which is a function call, not an import statement
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_functions() {
        let mut parser = LuaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
--- Calculate the sum.
function add(a, b)
    return a + b
end

local function _helper()
end

function MyModule:update(dt)
end
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "add"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "_helper"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Private));
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "MyModule:update" && s.kind == SymbolKind::Method)
        );
    }

    /// Parse `code` and return the symbols it yields.
    fn symbols_of(code: &str) -> Vec<Symbol> {
        let mut parser = LuaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        parser.parse_symbols(code, file_id, &mut counter)
    }

    #[test]
    fn a_local_variable_produces_a_symbol() {
        let symbols = symbols_of("local x = 5\n");
        assert!(
            symbols.iter().any(|s| s.name.as_ref() == "x"
                && s.kind == SymbolKind::Variable
                && s.visibility == Visibility::Private),
            "expected a private variable x, got {:?}",
            symbols.iter().map(|s| s.as_name()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_global_assignment_produces_a_symbol() {
        let symbols = symbols_of("y = 10\n");
        assert!(
            symbols.iter().any(|s| s.name.as_ref() == "y"
                && s.kind == SymbolKind::Variable
                && s.visibility == Visibility::Public),
            "expected a public variable y, got {:?}",
            symbols.iter().map(|s| s.as_name()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_name_in_a_multiple_assignment_produces_a_symbol() {
        let symbols = symbols_of("local a, b = 1, 2\n");
        assert!(symbols.iter().any(|s| s.name.as_ref() == "a"));
        assert!(
            symbols.iter().any(|s| s.name.as_ref() == "b"),
            "second name dropped, got {:?}",
            symbols.iter().map(|s| s.as_name()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_function_stored_in_a_table_literal_produces_a_symbol() {
        let symbols = symbols_of("local t = { fn = function() end }\n");
        assert!(
            symbols.iter().any(|s| s.name.as_ref() == "t.fn"),
            "expected t.fn, got {:?}",
            symbols.iter().map(|s| s.as_name()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_local_function_is_private_and_says_so_in_its_signature() {
        let symbols = symbols_of("local function priv() end\n");
        let priv_fn = symbols
            .iter()
            .find(|s| s.name.as_ref() == "priv")
            .expect("expected a symbol for priv");
        assert_eq!(priv_fn.visibility, Visibility::Private);
        assert_eq!(priv_fn.as_signature(), Some("local function priv()"));
    }

    #[test]
    fn a_global_function_stays_public() {
        let symbols = symbols_of("function pub() end\n");
        let pub_fn = symbols
            .iter()
            .find(|s| s.name.as_ref() == "pub")
            .expect("expected a symbol for pub");
        assert_eq!(pub_fn.visibility, Visibility::Public);
        assert_eq!(pub_fn.as_signature(), Some("function pub()"));
    }

    #[test]
    fn test_doc_comment_extraction() {
        let mut parser = LuaParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
--- Compute the product.
function multiply(a, b)
    return a * b
end
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let func = symbols
            .iter()
            .find(|s| s.name.as_ref() == "multiply")
            .expect("should find multiply");
        assert!(
            func.doc_comment
                .as_deref()
                .unwrap()
                .contains("Compute the product")
        );
    }
}
