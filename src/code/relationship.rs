//! Relationship types for the code intelligence graph.
//!
//! Symbols are connected by directional relationships: calls, extends,
//! implements, uses, defines, references. Each relationship has an
//! inverse (e.g. Calls/CalledBy) for bidirectional traversal.

use serde::{Deserialize, Serialize};
use std::fmt;

/// The kind of relationship between two symbols.
///
/// Each variant has an inverse, accessible via [`RelationKind::inverse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationKind {
    Calls,
    CalledBy,
    Extends,
    ExtendedBy,
    Implements,
    ImplementedBy,
    Uses,
    UsedBy,
    Defines,
    DefinedIn,
    References,
    ReferencedBy,
    /// A macro invocation. Separate from [`Calls`](Self::Calls) because a macro
    /// is not a function: counted as a call, `assert!` and `println!` are 4 921
    /// edges of this repository's index pointing at functions that do not exist.
    Expands,
    ExpandedBy,
}

/// What a `Calls` edge turned out to point at.
///
/// A call the index cannot place is not the same as a call to nothing, and
/// reporting both as an empty list is the failure this type removes: 31 503 of
/// this repository's 44 202 `Calls` edges used to answer "no callers", which
/// reads as "nothing calls this" and is wrong for all but a few of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallTarget {
    /// The cascade placed the call inside this index. More than one id means
    /// the rules narrowed it that far and no further.
    Resolved(Vec<crate::code::types::SymbolId>),
    /// The call site named an owner or module this index does not contain:
    /// `std::fs::write`, `tempfile::tempdir`. Nameable, not indexable.
    External { qualifier: String, name: String },
    /// A bare name with no candidate — a method on a receiver whose type the
    /// index does not know.
    Unknown { name: String },
}

impl RelationKind {
    /// Get the inverse relationship kind.
    #[must_use]
    pub fn inverse(self) -> Self {
        match self {
            Self::Calls => Self::CalledBy,
            Self::CalledBy => Self::Calls,
            Self::Extends => Self::ExtendedBy,
            Self::ExtendedBy => Self::Extends,
            Self::Implements => Self::ImplementedBy,
            Self::ImplementedBy => Self::Implements,
            Self::Uses => Self::UsedBy,
            Self::UsedBy => Self::Uses,
            Self::Defines => Self::DefinedIn,
            Self::DefinedIn => Self::Defines,
            Self::References => Self::ReferencedBy,
            Self::ReferencedBy => Self::References,
            Self::Expands => Self::ExpandedBy,
            Self::ExpandedBy => Self::Expands,
        }
    }
}

impl fmt::Display for RelationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Calls => "Calls",
            Self::CalledBy => "CalledBy",
            Self::Extends => "Extends",
            Self::ExtendedBy => "ExtendedBy",
            Self::Implements => "Implements",
            Self::ImplementedBy => "ImplementedBy",
            Self::Uses => "Uses",
            Self::UsedBy => "UsedBy",
            Self::Defines => "Defines",
            Self::DefinedIn => "DefinedIn",
            Self::References => "References",
            Self::ReferencedBy => "ReferencedBy",
            Self::Expands => "Expands",
            Self::ExpandedBy => "ExpandedBy",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inverse_is_symmetric() {
        let all = [
            RelationKind::Calls,
            RelationKind::CalledBy,
            RelationKind::Extends,
            RelationKind::ExtendedBy,
            RelationKind::Implements,
            RelationKind::ImplementedBy,
            RelationKind::Uses,
            RelationKind::UsedBy,
            RelationKind::Defines,
            RelationKind::DefinedIn,
            RelationKind::References,
            RelationKind::ReferencedBy,
            RelationKind::Expands,
            RelationKind::ExpandedBy,
        ];
        for kind in all {
            assert_eq!(
                kind.inverse().inverse(),
                kind,
                "inverse of inverse should be identity for {kind:?}"
            );
        }
    }

    #[test]
    fn test_inverse_pairs() {
        assert_eq!(RelationKind::Calls.inverse(), RelationKind::CalledBy);
        assert_eq!(RelationKind::Extends.inverse(), RelationKind::ExtendedBy);
        assert_eq!(
            RelationKind::Implements.inverse(),
            RelationKind::ImplementedBy
        );
        assert_eq!(RelationKind::Uses.inverse(), RelationKind::UsedBy);
        assert_eq!(RelationKind::Defines.inverse(), RelationKind::DefinedIn);
        assert_eq!(
            RelationKind::References.inverse(),
            RelationKind::ReferencedBy
        );
    }
}
