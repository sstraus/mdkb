//! Autohealing for a structurally-corrupt `index.sqlite`.
//!
//! Some SQLite corruption — torn pointer-map pages, freelist mismatches — is
//! invisible to ordinary reads: `SELECT ... FROM sqlite_master` still succeeds
//! and only `PRAGMA quick_check` (which walks every page) reports it. Reactively
//! catching `SQLITE_CORRUPT` from normal queries therefore does not work; we have
//! to probe. The probe reads the whole file, so on a multi-GB index it is not
//! free — a sidecar mtime throttles it to at most once per [`CHECK_INTERVAL`] so
//! a burst of one-shot CLI opens re-scans at most once.
//!
//! Most of the index is derived data — documents re-index from their `.md`
//! sources, code symbols from source files — but NOT all of it: `memory_entries`
//! and `memory_edges` live ONLY in this database (the markdown projection under
//! `.mdkb/memory/` is best-effort and never covers DB-only entries or edges).
//! Quarantining therefore risks silent memory loss, so healing does three things
//! the caller must wire up: quarantine the corrupt files, SALVAGE the memory
//! tables out of the quarantined file into the fresh one ([`salvage_memory`]),
//! and record a [`QuarantineReport`] so the loss is surfaced loudly (stderr now,
//! `mdkb stats` + SessionStart warmup until the corrupt file is cleaned up).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, params};

use crate::error::Result;

/// Re-run the integrity probe at most once per this interval per database.
pub const CHECK_INTERVAL: Duration = Duration::from_hours(6);

/// How long a probe waits on a locked database before it gives up. One value
/// for `index.sqlite` and `code.sqlite`, so both indexes tolerate the same
/// contention before an open fails.
pub const PROBE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a quarantined `*.corrupt-*` copy stays on disk before a store open
/// deletes it. Its `.report.json` sidecar is kept forever.
///
/// The forensics live in the report, not in the copy: [`diagnose`] already
/// extracted everything the file could say, and a quarantined database on its
/// own has never answered how the corruption happened. Meanwhile the copy is
/// the size of the index (56 MB in the case that motivated this) and keeps the
/// quarantine banner up long after the rebuild succeeded, which trains the
/// operator to ignore a corruption warning. Fifteen days covers an absence
/// long enough to miss the banner entirely without keeping the file for good.
// 15 days, spelled in hours because `Duration::from_days` is still unstable.
pub const QUARANTINE_RETENTION: Duration = Duration::from_hours(24 * 15);

/// [`QUARANTINE_RETENTION`] in whole days, for the surfaces that tell the
/// operator when the copy goes away.
///
/// Derived here so the two banners — `mdkb stats` and the MCP status payload —
/// cannot drift from the retention the sweep actually enforces.
pub const QUARANTINE_RETENTION_DAYS: u64 = QUARANTINE_RETENTION.as_secs() / 86_400;

/// Outcome of [`ensure_sound`].
#[derive(Debug, PartialEq, Eq)]
pub enum Heal {
    /// Database is sound, or was verified within [`CHECK_INTERVAL`] (probe skipped).
    Sound,
    /// Database was structurally corrupt; its files were renamed to
    /// `corrupt_path` (and `-wal`/`-shm` siblings). The caller must open a fresh
    /// database at the original path and trigger a reindex.
    Quarantined { corrupt_path: PathBuf },
    /// Database is structurally corrupt but another process holds a live
    /// connection to it, so it was left in place.
    ///
    /// Renaming under an open connection recycles the path onto a second inode
    /// while the survivor keeps deriving `-wal`/`-shm` from the same names — the
    /// surviving connection can then land its frames in the *replacement*
    /// database's WAL, which is how one quarantine seeds the next corruption.
    /// The caller must surface this: every mdkb process (daemon included) has to
    /// close before the next open can quarantine and rebuild.
    CorruptInUse,
}

/// Append `suffix` to a path's file name (`index.sqlite` + `.corrupt-1` →
/// `index.sqlite.corrupt-1`). Operates on the raw `OsString` so the full
/// `index.sqlite` name is preserved rather than treated as stem + extension.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Sidecar whose mtime records the last time `db_path` passed `quick_check`.
fn marker_path(db_path: &Path) -> PathBuf {
    with_suffix(db_path, ".integrity-ok")
}

/// True if `marker` was touched within `interval` and is no older than the
/// database generation it certifies.
///
/// Age alone is insufficient: a long-lived daemon can keep writing the DB/WAL
/// after a successful probe. A marker older than either file must never suppress
/// the next open-time integrity check.
fn checked_recently(db_path: &Path, marker: &Path, interval: Duration, now: SystemTime) -> bool {
    let Ok(mtime) = std::fs::metadata(marker).and_then(|m| m.modified()) else {
        return false;
    };
    let recent = now
        .duration_since(mtime)
        .map(|age| age < interval)
        .unwrap_or(false);
    if !recent {
        return false;
    }

    for path in [db_path.to_path_buf(), with_suffix(db_path, "-wal")] {
        if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) {
            if modified > mtime {
                return false;
            }
        }
    }
    true
}

/// Record a successful probe by creating/truncating the marker (updates mtime).
/// Best-effort: a failed touch just means the next open re-probes.
fn touch_marker(marker: &Path) {
    let _ = std::fs::File::create(marker);
}

/// Invalidate the last successful integrity probe before an index-wide write.
///
/// If the process crashes mid-mutation, the next open cannot trust an old
/// marker and will run `quick_check` before using the index.
pub fn invalidate_marker(db_path: &Path) {
    let _ = std::fs::remove_file(marker_path(db_path));
}

/// What one `PRAGMA quick_check` probe established about a database file.
#[derive(Debug)]
pub enum Soundness {
    /// `quick_check` answered `ok`.
    Sound,
    /// The file is torn: `quick_check` described damage, or SQLite could not
    /// read the file as a database at all. `reason` is SQLite's own wording.
    Corrupt { reason: String },
    /// The probe reached no verdict: the file was locked (`SQLITE_BUSY`),
    /// unreadable (`SQLITE_IOERR`), or memory ran out. The file may be healthy,
    /// so the caller must leave it in place and report the error instead.
    Undetermined(rusqlite::Error),
}

/// Open a throwaway connection for an integrity probe.
///
/// A fresh connection, because a long-lived one answers `quick_check` out of
/// its own page cache and reports a torn file as sound. Read-write, like every
/// other connection to this file, so closing it checkpoints and removes the
/// `-wal`/`-shm` it created instead of leaving them for the quarantine to move.
/// Waits [`PROBE_BUSY_TIMEOUT`] on a lock: a probe that gives up at once turns
/// every concurrent writer into a `BUSY` verdict.
fn open_probe(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.busy_timeout(PROBE_BUSY_TIMEOUT)?;
    Ok(conn)
}

/// Run `PRAGMA quick_check` and classify the answer.
///
/// `quick_check` returns the single row `"ok"` on a clean database and one row
/// per problem otherwise. An `Err` is [`Soundness::Corrupt`] only when SQLite
/// names the file as the problem ([`crate::error::is_sqlite_corruption`]);
/// any other failure is [`Soundness::Undetermined`], because a lock or an I/O
/// fault says nothing about the bytes on disk.
pub fn is_structurally_sound(conn: &Connection) -> Soundness {
    match conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)) {
        Ok(first) if first == "ok" => Soundness::Sound,
        Ok(first) => Soundness::Corrupt { reason: first },
        Err(e) if crate::error::is_sqlite_corruption(&e) => Soundness::Corrupt {
            reason: e.to_string(),
        },
        Err(e) => Soundness::Undetermined(e),
    }
}

/// Verify a connection after an index-wide mutation and record success.
///
/// The caller must hold the project mutation lock. On corruption the marker is
/// removed, forcing the next process to quarantine/rebuild before normal use.
/// An undetermined probe neither certifies nor condemns: the marker is left as
/// it is and the probe's own error is returned.
pub fn verify_and_mark(conn: &Connection, db_path: &Path) -> Result<()> {
    match is_structurally_sound(conn) {
        Soundness::Sound => {
            touch_marker(&marker_path(db_path));
            Ok(())
        }
        Soundness::Corrupt { .. } => {
            invalidate_marker(db_path);
            Err(crate::error::ErrorKind::IndexCorrupt {
                path: db_path.to_path_buf(),
            }
            .into())
        }
        Soundness::Undetermined(e) => Err(e.into()),
    }
}

