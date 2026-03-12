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
}

impl RelationKind {
    /// Get the inverse relationship kind.
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
        ];
        for kind in all {
            assert_eq!(kind.inverse().inverse(), kind, "inverse of inverse should be identity for {kind:?}");
        }
    }

    #[test]
    fn test_inverse_pairs() {
        assert_eq!(RelationKind::Calls.inverse(), RelationKind::CalledBy);
        assert_eq!(RelationKind::Extends.inverse(), RelationKind::ExtendedBy);
        assert_eq!(RelationKind::Implements.inverse(), RelationKind::ImplementedBy);
        assert_eq!(RelationKind::Uses.inverse(), RelationKind::UsedBy);
        assert_eq!(RelationKind::Defines.inverse(), RelationKind::DefinedIn);
        assert_eq!(RelationKind::References.inverse(), RelationKind::ReferencedBy);
    }
}
