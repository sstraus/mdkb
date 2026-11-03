//! A symbol's body text and the rename-invariant fingerprint of its shape.
//!
//! Nothing in the index holds a body — `code_symbols` stores a signature and a
//! range — so duplication re-derives it from the file plus the range, and
//! re-derives its AST from the parser the pipeline already built.
//!
//! The fingerprint hashes **node kinds only, never their text**. That is not an
//! optimisation: the embedding benchmark this feature is built on fails hardest
//! on exactly the case a reviewer cares about most — the same code with every
//! identifier renamed. Two bodies that differ only in names and literal values
//! walk the same kinds in the same order and land on the same simhash, which no
//! text-sensitive hash and no embedding can promise.

use tree_sitter::Node;

use crate::code::parsing::caching_parser::fnv1a_hash;

/// Kinds per shingle. Three is enough to carry ordering: a single kind is a
/// bag of words that cannot tell a loop containing an `if` from an `if`
/// containing a loop, and a longer window stops matching once a body gains one
/// statement.
const SHINGLE: usize = 3;

/// Bits that may differ before two bodies count as structurally unrelated.
///
/// 6 of 64. A simhash over shingles moves a handful of bits per edited
/// statement, so this tolerates a body that grew a line or two while rejecting
/// the pair that merely shares a language's boilerplate.
///
/// This was 12, and 12 was measured wrong on this repository: of the 706
/// clusters it reported, 497 sat at exactly 12 bits and 105 at 11 — 85% of the
/// mass pressed against the cut. A threshold that finds real duplication has
/// its mass near 0; one whose mass sits on its own boundary is reporting
/// whatever fits, and the widest cluster it produced joined `path_like_tokens`,
/// a `vectors.rs` test and a C++ parser. The shape held at every body size
/// (70% at the cut for bodies over 100 nodes, 63% over 200), so it was the
/// threshold and not an entropy floor on small bodies. 6 halves the reported
/// lines, 38931 to 19200, and pulls the mass off the boundary.
pub const SIMHASH_HAMMING_THRESHOLD: u32 = 6;

/// Field separator inside a shingle, so `["ab", "c"]` and `["a", "bc"]` do not
/// hash alike. ASCII unit separator: no grammar names a node with it.
const FIELD_SEP: char = '\u{1f}';

/// The source text of the lines a symbol spans.
///
/// `start_line` and `end_line` are **0-based inclusive** tree-sitter rows,
/// which is what `code_symbols.line_start`/`line_end` hold: `stage_index`
/// passes `Range::start_line` through untouched. Slicing on a 1-based
/// assumption shifts every body by one line, which is silent — the text still
/// parses, it just describes the neighbouring symbol.
///
/// The trailing line terminator is dropped, and a range reaching past the end
/// of the file yields `None` rather than a truncated body: that means the index
/// is stale against an edited file, and half a function scored against whole
/// ones is worse than one skipped.
pub fn body_text(source: &str, start_line: u32, end_line: u32) -> Option<&str> {
    if end_line < start_line {
        return None;
    }
    let (mut start, mut end) = (None, None);
    let mut offset = 0usize;
    for (row, line) in source.split_inclusive('\n').enumerate() {
        let row = row as u32;
        if row == start_line {
            start = Some(offset);
        }
        if row == end_line {
            end = Some(offset + line.trim_end_matches('\n').trim_end_matches('\r').len());
        }
        offset += line.len();
    }
    Some(&source[start?..end?])
}

/// A body's cache key: the hash of its text, as hex.
///
/// The same FNV-1a the tree cache keys a file on. One hash function in the
/// crate, so a body's identity cannot come out two different ways depending on
/// which layer asked.
pub fn body_hash(body: &str) -> String {
    format!("{:016x}", fnv1a_hash(body.as_bytes()))
}

/// The structural fingerprint of a subtree and the number of named nodes in it.
///
/// The count is the complexity filter: a three-node accessor is identical to
/// every other accessor in the repository and reporting it as duplication is
/// noise, so the caller drops small bodies before it ever compares them.
pub fn structural_simhash(root: Node) -> (u64, u32) {
    let kinds = named_kinds(root);
    (simhash(&kinds), kinds.len() as u32)
}