/// [`verify_and_mark`], skipped when the last probe is younger than
/// [`CHECK_INTERVAL`].
///
/// For a database that can reach gigabytes (the code index), a full-file
/// `quick_check` after every mutation would cost more than the mutation. The
/// throttle bounds it to one scan per interval while still bounding how long
/// corruption can go unnoticed — which matters because a long-lived connection
/// serves reads from its page cache and writes into the WAL, so it can operate
/// for days over a torn file without SQLite ever reporting it.
pub fn verify_and_mark_throttled(db_path: &Path) -> Result<()> {
    verify_and_mark_throttled_at(db_path, CHECK_INTERVAL, SystemTime::now())
}

/// [`verify_and_mark_throttled`] with an injectable interval and clock.
pub fn verify_and_mark_throttled_at(
    db_path: &Path,
    interval: Duration,
    now: SystemTime,
) -> Result<()> {
    if checked_recently(db_path, &marker_path(db_path), interval, now) {
        return Ok(());
    }
    if !db_path.exists() {
        return Ok(());
    }

    // Probe on a THROWAWAY connection, not the caller's. A long-lived
    // connection answers `quick_check` out of its own page cache, so damage
    // written to the file underneath it — the whole failure mode this guards —
    // reads back as sound. A fresh connection sees the file (plus its WAL).
    let probe = open_probe(db_path)?;
    verify_and_mark(&probe, db_path)
}

/// Rename `db_path` and its `-wal`/`-shm` sidecars to `*.corrupt-<unix_secs>` so
/// a fresh database can take the original path. Returns the quarantined main-DB
/// path. The `-wal`/`-shm` moves are best-effort (they may not exist).
pub fn quarantine(db_path: &Path) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let corrupt_path = available_quarantine_path(db_path, ts);
    std::fs::rename(db_path, &corrupt_path)?;
    let mut moved_sidecars = Vec::new();
    for ext in ["-wal", "-shm"] {
        let side = with_suffix(db_path, ext);
        if side.exists() {
            let target = with_suffix(&corrupt_path, ext);
            if let Err(error) = std::fs::rename(&side, &target) {
                // A fresh database must never open beside an orphaned WAL from the corrupt
                // generation. Restore every completed rename before returning the failure.
                for (moved, original) in moved_sidecars.into_iter().rev() {
                    let _ = std::fs::rename(moved, original);
                }
                let _ = std::fs::rename(&corrupt_path, db_path);
                return Err(error.into());
            }
            moved_sidecars.push((target, side));
        }
    }
    Ok(corrupt_path)
}

fn available_quarantine_path(db_path: &Path, timestamp: u64) -> PathBuf {
    let base_suffix = format!(".corrupt-{timestamp}");
    let mut candidate = with_suffix(db_path, &base_suffix);
    let mut collision = 0_u32;
    while candidate.exists() {
        collision += 1;
        candidate = with_suffix(db_path, &format!("{base_suffix}-{collision}"));
    }
    candidate
}

/// Count of rows recovered from a quarantined database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Salvage {
    pub entries: usize,
    pub edges: usize,
    /// Collection registrations recovered. Reported separately because their
    /// loss has a different symptom from memory loss: not "an entry is missing"
    /// but "every later `mdkb update` silently indexes the wrong thing".
    pub collections: usize,
    /// Memory revisions recovered — edit history, and since schema v19 the
    /// losing side of every file/DB conflict.
    pub revisions: usize,
    /// Mined behavioural priors (candidates + clusters) recovered.
    pub priors: usize,
    /// Did every table come across whole?
    ///
    /// False when the ATTACH failed, when a table's rows could not be counted
    /// or read, or when fewer rows landed than were present. The counts alone
    /// cannot carry this: a salvage that recovered nothing because the file
    /// would not open reports 0/0, and so does a store whose memory table was
    /// genuinely empty. Only this flag separates them, and the quarantine
    /// sweep needs that distinction before it deletes the only copy.
    pub complete: bool,
}

impl Salvage {
    /// A salvage that never got off the ground.
    fn failed() -> Self {
        Self {
            complete: false,
            ..Default::default()
        }
    }
}

/// Tables that a quarantine must carry into the fresh database, in the order
/// they are copied.
///
/// The rule is whether the rows can be re-derived from files still on disk.
/// `documents`, `content` and `edges` can: `mdkb update` rebuilds them from the
/// markdown, so dropping them costs a reindex. These cannot. `collections`
/// records the *decision* that a directory is a collection, which exists nowhere
/// else — story 012-19e7 is what its loss looks like from outside: a store went
/// from 2046 indexed documents to 3, `mdkb update` printed success and exited 0,
/// and the cause was blamed on an unrelated config edit for weeks.
///
/// Order matters: `memory_revisions` has a foreign key onto `memory_entries`, so
/// the parent is copied first and `INSERT OR IGNORE` drops any child whose
/// parent was in the torn pages.
///
/// `evolution` is deliberately absent — its foreign keys point at `documents`,
/// which the rebuild wipes, so every row would be rejected. Recovering it would
/// have to happen after a reindex, against document ids that are re-assigned.
const SALVAGED_TABLES: [&str; 5] = [
    "memory_entries",
    "memory_edges",
    "memory_revisions",
    "collections",
    "prior_clusters",
];

