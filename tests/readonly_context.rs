//! A read must not be a write.
//!
//! Story 018-56b2 names the obstacle precisely: `Context::open` is itself a
//! write. It runs migrations, creates FTS and vector virtual tables, and
//! initializes the stats schema — on every open, including the ones that only
//! want to answer `mdkb search`. So every one-shot CLI read is another writer
//! process against the same file the long-lived daemon is writing, which is one
//! of the two surviving hypotheses for the recurring `index.sqlite` corruption.
//!
//! A read-only path therefore has to SKIP init rather than repair: a binary that
//! cannot migrate must say so and stop, not quietly do half of it.

use mdkb::cli::handlers::handle_init;
use mdkb::core::Context;
use mdkb::store::schema::SCHEMA_VERSION;

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    (dir, root)
}

fn set_schema_version(root: &std::path::Path, v: i32) {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.execute("UPDATE schema_version SET version = ?1", [v])
        .expect("set version");
}

const OPTIONAL_TABLES: &[&str] = &[
    "sessions",
    "tool_usage",
    "call_log",
    "query_events",
    "experiments_schema_version",
    "experiments",
    "experiment_results",
];

fn drop_optional_tables(root: &std::path::Path) {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.execute_batch(
        "DROP TABLE experiment_results;
         DROP TABLE experiments;
         DROP TABLE experiments_schema_version;
         DROP TABLE query_events;
         DROP TABLE call_log;
         DROP TABLE tool_usage;
         DROP TABLE sessions;",
    )
    .expect("remove optional tables from legacy fixture");
}

fn assert_optional_tables_exist(conn: &rusqlite::Connection) {
    for table in OPTIONAL_TABLES {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
                [*table],
                |row| row.get(0),
            )
            .expect("inspect schema");
        assert!(exists, "optional table {table} was not initialized");
    }
}

#[test]
fn fresh_init_bootstraps_optional_read_schemas() {
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = Context::init(dir.path()).expect("init");

    assert_optional_tables_exist(&ctx.conn);
}

#[test]
fn write_capable_open_backfills_optional_read_schemas() {
    let (_dir, root) = store();
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.execute_batch(
        "DROP TABLE experiment_results;
         DROP TABLE experiments;
         DROP TABLE query_events;
         DROP TABLE call_log;
         DROP TABLE tool_usage;
         DROP TABLE sessions;",
    )
    .expect("remove optional tables but retain experiment schema version");
    drop(conn);

    let ctx = Context::open(&root).expect("write-capable open");

    assert_optional_tables_exist(&ctx.conn);
}

#[test]
fn legacy_store_without_optional_tables_reads_as_empty() {
    let (_dir, root) = store();
    drop_optional_tables(&root);
    let ctx = Context::open_read_only(&root).expect("read legacy store");

    let report = mdkb::cli::stats_report::collect_report(&ctx).expect("stats report");
    assert_eq!(report.sessions.total_sessions, 0);
    assert_eq!(report.sessions.total_calls, 0);
    assert!(report.sessions.top_tools.is_empty());

    let metrics = mdkb::core::ops::handle_metrics_show(&ctx, 30).expect("metrics");
    assert_eq!(metrics.total_queries, 0);
    assert!(
        mdkb::core::ops::handle_metrics_latency(&ctx)
            .expect("latency")
            .is_empty()
    );
    assert!(
        mdkb::core::ops::handle_metrics_export(&ctx, 30)
            .expect("export")
            .is_empty()
    );

    assert!(
        mdkb::core::ops::handle_experiment_status(&ctx, "missing")
            .expect("experiment status")
            .is_none()
    );
    assert!(
        mdkb::core::ops::handle_experiment_list(&ctx, false)
            .expect("experiment list")
            .is_empty()
    );
    assert!(
        mdkb::core::ops::handle_experiment_list(&ctx, true)
            .expect("active experiment list")
            .is_empty()
    );
}

