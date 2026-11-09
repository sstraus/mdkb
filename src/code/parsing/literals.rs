//! Bounded literal extraction from the tree already parsed for symbols.
use std::path::Path;

use tree_sitter::{Node, Tree};

use crate::code::symbol::Symbol;

/// Bounds apply to each file, independently of the number of AST nodes.
pub const MIN_LITERAL_CHARS: usize = 4;
pub const MAX_LITERAL_BYTES: usize = 1024;
pub const MAX_LITERALS_PER_FILE: usize = 4096;
pub const MAX_LITERAL_BYTES_PER_FILE: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct Literal {
    pub content: String,
    /// Zero-based, like symbol locations in the code index.
    pub line: u32,
    pub column: u32,
    pub enclosing_symbol: Option<String>,
}

/// Generated files still contribute symbols, but not thousands of literal rows.
pub fn skip_literals(path: &Path, source: &str) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.ends_with(".min.js")
        || name.ends_with(".min.ts")
        || name.ends_with(".generated.ts")
        || source.lines().take(5).any(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("@generated")
                || line.contains("code generated")
                || line.contains("auto-generated")
        })
        || source.lines().any(|line| line.len() > 4096)
}

/// Read string nodes, never comments or text that merely resembles a quote.
/// Interpolated templates contribute their static fragments, not a made-up value.
pub fn extract(tree: &Tree, source: &str, symbols: &[Symbol]) -> Vec<Literal> {
    let mut found = Vec::new();
    let mut bytes = 0;
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if found.len() >= MAX_LITERALS_PER_FILE || bytes >= MAX_LITERAL_BYTES_PER_FILE {
            break;
        }
        if let Some(content) = literal_value(node, source) {
            if content.len() <= MAX_LITERAL_BYTES
                && content.chars().count() >= MIN_LITERAL_CHARS
                && bytes + content.len() <= MAX_LITERAL_BYTES_PER_FILE
            {
                let mut position = node.start_position();
                if matches!(
                    node.kind(),
                    "jsx_text" | "string_fragment" | "string_content" | "string_literal_content"
                ) {
                    let raw = &source[node.byte_range()];
                    let prefix = &raw[..raw.len() - raw.trim_start().len()];
                    let newlines = prefix.bytes().filter(|byte| *byte == b'\n').count();
                    position.row += newlines;
                    position.column = if newlines == 0 {
                        position.column + prefix.len()
                    } else {
                        prefix.rsplit('\n').next().unwrap_or_default().len()
                    };
                }
                let enclosing_symbol = symbols
                    .iter()
                    .filter(|symbol| {
                        symbol.range.contains(
                            position.row as u32,
                            position.column.min(u16::MAX as usize) as u16,
                        )
                    })
                    .min_by_key(|symbol| {
                        (
                            symbol
                                .range
                                .end_line
                                .saturating_sub(symbol.range.start_line),
                            symbol
                                .range
                                .end_column
                                .saturating_sub(symbol.range.start_column),
                        )
                    })
                    .map(|symbol| symbol.name.to_string());
                bytes += content.len();
                found.push(Literal {
                    content,
                    line: position.row as u32,
                    column: position.column as u32,
                    enclosing_symbol,
                });
            }
            // A string's content children must not create duplicate rows.
            continue;
        }
        // Object keys name entries; the values are what a UI displays.
        if is_key(node) {
            continue;
        }
        let children: Vec<_> = node.named_children(&mut node.walk()).collect();
        stack.extend(children.into_iter().rev());
    }
    found
}

fn is_key(node: Node<'_>) -> bool {
    node.parent()
        .and_then(|parent| parent.child_by_field_name("key"))
        == Some(node)
}

fn literal_value(node: Node<'_>, source: &str) -> Option<String> {
    if is_key(node) {
        return None;
    }
    let raw = source.get(node.byte_range())?;
    match node.kind() {
        "jsx_text" | "string_fragment" | "string_content" | "string_literal_content" => {
            Some(raw.trim().to_owned())
        }
        "string"
        | "string_literal"
        | "interpreted_string_literal"
        | "raw_string_literal"
        | "raw_string"
        | "template_string"
        | "concatenated_string" => {
            if node.named_children(&mut node.walk()).any(|child| {
                matches!(
                    child.kind(),
                    "template_substitution" | "interpolation" | "string_interpolation"
                )
            }) {
                return None;
            }
            // JSON-compatible quotes decode escapes (including i18n Unicode).
            if let Ok(value) = serde_json::from_str::<String>(raw) {
                return Some(value);
            }
            // Other language spellings retain their source escapes. Remove only
            // matching outer delimiters, not quotes inside the actual value.
            let quote = raw.find(['\'', '"', '`'])?;
            let delimiter = raw.as_bytes()[quote] as char;
            let end = raw.rfind(delimiter)?;
            (end > quote).then(|| raw[quote + 1..end].to_owned())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::parsing::parser::LanguageParser;

    /// Catches: escaped JSON values, interpolated fragments and JSX labels are lost or duplicated.
    #[test]
    fn ast_literals_preserve_values_without_comments_keys_or_duplicate_fragments() {
        let mut parser = crate::code::parsing::typescript::TypeScriptParser::new().unwrap();
        let source = "// Not a literal\nconst x = { 'long key': 'Hello label' }; const y = `Before ${name} after`; const z = <p>JSX label</p>;";
        let tree = parser.tree(source).unwrap();
        let values: Vec<_> = extract(&tree, source, &[])
            .into_iter()
            .map(|literal| literal.content)
            .collect();
        assert_eq!(values, ["Hello label", "Before", "after", "JSX label"]);
    }

    /// Catches: leading JSX whitespace reports the opening tag's line instead of the label.
    #[test]
    fn multiline_jsx_points_to_the_first_non_whitespace_character() {
        let mut parser = crate::code::parsing::typescript::TypeScriptParser::new().unwrap();
        let source = "const x = <p>\n  Fork from here\n</p>;";
        let tree = parser.tree(source).unwrap();
        let found = extract(&tree, source, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].line, found[0].column), (1, 2));
    }

    /// Catches: generated/minified input or pathological strings make the literal index unbounded.
    #[test]
    fn extraction_bounds_are_enforced_without_truncating_values_into_false_hits() {
        assert!(skip_literals(
            Path::new("bundle.min.js"),
            "const x = 'hello';"
        ));
        assert!(skip_literals(
            Path::new("gen.ts"),
            "// @generated\nconst x = 'hello';"
        ));
        let mut parser = crate::code::parsing::typescript::TypeScriptParser::new().unwrap();
        let source = format!(
            "const too_short = 'abc'; const too_long = '{}';",
            "x".repeat(MAX_LITERAL_BYTES + 1)
        );
        let tree = parser.tree(&source).unwrap();
        assert!(extract(&tree, &source, &[]).is_empty());
        let source = "const x = 'literal';\n".repeat(MAX_LITERALS_PER_FILE + 1);
        let tree = parser.tree(&source).unwrap();
        assert_eq!(extract(&tree, &source, &[]).len(), MAX_LITERALS_PER_FILE);
        let source = format!("const x = '{}';\n", "x".repeat(MAX_LITERAL_BYTES)).repeat(300);
        let tree = parser.tree(&source).unwrap();
        let found = extract(&tree, &source, &[]);
        assert_eq!(
            found
                .iter()
                .map(|literal| literal.content.len())
                .sum::<usize>(),
            MAX_LITERAL_BYTES_PER_FILE
        );
    }
}
