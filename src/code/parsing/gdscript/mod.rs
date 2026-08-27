//! GDScript language parser implementation using tree-sitter-gdscript 6.1.

use crate::code::parsing::caching_parser::CachingParser;
use crate::code::parsing::context::{ParserContext, ScopeType};
use crate::code::parsing::import::Import;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::{
    LanguageParser, check_recursion_depth, is_plain_path, node_range,
};
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
                        (
                            Some(format!("class {n}")),
                            doc,
                            module_path,
                            Visibility::Public,
                        ),
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
                            (module_path, depth + 1),
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
                        (
                            Some(format!("func {name}{params}")),
                            doc,
                            module_path,
                            visibility,
                        ),
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
                            (
                                Some(code[node.byte_range()].trim().to_string()),
                                extract_gdscript_doc(&node, code),
                                module_path,
                                visibility,
                            ),
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
                        (
                            Some(format!("enum {name}")),
                            extract_gdscript_doc(&node, code),
                            module_path,
                            Visibility::Public,
                        ),
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
                            SymbolKind::Variable,
                            // signals as variables
                            file_id,
                            node_range(node),
                            (
                                Some(format!("signal {name}")),
                                extract_gdscript_doc(&node, code),
                                module_path,
                                Visibility::Public,
                            ),
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
                        (module_path, depth + 1),
                    );
                }
            }
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

    fn find_extends_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let root = tree.root_node();
        // A script is itself a class: its `extends` sits at the top level and the
        // derived side is whatever names the file. `class_name` is optional and
        // most scripts omit it, so fall back to the per-file `<module>` symbol
        // rather than dropping the base class of the whole project.
        let script_name = extract_gdscript_class_name_ref(root, code).unwrap_or("<module>");

        let mut extends = Vec::new();
        Self::find_extends_in_node(root, code, script_name, 0, &mut extends);
        extends
    }

    fn find_extends_in_node<'a>(
        node: Node,
        code: &'a str,
        derived: &'a str,
        depth: usize,
        extends: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        // An inner class renames the derived side for everything below it.
        let derived = if node.kind() == "class_definition" {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .unwrap_or(derived)
        } else {
            derived
        };

        if node.kind() == "extends_statement" {
            // `extends "res://base.gd"` names a file, not a symbol; only the
            // `type` form can resolve against the index.
            if let Some(base) = node.children(&mut node.walk()).find(|c| c.kind() == "type") {
                extends.push((derived, &code[base.byte_range()], node_range(base)));
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_extends_in_node(child, code, derived, depth + 1, extends);
        }
    }

    fn find_uses_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let root = tree.root_node();
        let script_name = extract_gdscript_class_name_ref(root, code).unwrap_or("<module>");
        let mut uses = Vec::new();
        Self::find_uses_in_node(root, code, script_name, 0, &mut uses);
        uses
    }

    fn find_uses_in_node<'a>(
        node: Node,
        code: &'a str,
        context: &'a str,
        depth: usize,
        uses: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        // The innermost named thing owns the types written inside it, the same
        // rule `find_calls` uses for the caller side.
        let context = match node.kind() {
            "class_definition" | "function_definition" => node
                .child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .unwrap_or(context),
            _ => context,
        };

        match node.kind() {
            "variable_statement" | "typed_parameter" => {
                push_gdscript_type(node.child_by_field_name("type"), context, code, uses);
            }
            "function_definition" => {
                push_gdscript_type(node.child_by_field_name("return_type"), context, code, uses);
            }
            _ => {}
        }

        for child in node.children(&mut node.walk()) {
            Self::find_uses_in_node(child, code, context, depth + 1, uses);
        }
    }

    fn find_defines_impl<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let root = tree.root_node();
        let script_name = extract_gdscript_class_name_ref(root, code).unwrap_or("<module>");
        let mut defines = Vec::new();
        Self::find_defines_in_node(root, code, script_name, 0, &mut defines);
        defines
    }

    fn find_defines_in_node<'a>(
        node: Node,
        code: &'a str,
        owner: &'a str,
        depth: usize,
        defines: &mut Vec<(&'a str, &'a str, Range)>,
    ) {
        if !check_recursion_depth(depth, node) {
            return;
        }

        let owner = if node.kind() == "class_definition" {
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .unwrap_or(owner)
        } else {
            owner
        };

        if node.kind() == "function_definition" {
            if let Some(name) = node.child_by_field_name("name") {
                defines.push((owner, &code[name.byte_range()], node_range(node)));
            }
        }

        for child in node.children(&mut node.walk()) {
            Self::find_defines_in_node(child, code, owner, depth + 1, defines);
        }
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
            node.child_by_field_name("name")
                .map(|n| &code[n.byte_range()])
                .or(current_fn)
        } else {
            current_fn
        };

        // A bare `helper()` is a `call`; anything reached through a dot —
        // `$Node.play()`, `super._ready()`, the tail of `get_tree().timer()` —
        // is an `attribute_call` under an `attribute`. Both hold the callee as
        // their first child, and GDScript is written mostly in the dotted form.
        if matches!(node.kind(), "call" | "attribute_call") {
            if let Some(func) = node.children(&mut node.walk()).next() {
                // The receiver is not inside the `attribute_call`: it is an
                // earlier child of the enclosing `attribute`, so the path is
                // read from where that starts. `$Node.play` keeps `$Node` as
                // its qualifier; `get_tree().timer` is not a name and keeps
                // only `timer`.
                let whole = node
                    .parent()
                    .filter(|p| p.kind() == "attribute")
                    .map(|p| &code[p.start_byte()..func.end_byte()])
                    .filter(|text| is_plain_path(text));
                let target = whole.unwrap_or(&code[func.byte_range()]);
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

/// Record an annotated type as a use, unless it is a Variant primitive.
///
/// `int`, `float`, `bool` and `void` are built into the language and are never
/// indexed as symbols, so an edge to them can only ever be noise. Everything
/// else — including `Vector2` and `String` — is a real Godot class.
fn push_gdscript_type<'a>(
    type_node: Option<Node>,
    context: &'a str,
    code: &'a str,
    uses: &mut Vec<(&'a str, &'a str, Range)>,
) {
    let Some(type_node) = type_node else {
        return;
    };
    let name = &code[type_node.byte_range()];
    if matches!(name, "int" | "float" | "bool" | "void") {
        return;
    }
    uses.push((context, name, node_range(type_node)));
}

fn extract_gdscript_class_name(root: Node, code: &str) -> Option<String> {
    extract_gdscript_class_name_ref(root, code).map(str::to_string)
}

fn extract_gdscript_class_name_ref<'a>(root: Node, code: &'a str) -> Option<&'a str> {
    for child in root.children(&mut root.walk()) {
        if child.kind() == "class_name_statement" {
            for gc in child.children(&mut child.walk()) {
                if gc.kind() == "name" || gc.kind() == "identifier" {
                    return Some(&code[gc.byte_range()]);
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
        // `@rpc("any_peer")` and friends sit between the `##` block and the
        // declaration as top-level siblings, so stopping at the first non-
        // comment loses the documentation of every annotated declaration.
        if sib.kind() == "annotation" {
            prev = sib.prev_sibling();
            continue;
        }
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
        Vec::new() // GDScript has no interfaces: a script only ever `extends`.
    }

    fn find_extends<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_extends_impl(code)
    }

    fn find_uses<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_uses_impl(code)
    }

    fn find_defines<'a>(&mut self, code: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        self.find_defines_impl(code)
    }

    fn find_imports(&mut self, _code: &str, _file_id: FileId) -> Vec<Import> {
        Vec::new() // GDScript uses preload/load which are function calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only classes and functions could carry documentation, and an annotation
    /// between the `##` block and the declaration stopped even those: the
    /// sibling walk broke on the first non-comment it met.
    #[test]
    fn every_documented_declaration_keeps_its_doc_comment() {
        let mut parser = GdscriptParser::new().unwrap();
        let code = "## Doc for rpc.\n\
                    @rpc(\"any_peer\")\n\
                    func handler():\n\
                    \tpass\n\
                    \n\
                    ## Doc for var.\n\
                    var speed := 1.0\n\
                    \n\
                    ## Doc for const.\n\
                    const MAX = 10\n\
                    \n\
                    ## Doc for signal.\n\
                    signal died\n\
                    \n\
                    ## Doc for enum.\n\
                    enum State { IDLE, RUN }\n\
                    \n\
                    # Not a doc comment.\n\
                    var plain := 2\n";
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, FileId::new(1).unwrap(), &mut counter);
        let doc = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| panic!("{name} not parsed"))
                .doc_comment
                .as_ref()
                .map(std::string::ToString::to_string)
        };

        assert_eq!(doc("handler"), Some("Doc for rpc.".to_string()));
        assert_eq!(doc("speed"), Some("Doc for var.".to_string()));
        assert_eq!(doc("MAX"), Some("Doc for const.".to_string()));
        assert_eq!(doc("died"), Some("Doc for signal.".to_string()));
        assert_eq!(doc("State"), Some("Doc for enum.".to_string()));
        // A single `#` is a comment, not documentation.
        assert_eq!(doc("plain"), None);
    }

    const CALLS: &str = "\
extends Node

