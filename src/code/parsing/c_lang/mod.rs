//! C language parser implementation using tree-sitter-c 0.24.

use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;
use crate::code::parsing::caching_parser::CachingParser;

pub struct CParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for CParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CParser")
            .field("language", &"C")
            .finish()
    }
}

impl CParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_c::LANGUAGE.into())
            .map_err(|e| format!("Failed to set C language: {e}"))?;

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
                                child, code, file_id, counter, symbols, module_path, depth + 1,
                            );
                        }
                    }

                    self.context.exit_scope();
                }
            }

            "struct_specifier" => {
                if let Some(symbol) =
                    self.process_struct(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
            }

            "enum_specifier" => {
                if let Some(symbol) =
                    self.process_enum(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
            }

            "type_definition" => {
                if let Some(symbol) =
                    self.process_typedef(node, code, file_id, counter, module_path)
                {
                    symbols.push(symbol);
                }
            }

            "declaration" => {
                // Top-level variable/function declarations
                if self.context.is_module_level() {
                    self.process_declaration(node, code, file_id, counter, symbols, module_path);
                }
            }

            _ => {
                for child in node.children(&mut node.walk()) {
                    self.extract_symbols_from_node(
                        child, code, file_id, counter, symbols, module_path, depth + 1,
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
        let name = extract_declarator_name(declarator, code)?;
        let doc = extract_c_doc(&node, code);

        // Build signature from return type + declarator
        let type_node = node.child_by_field_name("type");
        let sig = match type_node {
            Some(t) => format!("{} {}", &code[t.byte_range()], &code[declarator.byte_range()]),
            None => code[declarator.byte_range()].to_string(),
        };

        let visibility = if name.starts_with('_') {
            Visibility::Private
        } else {
            Visibility::Public
        };

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Function,
            file_id,
            node_range(node),
            Some(sig),
            doc,
            module_path,
            visibility,
        ))
    }

    fn process_struct(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let doc = extract_c_doc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Struct,
            file_id,
            node_range(node),
            Some(format!("struct {name}")),
            doc,
            module_path,
            Visibility::Public,
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
        let doc = extract_c_doc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Enum,
            file_id,
            node_range(node),
            Some(format!("enum {name}")),
            doc,
            module_path,
            Visibility::Public,
        ))
    }

    fn process_typedef(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        // typedef has the alias name as the last identifier child before ';'
        let declarator = node.child_by_field_name("declarator")?;
        let name = extract_declarator_name(declarator, code)?;
        let doc = extract_c_doc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::TypeAlias,
            file_id,
            node_range(node),
            Some(code[node.byte_range()].lines().next().unwrap_or("").to_string()),
            doc,
            module_path,
            Visibility::Public,
        ))
    }

    fn process_declaration(
        &mut self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        // Look for variable declarators
        for child in node.children(&mut node.walk()) {
            if child.kind() == "init_declarator" || child.kind() == "identifier" {
                let name = if child.kind() == "identifier" {
                    &code[child.byte_range()]
                } else if let Some(decl) = child.child_by_field_name("declarator") {
                    if let Some(n) = extract_declarator_name(decl, code) {
                        n
                    } else {
                        continue;
                    }
                } else {
                    continue;
                };

                let is_const = code[node.byte_range()].contains("const ");
                let kind = if is_const {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                };

                let symbol = self.create_symbol(
                    counter.next_id(),
                    name.to_string(),
                    kind,
                    file_id,
                    node_range(node),
                    Some(code[node.byte_range()].trim_end_matches(';').trim().to_string()),
                    None,
                    module_path,
                    Visibility::Public,
                );
                symbols.push(symbol);
            }
        }
    }

    fn extract_imports_impl(&mut self, code: &str, file_id: FileId) -> Vec<Import> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut imports = Vec::new();
        for child in tree.root_node().children(&mut tree.root_node().walk()) {
            if child.kind() == "preproc_include" {
                if let Some(path_node) = child.child_by_field_name("path") {
                    let raw = &code[path_node.byte_range()];
                    let path = raw
                        .trim_start_matches(|c| c == '"' || c == '<')
                        .trim_end_matches(|c| c == '"' || c == '>')
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

    fn find_calls_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let tree = match self.parser.parse_cached(code) {
            Some(tree) => tree,
            None => return Vec::new(),
        };
        let mut calls = Vec::new();
        self.find_calls_in_node(&tree.root_node(), code, None, &mut calls);
        calls
    }

    fn find_calls_in_node<'a>(
        &self,
        node: &Node,
        code: &'a str,
        current_fn: Option<&'a str>,
        calls: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        let fn_ctx = if node.kind() == "function_definition" {
            node.child_by_field_name("declarator")
                .and_then(|d| extract_declarator_name(d, code))
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

/// Extract the name from a C declarator (handles pointer declarators, function declarators, etc).
fn extract_declarator_name<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        "identifier" => Some(&code[node.byte_range()]),
        "pointer_declarator" | "array_declarator" | "parenthesized_declarator" => {
            node.child_by_field_name("declarator")
                .and_then(|d| extract_declarator_name(d, code))
        }
        "function_declarator" => {
            node.child_by_field_name("declarator")
                .and_then(|d| extract_declarator_name(d, code))
        }
        _ => None,
    }
}

/// Extract doc comment (/** ... */ or /// style) from preceding sibling.
fn extract_c_doc(node: &Node, code: &str) -> Option<String> {
    let sibling = node.prev_sibling()?;
    if sibling.kind() != "comment" {
        return None;
    }
    let text = &code[sibling.byte_range()];
    if text.starts_with("/**") {
        let inner = text
            .trim_start_matches("/**")
            .trim_end_matches("*/")
            .lines()
            .map(|l| l.trim().trim_start_matches('*').trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if inner.is_empty() { None } else { Some(inner) }
    } else if text.starts_with("///") {
        let inner = text.trim_start_matches("///").trim();
        if inner.is_empty() { None } else { Some(inner.to_string()) }
    } else {
        None
    }
}

impl LanguageParser for CParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::C
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_c_doc(node, code)
    }

    fn find_calls<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_calls_impl(code)
    }

    fn find_implementations<'a>(&mut self, _code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new() // C has no inheritance
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

    #[test]
    fn test_parse_functions_and_structs() {
        let mut parser = CParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
#include <stdio.h>

/** A point in 2D space. */
struct Point {
    int x;
    int y;
};

enum Color { RED, GREEN, BLUE };

/** Add two numbers. */
int add(int a, int b) {
    return a + b;
}

void _internal_helper() {}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols
            .iter()
            .any(|s| s.name.as_ref() == "Point" && s.kind == SymbolKind::Struct));
        assert!(symbols
            .iter()
            .any(|s| s.name.as_ref() == "Color" && s.kind == SymbolKind::Enum));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "add"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Public));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "_internal_helper"
            && s.visibility == Visibility::Private));
    }

    #[test]
    fn test_find_includes() {
        let mut parser = CParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();

        let code = r#"
#include <stdio.h>
#include "myheader.h"
"#;

        let imports = parser.find_imports(code, file_id);
        assert!(imports.iter().any(|i| i.path == "stdio.h"));
        assert!(imports.iter().any(|i| i.path == "myheader.h"));
    }

    #[test]
    fn test_find_calls() {
        let mut parser = CParser::new().unwrap();

        let code = r#"
void process() {}

int main() {
    process();
    printf("hello");
    return 0;
}
"#;

        let calls = parser.find_calls_impl(code);
        assert!(calls
            .iter()
            .any(|(caller, target, _)| *caller == "main" && *target == "process"));
        assert!(calls
            .iter()
            .any(|(caller, target, _)| *caller == "main" && *target == "printf"));
    }

    #[test]
    fn test_doc_comment_extraction() {
        let mut parser = CParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
/** Compute the sum. */
int sum(int a, int b) {
    return a + b;
}
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);
        let func = symbols
            .iter()
            .find(|s| s.name.as_ref() == "sum")
            .expect("should find sum");
        assert!(func
            .doc_comment
            .as_deref()
            .unwrap()
            .contains("Compute the sum"));
    }
}
