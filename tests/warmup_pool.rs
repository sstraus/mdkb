//! The warmup candidate pool must be wider than what is emitted.
//!
//! Story 009-686d: `get_warmup_entries` ran
//! `ORDER BY access_count DESC LIMIT warmup_limit`, so the set handed to BOTH
//! handoff selection AND affinity ranking was already truncated to the globally
//! hottest N before any project scoping happened. Two consequences:
//!
//! * an in-scope handoff below the access_count cut is invisible, so a session
//!   correctly refuses to inject a foreign handoff and then silently gets none
//!   when a legitimate one existed;
//! * affinity ranking can only reorder what made the cut, so foreign entries are
//!   demoted but no cold in-scope entry can be promoted in to replace them.
//!
//! Inert on a store whose `warmup_limit` exceeds its entry count; it bites at
//! the default limit of 10, and on any store that outgrows its limit.

use mdkb::store::memory::{
    EntryStatus, EntryType, MemoryEntry, SourceType, add_entry, get_warmup_entries,
    newest_handoff_for_scope,
};

fn conn() -> rusqlite::Connection {
    let c = rusqlite::Connection::open_in_memory().expect("open");
    mdkb::store::schema::init_schema(&c).expect("schema");
    c
}

#[allow(clippy::too_many_arguments)]
fn seed(
    c: &rusqlite::Connection,
    id: &str,
    entry_type: EntryType,
    tags: &[&str],
    access_count: u64,
    updated_at: i64,
) {
    let e = MemoryEntry {
        id: id.to_string(),
        title: format!("Title for {id}"),
        content: format!("Body of {id}, long enough to be a real entry body."),
        entry_type,
        tags: tags.iter().map(|t| (*t).to_string()).collect(),
        status: EntryStatus::Active,
        created_at: 1_700_000_000,
        updated_at,
        superseded_by: None,
        access_count,
        last_accessed: None,
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };
    add_entry(c, &e).expect("add");
}

/// The handoff anchor must come from its own query, not from the
/// access_count-ranked pool.
///
/// A handoff is written once at the end of a session and read once at the start
/// of the next, so its `access_count` is 0 or 1 by construction — it is
/// structurally guaranteed to lose an `ORDER BY access_count DESC` race against
/// every warm topic in the store. The one entry the ranking most needs to find is
/// the one the ranking is least able to see.
#[test]
fn an_in_scope_handoff_is_found_below_the_access_count_cut() {
    let c = conn();
    // 30 hot entries crowd out anything cold at the default limit of 10.
    for i in 0..30 {
        seed(
            &c,
            &format!("hot-{i:02}"),
            EntryType::Topic,
            &[],
            500 + i,
            1,
        );
    }
    seed(
        &c,
        "the-handoff",
        EntryType::Handoff,
        &["proj-alpha"],
        0,
        9_999,
    );

    let found = newest_handoff_for_scope(&c, Some("proj-alpha"))
        .expect("query")
        .expect("an in-scope handoff exists and must be found");
    assert_eq!(
        found.id, "the-handoff",
        "the anchor must be selected by a dedicated query, so access_count \
         cannot hide it"
    );
}

/// Scoping still holds: a handoff belonging to another project must not be
/// injected just because it is the newest one in the store.
#[test]
fn a_foreign_handoff_is_not_returned_for_a_scoped_session() {
    let c = conn();
    seed(&c, "theirs", EntryType::Handoff, &["proj-beta"], 0, 9_999);
    assert!(
        newest_handoff_for_scope(&c, Some("proj-alpha"))
            .expect("query")
            .is_none(),
        "another project's handoff must never be injected"
    );
    // Unscoped, the newest handoff in the store is the right answer.
    assert!(
        newest_handoff_for_scope(&c, None).expect("query").is_some(),
        "an unscoped session takes the newest handoff overall"
    );
}

/// Newest wins among several in-scope handoffs.
#[test]
fn the_newest_in_scope_handoff_wins() {
    let c = conn();
    seed(&c, "older", EntryType::Handoff, &["proj-alpha"], 99, 1_000);
    seed(&c, "newer", EntryType::Handoff, &["proj-alpha"], 0, 2_000);
    let found = newest_handoff_for_scope(&c, Some("proj-alpha"))
        .expect("query")
        .expect("found");
    assert_eq!(
        found.id, "newer",
        "recency decides, not access_count — otherwise `newest handoff` means \
         `newest among the entries that happened to be warm`"
    );
}

/// The ranking pool must be wider than the emitted list, or affinity can only
/// demote and never promote.
#[test]
fn the_candidate_pool_is_wider_than_the_emitted_limit() {
    let c = conn();
    for i in 0..200 {
        seed(&c, &format!("e-{i:03}"), EntryType::Topic, &[], 1000 - i, 1);
    }

    let (_due, entries) = get_warmup_entries(&c, 10).expect("warmup");
    assert!(
        entries.len() > 10,
        "the pool handed to ranking must exceed warmup_limit, or a cold in-scope \
         entry can never be promoted into the emitted list; got {}",
        entries.len()
    );
    assert!(
        entries.len() <= mdkb::store::memory::WARMUP_POOL_HARD_CAP,
        "the pool must stay hard-capped so a huge store cannot blow the hook's \
         latency budget; got {}",
        entries.len()
    );
}

