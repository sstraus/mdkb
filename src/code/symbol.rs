//! Symbol representation for extracted code entities.
//!
//! A `Symbol` represents a named entity in source code (function, class, struct, etc.)
//! with its location, signature, documentation, and scope information.

use serde::{Deserialize, Serialize};
use std::fmt;

use super::types::{CompactString, FileId, Range, SymbolId, SymbolKind, compact_string};

/// Visibility of a symbol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Visibility {
    Public,
    Crate,
    Module,
    Private,
}

/// Scope context where a symbol is defined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ScopeContext {
    /// Local to function/method/block.
    Local {
        hoisted: bool,
        parent_name: Option<CompactString>,
        parent_kind: Option<SymbolKind>,
    },
    /// Parameter of function/method.
    Parameter,
    /// Class/struct/trait member.
    ClassMember {
        class_name: Option<CompactString>,
    },
    /// Module/file level definition.
    #[default]
    Module,
    /// Package/namespace level export.
    Package,
    /// Global/builtin symbol.
    Global,
}

/// A named entity extracted from source code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SymbolId,
    pub name: CompactString,
    pub kind: SymbolKind,
    pub file_id: FileId,
    pub range: Range,
    pub file_path: Box<str>,
    pub signature: Option<Box<str>>,
    pub doc_comment: Option<Box<str>>,
    pub module_path: Option<Box<str>>,
    pub visibility: Visibility,
    pub scope_context: Option<ScopeContext>,
}

impl Symbol {
    pub fn new(
        id: SymbolId,
        name: impl Into<CompactString>,
        kind: SymbolKind,
        file_id: FileId,
        range: Range,
    ) -> Self {
        Self {
            id,
            name: name.into(),
            kind,
            file_id,
            range,
            file_path: "<unknown>".into(),
            signature: None,
            doc_comment: None,
            module_path: None,
            visibility: Visibility::Private,
            scope_context: None,
        }
    }

    pub fn with_file_path(mut self, file_path: impl Into<Box<str>>) -> Self {
        self.file_path = file_path.into();
        self
    }

    pub fn with_signature(mut self, signature: impl Into<Box<str>>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    pub fn with_doc(mut self, doc: impl Into<Box<str>>) -> Self {
        self.doc_comment = Some(doc.into());
        self
    }

    pub fn with_module_path(mut self, path: impl Into<Box<str>>) -> Self {
        self.module_path = Some(path.into());
        self
    }

    pub fn with_visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    pub fn with_scope(mut self, scope: ScopeContext) -> Self {
        self.scope_context = Some(scope);
        self
    }

    pub fn as_name(&self) -> &str {
        &self.name
    }

    pub fn as_signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }

    pub fn as_doc_comment(&self) -> Option<&str> {
        self.doc_comment.as_deref()
    }

    pub fn as_module_path(&self) -> Option<&str> {
        self.module_path.as_deref()
    }

    /// Convert to compact 32-byte representation for dense storage.
    pub fn to_compact(&self, string_table: &mut StringTable) -> CompactSymbol {
        let name_offset = string_table.intern(&self.name);
        CompactSymbol {
            name_offset,
            kind: self.kind.as_u8(),
            flags: 0,
            file_id: self.file_id.value() as u16,
            start_line: self.range.start_line,
            start_col: self.range.start_column,
            end_line: self.range.end_line,
            end_col: self.range.end_column,
            symbol_id: self.id.value(),
            _padding: [0; 2],
        }
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if let Some(sig) = &self.signature {
            write!(f, "\n  Signature: {sig}")?;
        }
        write!(f, "\n  Kind: {:?}", self.kind)?;
        write!(f, "\n  Visibility: {:?}", self.visibility)?;
        write!(f, "\n  Location: {} {}", self.file_id, self.range)?;
        if let Some(module) = &self.module_path {
            write!(f, "\n  Module: {module}")?;
        }
        if let Some(doc) = &self.doc_comment {
            let truncated = if doc.len() > 100 {
                format!("{}...", &doc[..100])
            } else {
                doc.to_string()
            };
            write!(f, "\n  Doc: {truncated}")?;
        }
        Ok(())
    }
}

