//! Import statement representation.

use crate::code::types::FileId;

/// An import statement extracted from source code.
#[derive(Debug, Clone)]
pub struct Import {
    /// Import path (e.g., "std::collections::HashMap").
    pub path: String,
    /// Optional alias (e.g., "use foo::Bar as Baz").
    pub alias: Option<String>,
    /// File where this import appears.
    pub file_id: FileId,
    /// Glob import (e.g., `use foo::*`).
    pub is_glob: bool,
    /// Type-only import (TypeScript `import type { Foo }`).
    pub is_type_only: bool,
}
