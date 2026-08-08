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
        &["code", "init"],
        &[
            "experiment",
            "create",
            "x",
            "--config-a",
            "{}",
            "--config-b",
            "{}",
        ],
        &["experiment", "end", "x"],
        &["experiment", "cancel", "x"],
        &["journal", "import", "journal.md"],
        &["journal", "import-all"],
        &["session", "index"],
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
        &["init"],
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
    drop(ctx);

    // A second routed mutation must take the same admission-refusal fallback.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(["memory", "rm", "fallback"])
        .current_dir(&root)
        .env("HOME", home.path())
        .env("MDKB_NO_SPAWN", "1")
        .output()
        .expect("run");
    assert!(
        out.status.success(),
        "a routed mutation must fall back rather than fail when no daemon exists: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let ctx = mdkb::core::Context::open_read_only(&root).expect("reopen read-only");
    assert!(
        mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "fallback")
            .unwrap()
            .is_none(),
        "the fallback reported success without performing the delete"
    );
}

/// A successfully routed mutation must not open the main store in the CLI
/// process. A fake daemon returns the typed success without touching SQLite;
/// any WAL/SHM sidecar therefore proves the CLI opened its own writer.
#[test]
fn a_routed_mutation_creates_no_local_sqlite_sidecars() {
    use std::io::{Read, Write};

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let db = root.join(".mdkb/index.sqlite");
    assert!(!std::path::PathBuf::from(format!("{}-wal", db.display())).exists());
    assert!(!std::path::PathBuf::from(format!("{}-shm", db.display())).exists());

    let home = tempfile::tempdir().expect("home");
    let daemon_home = home.path().join(".mdkb");
    std::fs::create_dir_all(&daemon_home).expect("daemon home");
    let listener = std::os::unix::net::UnixListener::bind(daemon_home.join("daemon-hook.sock"))
        .expect("bind fake daemon socket");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut header = [0_u8; 4];
            if stream.read_exact(&mut header).is_err() {
                continue;
            }
            let mut body = vec![0_u8; u32::from_le_bytes(header) as usize];
            if stream.read_exact(&mut body).is_err() {
                continue;
            }
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": { "result": "memory_added" }
            });
            let bytes = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(bytes.len() as u32).to_le_bytes())
                .unwrap();
            stream.write_all(&bytes).unwrap();
        }
    });

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args([
            "memory",
            "add",
            "remote",
            "--title",
            "Remote",
            "--content",
            "daemon-owned",
        ])
        .current_dir(&root)
        .env("HOME", home.path())
        .env("MDKB_NO_SPAWN", "1")
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!std::path::PathBuf::from(format!("{}-wal", db.display())).exists());
    assert!(!std::path::PathBuf::from(format!("{}-shm", db.display())).exists());
}

/// A daemon that took the request but never answered must NOT be second-guessed.
///
/// This is the other half of the fallback rule and the reason it needs one: once
/// the request is across the socket, silence is not evidence that nothing
/// happened. The daemon may be mid-write — `mdkb update` legitimately runs for
/// minutes — so a client that retries in-process becomes the second writer the
/// routing exists to remove, on the longest write in the program. Failing loudly
/// is the only honest answer (invariant I3: no silent success).
#[test]
fn a_mutation_is_not_retried_after_the_daemon_took_the_request() {
    use std::io::Read;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");

    let home = tempfile::tempdir().expect("home");
    let daemon_home = home.path().join(".mdkb");
    std::fs::create_dir_all(&daemon_home).expect("daemon home");
    let listener = std::os::unix::net::UnixListener::bind(daemon_home.join("daemon-hook.sock"))
        .expect("bind fake daemon socket");

    // A daemon that swallows whole frames and answers none of them. It has to
    // keep accepting: `ensure_daemon_running` probes with its own connection
    // first, and that probe would otherwise eat the only accept.
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut hdr = [0u8; 4];
            if stream.read_exact(&mut hdr).is_err() {
                continue; // the reachability probe, which sends nothing
            }
            let mut body = vec![0u8; u32::from_le_bytes(hdr) as usize];
            let _ = stream.read_exact(&mut body);
            // The request is ours now. Say nothing, and hang up.
        }
    });

    // `memory rm` is one of the commands that actually resolves to an RPC, so
    // it reaches the deaf daemon. Seed the entry it is asked to delete, using
    // the escape hatch so the seeding write does not itself go over the socket.
    let seed = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args([
            "memory",
            "add",
            "doomed",
            "--title",
            "Doomed",
            "--content",
            "delete me",
        ])
        .current_dir(&root)
        .env("HOME", home.path())
        .env("MDKB_NO_SPAWN", "1")
        .env("MDKB_NO_DAEMON", "1")
        .output()
        .expect("seed");
    assert!(
        seed.status.success(),
        "seeding must succeed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(["memory", "rm", "doomed"])
        .current_dir(&root)
        .env("HOME", home.path())
        .env("MDKB_NO_SPAWN", "1")
        .output()
        .expect("run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a mutation whose outcome is unknown must not report success: {stderr}"
    );
    assert!(
        stderr.contains("daemon"),
        "the error must name the daemon as the possible writer: {stderr}"
    );

    let ctx = mdkb::core::Context::open_read_only(&root).expect("open read-only");
    assert!(
        mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "doomed")
            .unwrap()
            .is_some(),
        "the CLI performed the delete itself — that is the second writer"
    );
}

