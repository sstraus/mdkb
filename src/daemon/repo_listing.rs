//! One compact row per known repository (story 220-711b).
//!
//! Every column is read from the stores at call time: nothing is cached or
//! persisted, so the listing cannot disagree with the stores. Reading is
//! strictly read-only (`SQLITE_OPEN_READ_ONLY`, no migration, no repair), and a
//! store that cannot be read becomes a row with a health, never an error for
//! the whole listing.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use super::repo_map::{RootHealth, classify};
use crate::code::storage::schema as code_schema;
use crate::error::Result;
use crate::store::{index_head, namespace, schema};

/// Why a repo is, or is not, usable by a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Health {
    Healthy,
    /// A current store with no documents.
    Empty,
    /// Older than this binary: skipped by `root="*"` until `repos refresh`.
    SchemaOutdated,
    /// Written by a newer binary.
    SchemaNewer,
    Gone,
    NoStore,
    /// The store exists and could not be read; `detail` says why.
    Unreadable,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoRow {
    /// Directory name of the main checkout (a worktree names its main repo).
    pub name: String,
    pub path: PathBuf,
    /// `git` when the checkout has a `.git`, otherwise `dir`.
    pub kind: &'static str,
    pub docs: Option<i64>,
    pub memory: Option<i64>,
    pub symbols: Option<i64>,
    pub docs_indexed_at: Option<i64>,
    pub code_indexed_at: Option<i64>,
    pub head_at: Option<i64>,
    pub schema: Option<i32>,
    pub health: Health,
    /// HEAD moved past what the index describes.
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// How many rows are read at once.
const LISTING_WIDTH: usize = 8;

/// The rows for `roots`, in the order given.
///
/// Each row opens two stores, scans their tables and runs up to three `git`
/// processes, so a sequential walk over ~30 repos took 0.5 s idle and 7 s under
/// load (#210-b83b). The rows are independent, so a few threads read them at
/// once; each row lands at the index of its root. Plain threads, not rayon:
/// the global rayon pool is capped to one worker once an embedder starts, and
/// a daemon listing would then queue behind embedding batches.
pub fn list_repos(roots: &[PathBuf]) -> Vec<RepoRow> {
    let next = AtomicUsize::new(0);
    let rows: Mutex<Vec<Option<RepoRow>>> = Mutex::new(vec![None; roots.len()]);
    std::thread::scope(|scope| {
        for _ in 0..LISTING_WIDTH.min(roots.len()) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(root) = roots.get(i) else { break };
                    let row = repo_row(root);
                    rows.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(row);
                }
            });
        }
    });
    rows.into_inner()
        .unwrap_or_else(|e| e.into_inner())
        .into_iter()
        .flatten()
        .collect()
}

fn repo_row(root: &Path) -> RepoRow {
    let main = crate::git::resolve_main_worktree(root);
    let name = main.file_name().map_or_else(
        || main.display().to_string(),
        |n| n.to_string_lossy().into(),
    );
    let is_git = main.join(".git").exists();
    let mut row = RepoRow {
        name,
        path: root.to_path_buf(),
        kind: if is_git { "git" } else { "dir" },
        docs: None,
        memory: None,
        symbols: None,
        docs_indexed_at: None,
        code_indexed_at: None,
        head_at: None,
        schema: None,
        health: Health::Healthy,
        stale: false,
        detail: None,
    };
    match classify(root) {
        RootHealth::Gone => row.health = Health::Gone,
        RootHealth::NoStore => row.health = Health::NoStore,
        RootHealth::Unreadable(why) => unreadable(&mut row, why),
        RootHealth::Healthy => {
            let recorded = match read_store(root, &mut row) {
                Ok(recorded) => recorded,
                Err(e) => {
                    unreadable(&mut row, e.to_string());
                    None
                }
            };
            if is_git && row.health == Health::Healthy {
                row.head_at = crate::git::head_commit_time(root);
                row.stale = is_stale(&row, recorded.as_deref(), root);
            }
        }
    }
    row
}

fn unreadable(row: &mut RepoRow, why: String) {
    row.health = Health::Unreadable;
    row.detail = Some(why);
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(2))?;
    Ok(conn)
}

fn count(conn: &Connection, table: &str) -> Option<i64> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .ok()
}

/// Fill the store columns of `row`; returns the commit the document index
/// recorded, when the store is new enough to record one. Counts are best
/// effort: an outdated store may lack a table, and its row says so by health.
fn read_store(root: &Path, row: &mut RepoRow) -> Result<Option<String>> {
    let dir = namespace::store_dir(root)?;
    let index = dir.join("index.sqlite");
    if !index.is_file() {
        row.health = Health::NoStore;
        return Ok(None);
    }
    let conn = open_read_only(&index)?;
    let version = schema::get_schema_version(&conn)?;
    row.schema = version;
    row.docs = count(&conn, "documents");
    row.memory = count(&conn, "memory_entries");
    row.docs_indexed_at = conn
        .query_row("SELECT MAX(indexed_at) FROM documents", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .ok()
        .flatten();
    row.health = match version {
        None => {
            row.detail = Some("no schema version; not an mdkb store".to_string());
            Health::Unreadable
        }
        Some(v) if v < schema::SCHEMA_VERSION => Health::SchemaOutdated,
        Some(v) if v > schema::SCHEMA_VERSION => Health::SchemaNewer,
        Some(_) if row.docs == Some(0) => Health::Empty,
        Some(_) => Health::Healthy,
    };

    let code = dir.join("code.sqlite");
    if code.is_file()
        && let Ok(code_conn) = open_read_only(&code)
    {
        row.symbols = count(&code_conn, "code_symbols");
        row.code_indexed_at = code_schema::last_index_scan_at(&code_conn).ok().flatten();
    }

    Ok(index_head::read(&conn).ok().flatten())
}

/// The documents are stale when the recorded commit is not HEAD; a store that
/// records none (not indexed since v34) falls back to HEAD's commit time
/// against the last index time. The code index records no commit, so it always
/// uses the time.
fn is_stale(row: &RepoRow, recorded_head: Option<&str>, root: &Path) -> bool {
    let Some(head_at) = row.head_at else {
        return false;
    };
    let docs_stale = match recorded_head {
        Some(recorded) => crate::git::head_commit(root).is_some_and(|head| head != recorded),
        None => row.docs_indexed_at.is_some_and(|at| head_at > at),
    };
    docs_stale || row.code_indexed_at.is_some_and(|at| head_at > at)
}

fn date(ts: Option<i64>) -> String {
    ts.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map_or_else(|| "-".to_string(), |d| d.format("%Y-%m-%d").to_string())
}

fn num(n: Option<i64>) -> String {
    n.map_or_else(|| "-".to_string(), |n| n.to_string())
}

/// One line per row: name, health, counts, the three dates, schema, flag, path.
pub fn render_text(rows: &[RepoRow]) -> String {
    let width = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for r in rows {
        let _ = write!(
            out,
            "{:<width$}  {:<14}  docs {} mem {} sym {}  docs@{} code@{} head@{}  v{}",
            r.name,
            format!("{:?}", r.health),
            num(r.docs),
            num(r.memory),
            num(r.symbols),
            date(r.docs_indexed_at),
            date(r.code_indexed_at),
            date(r.head_at),
            r.schema.map_or_else(|| "-".to_string(), |v| v.to_string()),
        );
        if r.stale {
            out.push_str("  STALE");
        }
        let _ = write!(out, "  {}", r.path.display());
        if let Some(detail) = &r.detail {
            let _ = write!(out, "  ({detail})");
        }
        out.push('\n');
    }
    out
}
