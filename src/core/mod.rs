//! The store handle every adapter shares.
//!
//! [`Context`] owns the connection to `index.sqlite` plus the invariants that
//! must hold for anyone who touches it: the canonical-path claim, the autoheal
//! probe, the live lock that stops a concurrent quarantine renaming the files
//! underneath an open connection, and the schema initialization that follows.
//!
//! It lives here, and not under `cli::`, because the CLI is one adapter among
//! three. The daemon and the MCP layer used to reach into `cli::handlers` for
//! it, which made the command-line adapter the de-facto core of the program and
//! inverted the dependency direction of every layer above it.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};
use crate::store::{schema, vectors};

/// Context for CLI operations.
pub struct Context {
    /// Database connection.
    pub conn: Connection,
    /// Config path.
    pub config_path: PathBuf,
    /// Database path.
    pub db_path: PathBuf,
    /// True when [`Context::open`] found the index structurally corrupt,
    /// quarantined it, and built this connection on a fresh empty database.
    /// The caller should trigger a reindex to repopulate it.
    pub rebuilt_from_corruption: bool,
    /// Always false for a successfully opened context.
    ///
    /// A corrupt in-use generation is now returned as
    /// [`ErrorKind::IndexCorruptInUse`] instead of exposing a connection to the
    /// malformed database. Retained so existing callers can inspect `Context`
    /// without an unrelated API break.
    pub corrupt_in_use: bool,
    /// Shared advisory lock announcing this connection for as long as the
    /// context lives, so no other process renames the database files underneath
    /// it. Never read — its whole job is to exist until drop.
    _live_guard: Option<crate::store::mutation_lock::MutationGuard>,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("config_path", &self.config_path)
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

/// Run a mutation on a long-lived context slot, CLOSING it if the index turns
/// out to be corrupt.
///
/// A one-shot CLI process heals on its next run because it reopens; a daemon
/// does not — it holds the connection (and with it the live lock that stops
/// autoheal renaming the file) for days. Without this, a corrupt index means
/// every subsequent mutation fails the post-mutation probe, `Context::open`
/// reports `CorruptInUse` because the daemon is the very holder blocking it,
/// and the loop never ends: itview logged that failure every 30s for 13 days
/// while every memory write in that window was lost.
///
/// Dropping the context releases the connection and the live lock, so the next
/// `Context::open` quarantines the file, salvages memory out of it, and rebuilds.
/// Returns `None` when the slot was already empty (nothing to run).
pub fn run_mutation<T>(
    slot: &mut Option<Context>,
    what: &str,
    f: impl FnOnce(&mut Context) -> Result<T>,
) -> Option<Result<T>> {
    let db_path = slot.as_ref()?.db_path.clone();
    let _writer_guard = match crate::store::mutation_lock::acquire_writer(&db_path, what) {
        Ok(guard) => guard,
        Err(error) => return Some(Err(error)),
    };

    // A crash during the mutation must not leave a pre-write health marker
    // capable of suppressing recovery on the next open.
    crate::store::heal::invalidate_marker(&db_path);
    let mut result = f(slot.as_mut().expect("slot was checked above"));

    // Always verify through a fresh connection. Mutation implementations that
    // already performed the same check have touched the marker after their last
    // write, so this call is throttled to a cheap metadata check.
    if let Err(error) = crate::store::heal::verify_and_mark_throttled(&db_path) {
        result = Err(error);
    }

    if let Err(e) = &result {
        if e.is_index_corrupt() {
            tracing::error!(
                operation = what,
                error = %e,
                "index is corrupt — closing this connection so the next open can quarantine, salvage memory and rebuild"
            );
            *slot = None;
        }
    }
    Some(result)
}

/// Run a small write under universal writer admission and close a long-lived
/// context immediately when SQLite reports structural corruption.
///
/// Telemetry uses this path: a full-file `quick_check` after every hook would be
/// disproportionate, but silently swallowing `SQLITE_CORRUPT` keeps the live
/// lock held forever and prevents autoheal.
pub fn run_guarded_write<T>(
    slot: &mut Option<Context>,
    what: &str,
    f: impl FnOnce(&Context) -> Result<T>,
) -> Option<Result<T>> {
    let db_path = slot.as_ref()?.db_path.clone();
    let _writer_guard = match crate::store::mutation_lock::acquire_writer(&db_path, what) {
        Ok(guard) => guard,
        Err(error) => return Some(Err(error)),
    };
    // Even tiny writes change bytes certified by the marker. Remove it before
    // the statement so coarse filesystem timestamp granularity cannot make a
    // pre-write marker appear current on the next open.
    crate::store::heal::invalidate_marker(&db_path);
    let result = f(slot.as_ref().expect("slot was checked above"));
    if result.as_ref().is_err_and(Error::is_index_corrupt) {
        tracing::error!(
            operation = what,
            "index corruption observed during write — closing the context for automatic recovery"
        );
        *slot = None;
    }
    Some(result)
}

/// Run a read and release a corrupt long-lived context when SQLite reports the
/// damaged file. The next `Context::open` can then quarantine and rebuild it.
pub fn run_guarded_read<T>(
    slot: &mut Option<Context>,
    what: &str,
    f: impl FnOnce(&Context) -> Result<T>,
) -> Option<Result<T>> {
    let result = f(slot.as_ref()?);
    if result.as_ref().is_err_and(Error::is_index_corrupt) {
        tracing::error!(
            operation = what,
            "index corruption observed during read — closing the context for automatic recovery"
        );
        *slot = None;
    }
    Some(result)
}

impl Context {
    /// Apply the connection pragmas for the main index DB.
    ///
    /// The daemon and one-shot CLI processes open this same file as independent
    /// connections. `busy_timeout` makes ordinary write-lock contention wait
    /// briefly instead of failing immediately with `SQLITE_BUSY` (set first, so
    /// the migrations that run right after also benefit). WAL +
    /// `synchronous = NORMAL` match the code-index DB and the intent of the
    /// otherwise-unused `Store::setup_pragmas`.
    ///
    /// Deliberately no `mmap_size`: a 256 MiB persistent memory-map on the
    /// long-lived daemon connection, combined with page relocation/truncation
    /// from a concurrent connection's `VACUUM`/`incremental_vacuum`, tore
    /// `index.sqlite` pointer-map pages. `code.sqlite` never mapped and never
    /// corrupted; the marginal read speedup is not worth the data-loss risk.
    fn configure_connection(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "
            PRAGMA busy_timeout = 5000;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA temp_store = memory;
            PRAGMA foreign_keys = ON;
            ",
        )?;
        Ok(())
    }

