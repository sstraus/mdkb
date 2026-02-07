//! Core types for code intelligence.
//!
//! Provides identifiers, enums, and value types used across the code
//! analysis pipeline: parsing, indexing, storage, and search.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Unique identifier for a symbol in the code index.
///
/// Zero is reserved as invalid, so valid IDs start at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(u32);

/// Unique identifier for an indexed file.
///
/// Zero is reserved as invalid, so valid IDs start at 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId(u32);

/// Source location range: start and end positions in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Range {
    pub start_line: u32,
    pub start_column: u16,
    pub end_line: u32,
    pub end_column: u16,
}

/// Classification of a symbol extracted from source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Trait,
    Interface,
    Class,
    Module,
    Variable,
    Constant,
    Field,
    Parameter,
    TypeAlias,
    Macro,
}

/// Heap-allocated compact string for symbol names and paths.
pub type CompactString = Box<str>;

/// Create a `CompactString` from a string slice.
pub fn compact_string(s: &str) -> CompactString {
    s.into()
}

// --- SymbolId ---

impl SymbolId {
    /// Create a new `SymbolId`. Returns `None` if value is 0.
    pub fn new(value: u32) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Get the raw u32 value.
    pub fn value(self) -> u32 {
        self.0
    }
}

impl fmt::Display for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sym#{}", self.0)
    }
}

// --- FileId ---

impl FileId {
    /// Create a new `FileId`. Returns `None` if value is 0.
    pub fn new(value: u32) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Get the raw u32 value.
    pub fn value(self) -> u32 {
        self.0
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "file#{}", self.0)
    }
}

// --- Range ---

impl Range {
    pub fn new(start_line: u32, start_column: u16, end_line: u32, end_column: u16) -> Self {
        Self {
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }

    /// Check whether a position (line, column) falls within this range.
    pub fn contains(&self, line: u32, column: u16) -> bool {
        if line < self.start_line || line > self.end_line {
            return false;
        }
        if line == self.start_line && column < self.start_column {
            return false;
        }
        if line == self.end_line && column > self.end_column {
            return false;
        }
        true
    }
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}-{}:{}",
            self.start_line, self.start_column, self.end_line, self.end_column
        )
    }
}

// --- SymbolKind ---

impl SymbolKind {
    /// Total number of symbol kind variants.
    pub const COUNT: usize = 14;

    /// Convert to a u8 discriminant for compact storage.
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Convert from a u8 discriminant.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Function),
            1 => Some(Self::Method),
            2 => Some(Self::Struct),
            3 => Some(Self::Enum),
            4 => Some(Self::Trait),
            5 => Some(Self::Interface),
            6 => Some(Self::Class),
            7 => Some(Self::Module),
            8 => Some(Self::Variable),
            9 => Some(Self::Constant),
            10 => Some(Self::Field),
            11 => Some(Self::Parameter),
            12 => Some(Self::TypeAlias),
            13 => Some(Self::Macro),
            _ => None,
        }
    }
}

impl FromStr for SymbolKind {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Function" | "function" => Ok(Self::Function),
            "Method" | "method" => Ok(Self::Method),
            "Struct" | "struct" => Ok(Self::Struct),
            "Enum" | "enum" => Ok(Self::Enum),
            "Trait" | "trait" => Ok(Self::Trait),
            "Interface" | "interface" => Ok(Self::Interface),
            "Class" | "class" => Ok(Self::Class),
            "Module" | "module" => Ok(Self::Module),
            "Variable" | "variable" => Ok(Self::Variable),
            "Constant" | "constant" => Ok(Self::Constant),
            "Field" | "field" => Ok(Self::Field),
            "Parameter" | "parameter" => Ok(Self::Parameter),
            "TypeAlias" | "type_alias" => Ok(Self::TypeAlias),
            "Macro" | "macro" => Ok(Self::Macro),
            _ => Err("Unknown symbol kind"),
        }
    }
}

