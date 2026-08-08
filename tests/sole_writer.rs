//! The daemon is the sole writer.
//!
//! Story 018-56b2. `mdkb mcp` and `mdkb hook` already route through the daemon;
//! the plain CLI never adopted the pattern, so every `mdkb memory add` was an
//! independent writer process with its own connection, its own migration run and
//! its own virtual-table init, racing the long-lived daemon on the same file.
//! Recurring corruption is confined to the memory write path, and multi-process
//! concurrency is one of the two surviving hypotheses.
//!
//! Routing does NOT prove the corruption is fixed — if the cause is the
//! in-process sqlite-vec extension, single-writer changes nothing — but it
//! removes a whole family of causes by construction, which is what makes the
//! remaining search space small enough to reason about.

use mdkb::cli::Command;
use mdkb::core::routing::{Routing, routing_for};

/// Parse an argv into the `Command` the CLI would dispatch.
fn parse(args: &[&str]) -> Command {
    use clap::Parser;
    let mut argv = vec!["mdkb"];
    argv.extend_from_slice(args);
    mdkb::cli::Cli::try_parse_from(argv)
        .unwrap_or_else(|e| panic!("`mdkb {}` must parse: {e}", args.join(" ")))
        .command
}

/// Every command that writes must be classified as a mutation.
///
/// The list is deliberately explicit rather than derived: a new mutating command
/// added without a thought about routing is exactly the regression this story
/// exists to prevent, and a derived rule would silently absorb it.
#[test]
fn mutating_commands_are_classified_as_mutations() {
    let mutating: &[&[&str]] = &[
        &["init"],
        &["update"],
        &["embed"],
        &["memory", "add", "x", "--title", "T", "--content", "C"],
        &["memory", "rm", "x"],
        &["memory", "link", "a", "supports", "b"],
        &["memory", "confirm", "x", "--outcome", "worked"],
        &["memory", "sync"],
        &["memory", "prune"],
        &["memory", "import", "f.json"],
        &["collection", "add", "n", "p"],
        &["collection", "remove", "n"],
        &["collection", "rename", "a", "b"],
        &["evolve", "supersedes", "a", "b"],
        &["compact"],
        &["code", "index"],
    ];
    for args in mutating {
        assert_eq!(
            routing_for(&parse(args)),
            Routing::Mutation,
            "`mdkb {}` writes to the store and must route through the daemon",
            args.join(" ")
        );
    }
}

/// And every read must NOT be, or routing them would make the daemon a
/// bottleneck for work that never needed it.
#[test]
fn read_commands_are_not_classified_as_mutations() {
    let reads: &[&[&str]] = &[
        &["search", "q"],
        &["get", "x"],
        &["stats"],
        &["graph", "hubs"],
        &["memory", "list"],
        &["memory", "show", "x"],
        &["code", "find", "sym"],
        &["surface"],
        &["cheatsheet"],
    ];
    for args in reads {
        assert_ne!(
            routing_for(&parse(args)),
            Routing::Mutation,
            "`mdkb {}` does not write and must not be routed",
            args.join(" ")
        );
    }
}

/// The classifier must be exhaustive over the command tree. A variant nobody
/// classified would default to "not a mutation" and quietly become a second
/// writer — the precise failure being fixed.
#[test]
fn no_command_is_left_unclassified() {
    let unclassified = mdkb::core::routing::unclassified_commands();
    assert!(
        unclassified.is_empty(),
        "these commands have no routing decision — classify each as Mutation, \
         Read or Local: {unclassified:?}"
    );
}

/// `MDKB_NO_DAEMON=1` is the single documented escape hatch, and the only way a
/// CLI process writes directly.
#[test]
fn the_escape_hatch_is_the_only_direct_write_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let home = tempfile::tempdir().expect("home");

    // With the escape hatch set, the write happens in-process and succeeds even
    // with no daemon reachable and none spawnable.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args([
            "memory",
            "add",
            "direct",
            "--title",
            "Direct",
            "--content",
            "written in-process",
        ])
        .current_dir(&root)
        .env("HOME", home.path())
        .env("MDKB_NO_SPAWN", "1")
        .env("MDKB_NO_DAEMON", "1")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "MDKB_NO_DAEMON=1 must still write directly: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let ctx = mdkb::core::Context::open_read_only(&root).expect("open read-only");
    assert!(
        mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "direct")
            .unwrap()
            .is_some(),
        "the entry must have been written"
    );
}

/// With no daemon reachable and no escape hatch, a mutation still has to work —
/// falling back in-process rather than failing, exactly as the hook path does
/// (story 021-0636). A routing layer that turns a daemon outage into a broken
/// CLI is worse than no routing.
#[test]
fn a_mutation_falls_back_in_process_when_the_daemon_is_unreachable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let home = tempfile::tempdir().expect("home");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args([
            "memory",
            "add",
            "fallback",
            "--title",
            "Fallback",
            "--content",
            "written despite no daemon",
        ])
        .current_dir(&root)
        .env("HOME", home.path())
        .env("MDKB_NO_SPAWN", "1")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "a daemon outage must not break the CLI: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let ctx = mdkb::core::Context::open_read_only(&root).expect("open read-only");
    assert!(
        mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "fallback")
            .unwrap()
            .is_some(),
        "the fallback must actually have performed the write"
    );
}

/// The routing is wired, not merely classified.
///
/// A classifier nothing calls is worse than none — it reads as a property the
/// program has. This asserts the commands the daemon already exposes an RPC for
/// actually resolve to one.
#[test]
fn routed_mutations_resolve_to_a_daemon_rpc() {
    use mdkb::core::routing::daemon_method;

    let (method, params) =
        daemon_method(&parse(&["memory", "rm", "gone"])).expect("memory rm must route");
    assert_eq!(method, "memory_delete");
    assert_eq!(params["id"], "gone");

    let (method, _) = daemon_method(&parse(&["update"])).expect("update must route");
    assert_eq!(method, "update");

    // A read never resolves to a mutation RPC.
    assert!(daemon_method(&parse(&["search", "q"])).is_none());
}

/// The shortfall is enumerable rather than implied.
///
/// The daemon does not yet expose an RPC for every mutation, and a story
/// claiming "the daemon is the sole writer" while half the commands still open
/// their own connection is worse than one that says which half — the first reads
/// as done and stops anyone looking.
#[test]
fn the_unrouted_mutations_are_named() {
    let gap = mdkb::core::routing::routing_gap();
    assert!(
        !gap.is_empty(),
        "if the gap is finally empty, delete routing_gap() and this test rather \
         than leaving an assertion that can no longer fail"
    );
    for entry in &gap {
        assert!(
            !entry.is_empty(),
            "every gap entry must name the commands it covers"
        );
    }
}
