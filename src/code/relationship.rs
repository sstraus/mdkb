//! Relationship types for the code intelligence graph.
//!
//! Symbols are connected by directional relationships: calls, extends,
//! implements, uses, defines, references. Each relationship has an
//! inverse (e.g. Calls/CalledBy) for bidirectional traversal.

use serde::{Deserialize, Serialize};

use super::types::SymbolId;

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

/// A weighted relationship with optional source location metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relationship {
    pub kind: RelationKind,
    pub weight: f32,
    pub metadata: Option<RelationshipMetadata>,
}

/// Source location and context where a relationship occurs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RelationshipMetadata {
    pub line: Option<u32>,
    pub column: Option<u16>,
    pub context: Option<Box<str>>,
}

/// A directed edge in the relationship graph.
#[derive(Debug)]
pub struct RelationshipEdge {
    pub source: SymbolId,
    pub target: SymbolId,
    pub relationship: Relationship,
}

// --- RelationKind ---

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

    /// Whether this is a type hierarchy relationship (extends/implements).
    pub fn is_hierarchical(self) -> bool {
        matches!(
            self,
            Self::Extends | Self::ExtendedBy | Self::Implements | Self::ImplementedBy
        )
    }

    /// Whether this is a usage relationship (calls/uses/references).
    pub fn is_usage(self) -> bool {
        matches!(
            self,
            Self::Calls
                | Self::CalledBy
                | Self::Uses
                | Self::UsedBy
                | Self::References
                | Self::ReferencedBy
        )
    }
}

// --- Relationship ---

impl Relationship {
    pub fn new(kind: RelationKind) -> Self {
        Self {
            kind,
            weight: 1.0,
            metadata: None,
        }
    }

    pub fn with_weight(mut self, weight: f32) -> Self {
        self.weight = weight;
        self
    }

    pub fn with_metadata(mut self, metadata: RelationshipMetadata) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

// --- RelationshipMetadata ---

impl RelationshipMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn at_position(mut self, line: u32, column: u16) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    pub fn with_context(mut self, context: impl Into<Box<str>>) -> Self {
        self.context = Some(context.into());
        self
    }
}

// --- RelationshipEdge ---

impl RelationshipEdge {
    pub fn new(source: SymbolId, target: SymbolId, relationship: Relationship) -> Self {
        Self {
            source,
            target,
            relationship,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_relationship_creation() {
        let rel = Relationship::new(RelationKind::Calls);
        assert_eq!(rel.kind, RelationKind::Calls);
        assert_eq!(rel.weight, 1.0);
        assert!(rel.metadata.is_none());
    }

    #[test]
    fn test_relationship_with_weight() {
        let rel = Relationship::new(RelationKind::Extends).with_weight(0.8);
        assert_eq!(rel.weight, 0.8);
    }

    #[test]
    fn test_relationship_with_metadata() {
        let metadata = RelationshipMetadata::new()
            .at_position(10, 5)
            .with_context("inside main function");

        let rel = Relationship::new(RelationKind::Calls).with_metadata(metadata);

        let meta = rel.metadata.unwrap();
        assert_eq!(meta.line, Some(10));
        assert_eq!(meta.column, Some(5));
        assert_eq!(meta.context.as_deref(), Some("inside main function"));
    }

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

    #[test]
    fn test_hierarchical_classification() {
        assert!(RelationKind::Extends.is_hierarchical());
        assert!(RelationKind::ExtendedBy.is_hierarchical());
        assert!(RelationKind::Implements.is_hierarchical());
        assert!(RelationKind::ImplementedBy.is_hierarchical());

        assert!(!RelationKind::Calls.is_hierarchical());
        assert!(!RelationKind::Defines.is_hierarchical());
    }

    #[test]
    fn test_usage_classification() {
        assert!(RelationKind::Calls.is_usage());
        assert!(RelationKind::CalledBy.is_usage());
        assert!(RelationKind::Uses.is_usage());
        assert!(RelationKind::UsedBy.is_usage());
        assert!(RelationKind::References.is_usage());
        assert!(RelationKind::ReferencedBy.is_usage());

        assert!(!RelationKind::Defines.is_usage());
        assert!(!RelationKind::DefinedIn.is_usage());
        assert!(!RelationKind::Extends.is_usage());
    }

    #[test]
    fn test_relationship_edge() {
        let source = SymbolId::new(1).unwrap();
        let target = SymbolId::new(2).unwrap();
        let rel = Relationship::new(RelationKind::Calls);

        let edge = RelationshipEdge::new(source, target, rel);
        assert_eq!(edge.source, source);
        assert_eq!(edge.target, target);
        assert_eq!(edge.relationship.kind, RelationKind::Calls);
    }

    #[test]
    fn test_serde_roundtrip() {
        let rel = Relationship::new(RelationKind::Implements)
            .with_weight(0.9)
            .with_metadata(
                RelationshipMetadata::new()
                    .at_position(42, 8)
                    .with_context("impl Display for Foo"),
            );

        let json = serde_json::to_string(&rel).unwrap();
        let back: Relationship = serde_json::from_str(&json).unwrap();
        assert_eq!(rel, back);
    }
}
