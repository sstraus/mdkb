//! What the duplication pass looks at, and what the call graph rules out.
//!
//! Two queries, both written here rather than reused from the symbol readers:
//! `SYMBOL_COLUMNS` does not select `owner_name`, and the owner is half the
//! signal — two methods of the same class sharing a shape is design, not
//! duplication.

use std::collections::HashSet;

use rusqlite::{Connection, params};

use crate::code::storage::{TIER_UNPLACED, resolved_edges, visibility_from_i64};
use crate::code::symbol::Visibility;

/// The most distant resolution tier a call edge may reach and still suppress.
///
/// Tier 1 matched the owner the call site wrote, tier 2 matched a module path
/// backed by a real import. Everything past that is a guess over names, and
/// [`TIER_UNPLACED`] is every symbol of that name anywhere in the index —
/// 6.95 candidates per edge on this repository, against 1.03 for tier 1.
///
/// Suppression must fail open. A false positive costs the reader one line; a
/// false suppression silently deletes the duplicate they asked to be shown, and
/// nothing in the output says it happened. So an edge that no rule placed
/// counts as "unknown", never as "these two are related".
pub const MAX_SUPPRESSING_TIER: i64 = 2;

/// A symbol the duplication pass may report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupCandidate {
    pub id: i64,
    pub name: String,
    pub file_path: String,
    pub module_path: Option<String>,
    /// `Function` or `Method`, as stored. Part of the member key: a free
    /// function and a method of the same name are not the same symbol.
    pub kind: String,
    /// The file's language, `None` when the indexer could not name one. Part of
    /// the member key, so a Rust `parse` and a Python `parse` at the same
    /// relative path cannot be read as the same member.
    pub language: Option<String>,
    /// The signature as stored, `None` when the parser produced none. Part of
    /// the member key: it is the only field that separates two overloads.
    pub signature: Option<String>,
    /// The class or trait this is a member of, `None` for a free function.
    pub owner_name: Option<String>,
    /// How far the symbol reaches. Ranking uses it: duplicated `pub` API is
    /// copied by callers who cannot see it is duplicated.
    pub visibility: Visibility,
    /// 0-based inclusive tree-sitter rows, as stored.
    pub line_start: u32,
    pub line_end: u32,
}

impl DupCandidate {
    /// Lines the symbol spans, inclusive.
    pub fn lines(&self) -> u32 {
        self.line_end.saturating_sub(self.line_start) + 1
    }
}

/// Symbols worth comparing: functions and methods whose span is at least
/// `min_lines`.
///
/// Only `Function` and `Method`. A `Field`, a `Constant` or a `Module` cannot
/// hold duplicated *logic*, and reporting two constants of the same shape is
/// noise that buries the findings that matter.
///
/// The line span is the first half of the complexity filter and the only half
/// SQL can do: the real filter is the named-node count, which does not exist
/// until something has parsed the body. Filtering here is what keeps the parse
/// from touching every file in the repository — see [`files_of`].
pub fn candidates(conn: &Connection, min_lines: u32) -> rusqlite::Result<Vec<DupCandidate>> {
    // LEFT JOIN, not JOIN: a symbol whose file row is missing still has to be
    // reported. Losing a finding because a join failed is the one outcome this
    // pass must never produce.
    let mut stmt = conn.prepare(
        "SELECT s.id, s.name, s.file_path, s.module_path, s.owner_name, s.visibility, \
                s.line_start, s.line_end, s.kind, s.signature, f.language \
         FROM code_symbols s \
         LEFT JOIN code_files f ON f.id = s.file_id \
         WHERE s.kind IN ('Function', 'Method') \
           AND s.line_end IS NOT NULL \
           AND s.line_end - s.line_start + 1 >= ?1 \
         ORDER BY s.file_path, s.line_start",
    )?;
    let rows = stmt.query_map(params![min_lines], |row| {
        Ok(DupCandidate {
            id: row.get(0)?,
            name: row.get(1)?,
            file_path: row.get(2)?,
            module_path: row.get(3)?,
            owner_name: row.get(4)?,
            visibility: visibility_from_i64(row.get(5)?),
            line_start: row.get(6)?,
            line_end: row.get(7)?,
            kind: row.get(8)?,
            signature: row.get(9)?,
            language: row.get(10)?,
        })
    })?;
    rows.collect()
}

