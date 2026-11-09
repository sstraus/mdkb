//! Go language parser using tree-sitter-go.

mod cgo;
mod parser;

pub use parser::GoParser;