/// A SQLite URI naming `path`, with `immutable=1`.
///
/// Not a `format!`, because three characters in a path change what the URI
/// means. SQLite parses everything after the first `?` as query parameters and
/// everything after `#` as a fragment, and in URI mode it wants `/` separators.
/// A Windows temp directory reaches this function as
/// `\\?\C:\Users\…\index.sqlite.corrupt-…`: the `?` of the extended-length
/// prefix ended the path after two characters and turned the rest of the drive
/// path into nonsense parameters, so `ATTACH` failed and the heal reported
/// "salvaged 0 memory entries" while the entries were sitting in the file.
/// Silent data loss on exactly the path whose job is to prevent it.
fn immutable_uri(path: &Path) -> String {
    let mut encoded = String::from("file:");
    // `\\?\` only ever prefixes an already-absolute Windows path, and SQLite
    // does not want it.
    let raw = path.to_string_lossy();
    let raw = raw.strip_prefix(r"\\?\").unwrap_or(&raw);
    // A drive-qualified path becomes `file:///C:/…`; a Unix path already starts
    // with `/`, which `file:` accepts as-is.
    if raw.as_bytes().get(1) == Some(&b':') {
        encoded.push_str("///");
    }
    for ch in raw.chars() {
        match ch {
            '?' => encoded.push_str("%3f"),
            '#' => encoded.push_str("%23"),
            // Only on Windows: `\` is an ordinary character in a Unix file name
            // and rewriting it would name a different file.
            '\\' if cfg!(windows) => encoded.push('/'),
            other => encoded.push(other),
        }
    }
    encoded.push_str("?immutable=1");
    encoded
}

/// Copy the non-derivable tables out of a quarantined database into the fresh
/// one via `ATTACH ... immutable=1`.
///
/// `immutable=1` tells SQLite the file will not change, so it skips locking and
/// hot-journal rollback — the only safe way to read a possibly-corrupt file. The
/// copy is best-effort: it never fails the caller's open. A table that cannot be
/// read (its pages are the torn ones) is logged loudly with the row count that
/// was present but lost, so a data-loss event is never silent.
pub fn salvage_memory(fresh: &Connection, corrupt_path: &Path) -> Salvage {
    let uri = immutable_uri(corrupt_path);
    if let Err(e) = fresh.execute("ATTACH DATABASE ?1 AS corrupt", params![uri]) {
        tracing::error!(
            "salvage: cannot attach quarantined {} ({e}) — memory entries and collection \
             registrations may be lost",
            corrupt_path.display()
        );
        return Salvage::failed();
    }
    let mut salvage = Salvage {
        complete: true,
        ..Default::default()
    };
    for table in SALVAGED_TABLES {
        let (rows, whole) = salvage_table(fresh, table);
        salvage.complete &= whole;
        match table {
            "memory_entries" => salvage.entries = rows,
            "memory_edges" => salvage.edges = rows,
            "memory_revisions" => salvage.revisions = rows,
            "collections" => salvage.collections = rows,
            _ => salvage.priors += rows,
        }
    }
    // Candidates reference a cluster, so clusters go first; counted together
    // because the pair is one feature to the operator.
    let (candidates, candidates_whole) = salvage_table(fresh, "prior_candidates");
    salvage.priors += candidates;
    salvage.complete &= candidates_whole;
    if let Err(e) = fresh.execute("DETACH DATABASE corrupt", []) {
        tracing::warn!("salvage: detach failed: {e}");
    }
    if salvage.collections > 0 {
        tracing::warn!(
            collections = salvage.collections,
            "salvaged collection registrations from the quarantined index — run `mdkb update` \
             to re-index their documents"
        );
    }
    salvage
}

/// Physical column names of `table` in the attached schema `schema`, in the
/// order the rows actually store them.
///
/// An absent or unreadable table yields an empty list, which the caller reads
/// as "nothing to copy" — the best-effort contract, not an error.
fn table_columns(conn: &Connection, schema: &str, table: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare("SELECT name FROM pragma_table_info(?1, ?2)") else {
        return Vec::new();
    };
    stmt.query_map(params![table, schema], |r| r.get::<_, String>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

/// Double-quote an identifier so it can be interpolated into SQL.
///
/// Table names in this module are constants, but column names are read out of a
/// quarantined database: they are data, and a name carrying a quote or a
/// reserved word must not be able to change the statement around it.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Copy one whole table from the attached `corrupt` db into `main`, returning the
/// number of rows recovered. `table` is a hardcoded constant (never user input),
/// so the format-string SQL carries no injection risk.
///
/// The columns are matched BY NAME, over the intersection of the two tables,
/// because two stores at the same schema version do NOT agree on physical
/// column order: `ALTER TABLE ADD COLUMN` appends to the end of the row while
/// `CREATE TABLE` puts the column where the schema text does. A store that
/// reached the version by migration and one created fresh therefore lay their
/// rows out differently, and `SELECT *` — which is positional — copies each
/// value into whatever column happens to sit at that index.
///
/// Measured 2026-09-20 on this repo, `pragma_table_info('memory_entries')` on
/// the live index against a quarantined copy: index 15 was `last_refuted_at` in
/// one and `source_type` in the other, and every column after it was shifted by
/// two. 113 of 119 salvaged entries held `source_type` in `last_refuted_at`,
/// `due_at` in `source_type` and `projected_hash` in `created_agent`; `mdkb
/// memory sync` died with `Invalid column type Text at index: 15, name:
/// last_refuted_at`, and the confidence multiplier read a blank `source_type`.
/// The salvage had logged "salvaged 113 memory entries" over it.
///
/// A column only the fresh schema has takes its own default. A column only the
/// quarantined store has cannot be kept, so it is named in a loud log with the
/// rows it affects — dropping data quietly is how this defect class hides.
fn salvage_table(fresh: &Connection, table: &str) -> (usize, bool) {
    let present: usize =
        match fresh.query_row(&format!("SELECT COUNT(*) FROM corrupt.{table}"), [], |r| {
            r.get(0)
        }) {
            Ok(n) => n,
            Err(e) => {
                // The count itself is a read of the table's pages. When those are
                // the torn ones this fails, and treating it as "empty" is how a
                // total loss gets reported as a clean salvage of nothing.
                tracing::error!(
                    "memory salvage: {table} could not even be counted ({e}) — its rows are LOST"
                );
                return (0, false);
            }
        };
    if present == 0 {
        return (0, true);
    }
    let target = table_columns(fresh, "main", table);
    let source = table_columns(fresh, "corrupt", table);
    let shared: Vec<&String> = target.iter().filter(|c| source.contains(c)).collect();
    if shared.is_empty() {
        tracing::error!(
            "memory salvage: {present} rows in {table} could NOT be recovered — the quarantined table shares no column name with the current schema — they are LOST"
        );
        return (0, false);
    }
    let dropped: Vec<&str> = source
        .iter()
        .filter(|c| !target.contains(c))
        .map(String::as_str)
        .collect();
    if !dropped.is_empty() {
        tracing::warn!(
            "memory salvage: {table} column(s) {} exist only in the quarantined store — their values are DROPPED from all {present} salvaged row(s), the current schema has nowhere to put them",
            dropped.join(", ")
        );
    }
    let defaulted: Vec<&str> = target
        .iter()
        .filter(|c| !source.contains(c))
        .map(String::as_str)
        .collect();
    if !defaulted.is_empty() {
        tracing::info!(
            "memory salvage: {table} column(s) {} are absent from the quarantined store — all {present} salvaged row(s) take the schema default",
            defaulted.join(", ")
        );
    }
    let columns = shared
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    match fresh.execute(
        &format!(
            "INSERT OR IGNORE INTO main.{table} ({columns}) SELECT {columns} FROM corrupt.{table}"
        ),
        [],
    ) {
        Ok(inserted) => {
            if inserted < present {
                let not_recovered = present - inserted;
                tracing::error!(
                    "memory salvage: {not_recovered} of {present} rows in {table} were NOT recovered (INSERT OR IGNORE skipped them)"
                );
            }
            (inserted, inserted >= present)
        }
        Err(e) => {
            tracing::error!(
                "memory salvage: {present} rows in {table} could NOT be recovered ({e}) — they are LOST"
            );
            (0, false)
        }
    }
}

/// A record of one quarantine event, persisted as a sidecar next to the corrupt
/// file so `mdkb stats` and SessionStart can surface the loss until the operator
/// cleans up. Serialized to `<corrupt_file>.report.json`.
///
/// The forensic fields exist because a quarantined file on its own has never
/// been enough to name a cause: every past post-mortem stalled at "the index is
/// malformed" with no record of *how*. They are captured once, at quarantine
/// time, on a file that is already known to be corrupt — so they cost nothing
/// on any healthy path.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct QuarantineReport {
    /// File name (not full path) of the quarantined database.
    pub corrupt_file: String,
    /// Unix seconds when the quarantine happened (from the corrupt file suffix).
    pub quarantined_at: i64,
    /// Memory entries recovered into the fresh database.
    pub memory_entries_salvaged: usize,
    /// Memory edges recovered into the fresh database.
    pub memory_edges_salvaged: usize,
    /// Did the salvage bring every table across whole?
    ///
    /// `serde(default)` is `false`, which is the safe direction: a report
    /// written before this field existed reads as "not known to have
    /// succeeded", so the sweep keeps its copy instead of deleting evidence on
    /// the strength of a field nobody wrote.
    #[serde(default)]
    pub salvage_succeeded: bool,
    /// `PRAGMA quick_check` rows: the damage as SQLite describes it.
    #[serde(default)]
    pub quick_check: Vec<String>,
    /// Tables owning the b-trees `quick_check` named, resolved through
    /// `sqlite_master.rootpage`. This is what distinguishes one recurrence from
    /// another — field damage has so far always landed on the memory tables.
    #[serde(default)]
    pub damaged_tables: Vec<String>,
    /// Size of the quarantined database and of its WAL, in bytes. A large WAL
    /// means the damage was taken with un-checkpointed frames outstanding.
    #[serde(default)]
    pub db_bytes: u64,
    #[serde(default)]
    pub wal_bytes: u64,
    /// Process that detected the corruption — NOT necessarily the one that
    /// caused it, which is exactly why the distinction is spelled out here.
    #[serde(default)]
    pub detected_by_pid: u32,
    #[serde(default)]
    pub detected_by_version: String,
}

/// Forensics read off a quarantined file. Best-effort throughout: a file too
/// damaged to answer a question contributes nothing rather than failing the
/// quarantine.
#[derive(Debug, Clone, Default)]
struct Diagnosis {
    quick_check: Vec<String>,
    damaged_tables: Vec<String>,
    db_bytes: u64,
    wal_bytes: u64,
}

/// Rows `quick_check` reports name b-trees by root page (`Tree 23 page 23 cell
/// 4: ...`). Extract those root pages so they can be resolved to table names.
fn root_pages_in(rows: &[String]) -> Vec<i64> {
    let mut pages = Vec::new();
    for row in rows {
        let Some(rest) = row.strip_prefix("Tree ") else {
            continue;
        };
        let Some(page) = rest.split_whitespace().next() else {
            continue;
        };
        if let Ok(page) = page.parse::<i64>()
            && !pages.contains(&page)
        {
            pages.push(page);
        }
    }
    pages
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Describe the damage in a quarantined database.
///
/// Read through `immutable=1`, the only safe way to open a file that may be
/// torn: no locking, no hot-journal rollback, no writes.
fn diagnose(corrupt_path: &Path) -> Diagnosis {
    let mut diagnosis = Diagnosis {
        db_bytes: file_bytes(corrupt_path),
        wal_bytes: file_bytes(&with_suffix(corrupt_path, "-wal")),
        ..Default::default()
    };

    let uri = immutable_uri(corrupt_path);
    let conn = match Connection::open(&uri) {
        Ok(conn) => conn,
        Err(e) => {
            diagnosis.quick_check = vec![format!("cannot open quarantined file: {e}")];
            return diagnosis;
        }
    };

    // Bounded: enough rows to characterise the damage, not a full page walk of
    // a file that can reach gigabytes.
    //
    // Stepped by hand rather than collected through an iterator because damage
    // bad enough to abort the walk surfaces as an Err *after* zero or more
    // rows, and that Err is itself the diagnosis — dropping it (as `flatten`
    // would) is how a badly torn file ends up recorded as "no damage found".
    diagnosis.quick_check = match conn.prepare("PRAGMA quick_check(20)") {
        Ok(mut stmt) => match stmt.query([]) {
            Ok(mut rows) => {
                let mut out = Vec::new();
                loop {
                    match rows.next() {
                        Ok(Some(row)) => match row.get::<_, String>(0) {
                            Ok(text) if text != "ok" => out.push(text),
                            Ok(_) => {}
                            Err(e) => out.push(format!("unreadable quick_check row: {e}")),
                        },
                        Ok(None) => break,
                        Err(e) => {
                            out.push(format!("quick_check aborted: {e}"));
                            break;
                        }
                    }
                }
                out
            }
            Err(e) => vec![format!("quick_check failed: {e}")],
        },
        Err(e) => vec![format!("quick_check unavailable: {e}")],
    };

    for page in root_pages_in(&diagnosis.quick_check) {
        let name = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE rootpage = ?1",
                params![page],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| format!("rootpage {page}"));
        if !diagnosis.damaged_tables.contains(&name) {
            diagnosis.damaged_tables.push(name);
        }
    }

    diagnosis
}

/// `.report.json` sidecar path for a quarantined database file.
fn report_path(corrupt_path: &Path) -> PathBuf {
    with_suffix(corrupt_path, ".report.json")
}

/// True only for the exact suffix shapes mdkb generates for a quarantined
/// copy: `.corrupt-<unix_secs>`, optionally followed by `-<collision>` (from
/// [`available_quarantine_path`]) and/or `-wal`/`-shm` (from the sidecar
/// rename in [`quarantine`]) — every one of those components is digits only.
/// A bare `contains(".corrupt-")` matches anything with that substring
/// anywhere under `.mdkb`, which is too wide a blast radius for the
/// irreversible `remove_file` the sweep gates on it. Returns the timestamp
/// digits on a match.
fn quarantine_suffix(name: &str) -> Option<&str> {
    let (_, suffix) = name.rsplit_once(".corrupt-")?;
    let suffix = suffix
        .strip_suffix("-wal")
        .or_else(|| suffix.strip_suffix("-shm"))
        .unwrap_or(suffix);
    let ts_part = match suffix.split_once('-') {
        Some((ts, collision)) => {
            if collision.is_empty() || !collision.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            ts
        }
        None => suffix,
    };
    (!ts_part.is_empty() && ts_part.bytes().all(|b| b.is_ascii_digit())).then_some(ts_part)
}

/// Parse the trailing `.corrupt-<unix_secs>` suffix into its timestamp. `None`
/// for anything that is not a name mdkb generates — see [`quarantine_suffix`].
fn quarantine_ts(name: &str) -> Option<i64> {
    quarantine_suffix(name).and_then(|ts| ts.parse::<i64>().ok())
}

/// Write the quarantine report sidecar. Best-effort — a failed write only costs
/// the persistent notification, never the salvage itself.
pub fn write_report(corrupt_path: &Path, salvage: Salvage) {
    let name = corrupt_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let diagnosis = diagnose(corrupt_path);
    if !diagnosis.damaged_tables.is_empty() {
        tracing::error!(
            tables = diagnosis.damaged_tables.join(", "),
            "index corruption damaged these tables"
        );
    }
    let report = QuarantineReport {
        quarantined_at: quarantine_ts(&name).unwrap_or(0),
        corrupt_file: name,
        memory_entries_salvaged: salvage.entries,
        memory_edges_salvaged: salvage.edges,
        salvage_succeeded: salvage.complete,
        quick_check: diagnosis.quick_check,
        damaged_tables: diagnosis.damaged_tables,
        db_bytes: diagnosis.db_bytes,
        wal_bytes: diagnosis.wal_bytes,
        detected_by_pid: std::process::id(),
        detected_by_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    match serde_json::to_string_pretty(&report) {
        Ok(json) => {
            if let Err(e) = std::fs::write(report_path(corrupt_path), json) {
                tracing::warn!("quarantine report write failed: {e}");
            }
        }
        Err(e) => tracing::warn!("quarantine report serialize failed: {e}"),
    }
}

/// All outstanding quarantine reports in `mdkb_dir`: one per `*.corrupt-*`
/// database still on disk. The corrupt DB file (not its `.report.json` sidecar)
/// is the gating artifact — the warning clears once the operator deletes it.
/// A missing/unreadable sidecar still yields a report (with zero salvage counts)
/// so the quarantine itself is never hidden.
pub fn quarantine_reports(mdkb_dir: &Path) -> Vec<QuarantineReport> {
    let Ok(entries) = std::fs::read_dir(mdkb_dir) else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // The corrupt DB itself, not the sidecar or WAL/SHM siblings.
        if quarantine_suffix(&name).is_none()
            || name.ends_with(".report.json")
            || name.ends_with("-wal")
            || name.ends_with("-shm")
        {
            continue;
        }
        let report = std::fs::read_to_string(report_path(&entry.path()))
            .ok()
            .and_then(|s| serde_json::from_str::<QuarantineReport>(&s).ok())
            .unwrap_or_else(|| QuarantineReport {
                quarantined_at: quarantine_ts(&name).unwrap_or(0),
                corrupt_file: name.clone(),
                ..Default::default()
            });
        reports.push(report);
    }
    reports.sort_by_key(|r| r.quarantined_at);
    reports
}

/// Delete every quarantined database copy in `mdkb_dir` older than
/// [`QUARANTINE_RETENTION`], keeping the `.report.json` sidecars.
///
/// The `-wal`/`-shm` siblings go with the copy: they belong to the corrupt
/// generation and are unreadable without it.
///
/// Age comes from the `.corrupt-<unix_secs>` suffix, not from the file mtime,
/// because a copy is renamed rather than written — its mtime is the mtime of
/// the last write to the *healthy* generation, which can be arbitrarily older
/// than the quarantine.
pub fn sweep_expired_quarantines(mdkb_dir: &Path) {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    sweep_expired_quarantines_at(mdkb_dir, QUARANTINE_RETENTION, now);
}

/// [`sweep_expired_quarantines`] with an injectable retention and clock.
fn sweep_expired_quarantines_at(mdkb_dir: &Path, retention: Duration, now_secs: i64) {
    let Ok(entries) = std::fs::read_dir(mdkb_dir) else {
        return;
    };
    // Which quarantines were actually salvaged?
    //
    // `memory_entries` and `memory_edges` live only in `index.sqlite`, so
    // until the salvage succeeds the quarantined copy is the ONLY carrier of
    // that data. Age is not evidence that it was recovered: a crash between
    // the quarantine and the salvage leaves no report at all, and an ATTACH
    // that failed leaves one reporting 0 entries — the same text a genuinely
    // empty memory table produces. Deleting on age alone turns a recoverable
    // state into a permanent loss, fifteen days later, in silence.
    let salvaged: std::collections::HashSet<i64> = std::fs::read_dir(mdkb_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".report.json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|raw| serde_json::from_str::<QuarantineReport>(&raw).ok())
        .filter(|r| r.salvage_succeeded)
        .map(|r| r.quarantined_at)
        .collect();

    let retention_secs = retention.as_secs() as i64;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // Everything the quarantine renamed — the DB and its `-wal`/`-shm` —
        // but never the report that outlives them.
        if quarantine_suffix(&name).is_none() || name.ends_with(".report.json") {
            continue;
        }
        let Some(quarantined_at) = quarantine_ts(&name) else {
            continue;
        };
        // A negative age (a copy stamped in the future by a skewed clock) is
        // not an expiry. Keep it.
        if now_secs - quarantined_at <= retention_secs {
            continue;
        }
        // Expired, but never salvaged: this copy is the last one standing
        // between the operator and the loss. Keep it and say why, once per
        // sweep, at a level they will see.
        if !salvaged.contains(&quarantined_at) {
            tracing::warn!(
                "quarantine {name} is past its retention but its salvage never succeeded;                  keeping it — it may hold the only copy of those memory entries"
            );
            continue;
        }
        // Best-effort. Windows refuses to unlink a file another process still
        // holds open, and a sweep must never fail the open that triggered it —
        // the copy just survives to the next one. The banner promises removal
        // "N days after quarantine", so a failure to keep that promise must be
        // visible at an operator-facing level, not buried at debug.
        if let Err(e) = std::fs::remove_file(entry.path()) {
            tracing::warn!("quarantine sweep left {name} in place: {e}");
        }
    }
}

/// Probe `db_path` for structural corruption (throttled by [`CHECK_INTERVAL`])
/// and quarantine it if corrupt.
///
/// Call BEFORE opening the working connection: on [`Heal::Quarantined`] the
/// original path no longer exists, so the caller's `Connection::open` creates a
/// clean database in its place.
pub fn ensure_sound(db_path: &Path) -> Result<Heal> {
    let _guard = crate::store::mutation_lock::acquire(db_path, "integrity-check")?;
    ensure_sound_locked(db_path)
}

/// Probe while the caller already holds the project mutation lock.
///
/// Used by `Context::open`, which must keep the same lock through schema and
/// virtual-table initialization so concurrent openers cannot race FTS setup.
pub(crate) fn ensure_sound_locked(db_path: &Path) -> Result<Heal> {
    ensure_sound_at_locked(db_path, CHECK_INTERVAL, SystemTime::now())
}

/// [`ensure_sound`] with an injectable interval and clock, for tests.
#[cfg(test)]
fn ensure_sound_at(db_path: &Path, interval: Duration, now: SystemTime) -> Result<Heal> {
    let _guard = crate::store::mutation_lock::acquire(db_path, "integrity-check")?;
    ensure_sound_at_locked(db_path, interval, now)
}

/// Integrity probe implementation. The caller must hold the mutation lock.
fn ensure_sound_at_locked(db_path: &Path, interval: Duration, now: SystemTime) -> Result<Heal> {
    if !db_path.exists() {
        return Ok(Heal::Sound); // fresh database — nothing to probe
    }
    let marker = marker_path(db_path);
    if checked_recently(db_path, &marker, interval, now) {
        return Ok(Heal::Sound);
    }

    // Probe on a throwaway connection so no open handle survives the rename.
    let verdict = {
        let probe = open_probe(db_path)?;
        is_structurally_sound(&probe)
    };

    match verdict {
        Soundness::Sound => {
            touch_marker(&marker);
            return Ok(Heal::Sound);
        }
        Soundness::Corrupt { .. } => {}
        // A locked or unreadable file is not a torn one. Leave it where it is,
        // leave the marker alone, and let the caller report why the open failed.
        Soundness::Undetermined(e) => return Err(e.into()),
    }

    // Only rename when nobody is holding the database open — see [`Heal::CorruptInUse`].
    let Some(_live) = crate::store::mutation_lock::try_acquire_live_exclusive(db_path)? else {
        let _ = std::fs::remove_file(&marker);
        return Ok(Heal::CorruptInUse);
    };

    let corrupt_path = quarantine(db_path)?;
    let _ = std::fs::remove_file(&marker);
    Ok(Heal::Quarantined { corrupt_path })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a small on-disk database with a table and rows so it occupies
    /// several pages — enough that truncation produces a torn b-tree.
    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, blob TEXT);
             PRAGMA journal_mode = DELETE;", // no -wal so the file is self-contained
        )
        .unwrap();
        let payload = "x".repeat(2000);
        for i in 0..200 {
            conn.execute("INSERT INTO t (id, blob) VALUES (?1, ?2)", (i, &payload))
                .unwrap();
        }
    }

    #[test]
    fn sound_db_passes_probe() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        let conn = Connection::open(&db).unwrap();
        assert!(matches!(is_structurally_sound(&conn), Soundness::Sound));
    }

    #[test]
    fn garbage_file_is_not_sound() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"this is definitely not a sqlite database").unwrap();

        let conn = Connection::open(&db).unwrap();
        assert!(matches!(
            is_structurally_sound(&conn),
            Soundness::Corrupt { .. }
        ));
    }

    #[test]
    fn truncated_db_is_not_sound() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        // Lop off the trailing pages: valid header, torn b-tree — exactly the
        // class of structural damage that ordinary reads miss but quick_check catches.
        let len = std::fs::metadata(&db).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        f.set_len(len / 2).unwrap();
        drop(f);

        let conn = Connection::open(&db).unwrap();
        assert!(matches!(
            is_structurally_sound(&conn),
            Soundness::Corrupt { .. }
        ));
    }

    #[test]
    fn a_locked_database_probes_as_undetermined() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);
        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();

        // rusqlite gives every connection a 5 s busy timeout; zero it so the
        // verdict is immediate.
        let probe = Connection::open(&db).unwrap();
        probe.busy_timeout(Duration::ZERO).unwrap();
        let verdict = is_structurally_sound(&probe);

        assert!(
            matches!(verdict, Soundness::Undetermined(_)),
            "BUSY says nothing about the file: {verdict:?}"
        );
        drop(holder);
    }

    #[test]
    fn root_pages_are_read_off_real_quick_check_rows() {
        // Verbatim rows from quarantined field stores.
        let rows = vec![
            "*** in database main ***".to_string(),
            "Tree 23 page 23 cell 35: 2nd reference to page 1820".to_string(),
            "Tree 23 page 23 cell 34: 2nd reference to page 1819".to_string(),
            "Tree 66 page 66 cell 0: 2nd reference to page 5191".to_string(),
            "wrong # of entries in index idx_memory_access".to_string(),
        ];
        assert_eq!(
            root_pages_in(&rows),
            vec![23, 66],
            "each damaged b-tree is named once, in report order"
        );
    }

    #[test]
    fn the_report_records_the_damage_not_just_the_loss() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        let len = std::fs::metadata(&db).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        f.set_len(len / 2).unwrap();
        drop(f);

        let corrupt = quarantine(&db).unwrap();
        write_report(&corrupt, Salvage::default());

        let report: QuarantineReport =
            serde_json::from_str(&std::fs::read_to_string(report_path(&corrupt)).unwrap()).unwrap();

        assert!(
            !report.quick_check.is_empty(),
            "the quarantine must record how SQLite described the damage"
        );
        assert_eq!(
            report.db_bytes,
            len / 2,
            "the size of the file that was set aside"
        );
        assert_eq!(report.detected_by_pid, std::process::id());
        assert_eq!(report.detected_by_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn quarantine_moves_db_and_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(with_suffix(&db, "-wal"), b"wal").unwrap();
        std::fs::write(with_suffix(&db, "-shm"), b"shm").unwrap();

        let corrupt = quarantine(&db).unwrap();

        assert!(!db.exists(), "original db removed");
        assert!(corrupt.exists(), "db quarantined");
        assert!(
            with_suffix(&corrupt, "-wal").exists(),
            "wal quarantined alongside db"
        );
        assert!(
            with_suffix(&corrupt, "-shm").exists(),
            "shm quarantined alongside db"
        );
        assert!(!with_suffix(&db, "-wal").exists(), "original wal removed");
    }

    #[test]
    fn quarantine_path_never_overwrites_an_existing_forensic_copy() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        let first = with_suffix(&db, ".corrupt-123");
        let second = with_suffix(&db, ".corrupt-123-1");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();

        let candidate = available_quarantine_path(&db, 123);

        assert_eq!(candidate, with_suffix(&db, ".corrupt-123-2"));
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        assert_eq!(quarantine_ts("index.sqlite.corrupt-123-2"), Some(123));
    }

    #[test]
    fn ensure_sound_quarantines_corrupt_db_and_frees_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"not a sqlite database at all").unwrap();

        let outcome = ensure_sound(&db).unwrap();

        match outcome {
            Heal::Quarantined { corrupt_path } => assert!(corrupt_path.exists()),
            other => panic!("corrupt db must be quarantined, got {other:?}"),
        }
        assert!(!db.exists(), "path is freed for a fresh database");
    }

    #[test]
    fn corrupt_db_is_left_in_place_while_a_connection_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"not a sqlite database at all").unwrap();

        // Somebody has the database open: renaming it now would recycle the path
        // onto a second inode while that connection keeps deriving its
        // `-wal`/`-shm` from the old name.
        let _live = crate::store::mutation_lock::acquire_live_shared(&db).unwrap();

        assert_eq!(
            ensure_sound(&db).unwrap(),
            Heal::CorruptInUse,
            "a live connection must veto the quarantine"
        );
        assert!(
            db.exists(),
            "the corrupt file stays where the holder sees it"
        );
        assert!(
            !marker_path(&db).exists(),
            "no integrity marker, so the next open re-probes instead of trusting it"
        );
    }

    #[test]
    fn quarantine_resumes_once_the_last_connection_closes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"not a sqlite database at all").unwrap();

        let live = crate::store::mutation_lock::acquire_live_shared(&db).unwrap();
        assert_eq!(ensure_sound(&db).unwrap(), Heal::CorruptInUse);
        drop(live);

        assert!(
            matches!(ensure_sound(&db).unwrap(), Heal::Quarantined { .. }),
            "with no holder left the corrupt file is quarantined as before"
        );
        assert!(!db.exists(), "path is freed for a fresh database");
    }

    #[test]
    fn ensure_sound_leaves_healthy_db_and_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        assert_eq!(ensure_sound(&db).unwrap(), Heal::Sound);
        assert!(db.exists(), "healthy db untouched");
        assert!(marker_path(&db).exists(), "successful probe records marker");
    }

    #[test]
    fn ensure_sound_skips_probe_when_recently_checked() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        // Corrupt bytes, but a fresh marker: the throttle must skip the probe so
        // the corrupt file survives (proving the probe genuinely didn't run).
        std::fs::write(&db, b"corrupt").unwrap();
        touch_marker(&marker_path(&db));

        let outcome = ensure_sound_at(&db, CHECK_INTERVAL, SystemTime::now()).unwrap();
        assert_eq!(outcome, Heal::Sound, "recent marker skips the probe");
        assert!(db.exists(), "throttled probe left the file untouched");
    }

    #[test]
    fn ensure_sound_does_not_trust_a_marker_older_than_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);
        touch_marker(&marker_path(&db));

        // The production incident had a marker from 10:10 and a DB modified at
        // 12:09. Preserve that ordering at a much smaller scale.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&db, b"corrupt after the successful probe").unwrap();

        let outcome = ensure_sound_at(&db, CHECK_INTERVAL, SystemTime::now()).unwrap();
        assert!(
            matches!(outcome, Heal::Quarantined { .. }),
            "a post-marker DB write must force a fresh integrity probe"
        );
    }

    #[test]
    fn ensure_sound_reprobes_after_interval_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"corrupt").unwrap();
        touch_marker(&marker_path(&db));

        // A zero-length interval forces every probe to run despite the marker.
        let outcome = ensure_sound_at(&db, Duration::ZERO, SystemTime::now()).unwrap();
        assert!(
            matches!(outcome, Heal::Quarantined { .. }),
            "elapsed interval re-probes and quarantines the corrupt file"
        );
    }

    #[test]
    fn ensure_sound_on_missing_db_is_sound() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        assert_eq!(ensure_sound(&db).unwrap(), Heal::Sound);
    }

    /// Names in `dir` that carry the quarantine suffix.
    fn quarantined_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".corrupt-"))
            .collect()
    }

    #[test]
    fn a_locked_database_is_not_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        // Another connection holds the write lock for the whole probe. The db
        // is in rollback-journal mode, so that lock also keeps readers out and
        // `quick_check` comes back with SQLITE_BUSY — a fact about the lock,
        // not about the file.
        let holder = Connection::open(&db).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE").unwrap();

        let outcome = ensure_sound(&db);

        let err = outcome.expect_err("a locked file yields no verdict, so the open fails");
        assert!(
            !err.is_index_corrupt(),
            "BUSY must not be reported as corruption: {err}"
        );
        assert!(
            db.exists(),
            "the locked file stays where its holder sees it"
        );
        assert!(
            quarantined_names(dir.path()).is_empty(),
            "nothing may be quarantined on an undetermined probe"
        );
        drop(holder);
    }

    #[test]
    fn a_truncated_database_is_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);
        let f = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        f.set_len(100).unwrap();
        drop(f);

        let outcome = ensure_sound(&db).unwrap();

        assert!(
            matches!(outcome, Heal::Quarantined { .. }),
            "a torn file is corrupt, not undetermined: {outcome:?}"
        );
        assert!(!db.exists(), "path is freed for a fresh database");
        assert_eq!(quarantined_names(dir.path()).len(), 1);
    }

    /// A database with memory rows, standing in for the quarantined file.
    fn make_memory_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        crate::store::schema::init_schema(&conn).unwrap();
        for i in 0..2 {
            conn.execute(
                "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
                 VALUES (?1, ?2, 'body', 'topic', 1, 1)",
                params![format!("m{i}"), format!("Title {i}")],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO memory_edges (source_id, target_ref, target_kind, relation, created_at)
             VALUES ('m0', 'm1', 'memory', 'supports', 1)",
            [],
        )
        .unwrap();
    }

    /// These run on every platform on purpose. The bug they pin only *fires* on
    /// Windows, and a `#[cfg(windows)]` test would have been written by someone
    /// who already knew to look — which is precisely what did not happen.
    #[test]
    fn a_question_mark_in_a_path_cannot_end_the_uri() {
        // `?` opens SQLite's query string, so a literal one must be encoded or
        // everything after it stops naming the file. This is the `\\?\` prefix
        // case reduced to its essence.
        let uri = immutable_uri(Path::new("/tmp/od?d/index.sqlite"));
        assert_eq!(uri, "file:/tmp/od%3fd/index.sqlite?immutable=1");
        assert_eq!(
            uri.matches("?immutable=1").count(),
            1,
            "exactly one query string"
        );
    }

    #[test]
    fn a_hash_in_a_path_cannot_start_a_fragment() {
        assert_eq!(
            immutable_uri(Path::new("/tmp/v#2/index.sqlite")),
            "file:/tmp/v%232/index.sqlite?immutable=1"
        );
    }

    #[test]
    fn an_ordinary_path_is_left_alone() {
        assert_eq!(
            immutable_uri(Path::new("/tmp/mdkb/index.sqlite")),
            "file:/tmp/mdkb/index.sqlite?immutable=1"
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_extended_length_windows_path_names_the_file_it_points_at() {
        assert_eq!(
            immutable_uri(Path::new(r"\\?\C:\Users\me\.mdkb\index.sqlite")),
            "file:///C:/Users/me/.mdkb/index.sqlite?immutable=1"
        );
    }

    #[test]
    fn salvage_recovers_memory_tables_from_quarantined_db() {
        // memory_entries/memory_edges live ONLY in the index — a quarantine must
        // salvage them into the fresh db or they are gone (the tuicommander bug).
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("index.sqlite.corrupt-123");
        make_memory_db(&corrupt);

        let fresh = Connection::open(dir.path().join("index.sqlite")).unwrap();
        crate::store::schema::init_schema(&fresh).unwrap();

        let salvage = salvage_memory(&fresh, &corrupt);
        assert_eq!(salvage.entries, 2, "both memory entries recovered");
        assert_eq!(salvage.edges, 1, "the memory edge recovered");

        let entries: i64 = fresh
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(entries, 2);
        let edges: i64 = fresh
            .query_row("SELECT COUNT(*) FROM memory_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 1);
    }

    #[test]
    fn salvage_on_unreadable_file_returns_zero_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("index.sqlite.corrupt-9");
        std::fs::write(&bogus, b"not a database").unwrap();
        let fresh = Connection::open(dir.path().join("index.sqlite")).unwrap();
        crate::store::schema::init_schema(&fresh).unwrap();

        // Best-effort: garbage yields no rows, never an error/panic.
        let salvage = salvage_memory(&fresh, &bogus);
        assert_eq!(salvage, Salvage::default());
    }

    #[test]
    fn quarantine_report_persists_and_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("index.sqlite.corrupt-1700000000");
        std::fs::write(&corrupt, b"corrupt bytes").unwrap();
        write_report(
            &corrupt,
            Salvage {
                entries: 5,
                edges: 2,
                ..Default::default()
            },
        );

        let reports = quarantine_reports(dir.path());
        assert_eq!(reports.len(), 1, "one outstanding quarantine");
        assert_eq!(reports[0].memory_entries_salvaged, 5);
        assert_eq!(reports[0].memory_edges_salvaged, 2);
        assert_eq!(reports[0].quarantined_at, 1_700_000_000);
        assert!(reports[0].corrupt_file.ends_with(".corrupt-1700000000"));
    }

    #[test]
    fn quarantine_reports_empty_on_healthy_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.sqlite"), b"db").unwrap();
        assert!(quarantine_reports(dir.path()).is_empty());
    }

    /// Lay down a quarantined copy stamped `age` seconds ago, with the `-wal`
    /// and `.report.json` siblings a real quarantine leaves beside it. Returns
    /// the `now` the sweep must be given.
    fn quarantine_aged(dir: &Path, age: i64) -> (PathBuf, i64) {
        quarantine_aged_with(dir, age, true)
    }

    /// As [`quarantine_aged`], but choosing whether the sidecar records a
    /// salvage that actually worked. The sweep reads exactly that.
    fn quarantine_aged_with(dir: &Path, age: i64, salvaged: bool) -> (PathBuf, i64) {
        let now = 1_800_000_000_i64;
        let stamp = now - age;
        let corrupt = dir.join(format!("index.sqlite.corrupt-{stamp}"));
        std::fs::write(&corrupt, b"corrupt bytes").unwrap();
        std::fs::write(with_suffix(&corrupt, "-wal"), b"wal").unwrap();
        std::fs::write(
            report_path(&corrupt),
            format!(
                r#"{{"corrupt_file":"x","quarantined_at":{stamp},"memory_entries_salvaged":0,"memory_edges_salvaged":0,"salvage_succeeded":{salvaged}}}"#
            ),
        )
        .unwrap();
        (corrupt, now)
    }

    /// An expired copy whose salvage never succeeded is KEPT.
    ///
    /// `memory_entries` and `memory_edges` exist nowhere else, so until the
    /// salvage lands this file is the only copy. Two reachable states produce
    /// it: a crash between quarantine and salvage, which writes no report at
    /// all, and an ATTACH that failed, which writes one reporting nothing
    /// recovered. Neither is distinguishable from a clean salvage by age, and
    /// deleting on age turned both into permanent loss.
    #[test]
    fn an_expired_copy_whose_salvage_failed_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let (corrupt, now) =
            quarantine_aged_with(dir.path(), QUARANTINE_RETENTION.as_secs() as i64 + 1, false);

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, now);

        assert!(
            corrupt.exists(),
            "the only carrier of those memory entries must survive its retention"
        );
        assert!(
            with_suffix(&corrupt, "-wal").exists(),
            "and so must its wal"
        );
    }

    /// A quarantine with no report at all — the crash case — is kept too.
    #[test]
    fn an_expired_copy_with_no_report_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000_i64;
        let stamp = now - (QUARANTINE_RETENTION.as_secs() as i64 + 1);
        let corrupt = dir.path().join(format!("index.sqlite.corrupt-{stamp}"));
        std::fs::write(&corrupt, b"corrupt bytes").unwrap();

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, now);

        assert!(
            corrupt.exists(),
            "no report means the salvage never ran, not that it succeeded"
        );
    }

    #[test]
    fn a_copy_one_second_past_the_retention_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let (corrupt, now) = quarantine_aged(dir.path(), QUARANTINE_RETENTION.as_secs() as i64 + 1);

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, now);

        assert!(!corrupt.exists(), "expired copy must be gone");
        assert!(
            !with_suffix(&corrupt, "-wal").exists(),
            "the -wal belongs to the corrupt generation and goes with it"
        );
        assert!(
            report_path(&corrupt).exists(),
            "the forensics outlive the copy"
        );
        assert!(
            quarantine_reports(dir.path()).is_empty(),
            "the banner is gated on the copy, so it must clear with it"
        );
    }

    #[test]
    fn a_copy_one_second_short_of_the_retention_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (corrupt, now) = quarantine_aged(dir.path(), QUARANTINE_RETENTION.as_secs() as i64 - 1);

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, now);

        assert!(corrupt.exists(), "inside the retention, nothing is removed");
        assert!(with_suffix(&corrupt, "-wal").exists());
        assert_eq!(
            quarantine_reports(dir.path()).len(),
            1,
            "and the warning stays up"
        );
    }

    #[test]
    fn the_sweep_spares_the_live_index_and_a_future_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("index.sqlite");
        std::fs::write(&live, b"the working database").unwrap();
        // A clock that ran backwards stamps a copy in the future. Negative age
        // is not expiry.
        let skewed = dir.path().join("index.sqlite.corrupt-1900000000");
        std::fs::write(&skewed, b"x").unwrap();

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, 1_800_000_000);

        assert!(live.exists(), "the sweep never touches the live database");
        assert!(skewed.exists());
    }

    #[test]
    fn a_copy_still_held_open_does_not_fail_the_sweep() {
        // Windows refuses to unlink an open file; Unix unlinks it happily. The
        // sweep must return normally either way — its caller is `Context::open`,
        // and a store must not fail to open because a stale copy is locked.
        let dir = tempfile::tempdir().unwrap();
        let (corrupt, now) = quarantine_aged(dir.path(), QUARANTINE_RETENTION.as_secs() as i64 + 1);
        let held = std::fs::File::open(&corrupt).unwrap();

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, now);

        assert!(
            report_path(&corrupt).exists(),
            "whatever happened to the copy, the report is still there"
        );
        drop(held);
    }

    #[test]
    fn quarantine_reports_without_sidecar_still_reports() {
        // A corrupt file with no .report.json (e.g. an older quarantine) must not
        // be hidden — the quarantine itself is surfaced with zero salvage counts.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.sqlite.corrupt-42"), b"x").unwrap();
        let reports = quarantine_reports(dir.path());
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].quarantined_at, 42);
        assert_eq!(reports[0].memory_entries_salvaged, 0);
    }

    #[test]
    fn a_copy_exactly_at_the_retention_boundary_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (corrupt, now) = quarantine_aged(dir.path(), QUARANTINE_RETENTION.as_secs() as i64);

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, now);

        assert!(
            corrupt.exists(),
            "exactly at the boundary is not yet expired"
        );
        assert_eq!(quarantine_reports(dir.path()).len(), 1);
    }

    #[test]
    fn sweep_never_deletes_a_file_that_merely_contains_the_corrupt_substring() {
        // Real quarantine names are `.corrupt-<unix_secs>`, optionally followed
        // by `-<collision>` and/or `-wal`/`-shm` — all digits. A name with a
        // leading digit token after `.corrupt-` but a non-digit remainder is
        // not a shape mdkb ever generates. The old bare `contains(".corrupt-")`
        // gate plus `quarantine_ts` taking only the first dash token read this
        // as timestamp 0 — maximally expired — and deleted it on first sweep.
        let dir = tempfile::tempdir().unwrap();
        let stray = dir.path().join("notes.corrupt-0-explains-the-outage.md");
        std::fs::write(&stray, b"not a quarantine copy").unwrap();

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, 1_800_000_000);

        assert!(
            stray.exists(),
            "a file matching only by substring must survive the sweep"
        );
    }

    #[test]
    fn a_malformed_timestamp_is_neither_reported_nor_swept() {
        // The sweep and the banner must agree on what an unparseable suffix
        // means: previously quarantine_reports defaulted it to timestamp 0
        // (always shown) while the sweep skipped it (never deleted) — a
        // banner the sweep could never clear.
        let dir = tempfile::tempdir().unwrap();
        let malformed = dir
            .path()
            .join("index.sqlite.corrupt-0-explains-the-outage.md");
        std::fs::write(&malformed, b"x").unwrap();

        sweep_expired_quarantines_at(dir.path(), QUARANTINE_RETENTION, 1_800_000_000);
        assert!(
            malformed.exists(),
            "sweep must not delete on an unparseable suffix"
        );

        assert!(
            quarantine_reports(dir.path()).is_empty(),
            "not a name mdkb generates, so not reported either — sweep and banner agree"
        );
    }

    /// A quarantined store whose `memory_entries` reached the current schema by
    /// `ALTER TABLE`, so its physical column order is the MIGRATION order, not
    /// the CREATE order. Every long-lived store on disk has this shape.
    ///
    /// Dropping then re-adding reproduces what the v25 and v28 migrations did:
    /// `ALTER TABLE ADD COLUMN` appends, so `last_refuted_at` and
    /// `last_audited_at` sit at the end of the row instead of after
    /// `last_confirmed_at`.
    fn make_migrated_memory_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        crate::store::schema::init_schema(&conn).unwrap();
        for col in ["last_refuted_at", "last_audited_at"] {
            conn.execute(&format!("ALTER TABLE memory_entries DROP COLUMN {col}"), [])
                .unwrap();
        }
        for col in ["last_refuted_at", "last_audited_at"] {
            conn.execute(
                &format!("ALTER TABLE memory_entries ADD COLUMN {col} INTEGER"),
                [],
            )
            .unwrap();
        }
        conn
    }

    /// One memory entry with a distinct, recognisable value in every column the
    /// two orders disagree about.
    fn insert_full_entry(conn: &Connection) {
        conn.execute(
            "INSERT INTO memory_entries (
                 id, title, content, entry_type, tags, status, created_at,
                 updated_at, corrections, last_confirmed_at, source_type,
                 expires_at, due_at, created_session, created_agent,
                 projected_at, projected_hash)
             VALUES ('m0', 'Title', 'body', 'decision', '[\"a\"]', 'active', 1, 2,
                 4, 100, 'official_docs', 400, 500, 'sess-9', 'agent-9', 600,
                 'hash-9')",
            [],
        )
        .unwrap();
    }

    /// The salvaged row of [`insert_full_entry`], read back BY NAME.
    fn read_full_entry(
        conn: &Connection,
    ) -> (i64, i64, String, i64, i64, String, String, i64, String) {
        conn.query_row(
            "SELECT corrections, last_confirmed_at, source_type, expires_at,
                    due_at, created_session, created_agent, projected_at,
                    projected_hash
             FROM memory_entries WHERE id = 'm0'",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            },
        )
        .unwrap()
    }

    fn expected_full_entry() -> (i64, i64, String, i64, i64, String, String, i64, String) {
        (
            4,
            100,
            "official_docs".to_string(),
            400,
            500,
            "sess-9".to_string(),
            "agent-9".to_string(),
            600,
            "hash-9".to_string(),
        )
    }

    /// Run `f` with a tracing subscriber that captures what was logged.
    ///
    /// A dropped column has to be provable, not taken on faith: the defect this
    /// guards against logged "salvaged 113 memory entries" over a scrambling.
    fn captured_logs(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Buf {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Buf {
                self.clone()
            }
        }
        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn salvage_copies_by_column_name_not_by_position() {
        // Story 128-870a: `SELECT *` is positional, and two stores at the SAME
        // schema version have different physical column order when one got
        // there by migration. Measured 2026-09-20 on this repo: 113 of 119 live
        // entries held source_type in last_refuted_at, due_at in source_type,
        // projected_hash in created_agent, and `memory sync` died with
        // "Invalid column type Text at index: 15, name: last_refuted_at".
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("index.sqlite.corrupt-456");
        let corrupt_conn = make_migrated_memory_db(&corrupt);
        insert_full_entry(&corrupt_conn);
        drop(corrupt_conn);

        let fresh = Connection::open(dir.path().join("index.sqlite")).unwrap();
        crate::store::schema::init_schema(&fresh).unwrap();

        assert_eq!(salvage_memory(&fresh, &corrupt).entries, 1);
        assert_eq!(
            read_full_entry(&fresh),
            expected_full_entry(),
            "every field must land in the column of its own NAME"
        );
    }

    #[test]
    fn a_column_the_quarantined_store_lacks_keeps_its_default() {
        // The fresh schema is ahead of the quarantined one. Positionally this
        // is 23 values into 24 columns — SQLite rejects the whole statement and
        // the table is lost. By name, the absent column simply defaults.
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("index.sqlite.corrupt-457");
        let corrupt_conn = Connection::open(&corrupt).unwrap();
        crate::store::schema::init_schema(&corrupt_conn).unwrap();
        corrupt_conn
            .execute("ALTER TABLE memory_entries DROP COLUMN confirmations", [])
            .unwrap();
        insert_full_entry(&corrupt_conn);
        drop(corrupt_conn);

        let fresh = Connection::open(dir.path().join("index.sqlite")).unwrap();
        crate::store::schema::init_schema(&fresh).unwrap();

        assert_eq!(salvage_memory(&fresh, &corrupt).entries, 1);
        let confirmations: i64 = fresh
            .query_row(
                "SELECT confirmations FROM memory_entries WHERE id = 'm0'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(confirmations, 0, "the column's own DEFAULT, not a shift");
        assert_eq!(
            read_full_entry(&fresh),
            expected_full_entry(),
            "no later column may shift up into the gap"
        );
    }

    #[test]
    fn a_column_only_the_quarantined_store_has_is_dropped_loudly() {
        // The quarantined store is ahead of the fresh schema (a downgrade, or a
        // column removed by a migration). Its extra column cannot be kept, so
        // it must be named in the log with the rows it affects — dropping data
        // silently is how this whole defect class hides.
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("index.sqlite.corrupt-458");
        let corrupt_conn = Connection::open(&corrupt).unwrap();
        crate::store::schema::init_schema(&corrupt_conn).unwrap();
        corrupt_conn
            .execute("ALTER TABLE memory_entries ADD COLUMN legacy_note TEXT", [])
            .unwrap();
        insert_full_entry(&corrupt_conn);
        corrupt_conn
            .execute(
                "UPDATE memory_entries SET legacy_note = 'gone' WHERE id = 'm0'",
                [],
            )
            .unwrap();
        drop(corrupt_conn);

        let fresh = Connection::open(dir.path().join("index.sqlite")).unwrap();
        crate::store::schema::init_schema(&fresh).unwrap();

        let mut entries = 0;
        let logs = captured_logs(|| entries = salvage_memory(&fresh, &corrupt).entries);

        assert_eq!(entries, 1, "the row is still salvaged");
        assert_eq!(read_full_entry(&fresh), expected_full_entry());
        assert!(
            logs.contains("legacy_note"),
            "the dropped column must be named: {logs}"
        );
        assert!(
            logs.contains("memory_entries"),
            "the table must be named: {logs}"
        );
        assert!(
            logs.contains('1'),
            "the affected row count must be stated: {logs}"
        );
    }

    #[test]
    fn a_column_name_that_is_a_reserved_word_is_still_copied() {
        // The column list is no longer a constant — it is read out of the
        // quarantined database and interpolated into SQL. Bare, `group` is a
        // syntax error and the whole table would be reported LOST.
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("index.sqlite.corrupt-459");
        let corrupt_conn = Connection::open(&corrupt).unwrap();
        crate::store::schema::init_schema(&corrupt_conn).unwrap();
        corrupt_conn
            .execute("ALTER TABLE memory_entries ADD COLUMN \"group\" TEXT", [])
            .unwrap();
        insert_full_entry(&corrupt_conn);
        corrupt_conn
            .execute(
                "UPDATE memory_entries SET \"group\" = 'kept' WHERE id = 'm0'",
                [],
            )
            .unwrap();
        drop(corrupt_conn);

        let fresh = Connection::open(dir.path().join("index.sqlite")).unwrap();
        crate::store::schema::init_schema(&fresh).unwrap();
        fresh
            .execute("ALTER TABLE memory_entries ADD COLUMN \"group\" TEXT", [])
            .unwrap();

        assert_eq!(salvage_memory(&fresh, &corrupt).entries, 1);
        let group: String = fresh
            .query_row(
                "SELECT \"group\" FROM memory_entries WHERE id = 'm0'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(group, "kept");
        assert_eq!(read_full_entry(&fresh), expected_full_entry());
    }

    #[test]
    fn an_identifier_with_a_quote_in_it_cannot_end_its_own_quoting() {
        // SQLite doubles an embedded `"` inside a quoted identifier. Anything
        // else lets a column name read from a foreign file close the quote and
        // continue the statement.
        assert_eq!(quote_ident("source_type"), "\"source_type\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }
}