// --- CompactSymbol ---

/// Compact 32-byte symbol representation for dense storage.
///
/// Uses a string table for name lookup instead of heap-allocated strings.
#[repr(C, align(32))]
#[derive(Debug, Clone, Copy)]
pub struct CompactSymbol {
    pub name_offset: u32,
    pub kind: u8,
    pub flags: u8,
    pub file_id: u16,
    pub start_line: u32,
    pub start_col: u16,
    pub end_line: u32,
    pub end_col: u16,
    pub symbol_id: u32,
    _padding: [u8; 2],
}

impl CompactSymbol {
    /// Convert back to a full Symbol using a string table for name lookup.
    pub fn to_symbol(&self, string_table: &StringTable) -> Option<Symbol> {
        let name = string_table.get(self.name_offset)?;
        let kind = SymbolKind::from_u8(self.kind)?;
        let file_id = FileId::new(u32::from(self.file_id))?;
        let id = SymbolId::new(self.symbol_id)?;

        Some(Symbol {
            id,
            name: compact_string(name),
            kind,
            file_id,
            range: Range::new(self.start_line, self.start_col, self.end_line, self.end_col),
            file_path: "<unknown>".into(),
            signature: None,
            doc_comment: None,
            module_path: None,
            visibility: Visibility::Private,
            scope_context: None,
        })
    }
}

// --- StringTable ---

/// Interned string storage for compact symbol representations.
///
/// Stores strings as null-terminated byte sequences in a contiguous buffer.
/// Returns offsets that can be used to retrieve strings later.
#[derive(Debug)]
pub struct StringTable {
    data: Vec<u8>,
    offsets: std::collections::HashMap<String, u32>,
}

impl Default for StringTable {
    fn default() -> Self {
        Self {
            data: vec![0], // null terminator at offset 0
            offsets: std::collections::HashMap::new(),
        }
    }
}

