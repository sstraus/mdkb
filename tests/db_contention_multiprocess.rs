//! Multi-PROCESS contention over one `index.sqlite`.
//!
//! `tests/db_contention.rs` already stresses concurrent writers, but all of its
//! workers are threads inside one process — they share a single SQLite pager and
//! a single WAL index, so they exercise none of the cross-process machinery.
//! That test has always passed while production stores kept going corrupt.
//!
//! Production shape, and therefore the shape reproduced here: one long-lived
//! connection (the daemon) plus a stream of short-lived `mdkb` processes fired
//! by editor hooks, all writing the memory tables of the same store.
//!
//! The damage seen in the field is always the same class — `2nd reference to
//! page N`, two b-tree cells claiming one overflow page — and always confined to
//! `memory_entries`, `memory_fts_data` and `memory_embeddings`. Every knob below
//! is aimed at that: entry bodies large enough to spill onto overflow pages, and
//! a `memory show` in the loop because reads are still writes on this schema —
//! `get_entry` bumps `access_count` and `last_accessed`. Since schema v18 that
//! bump no longer reaches the FTS index (`memory_au` is scoped to the indexed
//! columns), so the read path is a plain `memory_entries` write; the loop stays
//! because that write is the one production actually performs.
//!
//! Soak knobs for hunting outside CI:
//!   MDKB_STRESS_WORKERS  child processes      (default 6)
//!   MDKB_STRESS_ITERS    iterations per child (default 20)

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use mdkb::core::Context;
use mdkb::store::vectors;
use rusqlite::params;
use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mdkb"))
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A body large enough to need overflow pages — the page class that the field
/// corruption double-allocates. A single b-tree cell would otherwise hold the
/// whole entry and the failure mode could not appear.
fn big_body(seed: usize) -> String {
    format!("entry {seed} ").repeat(600)
}

/// Every quarantined store on disk. A run that corrupts and then *autoheals*
/// leaves a sound `index.sqlite` behind, so asserting only on the final
/// `quick_check` would report success on exactly the incident being hunted.
fn quarantined_files(mdkb_dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(mdkb_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".corrupt-") && !n.ends_with(".report.json"))
        .collect()
}

fn init_repo() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    let out = Command::new(bin())
        .args(["init"])
        .current_dir(&root)
        .stdin(Stdio::null())
        .output()
        .expect("spawn mdkb init");
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    (dir, root)
}

