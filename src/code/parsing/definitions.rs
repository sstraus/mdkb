//! Definition indexing for shell scripts, Ruby and SQL.
//!
//! These grammars expose named declarations. Calls, imports and inferred type
//! relationships are intentionally not guessed from their dynamic syntax.
use super::caching_parser::CachingParser;
use super::import::Import;
use super::language::Language;
use super::parser::{LanguageParser, check_recursion_depth, node_range};
use crate::code::symbol::{ScopeContext, Symbol, Visibility};
use crate::code::types::{FileId, SymbolCounter, SymbolKind};
use tree_sitter::Node;

pub struct DefinitionParser {
    parser: CachingParser,
    language: Language,
}

impl std::fmt::Debug for DefinitionParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefinitionParser")
            .field("language", &self.language)
            .finish_non_exhaustive()
    }
}

impl DefinitionParser {
    pub fn new(language: Language) -> Result<Self, String> {
        let grammar = match language {
            Language::Shell => tree_sitter_bash::LANGUAGE.into(),
            Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Language::Sql => tree_sitter_sqlite3::LANGUAGE.into(),
            Language::Json => tree_sitter_json::LANGUAGE.into(),
            _ => return Err(format!("No definition grammar for {language}")),
        };
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&grammar)
            .map_err(|e| format!("Failed to set {language} language: {e}"))?;
        Ok(Self {
            parser: CachingParser::new(parser),
            language,
        })
    }

    fn definition<'a>(&self, node: Node<'a>) -> Option<(Node<'a>, SymbolKind)> {
        let (name, kind) = match (self.language, node.kind()) {
            (Language::Shell, "function_definition") => {
                (node.child_by_field_name("name")?, SymbolKind::Function)
            }
            (Language::Ruby, "class") => (node.child_by_field_name("name")?, SymbolKind::Class),
            (Language::Ruby, "module") => (node.child_by_field_name("name")?, SymbolKind::Module),
            (Language::Ruby, "method" | "singleton_method") => {
                (node.child_by_field_name("name")?, SymbolKind::Method)
            }
            (
                Language::Sql,
                "create_table_statement"
                | "create_virtual_table_statement"
                | "create_view_statement"
                | "create_index_statement"
                | "create_trigger_statement",
            ) => {
                let object = node.child_by_field_name("name")?;
                (
                    object.child_by_field_name("name")?,
                    match node.kind() {
                        "create_index_statement" => SymbolKind::Constant,
                        "create_trigger_statement" => SymbolKind::Function,
                        _ => SymbolKind::Struct,
                    },
                )
            }
            _ => return None,
        };
        Some((name, kind))
    }
}

impl LanguageParser for DefinitionParser {
    fn parse(&mut self, code: &str, file_id: FileId, counter: &mut SymbolCounter) -> Vec<Symbol> {
        let Some(tree) = self.parser.parse_cached(code) else {
            return Vec::new();
        };
        let mut symbols = Vec::new();
        let mut stack = vec![(tree.root_node(), None::<String>, 0)];
        while let Some((node, owner, depth)) = stack.pop() {
            if !check_recursion_depth(depth, node) {
                continue;
            }
            let mut child_owner = owner.clone();
            if let Some((name_node, kind)) = self.definition(node) {
                let raw_name = &code[name_node.byte_range()];
                // SQL identifiers can be quoted; keep their logical spelling.
                let name = if self.language == Language::Sql {
                    raw_name.trim_matches(['"', '\'', '`', '[', ']'])
                } else {
                    raw_name
                };
                let signature_end = node
                    .child_by_field_name("body")
                    .map_or(node.end_byte(), |body| body.start_byte());
                let mut symbol =
                    Symbol::new(counter.next_id(), name, kind, file_id, node_range(node))
                        .with_signature(code[node.start_byte()..signature_end].trim());
                symbol.visibility = Visibility::Public;
                symbol.scope_context = Some(match (&owner, kind) {
                    (Some(owner), SymbolKind::Method) => ScopeContext::ClassMember {
                        class_name: Some(owner.as_str().into()),
                    },
                    _ => ScopeContext::Module,
                });
                if self.language == Language::Ruby
                    && matches!(kind, SymbolKind::Class | SymbolKind::Module)
                {
                    child_owner = Some(match &owner {
                        Some(owner) => format!("{owner}::{name}"),
                        None => name.to_owned(),
                    });
                }
                symbols.push(symbol);
            }
            // Reverse to preserve source order and deterministic symbol IDs.
            let children: Vec<_> = node.named_children(&mut node.walk()).collect();
            for child in children.into_iter().rev() {
                stack.push((child, child_owner.clone(), depth + 1));
            }
        }
        symbols
    }

    fn language(&self) -> Language {
        self.language
    }
    fn tree(&mut self, code: &str) -> Option<tree_sitter::Tree> {
        self.parser.parse_cached(code)
    }
    fn parser(&mut self) -> &mut CachingParser {
        &mut self.parser
    }
    fn extract_doc_comment(&self, _node: &Node, _code: &str) -> Option<String> {
        None
    }
    fn find_imports(&mut self, _code: &str, _file_id: FileId) -> Vec<Import> {
        Vec::new()
    }
}
