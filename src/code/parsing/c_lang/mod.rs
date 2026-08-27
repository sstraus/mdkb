//! C language parser implementation using tree-sitter-c 0.24.

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

pub struct CParser {
    parser: CachingParser,
    context: ParserContext,
}

impl std::fmt::Debug for CParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CParser").field("language", &"C").finish()
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

            "struct_specifier" | "union_specifier" => {
                if let Some(symbol) = self.process_record(node, code, file_id, counter, module_path)
                {
                    let record = symbol.name.as_ref().to_string();
                    symbols.push(symbol);
                    self.process_record_fields(
                        node,
                        code,
                        file_id,
                        counter,
                        symbols,
                        (&record, module_path),
                    );
                }
            }

            "enum_specifier" => {
                if let Some(symbol) = self.process_enum(node, code, file_id, counter, module_path) {
                    symbols.push(symbol);
                }
                self.process_enumerators(node, code, file_id, counter, symbols, module_path);
            }

            "preproc_def" | "preproc_function_def" => {
                if let Some(symbol) = self.process_macro(node, code, file_id, counter, module_path)
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
        let name = extract_declarator_name(declarator, code)?;
        let doc = extract_c_family_doc(&node, code);

        // Build signature from return type + declarator
        let type_node = node.child_by_field_name("type");
        let sig = match type_node {
            Some(t) => format!(
                "{} {}",
                &code[t.byte_range()],
                &code[declarator.byte_range()]
            ),
            None => code[declarator.byte_range()].to_string(),
        };

        let visibility = c_visibility(node, code, name);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Function,
            file_id,
            node_range(node),
            (Some(sig), doc, module_path, visibility),
        ))
    }

    /// A `struct` or `union` declaration. Both are record types with a name and
    /// a field list; the index has no Union kind, so both map to Struct.
    fn process_record(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let name = &code[name_node.byte_range()];
        let doc = extract_c_family_doc(&node, code);
        let keyword = if node.kind() == "union_specifier" {
            "union"
        } else {
            "struct"
        };

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Struct,
            file_id,
            node_range(node),
            (
                Some(format!("{keyword} {name}")),
                doc,
                module_path,
                Visibility::Public,
            ),
        ))
    }

    /// Members of a record, named `Record.field` — the spelling C uses to reach
    /// them, and the only one that tells two structs' `x` apart.
    fn process_record_fields(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        tail: (&str, &str),
    ) {
        let (record, module_path) = tail;
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        for decl in body.children(&mut body.walk()) {
            if decl.kind() != "field_declaration" {
                continue;
            }
            for field in decl.children(&mut decl.walk()) {
                if field.kind() != "field_identifier" {
                    continue;
                }
                let symbol = self.create_symbol(
                    counter.next_id(),
                    format!("{record}.{}", &code[field.byte_range()]),
                    SymbolKind::Field,
                    file_id,
                    node_range(decl),
                    (
                        Some(
                            code[decl.byte_range()]
                                .trim_end_matches(';')
                                .trim()
                                .to_string(),
                        ),
                        None,
                        module_path,
                        Visibility::Public,
                    ),
                );
                symbols.push(symbol);
            }
        }
    }

    /// Enumerators, named bare: C puts them in the enclosing scope, so `RED` is
    /// the only way source can spell one.
    fn process_enumerators(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        symbols: &mut Vec<Symbol>,
        module_path: &str,
    ) {
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        for member in body.children(&mut body.walk()) {
            if member.kind() != "enumerator" {
                continue;
            }
            let Some(name_node) = member.child_by_field_name("name") else {
                continue;
            };
            let symbol = self.create_symbol(
                counter.next_id(),
                code[name_node.byte_range()].to_string(),
                SymbolKind::Constant,
                file_id,
                node_range(member),
                (
                    Some(code[member.byte_range()].to_string()),
                    None,
                    module_path,
                    Visibility::Public,
                ),
            );
            symbols.push(symbol);
        }
    }

    /// A `#define`, object-like or function-like. Both carry the macro name in
    /// the `name` field and differ only in whether a parameter list follows.
    fn process_macro(
        &self,
        node: Node,
        code: &str,
        file_id: FileId,
        counter: &mut SymbolCounter,
        module_path: &str,
    ) -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        let doc = extract_c_family_doc(&node, code);

        Some(self.create_symbol(
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
        let doc = extract_c_family_doc(&node, code);

        Some(self.create_symbol(
            counter.next_id(),
            name.to_string(),
            SymbolKind::Enum,
            file_id,
            node_range(node),
            (
                Some(format!("enum {name}")),
                doc,
                module_path,
                Visibility::Public,
            ),
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
        let doc = extract_c_family_doc(&node, code);

        Some(
            self.create_symbol(
                counter.next_id(),
                name.to_string(),
                SymbolKind::TypeAlias,
                file_id,
                node_range(node),
                (
                    Some(
                        code[node.byte_range()]
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string(),
                    ),
                    doc,
                    module_path,
                    Visibility::Public,
                ),
            ),
        )
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
        // The declarator field names every object shape — plain, pointer,
        // array, initialised, function pointer — and a single declaration can
        // carry several of them (`int a, b;`). Scanning children by kind
        // instead reaches only the two simplest shapes.
        for declarator in node.children_by_field_name("declarator", &mut node.walk()) {
            if is_function_prototype(declarator) {
                continue;
            }
            let Some(name) = extract_declarator_name(declarator, code) else {
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
                (
                    Some(
                        code[node.byte_range()]
                            .trim_end_matches(';')
                            .trim()
                            .to_string(),
                    ),
                    None,
                    module_path,
                    c_visibility(node, code, name),
                ),
            );
            symbols.push(symbol);
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
            Self::find_calls_in_node(&child, code, fn_ctx, depth + 1, calls);
        }
    }
}

// ── Free helpers ────────────────────────────────────────────────────────

/// Extract the name from a C declarator (handles pointer declarators, function declarators, etc).
/// Is this declarator a function prototype (`int f(int);`) rather than an object?
///
/// A prototype and a function pointer both reach a `function_declarator`. They
/// part company one level below it: the pointer parenthesises its name —
/// `int (*fp)(int)` — while the prototype names it directly. The return type
/// The access a C declaration grants, from its storage class.
///
/// `static` is C's own word for "not visible outside this translation unit", so
/// it is the ground truth and outranks the naming convention. Without a storage
/// class the name is all there is to go on, and a leading underscore is the
/// convention the codebase already read that way.
fn c_visibility(node: Node, code: &str, name: &str) -> Visibility {
    let storage = node
        .children(&mut node.walk())
        .find(|c| c.kind() == "storage_class_specifier")
        .map(|c| &code[c.byte_range()]);
    match storage {
        Some("static") => Visibility::Module,
        // `extern` states the opposite explicitly; it outranks the convention
        // too, or a declared-elsewhere `_impl` would read as file-local.
        Some("extern") => Visibility::Public,
        _ if name.starts_with('_') => Visibility::Private,
        _ => Visibility::Public,
    }
}

/// can wrap either in pointers or arrays first (`int *ptr_ret(void);`), so the
/// chain has to be walked rather than inspected one level deep.
fn is_function_prototype(declarator: Node) -> bool {
    match declarator.kind() {
        "function_declarator" => declarator
            .child_by_field_name("declarator")
            .is_some_and(|d| d.kind() != "parenthesized_declarator"),
        "init_declarator" | "pointer_declarator" | "array_declarator" => declarator
            .child_by_field_name("declarator")
            .is_some_and(is_function_prototype),
        _ => false,
    }
}

fn extract_declarator_name<'a>(node: Node, code: &'a str) -> Option<&'a str> {
    match node.kind() {
        // A typedef alias arrives as `type_identifier`, an object name as
        // `identifier`. Both are the end of the declarator chain.
        "identifier" | "type_identifier" => Some(&code[node.byte_range()]),
        "init_declarator" | "pointer_declarator" | "array_declarator" | "function_declarator" => {
            node.child_by_field_name("declarator")
                .and_then(|d| extract_declarator_name(d, code))
        }
        // The grammar labels no field on `(*fp)`, so the chain has to be
        // followed by node kind. Descending by field here returns None and
        // loses every function pointer in the file.
        "parenthesized_declarator" => node
            .children(&mut node.walk())
            .find_map(|c| extract_declarator_name(c, code)),
        _ => {
            tracing::debug!(kind = node.kind(), "C declarator shape not recognised");
            None
        }
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
        extract_c_family_doc(node, code)
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

    /// `static` is internal linkage: the symbol cannot be named from another
    /// translation unit. Reporting it as Public put every file-local helper in
    /// the answer to "what does this library export".
    #[test]
    fn storage_class_decides_visibility_over_the_naming_convention() {
        let mut parser = CParser::new().unwrap();
        let code = "static int hidden(void) { return 0; }\n\
                    int visible(void) { return 0; }\n\
                    static int g_hidden = 1;\n\
                    extern int g_vis;\n\
                    int _by_convention(void) { return 0; }\n\
                    extern int _declared_elsewhere;\n";
        let mut counter = SymbolCounter::new();
        let symbols = parser.parse_symbols(code, FileId::new(1).unwrap(), &mut counter);

        let level = |name: &str| {
            symbols
                .iter()
                .find(|s| s.name.as_ref() == name)
                .unwrap_or_else(|| {
                    panic!(
                        "{name} not parsed: {:?}",
                        symbols.iter().map(Symbol::as_name).collect::<Vec<_>>()
                    )
                })
                .visibility
        };
        assert_eq!(level("hidden"), Visibility::Module, "static function");
        assert_eq!(level("visible"), Visibility::Public, "no storage class");
        assert_eq!(level("g_hidden"), Visibility::Module, "static global");
        assert_eq!(level("g_vis"), Visibility::Public, "extern global");
        // The convention still decides when the language says nothing.
        assert_eq!(level("_by_convention"), Visibility::Private);
        // ...and never outranks an explicit `extern`.
        assert_eq!(level("_declared_elsewhere"), Visibility::Public);
    }

    /// Names of the symbols `code` yields.
    fn names_of(code: &str) -> Vec<String> {
        let mut parser = CParser::new().unwrap();
        let file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();
        parser
            .parse_symbols(code, file_id, &mut counter)
            .iter()
            .map(|s| s.as_name().to_string())
            .collect()
    }

    #[test]
    fn a_function_pointer_typedef_produces_a_symbol() {
        let names = names_of("typedef void (*FuncPtr)(int);\n");
        assert!(names.iter().any(|n| n == "FuncPtr"), "{names:?}");
    }

    #[test]
    fn a_function_pointer_variable_produces_a_symbol() {
        let names = names_of("int (*global_fp)(int) = 0;\n");
        assert!(names.iter().any(|n| n == "global_fp"), "{names:?}");
    }

    #[test]
    fn a_plain_typedef_produces_a_symbol() {
        let names = names_of("typedef int MyInt;\n");
        assert!(names.iter().any(|n| n == "MyInt"), "{names:?}");
    }

    #[test]
    fn pointer_and_array_declarators_still_resolve() {
        let names = names_of("int *ptr = 0;\nint arr[10];\n");
        assert!(names.iter().any(|n| n == "ptr"), "{names:?}");
        assert!(names.iter().any(|n| n == "arr"), "{names:?}");
    }

    /// The `name`/`kind` pairs the symbols of `code` carry.
    fn kinds_of(code: &str) -> Vec<(String, SymbolKind)> {
        let mut parser = CParser::new().unwrap();
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
        let mut parser = CParser::new().unwrap();
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

    const GUARDED: &str = "/** Guarded. */\n#ifdef X\nint f(void) { return 1; }\n\
                           int g(void) { return 2; }\n#endif\n";

    #[test]
    fn a_function_inside_an_ifdef_keeps_a_doc_written_before_the_directive() {
        assert_eq!(doc_of(GUARDED, "f").as_deref(), Some("Guarded."));
    }

    #[test]
    fn only_the_first_declaration_in_an_ifdef_claims_the_doc() {
        // The comment describes `f`. `g` follows a real declaration, so nothing
        // above the directive belongs to it.
        assert_eq!(doc_of(GUARDED, "g"), None);
    }

    #[test]
    fn a_union_produces_a_symbol_with_its_fields() {
        let kinds = kinds_of("union Bits { int i; float f; };\n");
        assert!(
            kinds.contains(&("Bits".into(), SymbolKind::Struct)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("Bits.i".into(), SymbolKind::Field)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("Bits.f".into(), SymbolKind::Field)),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_struct_produces_symbols_for_its_fields() {
        let kinds = kinds_of("struct Point { int x; int y; };\n");
        assert!(
            kinds.contains(&("Point.x".into(), SymbolKind::Field)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("Point.y".into(), SymbolKind::Field)),
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
    fn enum_members_produce_symbols() {
        let kinds = kinds_of("enum Color { RED, GREEN = 2 };\n");
        assert!(
            kinds.contains(&("Color".into(), SymbolKind::Enum)),
            "{kinds:?}"
        );
        // C puts enumerators in the enclosing scope: `RED`, never `Color::RED`.
        assert!(
            kinds.contains(&("RED".into(), SymbolKind::Constant)),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&("GREEN".into(), SymbolKind::Constant)),
            "{kinds:?}"
        );
    }

    #[test]
    fn a_function_prototype_is_not_recorded_as_a_variable() {
        let names = names_of("int plain(int x);\nint *ptr_ret(void);\n");
        assert!(!names.iter().any(|n| n == "plain"), "{names:?}");
        assert!(!names.iter().any(|n| n == "ptr_ret"), "{names:?}");
    }

    #[test]
    fn every_declarator_of_a_multi_name_declaration_produces_a_symbol() {
        let names = names_of("int a, b = 2, *c;\n");
        for expected in ["a", "b", "c"] {
            assert!(names.iter().any(|n| n == expected), "{names:?}");
        }
    }

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

        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Point" && s.kind == SymbolKind::Struct)
        );
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "Color" && s.kind == SymbolKind::Enum)
        );
        assert!(symbols.iter().any(|s| s.name.as_ref() == "add"
            && s.kind == SymbolKind::Function
            && s.visibility == Visibility::Public));
        assert!(
            symbols
                .iter()
                .any(|s| s.name.as_ref() == "_internal_helper"
                    && s.visibility == Visibility::Private)
        );
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
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "process")
        );
        assert!(
            calls
                .iter()
                .any(|(caller, target, _)| *caller == "main" && *target == "printf")
        );
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
        assert!(
            func.doc_comment
                .as_deref()
                .unwrap()
                .contains("Compute the sum")
        );
    }
}
