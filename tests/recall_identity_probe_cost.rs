//! Story 065-9a07: every recall turn probes embedding identity several times, and
//! the probe once scanned every vector and joined every chunk body (search p50
//! 242 ms against 49 ms without it). Wall time is flaky in CI, so cost is counted
//! in SQLite virtual-machine steps, which are deterministic for a fixed store.
use mdkb::store::chunks::Chunk;
use mdkb::store::{schema, vectors};
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const DOCUMENTS: i64 = 40;
const CHUNKS_PER_DOCUMENT: usize = 50;

fn big_store() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    vectors::init_sqlite_vec();
    let conn = Connection::open(dir.path().join("index.sqlite")).unwrap();
    schema::init_schema(&conn).unwrap();
    vectors::init_vector_schema(&conn).unwrap();
    conn.execute_batch(
        "INSERT INTO collections(name,path,pattern,created_at,updated_at) VALUES('notes','.','*.md',0,0);
         INSERT INTO content(hash,body,created_at) VALUES('hash','document body',0);",
    )
    .unwrap();
    let body = "chunk body text ".repeat(64);
    for id in 1..=DOCUMENTS {
        conn.execute(
            "INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(?1,'notes',?1 || '.md','hash',0,0)",
            [id],
        )
        .unwrap();
        let chunks: Vec<Chunk> = (0..CHUNKS_PER_DOCUMENT)
            .map(|index| Chunk {
                index,
                content: format!("{body}{index}"),
                heading_path: None,
            })
            .collect();
        let vectors_for = vec![vec![0.25; 384]; CHUNKS_PER_DOCUMENT];
        vectors::store_chunk_embeddings(&conn, id, &chunks, &vectors_for, "AllMiniLML6V2").unwrap();
        vectors::store_embedding(&conn, id, &[0.25; 384], "AllMiniLML6V2").unwrap();
    }
    (dir, conn)
}

/// SQLite VM steps spent inside `work`.
fn steps(conn: &Connection, work: impl FnOnce()) -> u64 {
    let counter = Arc::new(AtomicU64::new(0));
    let seen = counter.clone();
    conn.progress_handler(
        1,
        Some(move || {
            seen.fetch_add(1, Ordering::Relaxed);
            false
        }),
    )
    .unwrap();
    work();
    conn.progress_handler(0, None::<fn() -> bool>).unwrap();
    counter.load(Ordering::Relaxed)
}

/// Database pages SQLite read (cache hits plus misses) inside `work`. Unlike VM
/// steps, this grows with the bytes a query touches: one opcode can read a whole
/// chunk body or vector blob.
fn pages(conn: &Connection, work: impl FnOnce()) -> u64 {
    let read = |reset: i32| {
        let (mut hit, mut miss, mut high) = (0, 0, 0);
        // SAFETY: `conn.handle()` is the live connection, used on this thread
        // only, and sqlite3_db_status writes plain ints through valid pointers.
        unsafe {
            let db = conn.handle();
            let rc = rusqlite::ffi::sqlite3_db_status(
                db,
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_HIT,
                &mut hit,
                &mut high,
                reset,
            );
            assert_eq!(rc, rusqlite::ffi::SQLITE_OK, "sqlite3_db_status failed");
            let rc = rusqlite::ffi::sqlite3_db_status(
                db,
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
                &mut miss,
                &mut high,
                reset,
            );
            assert_eq!(rc, rusqlite::ffi::SQLITE_OK, "sqlite3_db_status failed");
        }
        (hit + miss) as u64
    };
    read(1);
    work();
    read(0)
}

#[test]
fn identity_probe_reads_a_small_fraction_of_the_store() {
    // Catches a probe that reads vec0 vectors or chunk bodies. VM steps miss it:
    // a probe that scans `vec_chunks` and joins every `document_chunks` row costs
    // 49983 steps here, under the step ceiling below. It reads 750 of the 1963
    // pages of this store; the shadow-rowid anti-join reads 117. The bound is a
    // fraction of the store's own size, not of a measured revert.
    let (_dir, conn) = big_store();
    let store_pages: u64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap();
    let probe = pages(&conn, || {
        assert!(!vectors::identity_status(&conn).unwrap().ambiguous);
    });
    println!("identity probe: {probe} pages read of {store_pages}");
    assert!(probe > 0, "the page counter read nothing");
    assert!(
        probe < store_pages / 6,
        "identity probe read {probe} of {store_pages} pages"
    );
}

#[test]
fn identity_probe_cost_stays_far_below_a_scan_of_every_chunk() {
    // Catches the probe of story 065-9a07, which cost 94783 steps (about 47 per
    // chunk) against 29709 (about 15) for the shadow-rowid anti-join. The ceiling
    // of 30 per chunk sits between them with about 2x on each side. A probe that
    // reads more bytes in fewer opcodes can stay under it; the page test above
    // covers that.
    let (_dir, conn) = big_store();
    let chunks = DOCUMENTS as u64 * CHUNKS_PER_DOCUMENT as u64;
    let probe = steps(&conn, || {
        assert!(!vectors::identity_status(&conn).unwrap().ambiguous);
    });
    println!("identity probe: {probe} VM steps for {chunks} chunks");
    assert!(probe > 0, "the step counter never fired");
    assert!(
        probe < chunks * 30,
        "identity probe took {probe} VM steps for {chunks} chunks"
    );
}

#[test]
fn identity_probe_is_paid_once_per_read_snapshot() {
    // Catches a recall turn that re-runs the probe for each semantic read (about
    // nine per turn, 15-25 ms each warm) instead of once per read snapshot.
    let (_dir, conn) = big_store();
    let single = steps(&conn, || {
        vectors::identity_status(&conn).unwrap();
    });
    let nine = steps(&conn, || {
        vectors::with_read_snapshot(&conn, || {
            for _ in 0..9 {
                assert!(!vectors::identity_status(&conn)?.ambiguous);
            }
            Ok(())
        })
        .unwrap();
    });
    println!("one probe {single} steps, nine in a snapshot {nine} steps");
    assert!(
        nine < single * 2,
        "nine probes cost {nine}, one costs {single}"
    );
}
