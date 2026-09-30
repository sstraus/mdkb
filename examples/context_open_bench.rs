//! Times `Context::open` against a real store (story 201-481e).
//!
//!   cargo run --release --example context_open_bench -- <project-root>
//!
//! Three opens: marker absent (first open ever), marker fresh (nothing wrote
//! since the last probe), and marker stale (a write landed after the probe,
//! which is what a running daemon's telemetry does all day).

use std::time::Instant;

use mdkb::core::Context;

fn timed_open(label: &str, root: &str) {
    let t = Instant::now();
    let ctx = if label.starts_with("reuse") {
        Context::open_reusing_process_probe(root)
    } else {
        Context::open(root)
    }
    .unwrap();
    println!("{label:<28} {:>8.1} ms", t.elapsed().as_secs_f64() * 1e3);
    drop(ctx);
}

fn main() {
    let root = std::env::args().nth(1).expect("project root");
    let dir = std::path::Path::new(&root).join(".mdkb");
    let marker = dir.join("index.sqlite.integrity-ok");
    let _ = std::fs::remove_file(&marker);
    timed_open("marker absent", &root);
    timed_open("marker fresh", &root);
    let conn = rusqlite::Connection::open(dir.join("index.sqlite")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    conn.execute_batch("CREATE TABLE IF NOT EXISTS bench_touch(x); INSERT INTO bench_touch VALUES (1);")
        .unwrap();
    drop(conn);
    timed_open("marker stale (db written)", &root);
    timed_open("marker fresh again", &root);
    // The daemon reopening a repo its LRU evicted, after a write made the marker stale.
    let conn = rusqlite::Connection::open(dir.join("index.sqlite")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    conn.execute_batch("INSERT INTO bench_touch VALUES (2);").unwrap();
    drop(conn);
    timed_open("reuse: first (probes)", &root);
    timed_open("reuse: second (trusts)", &root);
}
