//! A wrapper around [`tree_sitter::Parser`] that caches the last parsed tree.
//!
//! When the same source code is parsed multiple times (e.g., for symbol
//! extraction, call detection, and relationship finding), the cached tree
//! is reused instead of re-parsing.

use tree_sitter::{Parser, Tree};

/// Wraps a [`tree_sitter::Parser`] with single-entry tree caching.
///
/// Caching is keyed on the raw pointer and length of the source `&str`.
/// This is safe because within a single `stage_parse` call the same
/// `&str` reference is passed to every extraction method.
pub struct CachingParser {
    parser: Parser,
    /// Cached tree keyed by (data pointer, length).
    cached: Option<(*const u8, usize, Tree)>,
}

// SAFETY: `Tree` is not `Send`, but `CachingParser` is only used within a
// single thread in `stage_parse`. We need `Send + Sync` on the trait bound
// for `LanguageParser` because parsers are stored in a `HashMap` that is
// created per-thread. The `Tree` cache is always consumed on the same thread.
unsafe impl Send for CachingParser {}
unsafe impl Sync for CachingParser {}

impl CachingParser {
    /// Create a new `CachingParser` wrapping an already-configured parser.
    pub fn new(parser: Parser) -> Self {
        Self {
            parser,
            cached: None,
        }
    }

    /// Parse source code, returning a cached tree if the source hasn't changed.
    ///
    /// The cache is keyed on the raw pointer and length of `code`, so the
    /// same `&str` slice will hit the cache on repeated calls.
    pub fn parse_cached(&mut self, code: &str) -> Option<Tree> {
        let ptr = code.as_ptr();
        let len = code.len();

        if let Some((cached_ptr, cached_len, ref tree)) = self.cached {
            if cached_ptr == ptr && cached_len == len {
                return Some(tree.clone());
            }
        }

        let tree = self.parser.parse(code, None)?;
        self.cached = Some((ptr, len, tree.clone()));
        Some(tree)
    }

    /// Invalidate the cache (e.g., between files).
    pub fn invalidate(&mut self) {
        self.cached = None;
    }
}