/// The connection must refuse writes at the SQLite level, not by convention.
///
/// A promise not to write is worth nothing against a code path that forgets;
/// `SQLITE_OPEN_READ_ONLY` makes the guarantee the database's rather than ours.
#[test]
fn a_read_only_context_cannot_write() {
    let (_dir, root) = store();
    let ctx = Context::open_read_only(&root).expect("open read-only");

    let err = ctx
        .conn
        .execute(
            "INSERT INTO memory_entries
             (id, title, content, entry_type, tags, status, created_at, updated_at, source_type)
             VALUES ('x','T','C','topic','[]','active',0,0,'user_statement')",
            [],
        )
        .expect_err("a read-only connection must refuse to write");
    assert!(
        err.to_string().contains("readonly") || err.to_string().contains("read-only"),
        "the refusal must come from SQLite, not from a convention: {err}"
    );
}

/// Reads still work — the point is a read path, not a broken one.
#[test]
fn a_read_only_context_can_read() {
    let (_dir, root) = store();
    {
        let ctx = Context::open(&root).expect("open");
        mdkb::cli::handlers::handle_memory_add(
            &ctx,
            "findme",
            "Find Me",
            "topic",
            None,
            "body text",
            None,
            None,
            None,
            None,
        )
        .expect("add");
    }

    let ctx = Context::open_read_only(&root).expect("open read-only");
    let entry = mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "findme")
        .expect("query")
        .expect("entry must be readable");
    assert_eq!(entry.title, "Find Me");
}

/// Opening read-only must not leave a `-wal`/`-shm` pair behind on a store that
/// had none. Creating them IS a write, and the whole point is that a read does
/// not touch the file.
#[test]
fn a_read_only_open_creates_no_sidecars() {
    let (_dir, root) = store();
    let db = root.join(".mdkb/index.sqlite");
    // Collapse the WAL so the store starts with no sidecars at all.
    {
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("checkpoint");
    }
    let wal = root.join(".mdkb/index.sqlite-wal");
    let shm = root.join(".mdkb/index.sqlite-shm");
    assert!(!wal.exists() && !shm.exists(), "fixture must start clean");

    let ctx = Context::open_read_only(&root).expect("open read-only");
    let _ = mdkb::store::memory::count_entries(&ctx.conn);
    drop(ctx);

    assert!(
        !wal.exists() && !shm.exists(),
        "a read-only open must not create WAL sidecars — that is a write"
    );
}

/// A store NEWER than this binary must fail with a clear message rather than be
/// migrated. This is the whole reason init is skipped instead of run.
#[test]
fn a_newer_store_is_refused_rather_than_migrated() {
    let (_dir, root) = store();
    let future = SCHEMA_VERSION + 2;
    set_schema_version(&root, future);

    let err = Context::open_read_only(&root).expect_err("must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains(&future.to_string()) && msg.contains(&SCHEMA_VERSION.to_string()),
        "the error must name both versions: {msg}"
    );
}

/// And an OLDER store must also be refused, not silently migrated.
///
/// This is the direction that would otherwise be tempting to "just fix": a read
/// command that migrates is a writer, which is exactly what this path exists to
/// eliminate. The remedy is named so the operator knows a write command will do
/// it.
#[test]
fn an_older_store_is_refused_rather_than_migrated() {
    let (_dir, root) = store();
    set_schema_version(&root, 11);

    let err = Context::open_read_only(&root).expect_err("must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("11") && msg.contains(&SCHEMA_VERSION.to_string()),
        "the error must name both versions: {msg}"
    );
    assert!(
        msg.contains("mdkb update") || msg.contains("migrat"),
        "the error must name how to migrate, since this path deliberately will \
         not: {msg}"
    );

    // And it really did not migrate.
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).unwrap();
    let v: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        v, 11,
        "a refused read must leave the store exactly as it was"
    );
}

