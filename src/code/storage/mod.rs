//! SQLite storage backend for the code intelligence index.

pub mod repair;
pub mod schema;

mod sqlite;
pub use sqlite::{CallSite, CodeDb, ImpactRadius, NameMatch, TIER_UNPLACED};

/// The resolver's own tier cascade, for the duplication pass.
///
/// Re-exported rather than copied: duplication suppresses a pair only on a call
/// edge the resolver actually placed, and a second copy of the cascade would
/// drift from the one that resolves the graph — the suppression would then be
/// judging edges by rules the index no longer uses.
pub(crate) use sqlite::resolved_edges;

/// The `visibility` column's decoding, for the duplication pass.
///
/// Re-exported for the same reason: ranking a cluster by how public it is has
/// to agree with what the symbol readers report, and a second copy of the
/// discriminants would disagree the day a level is added.
pub(crate) use sqlite::visibility_from_i64;