/// The widening must not change what a small store sees: a pool larger than the
/// corpus is just the corpus.
#[test]
fn a_small_store_yields_every_active_entry() {
    let c = conn();
    for i in 0..5 {
        seed(&c, &format!("e-{i}"), EntryType::Topic, &[], i as u64, 1);
    }
    let (_due, entries) = get_warmup_entries(&c, 10).expect("warmup");
    assert_eq!(entries.len(), 5, "no invented entries, no dropped ones");
}

/// Widening the pool must not widen the *output*. The emitted count stays
/// bounded by warmup_limit, which is what the token budget is sized against.
#[test]
fn the_pool_widening_does_not_widen_the_emitted_list() {
    let c = conn();
    for i in 0..200 {
        seed(&c, &format!("e-{i:03}"), EntryType::Topic, &[], 1000 - i, 1);
    }
    let (_due, entries) = get_warmup_entries(&c, 10).expect("warmup");
    let ranked = mdkb::mcp::dispatch::rank_warmup_entries_for_test(entries, 10, 0.0, 2_000, None);
    assert_eq!(
        ranked.len(),
        10,
        "ranking still truncates to warmup_limit — the pool is an input, not an \
         output"
    );
}

/// The in-scope handoff's BODY is what gets injected, and every handoff still
/// leaves the compact list.
///
/// These scenarios previously lived as in-memory unit tests over a helper that
/// both selected and stripped. Selection moved to a SQL query (a handoff's
/// access_count cannot win an access_count ordering), so the assertions moved
/// here, where they run against the query and the stripper that production
/// actually calls rather than a helper that only tests used.
#[test]
fn the_in_scope_handoff_body_is_injected_and_all_handoffs_leave_the_list() {
    let c = conn();
    seed(&c, "topic-a", EntryType::Topic, &[], 9, 1);
    // The globally newest handoff belongs to another project: injecting it would
    // hand this session someone else's state verbatim.
    seed(&c, "h-other", EntryType::Handoff, &["riscosity"], 0, 5_000);
    seed(&c, "h-mine-old", EntryType::Handoff, &["lattice"], 0, 1_000);
    seed(&c, "h-mine", EntryType::Handoff, &["lattice"], 0, 2_000);
    // A body shorter than HANDOFF_MIN_BODY_CHARS is deliberately not injected,
    // so give the anchor a realistic one.
    c.execute(
        "UPDATE memory_entries SET content = ?1 WHERE id = 'h-mine'",
        [format!(
            "Session state for h-mine. {}",
            "Restored context. ".repeat(6)
        )],
    )
    .unwrap();

    let anchor = newest_handoff_for_scope(&c, Some("lattice"))
        .expect("query")
        .expect("an in-scope handoff exists");
    assert_eq!(
        anchor.id, "h-mine",
        "newest IN-SCOPE wins over newest overall"
    );

    let (_due, pool) = get_warmup_entries(&c, 10).expect("warmup");
    let (body, rest) = mdkb::mcp::dispatch::strip_handoffs_for_test(pool, Some(&anchor));

    let body = body.expect("the in-scope handoff body must be injected");
    assert!(
        body.contains("h-mine"),
        "the injected body must be the anchor's: {body}"
    );
    assert!(
        rest.iter().all(|e| e.entry_type != EntryType::Handoff),
        "every handoff leaves the compact list — a truncated handoff title-line \
         is useless for restoration and would crowd out an entry that is not"
    );
    assert!(
        rest.iter().any(|e| e.id == "topic-a"),
        "non-handoff entries are untouched"
    );
}

/// No in-scope handoff means no body block at all. A foreign handoff is worse
/// than none.
#[test]
fn nothing_is_injected_when_no_handoff_is_in_scope() {
    let c = conn();
    seed(&c, "h-other", EntryType::Handoff, &["riscosity"], 0, 5_000);
    seed(&c, "topic-a", EntryType::Topic, &[], 9, 1);

    let anchor = newest_handoff_for_scope(&c, Some("lattice")).expect("query");
    assert!(anchor.is_none());

    let (_due, pool) = get_warmup_entries(&c, 10).expect("warmup");
    let (body, rest) = mdkb::mcp::dispatch::strip_handoffs_for_test(pool, None);
    assert!(body.is_none(), "no in-scope handoff means no injection");
    assert!(
        rest.iter().all(|e| e.entry_type != EntryType::Handoff),
        "handoffs are still dropped from the list even with nothing injected"
    );
}

/// A handoff whose body is only frontmatter is not worth injecting, but it is
/// still dropped from the compact list.
#[test]
fn an_empty_handoff_body_is_not_injected_but_is_still_dropped() {
    let c = conn();
    seed(&c, "topic-a", EntryType::Topic, &[], 9, 1);
    // Overwrite the seeded body with frontmatter only.
    seed(&c, "h-empty", EntryType::Handoff, &["lattice"], 0, 2_000);
    c.execute(
        "UPDATE memory_entries SET content = '---\ns: 1\n---\n' WHERE id = 'h-empty'",
        [],
    )
    .unwrap();

    let anchor = newest_handoff_for_scope(&c, Some("lattice"))
        .expect("query")
        .expect("found");
    let (_due, pool) = get_warmup_entries(&c, 10).expect("warmup");
    let (body, rest) = mdkb::mcp::dispatch::strip_handoffs_for_test(pool, Some(&anchor));

    assert!(body.is_none(), "a frontmatter-only body is not injected");
    assert!(
        rest.iter().all(|e| e.entry_type != EntryType::Handoff),
        "the handoff still leaves the compact list"
    );
}