/// `mdkb search` and `mdkb stats` must work with no daemon reachable and no
/// daemon spawnable — the read path cannot depend on the writer being up.
#[test]
fn search_and_stats_work_with_no_daemon() {
    let (_dir, root) = store();
    {
        let ctx = Context::open(&root).expect("open");
        mdkb::cli::handlers::handle_memory_add(
            &ctx,
            "daemonless",
            "Daemonless entry",
            "topic",
            None,
            "reachable without a daemon",
            None,
            None,
            None,
            None,
        )
        .expect("add");
    }
    let home = tempfile::tempdir().expect("home");

    for args in [
        vec!["search", "daemonless", "--scope", "memory"],
        vec!["stats"],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .args(&args)
            .current_dir(&root)
            .env("HOME", home.path())
            .env("MDKB_NO_SPAWN", "1")
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "`mdkb {}` must work with the daemon stopped: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The end-to-end proof for criterion 5, scoped to what it can actually assert
/// from outside: a read command must leave no trace on the store.
///
/// A write-capable connection under WAL creates `-wal` and `-shm` the moment it
/// opens. Their absence after a full command run is externally-observable
/// evidence that the process never held one — which is the property the story
/// wants, stated in terms a test can check without inspecting SQLite internals.
#[test]
fn read_commands_leave_no_write_trace_on_the_store() {
    let (_dir, root) = store();
    {
        let ctx = Context::open(&root).expect("open");
        mdkb::cli::handlers::handle_memory_add(
            &ctx, "seeded", "Seeded", "topic", None, "body", None, None, None, None,
        )
        .expect("add");
    }
    // Collapse the WAL so any sidecar afterwards was created by the command.
    {
        let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("checkpoint");
    }
    let code_db = root.join(".mdkb/code.sqlite");
    {
        let conn = rusqlite::Connection::open(&code_db).expect("open code index");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("checkpoint code index");
    }
    let wal = root.join(".mdkb/index.sqlite-wal");
    let shm = root.join(".mdkb/index.sqlite-shm");
    let code_wal = root.join(".mdkb/code.sqlite-wal");
    let code_shm = root.join(".mdkb/code.sqlite-shm");
    let home = tempfile::tempdir().expect("home");

    for args in [
        vec!["collection", "list"],
        vec!["search", "seeded", "--scope", "memory"],
        vec!["get", "seeded"],
        vec!["stats"],
        vec!["metrics", "show"],
        vec!["metrics", "latency"],
        vec!["memory", "show", "seeded"],
        vec!["memory", "list"],
        vec!["memory", "search", "seeded"],
        vec!["memory", "warmup"],
        vec!["memory", "history", "seeded"],
        vec!["experiment", "status", "missing"],
        vec!["experiment", "list"],
        vec!["graph", "hubs"],
        vec!["graph", "dangling"],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .args(&args)
            .current_dir(&root)
            .env("HOME", home.path())
            .env("MDKB_NO_SPAWN", "1")
            .output()
            .expect("run");
        assert!(
            out.status.success(),
            "`mdkb {}` must succeed on the read-only path: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !wal.exists() && !shm.exists() && !code_wal.exists() && !code_shm.exists(),
            "`mdkb {}` created WAL sidecars, so it held a write-capable \
             connection to a store the daemon may also be writing: \
             index={} code={}",
            args.join(" "),
            wal.exists() || shm.exists(),
            code_wal.exists() || code_shm.exists(),
        );
    }

    for (args, succeeds) in [
        (vec!["search", "missing", "--scope", "code"], true),
        (vec!["search", "missing", "--scope", "symbols"], true),
        (vec!["code", "search", "missing"], true),
        (vec!["code", "find", "missing"], true),
        (vec!["code", "info"], true),
        (vec!["code", "calls", "missing"], false),
        (vec!["code", "callers", "missing"], false),
        (vec!["code", "impact", "missing"], false),
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .args(&args)
            .current_dir(&root)
            .env("HOME", home.path())
            .env("MDKB_NO_SPAWN", "1")
            .output()
            .expect("run code read");
        assert_eq!(
            out.status.success(),
            succeeds,
            "unexpected status for `mdkb {}`: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !wal.exists() && !shm.exists() && !code_wal.exists() && !code_shm.exists(),
            "`mdkb {}` created SQLite sidecars: index={} code={}",
            args.join(" "),
            wal.exists() || shm.exists(),
            code_wal.exists() || code_shm.exists(),
        );
    }
}

#[test]
fn a_code_read_does_not_create_a_missing_index() {
    let (_dir, root) = store();
    let code_db = root.join(".mdkb/code.sqlite");
    std::fs::remove_file(&code_db).expect("remove fixture code index");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(["code", "info"])
        .current_dir(&root)
        .env("MDKB_NO_SPAWN", "1")
        .output()
        .expect("run code info");

    assert!(!out.status.success(), "a missing index must be reported");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Code index not found"),
        "error must explain how to create the index: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !code_db.exists(),
        "a read must not create an empty code index"
    );
}