impl StringTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern a string, returning its offset. Reuses existing entries.
    pub fn intern(&mut self, s: &str) -> u32 {
        if let Some(&offset) = self.offsets.get(s) {
            return offset;
        }
        let offset = self.data.len() as u32;
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
        self.offsets.insert(s.to_string(), offset);
        offset
    }

    /// Look up a string by offset.
    pub fn get(&self, offset: u32) -> Option<&str> {
        let start = offset as usize;
        if start >= self.data.len() {
            return None;
        }
        let end = self.data[start..]
            .iter()
            .position(|&b| b == 0)
            .map(|pos| start + pos)?;
        std::str::from_utf8(&self.data[start..end]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem;

    #[test]
    fn test_symbol_creation() {
        let id = SymbolId::new(1).unwrap();
        let file_id = FileId::new(10).unwrap();
        let range = Range::new(5, 10, 5, 20);

        let sym = Symbol::new(id, "test_function", SymbolKind::Function, file_id, range);

        assert_eq!(sym.id, id);
        assert_eq!(sym.as_name(), "test_function");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.file_id, file_id);
        assert_eq!(sym.range, range);
        assert!(sym.signature.is_none());
        assert_eq!(sym.visibility, Visibility::Private);
    }

    #[test]
    fn test_symbol_builders() {
        let sym = Symbol::new(
            SymbolId::new(1).unwrap(),
            "add",
            SymbolKind::Function,
            FileId::new(1).unwrap(),
            Range::new(1, 0, 3, 1),
        )
        .with_signature("fn add(a: i32, b: i32) -> i32")
        .with_doc("Adds two numbers.")
        .with_module_path("crate::math")
        .with_visibility(Visibility::Public)
        .with_scope(ScopeContext::Module)
        .with_file_path("src/math.rs");

        assert_eq!(sym.as_signature(), Some("fn add(a: i32, b: i32) -> i32"));
        assert_eq!(sym.as_doc_comment(), Some("Adds two numbers."));
        assert_eq!(sym.as_module_path(), Some("crate::math"));
        assert_eq!(sym.visibility, Visibility::Public);
        assert_eq!(sym.scope_context, Some(ScopeContext::Module));
        assert_eq!(&*sym.file_path, "src/math.rs");
    }

    #[test]
    fn test_compact_symbol_size_and_alignment() {
        assert_eq!(mem::size_of::<CompactSymbol>(), 32);
        assert_eq!(mem::align_of::<CompactSymbol>(), 32);
    }

    #[test]
    fn test_string_table_intern_and_get() {
        let mut table = StringTable::new();

        let o1 = table.intern("hello");
        let o2 = table.intern("world");
        let o3 = table.intern("hello"); // reuse

        assert_eq!(o1, 1); // offset 0 is the initial null
        assert_ne!(o1, o2);
        assert_eq!(o1, o3);

        assert_eq!(table.get(o1), Some("hello"));
        assert_eq!(table.get(o2), Some("world"));
        assert_eq!(table.get(999), None);
    }

    #[test]
    fn test_compact_symbol_roundtrip() {
        let mut string_table = StringTable::new();

        let original = Symbol::new(
            SymbolId::new(42).unwrap(),
            "test_method",
            SymbolKind::Method,
            FileId::new(7).unwrap(),
            Range::new(10, 5, 15, 20),
        );

        let compact = original.to_compact(&mut string_table);
        let restored = compact.to_symbol(&string_table).unwrap();

        assert_eq!(original.id, restored.id);
        assert_eq!(original.name, restored.name);
        assert_eq!(original.kind, restored.kind);
        assert_eq!(original.file_id, restored.file_id);
        assert_eq!(original.range, restored.range);
    }

    #[test]
    fn test_all_symbol_kinds_compact_roundtrip() {
        let mut string_table = StringTable::new();

        for i in 0..SymbolKind::COUNT as u8 {
            let kind = SymbolKind::from_u8(i).unwrap();
            let sym = Symbol::new(
                SymbolId::new(u32::from(i) + 1).unwrap(),
                format!("test_{i}"),
                kind,
                FileId::new(1).unwrap(),
                Range::new(1, 0, 1, 10),
            );

            let compact = sym.to_compact(&mut string_table);
            let restored = compact.to_symbol(&string_table).unwrap();
            assert_eq!(sym.kind, restored.kind);
        }
    }

    #[test]
    fn test_symbol_display() {
        let sym = Symbol::new(
            SymbolId::new(1).unwrap(),
            "process",
            SymbolKind::Function,
            FileId::new(1).unwrap(),
            Range::new(10, 0, 20, 1),
        )
        .with_signature("fn process(data: &[u8]) -> Result<()>");

        let display = sym.to_string();
        assert!(display.contains("process"));
        assert!(display.contains("Signature:"));
        assert!(display.contains("Function"));
    }

    #[test]
    fn test_scope_context_default() {
        let scope = ScopeContext::default();
        assert_eq!(scope, ScopeContext::Module);
    }

    #[test]
    fn test_scope_context_local() {
        let scope = ScopeContext::Local {
            hoisted: true,
            parent_name: Some(compact_string("main")),
            parent_kind: Some(SymbolKind::Function),
        };
        if let ScopeContext::Local { hoisted, parent_name, parent_kind } = &scope {
            assert!(hoisted);
            assert_eq!(parent_name.as_deref(), Some("main"));
            assert_eq!(*parent_kind, Some(SymbolKind::Function));
        } else {
            panic!("expected Local");
        }
    }

    #[test]
    fn test_serde_roundtrip() {
        let sym = Symbol::new(
            SymbolId::new(5).unwrap(),
            "MyStruct",
            SymbolKind::Struct,
            FileId::new(3).unwrap(),
            Range::new(1, 0, 10, 1),
        )
        .with_visibility(Visibility::Public)
        .with_scope(ScopeContext::Module);

        let json = serde_json::to_string(&sym).unwrap();
        let back: Symbol = serde_json::from_str(&json).unwrap();
        assert_eq!(sym, back);
    }
}