/// One short-lived writer, exactly as a hook fires it: a fresh process per
/// command, each one opening and closing its own connection.
fn hook_writer(root: &Path, worker: usize, iters: usize) {
    for n in 0..iters {
        let id = format!("stress-{worker}-{n}");
        let body = big_body(n);
        let add = Command::new(bin())
            .args([
                "memory",
                "add",
                &id,
                "-t",
                "Stress entry",
                "-T",
                "topic",
                "--tags",
                "stress",
                "-c",
                &body,
            ])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn memory add");
        assert!(
            add.status.success(),
            "memory add {id}: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        // A read on this schema is a write: `get_entry` updates `access_count`,
        // the `memory_au` trigger deletes and reinserts the FTS5 row.
        let show = Command::new(bin())
            .args(["memory", "show", &id])
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn memory show");
        assert!(
            show.status.success(),
            "memory show {id}: {}",
            String::from_utf8_lossy(&show.stderr)
        );

        if n.is_multiple_of(5) {
            let del = Command::new(bin())
                .args(["memory", "rm", &id])
                .current_dir(root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
                .expect("spawn memory delete");
            assert!(
                del.status.success(),
                "memory rm {id}: {}",
                String::from_utf8_lossy(&del.stderr)
            );
        }
    }
}

/// The daemon's half of the contention: one connection held open for the whole
/// run, writing the same tables the hook processes write.
fn long_lived_writer(root: &Path, iters: usize) {
    let ctx = Context::open(root).expect("open long-lived context");
    let embedding = vec![0.25_f32; vectors::EMBEDDING_DIM];

    for n in 0..iters {
        let id = format!("daemon-{n}");
        ctx.conn
            .execute(
                "INSERT OR REPLACE INTO memory_entries
                 (id, title, content, entry_type, tags, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'topic', '[]', 'active', ?4, ?4)",
                params![id, format!("Daemon {n}"), big_body(n), n as i64],
            )
            .expect("insert memory entry");
        let rowid = ctx.conn.last_insert_rowid();
        vectors::store_memory_embedding(&ctx.conn, rowid, &embedding, "stress")
            .expect("store memory embedding");
    }
}

/// A writer that does not participate in the protocol at all: a bare connection
/// with none of `Context`'s pragmas and none of its locks — the shape of a raw
/// `sqlite3` shell, an older binary, or any tool that opens the file directly.
///
/// Story 017-a378 records one such write against a live store producing exactly
/// the field signature (`Rowid out of order`, `2nd reference to page 12862`, in
/// `memory_fts_data`). This is the probe for that mechanism.
///
/// `#[ignore]`d on purpose: it is a hypothesis probe, not a regression test.
/// mdkb cannot stop a foreign process from opening its file, so a green run
/// proves nothing and a red run is the finding. Run it deliberately:
/// `cargo test --release --test db_contention_multiprocess -- --ignored --nocapture`
#[test]
#[ignore = "diagnostic probe for the unserialized-foreign-writer hypothesis"]
fn probe_unserialized_foreign_writer() {
    let workers = env_usize("MDKB_STRESS_WORKERS", 4);
    let iters = env_usize("MDKB_STRESS_ITERS", 60);

    let (_dir, root) = init_repo();
    let db = root.join(".mdkb").join("index.sqlite");

    let mut hands = Vec::new();
    for worker in 0..workers {
        let root = root.clone();
        hands.push(std::thread::spawn(move || {
            hook_writer(&root, worker, iters);
        }));
    }

    let foreign_db = db.clone();
    hands.push(std::thread::spawn(move || {
        // Deliberately bare: no busy_timeout, no WAL pragma, no mutation lock,
        // no live lock. Exactly what `sqlite3 index.sqlite "INSERT ..."` does.
        let conn = rusqlite::Connection::open(&foreign_db).expect("foreign open");
        for n in 0..iters {
            let id = format!("foreign-{n}");
            let _ = conn.execute(
                "INSERT OR REPLACE INTO memory_entries
                 (id, title, content, entry_type, tags, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'topic', '[]', 'active', ?4, ?4)",
                params![id, format!("Foreign {n}"), big_body(n), n as i64],
            );
        }
    }));

    for hand in hands {
        hand.join().expect("writer");
    }

    let quarantined = quarantined_files(&root.join(".mdkb"));
    let conn = rusqlite::Connection::open(&db).expect("reopen");
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap_or_else(|e| format!("quick_check aborted: {e}"));

    println!("foreign-writer probe: quick_check={check}, quarantined={quarantined:?}");
    assert!(
        quarantined.is_empty() && check == "ok",
        "REPRODUCED: an unserialized foreign writer damaged the store \
         (quick_check={check}, quarantined={quarantined:?})"
    );
}

/// The same probe, but the foreign writer is the SYSTEM `sqlite3` binary rather
/// than our own bundled library.
///
/// This is the distinction the previous probe cannot make. mdkb links SQLite
/// statically; every mdkb process — however unserialized — therefore shares one
/// VFS and one locking implementation, which is why they never damage the file.
/// Apple's `/usr/bin/sqlite3` is a different build, and if it selects a
/// different locking style then the two processes do not share a locking domain
/// at all. Story 017-a378's corruption came from exactly that binary.
///
/// `#[ignore]`d: hypothesis probe, not a regression test. As above, a green run
/// proves nothing — it only means this run did not happen to interleave badly —
/// and a red run is the finding. Skips itself when `sqlite3` is absent rather
/// than failing for the wrong reason.
#[test]
#[ignore = "diagnostic probe for the system-sqlite3 locking-domain hypothesis"]
fn probe_system_sqlite3_writer() {
    let workers = env_usize("MDKB_STRESS_WORKERS", 4);
    let iters = env_usize("MDKB_STRESS_ITERS", 60);

    if Command::new("sqlite3").arg("--version").output().is_err() {
        println!("system sqlite3 not available — probe skipped");
        return;
    }

    let (_dir, root) = init_repo();
    let db = root.join(".mdkb").join("index.sqlite");

    let mut hands = Vec::new();
    for worker in 0..workers {
        let root = root.clone();
        hands.push(std::thread::spawn(move || {
            hook_writer(&root, worker, iters);
        }));
    }

    // The daemon's half. Story 017's incident was a foreign write against a
    // LIVE store, and a long-lived connection is the variable that makes it
    // live: it holds a populated page cache and an open WAL read mark while the
    // foreign process writes and checkpoints underneath it.
    let daemon_root = root.clone();
    hands.push(std::thread::spawn(move || {
        long_lived_writer(&daemon_root, iters);
    }));

    let foreign_db = db.clone();
    hands.push(std::thread::spawn(move || {
        for n in 0..iters {
            let sql = format!(
                "INSERT OR REPLACE INTO memory_entries
                 (id, title, content, entry_type, tags, status, created_at, updated_at)
                 VALUES ('sys-{n}', 'System {n}', '{}', 'topic', '[]', 'active', {n}, {n});",
                big_body(n)
            );
            let _ = Command::new("sqlite3")
                .arg(&foreign_db)
                .arg(&sql)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .output();
        }
    }));

    for hand in hands {
        hand.join().expect("writer");
    }

    let quarantined = quarantined_files(&root.join(".mdkb"));
    let conn = rusqlite::Connection::open(&db).expect("reopen");
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .unwrap_or_else(|e| format!("quick_check aborted: {e}"));

    println!("system-sqlite3 probe: quick_check={check}, quarantined={quarantined:?}");
    assert!(
        quarantined.is_empty() && check == "ok",
        "REPRODUCED: the system sqlite3 binary damaged the store \
         (quick_check={check}, quarantined={quarantined:?})"
    );
}

#[test]
fn concurrent_processes_and_a_long_lived_connection_leave_one_sound_index() {
    let workers = env_usize("MDKB_STRESS_WORKERS", 6);
    let iters = env_usize("MDKB_STRESS_ITERS", 20);

    let (_dir, root) = init_repo();

    let mut children = Vec::new();
    for worker in 0..workers {
        let root = root.clone();
        children.push(std::thread::spawn(move || {
            hook_writer(&root, worker, iters);
        }));
    }
    let daemon_root = root.clone();
    let daemon = std::thread::spawn(move || long_lived_writer(&daemon_root, iters));

    for child in children {
        child.join().expect("hook writer");
    }
    daemon.join().expect("long-lived writer");

    let mdkb_dir = root.join(".mdkb");
    let quarantined = quarantined_files(&mdkb_dir);
    assert!(
        quarantined.is_empty(),
        "the store went corrupt and autohealed during the run: {quarantined:?}"
    );

    let ctx = Context::open(&root).expect("reopen index");
    let check: String = ctx
        .conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .expect("quick_check");
    assert_eq!(check, "ok", "index is structurally damaged after the run");
}