impl fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Function => "Function",
            Self::Method => "Method",
            Self::Struct => "Struct",
            Self::Enum => "Enum",
            Self::Trait => "Trait",
            Self::Interface => "Interface",
            Self::Class => "Class",
            Self::Module => "Module",
            Self::Variable => "Variable",
            Self::Constant => "Constant",
            Self::Field => "Field",
            Self::Parameter => "Parameter",
            Self::TypeAlias => "TypeAlias",
            Self::Macro => "Macro",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_symbol_id_zero_is_invalid() {
        assert!(SymbolId::new(0).is_none());
    }

    #[test]
    fn test_symbol_id_nonzero() {
        let id = SymbolId::new(42).unwrap();
        assert_eq!(id.value(), 42);
    }

    #[test]
    fn test_file_id_zero_is_invalid() {
        assert!(FileId::new(0).is_none());
    }

    #[test]
    fn test_file_id_nonzero() {
        let id = FileId::new(100).unwrap();
        assert_eq!(id.value(), 100);
    }

    #[test]
    fn test_range_new() {
        let r = Range::new(10, 5, 15, 20);
        assert_eq!(r.start_line, 10);
        assert_eq!(r.start_column, 5);
        assert_eq!(r.end_line, 15);
        assert_eq!(r.end_column, 20);
    }

    #[test]
    fn test_range_contains_inside() {
        let r = Range::new(10, 5, 15, 20);
        assert!(r.contains(12, 10));
        assert!(r.contains(10, 5)); // exact start
        assert!(r.contains(15, 20)); // exact end
    }

    #[test]
    fn test_range_contains_outside() {
        let r = Range::new(10, 5, 15, 20);
        assert!(!r.contains(9, 10));
        assert!(!r.contains(16, 10));
        assert!(!r.contains(10, 4));
        assert!(!r.contains(15, 21));
    }

    #[test]
    fn test_symbol_kind_count() {
        let kinds = [
            SymbolKind::Function,
            SymbolKind::Method,
            SymbolKind::Struct,
            SymbolKind::Enum,
            SymbolKind::Trait,
            SymbolKind::Interface,
            SymbolKind::Class,
            SymbolKind::Module,
            SymbolKind::Variable,
            SymbolKind::Constant,
            SymbolKind::Field,
            SymbolKind::Parameter,
            SymbolKind::TypeAlias,
            SymbolKind::Macro,
        ];
        assert_eq!(kinds.len(), SymbolKind::COUNT);
    }

    #[test]
    fn test_symbol_kind_u8_roundtrip() {
        for i in 0..SymbolKind::COUNT as u8 {
            let kind = SymbolKind::from_u8(i).unwrap();
            assert_eq!(kind.as_u8(), i);
        }
        assert!(SymbolKind::from_u8(14).is_none());
    }

    #[test]
    fn test_symbol_kind_from_str() {
        assert_eq!("Function".parse::<SymbolKind>().unwrap(), SymbolKind::Function);
        assert_eq!("method".parse::<SymbolKind>().unwrap(), SymbolKind::Method);
        assert!("unknown".parse::<SymbolKind>().is_err());
    }

    #[test]
    fn test_symbol_kind_display() {
        assert_eq!(SymbolKind::Function.to_string(), "Function");
        assert_eq!(SymbolKind::TypeAlias.to_string(), "TypeAlias");
    }

    #[test]
    fn test_compact_string() {
        let s = compact_string("hello world");
        assert_eq!(&*s, "hello world");
    }

    #[test]
    fn test_id_equality_and_hash() {
        let id1 = SymbolId::new(42).unwrap();
        let id2 = SymbolId::new(42).unwrap();
        let id3 = SymbolId::new(43).unwrap();

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);

        let mut set = HashSet::new();
        set.insert(id1);
        assert!(set.contains(&id2));
        assert!(!set.contains(&id3));
    }

    #[test]
    fn test_serde_roundtrip() {
        let id = SymbolId::new(42).unwrap();
        let json = serde_json::to_string(&id).unwrap();
        let back: SymbolId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);

        let kind = SymbolKind::Trait;
        let json = serde_json::to_string(&kind).unwrap();
        let back: SymbolKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, back);

        let range = Range::new(1, 0, 10, 30);
        let json = serde_json::to_string(&range).unwrap();
        let back: Range = serde_json::from_str(&json).unwrap();
        assert_eq!(range, back);
    }
}