func _ready():
\thelper()
\t$Sprite2D.play()
\tget_tree().create_timer(1.0)
\tsuper._ready()
\temit_signal(\"died\")

func helper():
\tpass
";

    /// Call targets recorded for `caller`.
    fn targets_of(code: &str, caller: &str) -> Vec<String> {
        let mut parser = GdscriptParser::new().unwrap();
        parser
            .find_calls_impl(code)
            .iter()
            .filter(|(c, _, _)| *c == caller)
            .map(|(_, target, _)| (*target).to_string())
            .collect()
    }

    #[test]
    fn a_call_on_a_node_path_keeps_the_node_it_named() {
        let targets = targets_of(CALLS, "_ready");
        assert!(
            targets.iter().any(|t| t == "$Sprite2D.play"),
            "expected $Sprite2D.play, got {targets:?}"
        );
    }

    #[test]
    fn both_halves_of_a_chained_call_are_recorded() {
        let targets = targets_of(CALLS, "_ready");
        assert!(
            targets.iter().any(|t| t == "get_tree"),
            "expected get_tree, got {targets:?}"
        );
        assert!(
            targets.iter().any(|t| t == "create_timer"),
            "expected create_timer, got {targets:?}"
        );
    }

    #[test]
    fn a_super_call_is_recorded() {
        let targets = targets_of(CALLS, "_ready");
        assert!(
            targets.iter().any(|t| t == "super._ready"),
            "expected the super call to _ready, got {targets:?}"
        );
    }

    #[test]
    fn a_call_on_a_computed_receiver_keeps_only_the_method_name() {
        // `get_tree()` is a call, not a name: there is nothing to qualify with.
        let targets = targets_of(CALLS, "_ready");
        assert!(
            targets.iter().any(|t| t == "create_timer"),
            "expected a bare create_timer, got {targets:?}"
        );
    }

    #[test]
    fn bare_calls_are_still_recorded() {
        let targets = targets_of(CALLS, "_ready");
        assert!(targets.iter().any(|t| t == "helper"), "{targets:?}");
        assert!(targets.iter().any(|t| t == "emit_signal"), "{targets:?}");
    }

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

    /// Type annotations are the only place a Godot script names another class
    /// outside its `extends` line, and every `func` it declares is part of its
    /// contract.
    #[test]
    fn typed_declarations_and_functions_are_recorded() {
        let mut parser = GdscriptParser::new().unwrap();

        let code = "extends Node2D\n\
                    class_name Player\n\
                    \n\
                    var speed: float = 1.0\n\
                    @export var target: Enemy\n\
                    \n\
                    func hit(other: Enemy, n: int) -> Damage:\n\
                    \tpass\n\
                    \n\
                    class Inner extends Resource:\n\
                    \tvar pos: Vector2\n\
                    \tfunc go() -> void:\n\
                    \t\tpass\n";

        let uses: Vec<(&str, &str)> = parser
            .find_uses(code)
            .iter()
            .map(|(c, t, _)| (*c, *t))
            .collect();

        assert!(
            uses.contains(&("Player", "Enemy")),
            "script property: {uses:?}"
        );
        assert!(uses.contains(&("hit", "Enemy")), "parameter: {uses:?}");
        assert!(uses.contains(&("hit", "Damage")), "return type: {uses:?}");
        assert!(
            uses.contains(&("Inner", "Vector2")),
            "inner class property: {uses:?}"
        );
        // Variant primitives are built into the language and never indexed.
        assert!(
            !uses
                .iter()
                .any(|(_, t)| matches!(*t, "int" | "float" | "void")),
            "a primitive is not a used class: {uses:?}"
        );

        let defines: Vec<(&str, &str)> = parser
            .find_defines(code)
            .iter()
            .map(|(c, t, _)| (*c, *t))
            .collect();
        assert!(
            defines.contains(&("Player", "hit")),
            "script method: {defines:?}"
        );
        assert!(
            defines.contains(&("Inner", "go")),
            "inner class method: {defines:?}"
        );
    }

    /// `extends "res://base.gd"` names a file. There is no symbol to point at,
    /// so recording the path as a base class would only create a dead edge.
    #[test]
    fn extends_by_script_path_records_nothing() {
        let mut parser = GdscriptParser::new().unwrap();
        assert!(
            parser
                .find_extends("extends \"res://base.gd\"\nclass_name Player\n")
                .is_empty()
        );
    }
}