/// The routing is wired, not merely classified.
///
/// A classifier nothing calls is worse than none — it reads as a property the
/// program has. This asserts mutations actually resolve to the single typed
/// internal request while reads do not.
#[test]
fn routed_mutations_resolve_to_the_typed_daemon_request() {
    use mdkb::core::cli_mutation::CliMutation;
    use mdkb::core::routing::mutation_request;

    let cwd = std::env::current_dir().unwrap();
    let mut command = parse(&["memory", "rm", "gone"]);
    assert!(
        matches!(mutation_request(&mut command, &cwd, &cwd).unwrap(), Some(CliMutation::MemoryRemove { id }) if id == "gone")
    );

    let mut command = parse(&["update"]);
    assert!(matches!(
        mutation_request(&mut command, &cwd, &cwd).unwrap(),
        Some(CliMutation::Update { .. })
    ));

    let mut command = parse(&["search", "q"]);
    assert!(
        mutation_request(&mut command, &cwd, &cwd)
            .unwrap()
            .is_none()
    );
}

/// Routing must carry the command's arguments, not just its name.
///
/// Dropping them is worse than not routing at all: `mdkb update --force` looked
/// like it ran and silently skipped every unchanged file, and
/// `mdkb update one.md` quietly reindexed the entire repository. Both reported
/// success. A flag that is parsed, accepted and then discarded is a lie the CLI
/// tells on every invocation.
#[test]
fn routed_mutations_carry_their_arguments() {
    use mdkb::core::cli_mutation::CliMutation;
    let cwd = std::env::current_dir().unwrap();
    let mut command = parse(&["update", "--force", "--files", "docs/a.md", "docs/b.md"]);
    let request = mdkb::core::routing::mutation_request(&mut command, &cwd, &cwd)
        .unwrap()
        .unwrap();
    match request {
        CliMutation::Update { request } => {
            assert!(request.force, "--force must reach the daemon");
            assert_eq!(request.files, ["docs/a.md", "docs/b.md"]);
        }
        other => panic!("wrong request: {other:?}"),
    }
}

#[test]
fn every_classified_mutation_has_a_typed_request() {
    let commands: &[&[&str]] = &[
        &["update"],
        &["embed"],
        &["compact"],
        &["collection", "add", "n", "p"],
        &["collection", "remove", "n"],
        &["collection", "rename", "a", "b"],
        &["memory", "add", "x", "--title", "T", "--content", "C"],
        &["memory", "confirm", "x", "--outcome", "confirmed"],
        &["memory", "link", "x", "supports", "y"],
        &["memory", "rm", "x"],
        &["memory", "sync"],
        &["memory", "import", "f.json"],
        &["memory", "prune"],
        &["evolve", "supersedes", "a", "b"],
        &["evolve", "updates", "a", "b"],
        &["evolve", "corrects", "a", "b"],
        &["evolve", "retracts", "a", "b"],
        &["evolve", "extends", "a", "b"],
        &[
            "experiment",
            "create",
            "x",
            "--config-a",
            "{}",
            "--config-b",
            "{}",
        ],
        &["experiment", "end", "x"],
        &["experiment", "cancel", "x"],
        &["journal", "import", "j.md"],
        &["journal", "import-all"],
        &["code", "init"],
        &["code", "index"],
        &["session", "index"],
    ];
    let cwd = std::env::current_dir().unwrap();
    for args in commands {
        let mut command = parse(args);
        assert_eq!(routing_for(&command), Routing::Mutation);
        assert!(
            mdkb::core::routing::mutation_request(&mut command, &cwd, &cwd)
                .unwrap()
                .is_some(),
            "missing request for mdkb {}",
            args.join(" ")
        );
    }
}
