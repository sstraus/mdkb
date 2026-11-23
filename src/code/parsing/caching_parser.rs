//! A wrapper around [`tree_sitter::Parser`] that caches the last parsed tree.
//!
//! When the same source code is parsed multiple times (e.g., for symbol
//! extraction, call detection, and relationship finding), the cached tree
//! is reused instead of re-parsing. Tree-sitter parsing is O(n) in source
//! size; `Tree::clone()` (ts_tree_copy) is O(tree nodes) which is cheaper.

use tree_sitter::{Node, Parser, Tree};

/// Wraps a [`tree_sitter::Parser`] with single-entry tree caching.
///
/// The cache is keyed on a content hash (FNV-1a) of the source string.
/// Within `stage_parse`, the same source is passed to 4-7 extraction
/// methods; only the first call invokes tree-sitter, the rest clone
/// the cached tree (cheaper than re-parsing).
pub struct CachingParser {
    parser: Parser,
    /// Cached tree keyed by content hash.
    cached: Option<(u64, Tree)>,
}

impl std::fmt::Debug for CachingParser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachingParser")
            .field("parser", &"<configured>")
            .field("cached", &self.cached.as_ref().map(|(hash, _)| hash))
            .finish()
    }
}

// SAFETY: `CachingParser` contains a raw pointer inside `tree_sitter::Tree`
// which makes it `!Send`. Parsers are created in `create_parser` and moved
// into the parse thread's local HashMap. After that, they are only used from
// that single thread. No cross-thread sharing occurs.
unsafe impl Send for CachingParser {}

impl CachingParser {
    /// Create a new `CachingParser` wrapping an already-configured parser.
    pub fn new(parser: Parser) -> Self {
        Self {
            parser,
            cached: None,
        }
    }

    /// Parse source code, returning an owned tree.
    ///
    /// On cache hit (same content hash), clones the stored tree.
    /// On cache miss, parses with tree-sitter and caches the result.
    ///
    /// `Tree::clone()` calls `ts_tree_copy` which is a deep copy but still
    /// cheaper than re-parsing: it copies the node array without re-running
    /// the parser state machine over the source text.
    pub fn parse_cached(&mut self, code: &str) -> Option<Tree> {
        let hash = fnv1a_hash(code.as_bytes());

        if let Some((cached_hash, ref tree)) = self.cached {
            if cached_hash == hash {
                return Some(tree.clone());
            }
        }

        let tree = self.parser.parse(code, None)?;
        self.cached = Some((hash, tree.clone()));
        Some(tree)
    }

    /// Parse `code`, walk it, and return what the walk collected.
    ///
    /// Every language parser needs the same four steps around its own walk:
    /// parse, give up quietly on source tree-sitter cannot read, allocate the
    /// output, hand the root over. `mdkb code dup` found that shape written out 42
    /// times across 13 parsers, so it lives here once instead.
    ///
    /// Giving up returns an empty `Vec` rather than an error: a file that does
    /// not parse contributes nothing, and one bad file must not fail the index.
    pub fn collect<'a, T>(
        &mut self,
        code: &'a str,
        walk: impl FnOnce(&Node<'_>, &'a str, &mut Vec<T>),
    ) -> Vec<T> {
        let Some(tree) = self.parse_cached(code) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        walk(&tree.root_node(), code, &mut found);
        found
    }
}

/// FNV-1a hash for cache keying. Fast, no allocation, good distribution.
///
/// `pub(crate)` so the duplication cache keys a body on the same hash the tree
/// cache keys a file on — one hash function, not two that could disagree.
pub(crate) fn fnv1a_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_parser() -> CachingParser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("rust grammar");
        CachingParser::new(parser)
    }

    #[test]
    fn collect_hands_the_root_to_the_walk_and_returns_what_it_found() {
        let mut parser = rust_parser();

        let kinds: Vec<&str> = parser.collect("fn a() {}\n", |root, _code, out| {
            out.push(root.kind());
            let mut cursor = root.walk();
            for child in root.children(&mut cursor) {
                out.push(child.kind());
            }
        });

        assert_eq!(kinds, vec!["source_file", "function_item"]);
    }

    #[test]
    fn collect_borrows_the_source_for_what_it_returns() {
        // The whole point of the `'a` on the code: a walk returns slices of the
        // source, which outlive the tree they were found through.
        let mut parser = rust_parser();
        let code = String::from("fn named() {}\n");

        let names: Vec<&str> = parser.collect(&code, |root, code, out| {
            let mut cursor = root.walk();
            for child in root.children(&mut cursor) {
                out.push(&code[child.byte_range()]);
            }
        });

        assert_eq!(names, vec!["fn named() {}"]);
    }

    #[test]
    fn source_the_grammar_cannot_read_collects_nothing_rather_than_failing() {
        // One unparseable file must contribute nothing, not fail the index.
        // Every one of the 42 call sites relied on this, so it is pinned once.
        let mut parser = CachingParser::new(Parser::new());

        let out: Vec<&str> = parser.collect("fn a() {}\n", |root, _code, out| {
            out.push(root.kind());
        });

        assert!(
            out.is_empty(),
            "a parser with no grammar set cannot parse, and must say so by \
             finding nothing: {out:?}"
        );
    }
}