/// Bits by which two fingerprints differ.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Kinds of every named node, in pre-order.
///
/// Iterative, over a `TreeCursor`: the parsers cap their own recursion at
/// [`MAX_AST_DEPTH`] because minified and generated sources nest past what the
/// stack holds, and overflowing here aborts the process rather than failing one
/// file.
///
/// [`MAX_AST_DEPTH`]: crate::code::parsing::parser::MAX_AST_DEPTH
fn named_kinds(root: Node) -> Vec<&'static str> {
    let mut cursor = root.walk();
    let mut kinds = Vec::new();
    'walk: loop {
        let node = cursor.node();
        if node.is_named() {
            kinds.push(node.kind());
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.node().id() == root.id() {
                break 'walk;
            }
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    kinds
}

/// Simhash over the shingles of a kind sequence.
///
/// Each shingle votes on all 64 bits; a bit is set when the votes for it are
/// positive. Two sequences sharing most shingles therefore agree on most bits,
/// which a plain hash of the whole sequence cannot do — one changed statement
/// would move every bit.
fn simhash(kinds: &[&str]) -> u64 {
    if kinds.is_empty() {
        return 0;
    }
    let mut votes = [0i32; 64];
    let mut buf = String::new();
    // A body shorter than one shingle is hashed whole, rather than skipped:
    // dropping it would give every tiny body the same fingerprint, 0.
    let windows: Box<dyn Iterator<Item = &[&str]>> = if kinds.len() < SHINGLE {
        Box::new(std::iter::once(kinds))
    } else {
        Box::new(kinds.windows(SHINGLE))
    };
    for window in windows {
        buf.clear();
        for kind in window {
            buf.push_str(kind);
            buf.push(FIELD_SEP);
        }
        let hash = fnv1a_hash(buf.as_bytes());
        for (bit, vote) in votes.iter_mut().enumerate() {
            if hash >> bit & 1 == 1 {
                *vote += 1;
            } else {
                *vote -= 1;
            }
        }
    }
    votes
        .iter()
        .enumerate()
        .filter(|&(_, &v)| v > 0)
        .fold(0u64, |acc, (bit, _)| acc | 1 << bit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::indexing::pipeline::create_parser;
    use crate::code::parsing::language::Language;

    /// Fingerprint of a whole Rust source, through the same parser the pipeline
    /// builds — not a hand-rolled one, so the test cannot pass against a
    /// grammar the indexer does not use.
    fn rust_fingerprint(code: &str) -> (u64, u32) {
        let mut parser = create_parser(Language::Rust).expect("rust parser");
        let tree = parser.tree(code).expect("parses");
        structural_simhash(tree.root_node())
    }

    #[test]
    fn a_body_is_sliced_on_0_based_inclusive_lines() {
        // Rows:      0            1                 2        3
        let source = "fn a() {\n    let x = 1;\n    x\n}\nfn b() {}\n";

        // `fn a` spans rows 0..=3. Written out rather than computed, so a
        // 1-based slip cannot make the expectation move with the bug.
        assert_eq!(
            body_text(source, 0, 3),
            Some("fn a() {\n    let x = 1;\n    x\n}")
        );
        assert_eq!(body_text(source, 1, 1), Some("    let x = 1;"));
        assert_eq!(body_text(source, 4, 4), Some("fn b() {}"));
    }

    #[test]
    fn a_body_carries_no_trailing_newline() {
        let source = "one\ntwo\n";
        assert_eq!(body_text(source, 0, 0), Some("one"));
        assert_eq!(body_text(source, 0, 1), Some("one\ntwo"));
    }

    #[test]
    fn a_crlf_body_keeps_neither_terminator() {
        let source = "one\r\ntwo\r\n";
        assert_eq!(body_text(source, 0, 0), Some("one"));
    }

    #[test]
    fn a_range_past_the_end_of_the_file_is_none() {
        // A stale index against an edited file. Half a function scored against
        // whole ones is worse than one skipped.
        let source = "fn a() {}\n";
        assert_eq!(body_text(source, 0, 9), None);
        assert_eq!(body_text(source, 5, 6), None);
    }

    #[test]
    fn an_inverted_range_is_none() {
        assert_eq!(body_text("a\nb\n", 1, 0), None);
    }

    #[test]
    fn the_same_body_hashes_the_same_and_a_different_one_does_not() {
        assert_eq!(body_hash("fn a() {}"), body_hash("fn a() {}"));
        assert_ne!(body_hash("fn a() {}"), body_hash("fn b() {}"));
        assert_eq!(body_hash("x").len(), 16, "fixed-width hex");
    }

    /// The case the embedding benchmark fails on, and the reason this
    /// fingerprint exists: same shape, every name and literal changed.
    #[test]
    fn renaming_every_identifier_and_literal_leaves_the_simhash_identical() {
        let (original, n1) = rust_fingerprint(
            "fn total(items: &[u32]) -> u32 {\n\
             \x20   let mut sum = 0;\n\
             \x20   for item in items {\n\
             \x20       sum += item * 2;\n\
             \x20   }\n\
             \x20   sum\n\
             }\n",
        );
        let (renamed, n2) = rust_fingerprint(
            "fn aggregate(values: &[u32]) -> u32 {\n\
             \x20   let mut acc = 7;\n\
             \x20   for value in values {\n\
             \x20       acc += value * 9;\n\
             \x20   }\n\
             \x20   acc\n\
             }\n",
        );

        assert_eq!(original, renamed, "kinds are identical, so the hash is");
        assert_eq!(n1, n2);
        assert_eq!(hamming(original, renamed), 0);
    }

    #[test]
    fn the_same_signature_over_a_different_body_lands_past_the_threshold() {
        let (a, _) = rust_fingerprint(
            "fn run(input: &str) -> usize {\n\
             \x20   input.len()\n\
             }\n",
        );
        let (b, _) = rust_fingerprint(
            "fn run(input: &str) -> usize {\n\
             \x20   let mut n = 0;\n\
             \x20   for c in input.chars() {\n\
             \x20       if c.is_alphabetic() {\n\
             \x20           n += 1;\n\
             \x20       } else {\n\
             \x20           n -= 1;\n\
             \x20       }\n\
             \x20   }\n\
             \x20   match n {\n\
             \x20       0 => 0,\n\
             \x20       other => other as usize,\n\
             \x20   }\n\
             }\n",
        );

        assert!(
            hamming(a, b) > SIMHASH_HAMMING_THRESHOLD,
            "same signature, unrelated body: hamming {} must exceed {}",
            hamming(a, b),
            SIMHASH_HAMMING_THRESHOLD
        );
    }

    #[test]
    fn the_node_count_grows_with_the_body_not_the_name() {
        let (_, small) = rust_fingerprint("fn a() -> u32 { 1 }\n");
        let (_, large) = rust_fingerprint(
            "fn a_much_longer_name() -> u32 {\n\
             \x20   let x = 1;\n\
             \x20   let y = 2;\n\
             \x20   x + y\n\
             }\n",
        );
        assert!(large > small, "{large} must exceed {small}");
    }

    #[test]
    fn an_empty_source_fingerprints_without_panicking() {
        let (hash, nodes) = rust_fingerprint("");
        assert_eq!(nodes, 1, "the source_file node itself");
        assert_ne!(hash, 0, "a single kind still votes");
    }

    #[test]
    fn a_shingle_is_ordered_not_a_bag_of_kinds() {
        // A loop holding an `if` and an `if` holding a loop walk the same
        // kinds. Only the order tells them apart, so the shingles must.
        let (loop_first, _) = rust_fingerprint(
            "fn a(v: &[u32]) {\n\
             \x20   for x in v {\n\
             \x20       if *x > 0 {\n\
             \x20           println!();\n\
             \x20       }\n\
             \x20   }\n\
             }\n",
        );
        let (if_first, _) = rust_fingerprint(
            "fn a(v: &[u32]) {\n\
             \x20   if v.len() > 0 {\n\
             \x20       for x in v {\n\
             \x20           println!();\n\
             \x20       }\n\
             \x20   }\n\
             }\n",
        );
        assert_ne!(loop_first, if_first);
    }

    #[test]
    fn tree_answers_for_every_language_the_indexer_supports() {
        // The default impl returns None. A parser that forgot to override it
        // would contribute no bodies at all, in silence — the duplication pass
        // would simply never report that language.
        let languages = [
            Language::Rust,
            Language::Python,
            Language::JavaScript,
            Language::TypeScript,
            Language::Go,
            Language::C,
            Language::Cpp,
            Language::CSharp,
            Language::Java,
            Language::Kotlin,
            Language::Php,
            Language::Swift,
            Language::Lua,
            Language::Gdscript,
            Language::Objc,
            Language::Shell,
            Language::Ruby,
            Language::Sql,
            Language::Json,
        ];
        for language in languages {
            let mut parser =
                create_parser(language).unwrap_or_else(|| panic!("{language:?} parser must build"));
            assert!(
                parser.tree("").is_some(),
                "{language:?} must answer tree(), not fall through to the default"
            );
        }
    }
}