    /// Open or create context at the given root.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_impl(root.as_ref(), false)
    }

    /// Open while the caller already holds the project writer-admission lock.
    ///
    /// The direct CLI admits the complete command before dispatch, so acquiring
    /// the same non-reentrant file lock again here would deadlock. All other
    /// callers must use [`Self::open`].
    pub fn open_writer_admitted(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_impl(root.as_ref(), true)
    }

    fn open_impl(root: &Path, writer_admitted: bool) -> Result<Self> {
        let mdkb_dir = root.join(".mdkb");

        if !mdkb_dir.exists() {
            return Err(ErrorKind::DatabaseNotFound {
                path: mdkb_dir.join("index.sqlite"),
            }
            .into());
        }

        // Every cross-process identity below is derived from `db_path` as a
        // STRING: the `.writer.lock`, `.mutation.lock` and `.live.lock`
        // sidecars, and the `-wal`/`-shm` files SQLite itself names. Two
        // spellings of one file
        // therefore give two lock domains over a single inode — no shared WAL
        // index, no mutual exclusion, and the doubly-referenced pages that
        // follow. On a case-insensitive volume (APFS default) `Gits` and `GITS`
        // are exactly such a pair. Canonicalizing here makes the identity a
        // property of the store rather than of whatever spelling the caller
        // happened to hold.
        let mdkb_dir = mdkb_dir.canonicalize().map_err(|e| {
            Error::other(format!(
                "cannot canonicalize {}: {e} — refusing to open, because locks \
                 keyed on a non-canonical path silently corrupt the index",
                mdkb_dir.display()
            ))
        })?;
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");

        // Schema initialization, integrity recovery and ordinary mutations are
        // all writers. Admit them through the same cross-process lock. The
        // direct CLI already holds it across complete command dispatch.
        let _writer_guard = if writer_admitted {
            None
        } else {
            Some(crate::store::mutation_lock::acquire_writer(
                &db_path,
                "open-schema",
            )?)
        };

        // Initialize sqlite-vec extension before opening connection
        vectors::init_sqlite_vec();

        // Opening is not read-only: every process runs migrations and creates
        // FTS/vector virtual tables if needed. Keep one project-wide lock from
        // the integrity probe through schema initialization and salvage; two
        // concurrent virtual-table constructors can otherwise fail with
        // `SQLITE_SCHEMA: vtable constructor failed`.
        let _open_guard = crate::store::mutation_lock::acquire(&db_path, "open-schema")?;

        // Refuse a second spelling of a store already coordinated under another
        // one: the locks taken above and the WAL files below are named from this
        // path, so two spellings are two lock domains over one database.
        crate::store::identity::claim(&db_path)?;

        // Establish version compatibility before autoheal can quarantine or
        // replace the file. If the version remains readable, an older binary
        // must not mutate even a structurally damaged store from the future.
        // A corruption error while inspecting the version falls through to the
        // integrity recovery path because compatibility could not be established.
        if db_path.exists() {
            let version_check = (|| -> Result<()> {
                let version_probe = Connection::open_with_flags(
                    &db_path,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                schema::refuse_future_schema(&version_probe)
            })();
            match version_check {
                Ok(()) => {}
                Err(error) if error.is_index_corrupt() => {
                    tracing::warn!(
                        "schema version could not be read from a corrupt index; continuing to autoheal: {error}"
                    );
                }
                Err(error) => return Err(error),
            }
        }

        // Autoheal: quarantine a structurally-corrupt index before we build on
        // it, so the `Connection::open` below lands on a clean file. Throttled,
        // so this is cheap on the hot open path.
        let quarantined = match crate::store::heal::ensure_sound_locked(&db_path)? {
            crate::store::heal::Heal::Sound => None,
            crate::store::heal::Heal::Quarantined { corrupt_path } => Some(corrupt_path),
            crate::store::heal::Heal::CorruptInUse => {
                // Do not turn a read command into another holder of a database
                // already known to be corrupt. `Context::open` initializes
                // schemas and ordinary reads update access statistics, so
                // continuing here both writes into the malformed generation
                // and extends the live-lock veto that prevents recovery.
                return Err(ErrorKind::IndexCorruptInUse {
                    path: db_path.clone(),
                }
                .into());
            }
        };

        // Announce this connection before opening it, so any concurrent heal
        // sees a live holder and leaves the files alone. Taken after the probe
        // above so it never vetoes our own quarantine.
        let live_guard = crate::store::mutation_lock::acquire_live_shared(&db_path)?;

        let conn = Connection::open(&db_path)?;
        Self::configure_connection(&conn)?;

        // Run schema migrations and vector table creation on every open.
        // This ensures tables added in newer versions (e.g. vec_memory)
        // exist even on databases created by older versions.
        schema::init_schema(&conn)?;
        vectors::init_vector_schema(&conn)?;
        // Stats tables (sessions/call_log/query_events) so hook-call telemetry
        // and query_events work on every transport, including the daemon-less
        // in-process CLI hook path (previously only the MCP server created them).
        crate::store::stats::init_stats_schema(&conn)?;
        crate::store::stats::init_experiments_schema(&conn)?;

        // memory_entries/memory_edges live ONLY in this DB — salvage them out of
        // the quarantined file into the fresh schema before it takes over, and
        // record the loss so it surfaces loudly (stderr now, stats/warmup later).
        let rebuilt_from_corruption = if let Some(corrupt_path) = quarantined {
            let salvage = crate::store::heal::salvage_memory(&conn, &corrupt_path);
            crate::store::heal::write_report(&corrupt_path, salvage);
            tracing::warn!(
                corrupt = %corrupt_path.display(),
                salvaged_entries = salvage.entries,
                salvaged_edges = salvage.edges,
                "index.sqlite was structurally corrupt; quarantined and rebuilt empty — docs/code re-index from source"
            );
            eprintln!(
                "mdkb: index.sqlite was CORRUPT — quarantined to {}; salvaged {} memory entries + {} edges; run `mdkb update` to re-index docs",
                corrupt_path.display(),
                salvage.entries,
                salvage.edges
            );
            true
        } else {
            false
        };

        Ok(Self {
            conn,
            config_path,
            db_path,
            rebuilt_from_corruption,
            corrupt_in_use: false,
            _live_guard: Some(live_guard),
        })
    }

    /// Open the store for READING ONLY: no migration, no schema init, no locks.
    ///
    /// [`Context::open`] is itself a write. It runs migrations, creates the FTS
    /// and vector virtual tables and initializes the stats schema — on every
    /// open, including the ones that only want to answer `mdkb search`. So every
    /// one-shot read was another writer process against the file the long-lived
    /// daemon is also writing, which is one of the two surviving hypotheses for
    /// the recurring corruption (story 018-56b2).
    ///
    /// Three deliberate differences from `open`:
    ///
    /// * `SQLITE_OPEN_READ_ONLY` — the guarantee is the database's, not a
    ///   promise from code that might forget it;
    /// * `query_only` and no `journal_mode` pragma, so the open cannot create a
    ///   `-wal`/`-shm` pair on a store that had none. Creating those files is a
    ///   write, and "a read that writes two files" is the bug, not a detail;
    /// * a schema version MISMATCH IN EITHER DIRECTION is an error naming both
    ///   versions and the remedy. Migrating here would make this path a writer
    ///   again — the very thing it exists to remove — so it refuses and says
    ///   which command will do it.
    ///
    /// No autoheal probe and no live lock either: quarantining is a write, and a
    /// reader has no business renaming files underneath the daemon.
    pub fn open_read_only(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mdkb_dir = root.join(".mdkb");
        if !mdkb_dir.exists() {
            return Err(ErrorKind::DatabaseNotFound {
                path: mdkb_dir.join("index.sqlite"),
            }
            .into());
        }
        let mdkb_dir = mdkb_dir.canonicalize().map_err(|e| {
            Error::other(format!("cannot canonicalize {}: {e}", mdkb_dir.display()))
        })?;
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");

        // sqlite-vec must still be registered: a read-only connection can query
        // the vector tables, it just cannot create them.
        vectors::init_sqlite_vec();

        let conn = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                | rusqlite::OpenFlags::SQLITE_OPEN_URI
                | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA query_only = ON;")?;

        let found: i32 = conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .map_err(|e| {
                Error::other(format!(
                    "cannot read the schema version of {}: {e}",
                    db_path.display()
                ))
            })?;
        // The two directions are not symmetric, and the old single `!=` branch
        // told a store from the FUTURE to "run mdkb update to migrate" — advice
        // that cannot work, since only a newer binary can read it at all.
        schema::refuse_future_schema(&conn)?;
        if found < schema::SCHEMA_VERSION {
            return Err(ErrorKind::SchemaStale {
                found,
                expected: schema::SCHEMA_VERSION,
            }
            .into());
        }

        Ok(Self {
            conn,
            config_path,
            db_path,
            rebuilt_from_corruption: false,
            corrupt_in_use: false,
            _live_guard: None,
        })
    }

    /// Open for reading, migrating first if the store is older than this binary.
    ///
    /// [`Self::open_read_only`] stays the primitive that refuses: a read that
    /// migrates is a writer, and making every read a writer is the bug that path
    /// exists to remove. But refusing outright blocked every read on a store no
    /// one had written since the upgrade — a whole fleet stuck behind a store
    /// nothing was going to touch on its own. So the read does not migrate
    /// itself: it hands the job to [`Self::open`], which admits it through the
    /// same project writer lock every other writer uses, and only then reads.
    ///
    /// The retry is exactly one, by construction rather than by a counter: a
    /// store still stale after a successful migration means the migration did
    /// not do what it says, and looping on that would spin forever instead of
    /// reporting the fault. A store NEWER than the binary is not stale and never
    /// reaches here — [`Self::open_read_only`] refuses it, and migrating could
    /// not fix it anyway.
    pub fn open_read_only_migrating(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        match Self::open_read_only(root) {
            Err(e) if matches!(e.kind(), ErrorKind::SchemaStale { .. }) => {
                tracing::info!("{e}; migrating under the writer lock, then retrying the read");
                // Dropped immediately: the migration is the point, the write
                // connection is not. The read below reopens read-only.
                drop(Self::open(root)?);
                Self::open_read_only(root)
            }
            other => other,
        }
    }

    /// Initialize a new mdkb directory.
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mdkb_dir = root.join(".mdkb");

        // Create directory if needed
        if !mdkb_dir.exists() {
            std::fs::create_dir_all(&mdkb_dir)?;
        }

        // Auto-init can be entered concurrently by several hook/MCP processes.
        // Derive every sidecar from the canonical store identity and serialize
        // config + virtual-table creation exactly like `open` does.
        let mdkb_dir = mdkb_dir.canonicalize().map_err(|e| {
            Error::other(format!(
                "cannot canonicalize {} during initialization: {e}",
                mdkb_dir.display()
            ))
        })?;
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");
        let _writer_guard = crate::store::mutation_lock::acquire_writer(&db_path, "init-schema")?;
        let _init_guard = crate::store::mutation_lock::acquire(&db_path, "init-schema")?;

        // Create memory directories
        let memory_dir = mdkb_dir.join("memory");
        std::fs::create_dir_all(memory_dir.join("entries"))?;
        std::fs::create_dir_all(memory_dir.join("archive"))?;
        // Split the store into the derived part git must never see and the
        // durable entry projection it should track.
        ensure_store_gitignore(&memory_dir)?;

        // Create default config
        let config = Config::default();
        let config_str = toml::to_string_pretty(&config)?;
        std::fs::write(&config_path, config_str)?;

        // Initialize sqlite-vec extension
        vectors::init_sqlite_vec();

        // Create and initialize database
        let live_guard = crate::store::mutation_lock::acquire_live_shared(&db_path)?;
        let conn = Connection::open(&db_path)?;
        Self::configure_connection(&conn)?;
        schema::init_schema(&conn)?;
        vectors::init_vector_schema(&conn)?;
        crate::store::stats::init_stats_schema(&conn)?;
        crate::store::stats::init_experiments_schema(&conn)?;

        Ok(Self {
            conn,
            config_path,
            db_path,
            rebuilt_from_corruption: false,
            corrupt_in_use: false,
            _live_guard: Some(live_guard),
        })
    }

    /// Get the project root directory (parent of `.mdkb/`).
    pub fn root(&self) -> &Path {
        self.db_path
            .parent()
            .and_then(|p| p.parent())
            .expect("db_path must be inside .mdkb/")
    }

    /// Get the memory directory path.
    pub fn memory_dir(&self) -> PathBuf {
        self.db_path.parent().unwrap().join("memory")
    }
}