/// The distinct files the candidates live in, in first-seen order.
///
/// The parse reads these and nothing else. A repository where one file in ten
/// holds a function long enough to matter is parsed a tenth as many times.
pub fn files_of(candidates: &[DupCandidate]) -> Vec<String> {
    let mut seen = HashSet::new();
    candidates
        .iter()
        .filter(|c| seen.insert(c.file_path.as_str()))
        .map(|c| c.file_path.clone())
        .collect()
}

/// Symbol pairs joined by a call edge the cascade actually placed.
///
/// Returned with the lower id first, so a caller can look a pair up without
/// knowing which way the call went — for suppression the direction is
/// irrelevant: a wrapper resembling what it wraps is not a duplicate either way
/// round.
///
/// `tier = nearest` keeps only the first rule of the cascade that yielded
/// anything, which is how [`resolved_edges`] is meant to be read; `nearest <=
/// max_tier` then drops every edge whose best rule was a guess. An edge that
/// only ever reached [`TIER_UNPLACED`] has `nearest = 7` and never appears
/// here, which is the whole point: it means "unknown", and unknown must not
/// delete a cluster.
pub fn confident_call_pairs(
    conn: &Connection,
    max_tier: i64,
) -> rusqlite::Result<HashSet<(i64, i64)>> {
    debug_assert!(
        max_tier < TIER_UNPLACED,
        "suppressing at the unplaced tier would delete clusters on a bare name match"
    );
    let edges = resolved_edges("r.from_symbol_id IS NOT NULL");
    let mut stmt = conn.prepare(&format!(
        "SELECT from_id, sym_id FROM ({edges}) \
         WHERE tier = nearest AND nearest <= ?1 AND from_id <> sym_id"
    ))?;
    let rows = stmt.query_map(params![max_tier], |row| {
        let (a, b): (i64, i64) = (row.get(0)?, row.get(1)?);
        Ok(if a <= b { (a, b) } else { (b, a) })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code::storage::schema::init_schema;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO code_files (id, path, rel_path, hash, language) \
             VALUES (1, 'src/a.rs', 'src/a.rs', 'h', 'rust'), \
                    (2, 'src/b.rs', 'src/b.rs', 'h', 'rust')",
            [],
        )
        .unwrap();
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn sym(
        conn: &Connection,
        id: i64,
        name: &str,
        kind: &str,
        file_id: i64,
        path: &str,
        start: u32,
        end: u32,
        owner: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO code_symbols \
             (id, name, kind, file_id, file_path, line_start, line_end, visibility, owner_name) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
            params![id, name, kind, file_id, path, start, end, owner],
        )
        .unwrap();
    }

    #[test]
    fn only_functions_and_methods_are_candidates() {
        let conn = db();
        sym(&conn, 1, "f", "Function", 1, "src/a.rs", 0, 20, None);
        sym(&conn, 2, "m", "Method", 1, "src/a.rs", 30, 50, Some("T"));
        sym(&conn, 3, "S", "Struct", 1, "src/a.rs", 60, 90, None);
        sym(&conn, 4, "C", "Constant", 1, "src/a.rs", 100, 130, None);
        sym(&conn, 5, "M", "Module", 1, "src/a.rs", 140, 200, None);

        let got = candidates(&conn, 5).unwrap();

        assert_eq!(
            got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["f", "m"],
            "a struct, a constant and a module hold no duplicated logic"
        );
    }

    #[test]
    fn a_body_shorter_than_the_minimum_is_not_a_candidate() {
        let conn = db();
        sym(&conn, 1, "tiny", "Function", 1, "src/a.rs", 0, 2, None);
        sym(&conn, 2, "big", "Function", 1, "src/a.rs", 10, 30, None);

        let got = candidates(&conn, 5).unwrap();

        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "big");
        // Inclusive: rows 10..=30 is 21 lines, not 20.
        assert_eq!(got[0].lines(), 21);
    }

    #[test]
    fn a_span_exactly_at_the_minimum_is_kept() {
        // Off-by-one at the boundary silently drops a whole band of symbols.
        let conn = db();
        sym(&conn, 1, "exact", "Function", 1, "src/a.rs", 0, 4, None);

        assert_eq!(candidates(&conn, 5).unwrap().len(), 1);
        assert_eq!(candidates(&conn, 6).unwrap().len(), 0);
    }

    #[test]
    fn a_symbol_with_no_end_line_is_not_a_candidate() {
        // line_end is nullable. Without the guard the arithmetic is NULL and
        // the row is dropped anyway — but by accident, not by decision.
        let conn = db();
        conn.execute(
            "INSERT INTO code_symbols (id, name, kind, file_id, file_path, line_start, visibility) \
             VALUES (1, 'headless', 'Function', 1, 'src/a.rs', 0, 0)",
            [],
        )
        .unwrap();

        assert!(candidates(&conn, 1).unwrap().is_empty());
    }

    #[test]
    fn owner_name_travels_with_the_candidate() {
        // SYMBOL_COLUMNS does not select it, which is why this query exists.
        let conn = db();
        sym(&conn, 1, "m", "Method", 1, "src/a.rs", 0, 20, Some("Store"));
        sym(&conn, 2, "f", "Function", 1, "src/a.rs", 30, 50, None);

        let got = candidates(&conn, 5).unwrap();

        assert_eq!(got[0].owner_name.as_deref(), Some("Store"));
        assert_eq!(got[1].owner_name, None);
    }

    #[test]
    fn only_files_holding_a_candidate_are_listed() {
        let conn = db();
        sym(&conn, 1, "a1", "Function", 1, "src/a.rs", 0, 20, None);
        sym(&conn, 2, "a2", "Function", 1, "src/a.rs", 30, 50, None);
        sym(&conn, 3, "b1", "Function", 2, "src/b.rs", 0, 20, None);
        // src/c.rs holds only a short function and must not be read at all.
        sym(&conn, 4, "c1", "Function", 2, "src/c.rs", 0, 1, None);

        let files = files_of(&candidates(&conn, 5).unwrap());

        assert_eq!(files, ["src/a.rs", "src/b.rs"], "each file once, in order");
    }

    /// A `Calls` edge whose qualifier names the target's owner resolves at tier
    /// 1 — the cascade placed it, so the pair is a wrapper and its callee, not
    /// duplication.
    #[test]
    fn a_tier_1_call_edge_suppresses_the_pair() {
        let conn = db();
        sym(&conn, 1, "wrapper", "Function", 1, "src/a.rs", 0, 20, None);
        sym(
            &conn,
            2,
            "open",
            "Method",
            1,
            "src/a.rs",
            30,
            50,
            Some("Store"),
        );
        conn.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_qualifier) \
             VALUES (1, 'wrapper', 'open', 'Calls', 1, 'Store')",
            [],
        )
        .unwrap();

        let pairs = confident_call_pairs(&conn, MAX_SUPPRESSING_TIER).unwrap();

        assert!(
            pairs.contains(&(1, 2)),
            "tier 1 is a placed edge: {pairs:?}"
        );
    }

    /// The raw cascade, so a test can show which tier an edge reached without
    /// asking `confident_call_pairs` to suppress at a tier it must refuse.
    fn nearest_tiers(conn: &Connection) -> Vec<(i64, i64, i64)> {
        let edges = resolved_edges("r.from_symbol_id IS NOT NULL");
        let mut stmt = conn
            .prepare(&format!(
                "SELECT from_id, sym_id, nearest FROM ({edges}) WHERE tier = nearest"
            ))
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    /// Tier 2 — the qualifier names a module path, and an import backs it. The
    /// far side of `MAX_SUPPRESSING_TIER`, so it must still suppress.
    #[test]
    fn a_tier_2_call_edge_suppresses_the_pair() {
        let conn = db();
        sym(&conn, 1, "caller", "Function", 1, "src/a.rs", 0, 20, None);
        conn.execute(
            "INSERT INTO code_symbols \
             (id, name, kind, file_id, file_path, line_start, line_end, visibility, module_path) \
             VALUES (2, 'helper', 'Function', 2, 'src/b.rs', 0, 20, 0, 'util')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_qualifier) \
             VALUES (1, 'caller', 'helper', 'Calls', 1, 'util')",
            [],
        )
        .unwrap();

        assert_eq!(nearest_tiers(&conn), [(1, 2, 2)]);
        assert!(
            confident_call_pairs(&conn, MAX_SUPPRESSING_TIER)
                .unwrap()
                .contains(&(1, 2))
        );
    }

    /// Tier 4 — "declared in the calling file" is a rule, but a weak one: it
    /// places nothing, it just notices proximity. One step past the cut, so it
    /// must not suppress. This is the assertion that pins the constant.
    #[test]
    fn an_edge_placed_only_by_proximity_does_not_suppress() {
        let conn = db();
        sym(&conn, 1, "caller", "Function", 1, "src/a.rs", 0, 20, None);
        sym(&conn, 2, "helper", "Function", 1, "src/a.rs", 30, 50, None);
        conn.execute(
            "INSERT INTO code_relationships (from_symbol_id, from_name, to_name, kind, file_id) \
             VALUES (1, 'caller', 'helper', 'Calls', 1)",
            [],
        )
        .unwrap();

        assert_eq!(nearest_tiers(&conn), [(1, 2, 4)], "same file is tier 4");
        assert!(
            confident_call_pairs(&conn, MAX_SUPPRESSING_TIER)
                .unwrap()
                .is_empty(),
            "tier 4 is a guess, and a guess must not delete a finding"
        );
    }

    /// The case the whole fail-open rule exists for. A bare call to a name that
    /// two symbols share reaches only tier 7, and tier 7 means "no rule placed
    /// this". Suppressing on it would delete a real duplicate because an
    /// unrelated third function happens to share a name.
    #[test]
    fn a_tier_7_only_edge_does_not_suppress() {
        let conn = db();
        // Different files, no qualifier, no import, no shared module path:
        // nothing for the cascade to match on.
        sym(&conn, 1, "caller", "Function", 1, "src/a.rs", 0, 20, None);
        sym(&conn, 2, "helper", "Function", 2, "src/b.rs", 0, 20, None);
        conn.execute(
            "INSERT INTO code_relationships (from_symbol_id, from_name, to_name, kind, file_id) \
             VALUES (1, 'caller', 'helper', 'Calls', 1)",
            [],
        )
        .unwrap();

        // The edge is real and it does resolve — at the tier that means the
        // resolver gave up. Without this the next assertion would pass just as
        // well against a query that found no edges at all.
        assert_eq!(nearest_tiers(&conn), [(1, 2, TIER_UNPLACED)]);

        let pairs = confident_call_pairs(&conn, MAX_SUPPRESSING_TIER).unwrap();

        assert!(
            pairs.is_empty(),
            "unknown must never delete a cluster: {pairs:?}"
        );
    }

    /// A symbol that calls itself is one symbol, not a pair, and must not be
    /// handed to a clusterer that would union it with itself.
    #[test]
    fn a_self_call_is_not_a_pair() {
        let conn = db();
        // Owner plus matching qualifier, so the edge resolves at tier 1 and the
        // test cannot pass merely because the tier filter rejected it.
        sym(
            &conn,
            1,
            "recurse",
            "Method",
            1,
            "src/a.rs",
            0,
            20,
            Some("T"),
        );
        conn.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_qualifier) \
             VALUES (1, 'recurse', 'recurse', 'Calls', 1, 'T')",
            [],
        )
        .unwrap();

        assert_eq!(
            nearest_tiers(&conn),
            [(1, 1, 1)],
            "tier 1, and it is a loop"
        );
        assert!(
            confident_call_pairs(&conn, MAX_SUPPRESSING_TIER)
                .unwrap()
                .is_empty(),
            "a self-edge is not a pair"
        );
    }

    #[test]
    fn a_pair_comes_back_with_the_lower_id_first() {
        // Direction is irrelevant to suppression, so the caller must not have
        // to try both orders.
        let conn = db();
        sym(
            &conn,
            5,
            "callee",
            "Method",
            1,
            "src/a.rs",
            0,
            20,
            Some("T"),
        );
        sym(&conn, 9, "caller", "Function", 1, "src/a.rs", 30, 50, None);
        conn.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_qualifier) \
             VALUES (9, 'caller', 'callee', 'Calls', 1, 'T')",
            [],
        )
        .unwrap();

        let pairs = confident_call_pairs(&conn, MAX_SUPPRESSING_TIER).unwrap();

        assert!(pairs.contains(&(5, 9)), "normalised, not (9, 5): {pairs:?}");
    }

    #[test]
    fn a_non_call_relationship_never_suppresses() {
        // Only `Calls` explains a shared shape as a wrapper. `Uses` between two
        // functions of the same shape is exactly the duplication being hunted.
        let conn = db();
        // Owner and qualifier line up, so this would be tier 1 if only its kind
        // were `Calls`. The test therefore fails on the kind, not on the tier.
        sym(&conn, 1, "a", "Function", 1, "src/a.rs", 0, 20, None);
        sym(&conn, 2, "b", "Method", 1, "src/a.rs", 30, 50, Some("T"));
        conn.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_qualifier) \
             VALUES (1, 'a', 'b', 'Uses', 1, 'T')",
            [],
        )
        .unwrap();

        assert!(
            nearest_tiers(&conn).is_empty(),
            "resolved_edges is Calls-only"
        );
        assert!(
            confident_call_pairs(&conn, MAX_SUPPRESSING_TIER)
                .unwrap()
                .is_empty()
        );
    }
}
