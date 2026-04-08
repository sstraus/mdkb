//! A wrapper around [`tree_sitter::Parser`] that caches the last parsed tree.
//!
//! When the same source code is parsed multiple times (e.g., for symbol
//! extraction, call detection, and relationship finding), the cached tree
//! is reused instead of re-parsing. Tree-sitter parsing is O(n) in source
//! size; `Tree::clone()` (ts_tree_copy) is O(tree nodes) which is cheaper.

use tree_sitter::{Parser, Tree};

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
            .field("cached_hash", &self.cached.as_ref().map(|(h, _)| h))
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
}

/// FNV-1a hash for cache keying. Fast, no allocation, good distribution.
fn fnv1a_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
