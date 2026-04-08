//! GDScript language parser implementation using tree-sitter-gdscript 6.1.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{LanguageParser, check_recursion_depth};
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct GdscriptParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for GdscriptParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GdscriptParser")
            .field("language", &"GDScript")
            .finish()
    }
}

impl GdscriptParser {
    pub fn new() -> Result<Self, String> {
        let mut ts_parser = tree_sitter::Parser::new();
        ts_parser
            .set_language(&tree_sitter_gdscript::LANGUAGE.into())
            .map_err(|e| format!("Failed to set GDScript language: {e}"))?;

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

        // Extract class_name if present
        let class_name = extract_gdscript_class_name(tree.root_node(), code);

        let mut symbols = Vec::new();
        let module_path = class_name.as_deref().unwrap_or("");

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
            "class_definition" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|n| code[n.byte_range()].to_string());

                if let Some(ref n) = name {
                    let doc = extract_gdscript_doc(&node, code);
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        n.clone(),
                        SymbolKind::Class,
                        file_id,
                        node_range(node),
                        Some(format!("class {n}")),
                        doc,
                        module_path,
                        Visibility::Public,
                    );
                    symbols.push(symbol);
                }

                self.context.enter_scope(ScopeType::Class);
                let saved_cls = self.context.current_class().map(|s| s.to_string());
                self.context.set_current_class(name);

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

            "function_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let doc = extract_gdscript_doc(&node, code);
                    let params = node
                        .child_by_field_name("parameters")
                        .map(|n| &code[n.byte_range()])
                        .unwrap_or("()");

                    let visibility = if name.starts_with('_') {
                        Visibility::Private
                    } else {
                        Visibility::Public
                    };

                    let kind = if self.context.is_in_class() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
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
                        node_range(node),
                        Some(format!("func {name}{params}")),
                        doc,
                        module_path,
                        visibility,
                    );
                    symbols.push(symbol);
                }
            }

            "variable_statement" | "const_statement" => {
                let is_const = node.kind() == "const_statement";
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "name" || child.kind() == "identifier" {
                        let name = &code[child.byte_range()];
                        let visibility = if name.starts_with('_') {
                            Visibility::Private
                        } else {
                            Visibility::Public
                        };
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
                            Some(code[node.byte_range()].trim().to_string()),
                            None,
                            module_path,
                            visibility,
                        );
                        symbols.push(symbol);
                        break;
                    }
                }
            }

            "enum_definition" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = &code[name_node.byte_range()];
                    let symbol = self.create_symbol(
                        counter.next_id(),
                        name.to_string(),
                        SymbolKind::Enum,
                        file_id,
                        node_range(node),
                        Some(format!("enum {name}")),
                        None,
                        module_path,
                        Visibility::Public,
                    );
                    symbols.push(symbol);
                }
            }

            "signal_statement" => {
                for child in node.children(&mut node.walk()) {
                    if child.kind() == "name" || child.kind() == "identifier" {
                        let name = &code[child.byte_range()];
                        let symbol = self.create_symbol(
                            counter.next_id(),
                            name.to_string(),
                            SymbolKind::Variable, // signals as variables
                            file_id,
                            node_range(node),
                            Some(format!("signal {name}")),
                            None,
                            module_path,
                            Visibility::Public,
                        );
                        symbols.push(symbol);
                        break;
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
        let fn_ctx = if node.kind() == "function_definition" {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn)
        } else {
            current_fn
        };

        if node.kind() == "call" {
            if let Some(func) = node.children(&mut node.walk()).next() {
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

fn extract_gdscript_class_name(root: Node, code: &str) -> Option<String> {
    for child in root.children(&mut root.walk()) {
        if child.kind() == "class_name_statement" {
            for gc in child.children(&mut child.walk()) {
                if gc.kind() == "name" || gc.kind() == "identifier" {
                    return Some(code[gc.byte_range()].to_string());
                }
            }
        }
    }
    None
}

fn extract_gdscript_doc(node: &Node, code: &str) -> Option<String> {
    // GDScript uses ## for doc comments
    let mut lines = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(sib) = prev {
        if sib.kind() == "comment" {
            let text = &code[sib.byte_range()];
            if text.starts_with("##") {
                let content = text.trim_start_matches("##").trim();
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

impl LanguageParser for GdscriptParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        self.parse_symbols(code, file_id, counter)
    }

    fn language(&self) -> Language {
        Language::Gdscript
    }

    fn extract_doc_comment(&self, node: &Node, code: &str) -> Option<String> {
        extract_gdscript_doc(node, code)
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
        Vec::new() // GDScript uses preload/load which are function calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_functions() {
        let mut parser = GdscriptParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
extends Node

func _ready():
    pass

func process(delta):
    pass

func _private_helper():
    pass
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(symbols.iter().any(|s| s.name.as_ref() == "_ready"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Private));
        assert!(symbols.iter().any(|s| s.name.as_ref() == "process"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Public));
    }

    #[test]
    fn test_parse_inner_class() {
        let mut parser = GdscriptParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let code = r#"
class_name MyNode

class InnerClass:
    func inner_method():
        pass

func outer_func():
    pass
"#;

        let symbols = parser.parse_symbols(code, file_id, &mut counter);

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "InnerClass" && s.kind == SymbolKind::Class)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "outer_func" && s.kind == SymbolKind::Function)
        );
    }
}
