//! Lua language parser implementation using tree-sitter-lua 0.4.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::ParserContext;
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
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
        self.extract_symbols_from_node(
            tree.root_node(),
            code,
            file_id,
            counter,
            &mut symbols,
            "",
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

                    let visibility = if name.starts_with('_') {
                        Visibility::Private
                    } else {
                        Visibility::Public
                    };

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        qualified_name,
                        kind,
                        file_id,
                        node_range(node),
                        Some(format!("function {name}{params}")),
                        doc,
                        module_path,
                        visibility,
                    );
                    symbols.push(symbol);
                }
            }

            "local_function" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_lua_doc(&node, code);
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Function,
                        file_id,
                        node_range(node),
                        Some(format!("local function {name}{params}")),
                        doc,
                        module_path,
                        Visibility::Private,
                    );
                    symbols.push(symbol);
                }
            }

            "variable_declaration" => {
                // local x = ... or local x, y = ...
                let is_local = code[node.byte_range()].starts_with("local ");
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "assignment_statement" || child.kind() == "variable_list" {
                        for gc in child.children(&mut child.walk()) {
                            if gc.kind() == "identifier" {
                                let name = &code[gc.byte_range()];
                                let visibility = if is_local || name.starts_with('_') {
                                    Visibility::Private
                                } else {
                                    Visibility::Public
                                };
                                let symbol = self.create_symbol(
                                    counter.next_id(),
                                    name.to_string(),
                                    SymbolKind::Variable,
                                    file_id,
                                    node_range(node),
                                    None,
                                    None,
                                    module_path,
                                    visibility,
                                );
                                symbols.push(symbol);
                                break; // Only first identifier in declaration
                            }
                        }
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
            "function_declaration" | "local_function" => node
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
            self.find_calls_in_node(&child, code, fn_ctx, calls);
        }
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
