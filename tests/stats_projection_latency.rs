//! Real projection I/O regressions: stats must read concurrently, not skip diagnostics.

use mdkb::cli::handlers::{handle_init, handle_memory_add};
use mdkb::cli::stats_report::collect_report;
use mdkb::core::Context;

#[path = "common/cli.rs"]
mod cli;

fn env() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    handle_init(dir.path()).unwrap();
    let ctx = Context::open(dir.path()).unwrap();
    (dir, ctx)
}

fn add(ctx: &Context, id: &str) {
    handle_memory_add(
        ctx,
        id,
        id,
        "topic",
        None,
        &format!("Stored memory about {id}."),
        None,
        None,
        None,
        None,
        &[],
        None,
        None,
        false,
    )
    .unwrap();
}

fn rows(ctx: &Context) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = ctx
        .conn
        .prepare("SELECT * FROM memory_entries ORDER BY id")
        .unwrap();
    let columns = stmt.column_count();
    stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn parallel_stats_preserves_mixed_diagnostics_without_repair_or_access_tracking() {
    let (_dir, ctx) = env();
    for id in [
        "valid", "db-only", "archived", "mismatch", "bad-yaml", "bad-utf8", "conflict",
    ] {
        add(&ctx, id);
    }
    let entries = ctx.memory_dir().join("entries");
    std::fs::remove_file(entries.join("db-only.md")).unwrap();
    mdkb::store::memory::set_status(
        &ctx.conn,
        "archived",
        mdkb::store::memory::EntryStatus::Archived,
    )
    .unwrap();
    std::fs::remove_file(entries.join("archived.md")).unwrap();
    let original = std::fs::read_to_string(entries.join("mismatch.md")).unwrap();
    std::fs::write(
        entries.join("mismatch.md"),
        original.replace("id: mismatch", "id: another-id"),
    )
    .unwrap();
    std::fs::write(entries.join("bad-yaml.md"), "---\nid: [\n---\nbody").unwrap();
    std::fs::write(entries.join("bad-utf8.md"), [0xff, 0xfe]).unwrap();
    std::fs::write(
        entries.join("conflict.md"),
        "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n",
    )
    .unwrap();
    // Catches accepting non-md files when the extension/stem filter uses OR.
    std::fs::write(entries.join("unrelated.txt"), [0xff, 0xfe]).unwrap();
    // An md-named directory exercises an actual read error even under a privileged user.
    std::fs::create_dir(entries.join("directory.md")).unwrap();
    let before = rows(&ctx);
    let files_before: Vec<_> = ["valid", "mismatch", "bad-yaml", "bad-utf8", "conflict"]
        .map(|id| std::fs::read(entries.join(format!("{id}.md"))).unwrap())
        .into();
    for _ in 0..2 {
        let report = collect_report(&ctx).unwrap();
        assert_eq!(report.memory.files_unreadable, 5);
        assert_eq!(
            report.memory.entries_unprojected, 5,
            "DB-only plus four rejected projections; archived is excluded"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["memory"]["files_unreadable"], 5);
        assert_eq!(json["memory"]["entries_unprojected"], 5);
    }
    assert_eq!(
        rows(&ctx),
        before,
        "stats must not repair rows or record memory use"
    );
    for (id, bytes) in ["valid", "mismatch", "bad-yaml", "bad-utf8", "conflict"]
        .into_iter()
        .zip(files_before)
    {
        assert_eq!(
            std::fs::read(entries.join(format!("{id}.md"))).unwrap(),
            bytes
        );
    }
    assert!(!entries.join("db-only.md").exists());
    assert_eq!(
        std::fs::read(entries.join("unrelated.txt")).unwrap(),
        [0xff, 0xfe]
    );
}

/// A serial reader cannot open all four FIFOs before any writer sends bytes.
/// This checks actual OS I/O overlap, not an elapsed-time speedup or a mocked reader.
#[cfg(unix)]
#[test]
fn stats_reads_four_projections_before_waiting_for_the_first_contents() {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    let (dir, ctx) = env();
    let entries = ctx.memory_dir().join("entries");
    std::fs::create_dir_all(&entries).unwrap();
    let paths: Vec<_> = (0..4)
        .map(|i| entries.join(format!("pipe-{i}.md")))
        .collect();
    for path in &paths {
        assert!(
            std::process::Command::new("mkfifo")
                .arg(path)
                .status()
                .unwrap()
                .success()
        );
    }
    drop(ctx);
    let mut child = cli::command()
        .args(["--format", "json", "status"])
        .current_dir(dir.path())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut writers: Vec<Option<std::fs::File>> = (0..4).map(|_| None).collect();
    while writers.iter().any(Option::is_none) && Instant::now() < deadline {
        for (path, writer) in paths.iter().zip(&mut writers) {
            if writer.is_none() {
                match std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
                {
                    Ok(file) => *writer = Some(file),
                    Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {}
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("FIFO open: {e}");
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let concurrent = writers.iter().all(Option::is_some);
    if concurrent {
        for mut writer in writers.into_iter().flatten() {
            writer.write_all(b"not a memory projection\n").unwrap();
        }
    } else {
        // Kill and reap before asserting, so the deliberately blocked serial baseline cannot leak.
        let _ = child.kill();
        drop(writers);
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        concurrent,
        "stats serialized projection reads: four OS readers never opened together"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["memory"]["files_unreadable"], 4,
        "parallel I/O must still parse every file"
    );
}
