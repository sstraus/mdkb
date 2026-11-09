//! Extract definitions from the comment group attached to `import "C"`.
use crate::code::parsing::c_lang::CParser;
use crate::code::parsing::parser::LanguageParser;
use crate::code::symbol::Symbol;
use crate::code::types::{FileId, SymbolCounter};
use tree_sitter::Node;

pub(super) fn symbols(
    root: Node,
    code: &str,
    file_id: FileId,
    counter: &mut SymbolCounter,
) -> Vec<Symbol> {
    let mut ranges = Vec::new();
    let mut stack: Vec<_> = root
        .named_children(&mut root.walk())
        .filter(|node| node.kind() == "import_declaration")
        .collect();
    while let Some(node) = stack.pop() {
        if node.kind() == "import_spec"
            && node
                .child_by_field_name("path")
                .is_some_and(|path| &code[path.byte_range()] == "\"C\"")
        {
            // A single import's documentation belongs to the declaration;
            // a grouped import's documentation belongs to its spec.
            let parent = node.parent().unwrap_or(node);
            let anchor = if parent.kind() == "import_declaration" {
                parent
            } else {
                node
            };
            let mut previous = anchor.prev_named_sibling();
            let mut start = anchor.start_byte();
            while let Some(comment) = previous {
                let gap = &code[comment.end_byte()..start];
                if comment.kind() != "comment"
                    || !gap.chars().all(char::is_whitespace)
                    || gap.bytes().filter(|b| *b == b'\n').count() > 1
                {
                    break;
                }
                ranges.push(comment.byte_range());
                start = comment.start_byte();
                previous = comment.prev_named_sibling();
            }
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    if ranges.is_empty() {
        return Vec::new();
    }
    // Preserve byte offsets, columns and line numbers in the original Go file.
    let mut source: Vec<u8> = code
        .bytes()
        .map(|b| if b == b'\n' { b'\n' } else { b' ' })
        .collect();
    for range in ranges {
        let raw = &code[range.clone()];
        let inner = if raw.starts_with("/*") {
            range.start + 2..range.end - 2
        } else {
            range.start + 2..range.end
        };
        source[inner.clone()].copy_from_slice(&code.as_bytes()[inner]);
    }
    // #cgo is a Go build directive, not a C preprocessor directive.
    for line in source.split_mut(|b| *b == b'\n') {
        if line
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .is_some_and(|start| line[start..].starts_with(b"#cgo"))
        {
            line.fill(b' ');
        }
    }
    let source = String::from_utf8(source).expect("complete UTF-8 comment ranges");
    match CParser::new_objc() {
        Ok(mut parser) => parser.parse(&source, file_id, counter),
        Err(error) => {
            tracing::warn!(%error, "Cannot parse cgo preamble");
            Vec::new()
        }
    }
}
