//! Integration test: `Context::open` autoheals a structurally-corrupt index.
//!
//! Reproduces the failure mode that motivated the feature — a corrupt
//! `index.sqlite` — and asserts that opening it quarantines the damaged file,
//! rebuilds a clean empty database in its place, flags the rebuild for the
//! caller to reindex, and preserves the on-disk memory entry store.

use std::fs;

use mdkb::cli::handlers::handle_init;
use mdkb::core::Context;

/// Truncate the index file to half its length: a valid header over a torn
/// b-tree — the structural damage class ordinary reads miss but `quick_check`
/// catches. Removes the `-wal`/`-shm` sidecars so the truncated main file is
/// what gets probed.
fn corrupt_index(mdkb_dir: &std::path::Path) {
    for side in ["index.sqlite-wal", "index.sqlite-shm"] {
        let _ = fs::remove_file(mdkb_dir.join(side));
    }
    let db = mdkb_dir.join("index.sqlite");
    let len = fs::metadata(&db).unwrap().len();
    let f = fs::OpenOptions::new().write(true).open(&db).unwrap();
    f.set_len(len / 2).unwrap();
}

#[test]
fn open_quarantines_corrupt_index_and_rebuilds_clean() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    handle_init(&root).expect("init mdkb");
    let mdkb_dir = root.join(".mdkb");

    // Open once so the schema is materialized into a real multi-page file, then
    // corrupt it.
    drop(Context::open(&root).expect("first open"));
    corrupt_index(&mdkb_dir);

    // Drop the throttle marker written by the healthy first open so the probe runs.
    let _ = fs::remove_file(mdkb_dir.join("index.sqlite.integrity-ok"));

    let ctx = Context::open(&root).expect("open must succeed by healing, not error");

    assert!(
        ctx.rebuilt_from_corruption,
        "open must report that it rebuilt from corruption"
    );

    // The corrupt file was quarantined, not deleted (the `.report.json` salvage
    // sidecar shares the prefix but is a distinct artifact, not a second copy).
    let quarantined: Vec<_> = fs::read_dir(&mdkb_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("index.sqlite.corrupt-") && !name.ends_with(".report.json")
        })
        .collect();
    assert_eq!(
        quarantined.len(),
        1,
        "exactly one quarantined copy of the corrupt index must remain for forensics"
    );

    // A quarantine records a loud, persistent report sidecar so the data-loss
    // event surfaces in stats/warmup until the operator cleans up.
    let report = fs::read_dir(&mdkb_dir).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".report.json")
    });
    assert!(report, "quarantine must write a report sidecar");

    // The rebuilt database is clean and usable.
    let check: String = ctx
        .conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .expect("rebuilt db is probeable");
    assert_eq!(check, "ok", "rebuilt index must pass quick_check");

    let mode: i64 = ctx
        .conn
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, 0, "rebuilt index must be auto_vacuum=NONE");

    let rows: i64 = ctx
        .conn
        .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
        .expect("rebuilt db is queryable");
    assert_eq!(rows, 0, "rebuilt index starts empty, ready to reindex");
}

#[test]
fn open_leaves_healthy_index_untouched() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();
    handle_init(&root).expect("init mdkb");

    drop(Context::open(&root).expect("first open"));
    let ctx = Context::open(&root).expect("second open");

    assert!(
        !ctx.rebuilt_from_corruption,
        "a healthy index must never be reported as rebuilt"
    );
    let mdkb_dir = root.join(".mdkb");
    let quarantined = fs::read_dir(&mdkb_dir).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("index.sqlite.corrupt-")
    });
    assert!(!quarantined, "a healthy index must not be quarantined");
}