/// An allow-list, not a deny-list: everything under the store is derived,
/// per-machine or merge-hostile — sqlite indexes and their `-wal`/`-shm`/lock/
/// integrity sidecars, `vectors.bin`, hook telemetry, backups, quarantined
/// corrupt databases, the regenerated memory warm-up index, the per-machine
/// archive — and an enumeration would rot the next time a sidecar is added.
/// Only the durable entry projection is tracked.
///
/// Every line is load-bearing. `!memory/` must precede `memory/*`: git does not
/// descend into an excluded directory, so the directory has to be re-included
/// before its contents are re-excluded. Omitting it fails silently.
const STORE_GITIGNORE: &str = "\
# Managed by `mdkb init`. Everything in the store is derived, per-machine or
# merge-hostile; only the durable memory entry projection is tracked.
*
!.gitignore
!memory/
memory/*
!memory/entries/
memory/entries/*
!memory/entries/*.md
";

/// Write `.mdkb/.gitignore` if absent. Never overwrites: the file is the user's
/// once it exists, and an existing store must be able to opt out.
pub(crate) fn ensure_store_gitignore(memory_dir: &Path) -> Result<()> {
    let Some(store_dir) = memory_dir.parent() else {
        return Ok(());
    };
    let path = store_dir.join(".gitignore");
    if !path.exists() {
        std::fs::write(path, STORE_GITIGNORE)?;
    }
    Ok(())
}

pub mod cli_mutation;
pub mod code;
pub mod graph;
pub mod indexing;
pub mod memory;
pub mod memory_sync;
pub mod ops;
pub mod routing;
pub mod search;
pub mod sessions;
pub mod surface;
