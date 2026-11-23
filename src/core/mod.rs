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
use crate::store::{namespace, schema, vectors};

/// Context for CLI operations.
pub struct Context {
    /// Database connection.
    pub conn: Connection,
    /// Project root, the parent of `.mdkb/`. Held rather than derived from
    /// `db_path`, because a namespaced store sits two levels deeper.
    root: PathBuf,
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
    /// The schema version this store was on before
    /// [`Context::open_read_only_migrating`] migrated it, when it did.
    ///
    /// `None` on every other path, including a store already current. The
    /// migration is deliberate — a read command on a stale store is more
    /// useful than a refusal — but it is not a schema bump alone: v21 deletes
    /// memory entries with unreadable ids, v23 and v26 archive prior clusters,
    /// v27 moves prior candidates. A caller that does not say it happened is
    /// mutating data behind the user, which is what a reporting command was
    /// doing until this field existed.
    pub migrated_from: Option<i32>,
    /// Shared advisory lock announcing this connection for as long as the
    /// context lives, so no other process renames the database files underneath
    /// it. Never read — its whole job is to exist until drop.
    _live_guard: Option<crate::store::mutation_lock::MutationGuard>,
    /// Which opening of the store this is, unique per process. A slot that
    /// released its lock for a probe uses it to tell whether the context it
    /// probed is still the one in the slot.
    pub(crate) generation: u64,
}

fn next_generation() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
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
///
/// The full-file integrity probe that certifies the write runs only after the
/// slot and the writer admission are released: a scan of a store of hundreds of
/// megabytes under the slot queues every request for that repo behind it (the
/// hook's context phase, #201-481e). Blocking: call from `spawn_blocking`.
pub fn run_mutation<T>(
    slot: &tokio::sync::Mutex<Option<Context>>,
    what: &str,
    f: impl FnOnce(&mut Context) -> Result<T>,
) -> Option<Result<T>> {
    run_mutation_verify_after_release(
        slot,
        what,
        f,
        crate::store::heal::verify_and_mark_unadmitted,
    )
}

/// A slot hold at or above this is logged at WARN; shorter holds go to DEBUG.
/// Routine warm opens and hook reads hold the slot for 100 ms to a second, and
/// at the old 100 ms threshold they wrote 27k warnings in 15 days (#064-3c58).
/// One second is the point where a hook read that cannot bypass the slot waits
/// long enough to matter (#209-bc4b). Documented in `docs/hooks.md`.
pub(crate) const SLOT_HOLD_WARN: std::time::Duration = std::time::Duration::from_millis(1000);

/// Whether a hold of this length is worth a WARN line.
pub(crate) fn slot_hold_is_warning(held: std::time::Duration) -> bool {
    held >= SLOT_HOLD_WARN
}

/// Measures how long a mutation holds the store slot and logs it on drop, so
/// the holders and their worst hold times come from the live daemon's log
/// (`slot held`) rather than from reading the code.
pub(crate) struct SlotHold {
    what: String,
    repo: String,
    since: std::time::Instant,
}

impl SlotHold {
    /// Start timing; create it right after the slot is acquired and let it drop
    /// when the slot is released.
    pub(crate) fn start(what: &str) -> Self {
        Self {
            what: what.to_string(),
            repo: String::new(),
            since: std::time::Instant::now(),
        }
    }

    /// Name the repo whose slot this is, once the context is at hand: the log
    /// line is how the worst hold of each repo is found.
    pub(crate) fn in_repo(&mut self, root: &Path) {
        self.repo = root.display().to_string();
    }
}

impl Drop for SlotHold {
    fn drop(&mut self) {
        let held = self.since.elapsed();
        let held_ms = held.as_millis() as u64;
        if slot_hold_is_warning(held) {
            tracing::warn!(operation = %self.what, repo = %self.repo, held_ms, "slot held");
        } else {
            tracing::debug!(operation = %self.what, repo = %self.repo, held_ms, "slot held");
        }
    }
}

/// [`run_mutation`] with the post-write probe injected.
pub fn run_mutation_verify_after_release<T>(
    slot: &tokio::sync::Mutex<Option<Context>>,
    what: &str,
    f: impl FnOnce(&mut Context) -> Result<T>,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> Option<Result<T>> {
    let (db_path, generation, mut result) = loop {
        let (db_path, generation) = {
            let guard = slot.blocking_lock();
            let ctx = guard.as_ref()?;
            (ctx.db_path.clone(), ctx.generation)
        };
        let writer = match crate::store::mutation_lock::acquire_writer(&db_path, what) {
            Ok(guard) => guard,
            Err(error) => return Some(Err(error)),
        };
        let mut guard = slot.blocking_lock();
        let ctx = guard.as_mut()?;
        if ctx.db_path != db_path || ctx.generation != generation {
            drop(guard);
            drop(writer);
            continue;
        }
        let mut hold = SlotHold::start(what);
        hold.in_repo(&ctx.root);
        crate::store::heal::invalidate_marker(&db_path);
        let result = f(ctx);
        break (db_path, generation, result);
    };
    if let Err(error) = verify(&db_path) {
        result = Err(error);
    }
    if let Err(e) = &result {
        if e.is_index_corrupt() {
            tracing::error!(
                operation = what,
                error = %e,
                "index is corrupt — closing this connection so the next open can quarantine, salvage memory and rebuild"
            );
            // The slot was released for the probe, so it may hold a newer
            // context now (reopened over a rebuilt file). The verdict is about
            // the file this one opened; it must not tear down its successor.
            close_over_corruption_of(&mut slot.blocking_lock(), generation);
        }
    }
    Some(result)
}

/// [`close_over_corruption`] for a verdict about one opening of the store
/// (`generation`): a slot that holds a newer context is left alone.
pub(crate) fn close_over_corruption_of(slot: &mut Option<Context>, generation: u64) {
    if slot
        .as_ref()
        .is_some_and(|ctx| ctx.generation == generation)
    {
        close_over_corruption(slot);
    }
}

/// Drop a long-lived context that saw corruption, and with it the process's
/// memory of having probed the file sound, so the next open probes.
pub(crate) fn close_over_corruption(slot: &mut Option<Context>) {
    if let Some(ctx) = slot.take() {
        crate::store::heal::forget_process_probe(&ctx.db_path);
    }
}

/// Run one small write under an existing writer admission for this store.
///
/// Queued telemetry shares admission across a batch, but each row still
/// invalidates its health marker and closes the slot on typed corruption.
pub(crate) fn run_admitted_write<T>(
    slot: &mut Option<Context>,
    what: &str,
    _admission: &crate::store::mutation_lock::MutationGuard,
    f: impl FnOnce(&Context) -> Result<T>,
) -> Option<Result<T>> {
    let db_path = slot.as_ref()?.db_path.clone();
    // Even tiny writes change bytes certified by the marker. Remove it before
    // the statement so coarse filesystem timestamp granularity cannot make a
    // pre-write marker appear current on the next open.
    let result = crate::store::heal::keep_process_probe_across(&db_path, || {
        f(slot.as_ref().expect("slot was checked above"))
    });
    if result.as_ref().is_err_and(Error::is_index_corrupt) {
        tracing::error!(
            operation = what,
            "index corruption observed during write — closing the context for automatic recovery"
        );
        close_over_corruption(slot);
    }
    Some(result)
}

/// [`run_guarded_read`] inside one read snapshot, so its semantic reads share one
/// embedding identity probe ([`crate::store::vectors::with_read_snapshot`]).
pub fn run_guarded_snapshot_read<T>(
    slot: &mut Option<Context>,
    what: &str,
    f: impl FnOnce(&Context) -> Result<T>,
) -> Option<Result<T>> {
    run_guarded_read(slot, what, |ctx| {
        crate::store::vectors::with_read_snapshot(&ctx.conn, || f(ctx))
    })
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
        close_over_corruption(slot);
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
        Self::open_impl(root.as_ref(), false, false)
    }

    /// [`Self::open`] for a long-lived holder that reopens a store it already
    /// served: skips the full-file integrity scan when this process probed the
    /// file sound within [`crate::store::heal::CHECK_INTERVAL`]. Such a holder
    /// must call [`crate::store::heal::forget_process_probe`] when it closes a
    /// context over corruption; the `run_guarded_*` helpers do.
    pub fn open_reusing_process_probe(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_impl(root.as_ref(), false, true)
    }

    /// Open while the caller already holds the project writer-admission lock.
    ///
    /// The direct CLI admits the complete command before dispatch, so acquiring
    /// the same non-reentrant file lock again here would deadlock. All other
    /// callers must use [`Self::open`].
    pub fn open_writer_admitted(root: impl AsRef<Path>) -> Result<Self> {
        Self::open_impl(root.as_ref(), true, false)
    }

    /// Open a background slot under admission, reusing its process health probe.
    pub(crate) fn open_writer_admitted_reusing_process_probe(root: &Path) -> Result<Self> {
        Self::open_impl(root, true, true)
    }

    fn open_impl(root: &Path, writer_admitted: bool, trust_process_probe: bool) -> Result<Self> {
        let mdkb_dir = namespace::store_dir(root)?;
        // Admission creates the sidecar directory before a cold open. Keep
        // that preparation from bypassing the ordinary first-store layout.
        if writer_admitted
            && !mdkb_dir.join("index.sqlite").exists()
            && !mdkb_dir.join("config.toml").exists()
        {
            return Self::init_writer_admitted(root);
        }

        if namespace::active()?.is_some() {
            // A namespace is created by its first write: nobody runs `init`
            // for a scratch store. Idempotent, so an existing one is untouched.
            ensure_store_layout(&mdkb_dir)?;
        } else if !mdkb_dir.exists() {
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
        let mdkb_dir = crate::domain::canonicalize_plain(&mdkb_dir).map_err(|e| {
            Error::other(format!(
                "cannot canonicalize {}: {e} — refusing to open, because locks \
                 keyed on a non-canonical path silently corrupt the index",
                mdkb_dir.display()
            ))
        })?;
        let config_path = mdkb_dir.join("config.toml");
        Config::load_for_repo(&config_path)?;
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
        let quarantined =
            match crate::store::heal::ensure_sound_locked(&db_path, trust_process_probe)? {
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
        //
        // Read BEFORE the migration, because afterwards there is nothing left
        // to tell you it happened. The write path migrates exactly like the
        // read one and it is the path an operator reaches for deliberately on
        // an old store, so it owes the same answer.
        let version_before = schema::get_schema_version(&conn).unwrap_or(None);
        schema::init_schema(&conn)?;
        let migrated_from = version_before.filter(|v| *v < schema::SCHEMA_VERSION);
        vectors::init_vector_schema(&conn)?;
        // Stats tables (sessions/call_log/query_events) so hook-call telemetry
        // and query_events work on every transport, including the daemon-less
        // in-process CLI hook path (previously only the MCP server created them).
        crate::store::stats::init_stats_schema(&conn)?;

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

        // Retire quarantined copies that have outlived their retention. On the
        // open path rather than on a timer: this is the only moment mdkb is
        // guaranteed to run against the store, and the sweep is one directory
        // listing. It runs after the salvage above, so a copy can never be
        // removed before the entries have been read out of it.
        crate::store::heal::sweep_expired_quarantines(&mdkb_dir);

        Ok(Self {
            conn,
            root: root.to_path_buf(),
            config_path,
            db_path,
            rebuilt_from_corruption,
            corrupt_in_use: false,
            migrated_from,
            _live_guard: Some(live_guard),
            generation: next_generation(),
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
        let mdkb_dir = namespace::store_dir(root)?;
        if !mdkb_dir.exists() {
            return Err(ErrorKind::DatabaseNotFound {
                path: mdkb_dir.join("index.sqlite"),
            }
            .into());
        }
        let mdkb_dir = crate::domain::canonicalize_plain(&mdkb_dir).map_err(|e| {
            Error::other(format!("cannot canonicalize {}: {e}", mdkb_dir.display()))
        })?;
        let config_path = mdkb_dir.join("config.toml");
        Config::load_for_repo(&config_path)?;
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
            root: root.to_path_buf(),
            config_path,
            db_path,
            rebuilt_from_corruption: false,
            corrupt_in_use: false,
            migrated_from: None,
            _live_guard: None,
            generation: next_generation(),
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
                let found = match e.kind() {
                    ErrorKind::SchemaStale { found, .. } => Some(*found),
                    _ => None,
                };
                tracing::info!("{e}; migrating under the writer lock, then retrying the read");
                // Dropped immediately: the migration is the point, the write
                // connection is not. The read below reopens read-only.
                drop(Self::open(root)?);
                // Recorded, not merely logged: `tracing::info!` is invisible at
                // the default level, and the caller is the only one who can
                // tell the user that a command they read as read-only just
                // rewrote their store.
                Self::open_read_only(root).map(|mut ctx| {
                    ctx.migrated_from = found;
                    ctx
                })
            }
            other => other,
        }
    }

    /// Initialize a new mdkb directory.
    pub fn init(root: impl AsRef<Path>) -> Result<Self> {
        Self::init_impl(root.as_ref(), false)
    }

    pub(crate) fn init_writer_admitted(root: &Path) -> Result<Self> {
        Self::init_impl(root, true)
    }

    fn init_impl(root: &Path, writer_admitted: bool) -> Result<Self> {
        let mdkb_dir = namespace::store_dir(root)?;
        ensure_store_layout(&mdkb_dir)?;

        // Auto-init can be entered concurrently by several hook/MCP processes.
        // Derive every sidecar from the canonical store identity and serialize
        // config + virtual-table creation exactly like `open` does.
        let mdkb_dir = crate::domain::canonicalize_plain(&mdkb_dir).map_err(|e| {
            Error::other(format!(
                "cannot canonicalize {} during initialization: {e}",
                mdkb_dir.display()
            ))
        })?;
        let config_path = mdkb_dir.join("config.toml");
        let db_path = mdkb_dir.join("index.sqlite");
        let _writer_guard = if writer_admitted {
            None
        } else {
            Some(crate::store::mutation_lock::acquire_writer(
                &db_path,
                "init-schema",
            )?)
        };
        let _init_guard = crate::store::mutation_lock::acquire(&db_path, "init-schema")?;

        // Write the defaults commented out, so they stay discoverable without
        // being frozen into this store. See `Config::commented_default_toml`.
        std::fs::write(&config_path, Config::commented_default_toml()?)?;

        // Initialize sqlite-vec extension
        vectors::init_sqlite_vec();

        // Create and initialize database
        let live_guard = crate::store::mutation_lock::acquire_live_shared(&db_path)?;
        let conn = Connection::open(&db_path)?;
        Self::configure_connection(&conn)?;
        schema::init_schema(&conn)?;
        vectors::init_vector_schema(&conn)?;
        crate::store::stats::init_stats_schema(&conn)?;

        Ok(Self {
            conn,
            root: root.to_path_buf(),
            config_path,
            db_path,
            rebuilt_from_corruption: false,
            corrupt_in_use: false,
            migrated_from: None,
            _live_guard: Some(live_guard),
            generation: next_generation(),
        })
    }

    /// Get the project root directory (parent of `.mdkb/`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Get the memory directory path.
    pub fn memory_dir(&self) -> PathBuf {
        self.db_path.parent().unwrap().join("memory")
    }
}

/// Create the directories a store needs before anything is written to it: the
/// store itself, the memory projection and its archive, and the store-level
/// `.gitignore` that splits the derived part git must never see from the
/// durable entry projection it should track. Idempotent.
fn ensure_store_layout(mdkb_dir: &Path) -> Result<()> {
    let memory_dir = mdkb_dir.join("memory");
    std::fs::create_dir_all(memory_dir.join("entries"))?;
    std::fs::create_dir_all(memory_dir.join("archive"))?;
    ensure_store_gitignore(&memory_dir)
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
pub mod coupling;
pub mod coupling_docs;
pub mod dup;
pub mod graph;
pub mod indexing;
pub mod memory;
pub mod memory_audit;
mod memory_audit_symbols;
pub mod memory_sync;
pub mod ops;
pub mod refresh;
pub mod routing;
pub mod search;
pub mod sessions;
pub mod surface;

#[cfg(test)]
mod close_over_corruption_tests {
    use super::*;
    use crate::store::heal;

    fn corrupt_error(ctx: &Context) -> Error {
        ErrorKind::IndexCorrupt {
            path: ctx.db_path.clone(),
        }
        .into()
    }

    /// A slot holding a context whose store this process has probed sound.
    fn probed_slot(dir: &Path) -> Context {
        let ctx = Context::init(dir).unwrap();
        let _guard = crate::store::mutation_lock::acquire(&ctx.db_path, "test").unwrap();
        heal::ensure_sound_locked(&ctx.db_path, false).unwrap();
        assert!(heal::has_process_probe(&ctx.db_path));
        ctx
    }

    /// Catches: a corruption-close path that drops the context but keeps the
    /// "probed sound" record, so the reopen that must quarantine is skipped.
    #[test]
    fn every_corruption_close_forgets_the_process_probe() {
        let dir = tempfile::tempdir().unwrap();
        let mut slot = Some(probed_slot(dir.path()));
        let db_path = slot.as_ref().unwrap().db_path.clone();
        run_guarded_read(&mut slot, "test read", |ctx| -> Result<()> {
            Err(corrupt_error(ctx))
        });
        assert!(slot.is_none() && !heal::has_process_probe(&db_path));

        let dir = tempfile::tempdir().unwrap();
        let mut slot = Some(probed_slot(dir.path()));
        let db_path = slot.as_ref().unwrap().db_path.clone();
        let admission = crate::store::mutation_lock::acquire_writer(&db_path, "test").unwrap();
        run_admitted_write(&mut slot, "test write", &admission, |ctx| -> Result<()> {
            Err(corrupt_error(ctx))
        });
        assert!(slot.is_none() && !heal::has_process_probe(&db_path));

        let dir = tempfile::tempdir().unwrap();
        let slot = tokio::sync::Mutex::new(Some(probed_slot(dir.path())));
        let db_path = slot.try_lock().unwrap().as_ref().unwrap().db_path.clone();
        run_mutation(&slot, "test mutation", |ctx| -> Result<()> {
            Err(corrupt_error(ctx))
        });
        assert!(slot.try_lock().unwrap().is_none() && !heal::has_process_probe(&db_path));
    }

    /// Catches: the telemetry write that runs on every hook voiding the record
    /// (invalidate_marker forgets), which would leave the daemon's reopen
    /// re-scanning after all; and a FAILED write keeping it.
    #[test]
    fn a_successful_admitted_write_keeps_the_probe_and_a_failed_one_drops_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut slot = Some(probed_slot(dir.path()));
        let db_path = slot.as_ref().unwrap().db_path.clone();
        let admission = crate::store::mutation_lock::acquire_writer(&db_path, "test").unwrap();
        run_admitted_write(&mut slot, "ok write", &admission, |_| -> Result<()> {
            Ok(())
        });
        assert!(heal::has_process_probe(&db_path));
        run_admitted_write(&mut slot, "failed write", &admission, |_| -> Result<()> {
            Err(Error::other("disk full"))
        });
        assert!(slot.is_some() && !heal::has_process_probe(&db_path));
    }

    /// Catches (critic r6): `run_admitted_write` re-acquiring the non-reentrant
    /// writer lock (self-deadlock under a held admission), skipping the marker
    /// invalidation for every row after the first of a shared admission, or
    /// keeping the process probe across a corrupt row.
    #[test]
    fn admitted_rows_each_invalidate_the_marker_under_one_admission() {
        let dir = tempfile::tempdir().unwrap();
        let mut slot = Some(probed_slot(dir.path()));
        let db_path = slot.as_ref().unwrap().db_path.clone();
        let mut marker = db_path.as_os_str().to_os_string();
        marker.push(".integrity-ok");
        let marker = std::path::PathBuf::from(marker);
        let admission = crate::store::mutation_lock::acquire_writer(&db_path, "r6").unwrap();
        for row in 0..3 {
            std::fs::write(&marker, b"").unwrap();
            let outcome = run_admitted_write(&mut slot, "r6 row", &admission, |_| -> Result<()> {
                assert!(!marker.exists(), "row {row} ran with a stale health marker");
                Ok(())
            });
            assert!(matches!(outcome, Some(Ok(()))));
            assert!(
                heal::has_process_probe(&db_path),
                "row {row} lost the probe"
            );
        }
        let outcome =
            run_admitted_write(&mut slot, "r6 corrupt", &admission, |ctx| -> Result<()> {
                Err(corrupt_error(ctx))
            });
        assert!(matches!(outcome, Some(Err(ref e)) if e.is_index_corrupt()));
        assert!(slot.is_none() && !heal::has_process_probe(&db_path));
        // A row after the closure finds an empty slot and does not run.
        let later = run_admitted_write(&mut slot, "r6 later", &admission, |_| -> Result<()> {
            panic!("ran against a closed slot")
        });
        assert!(later.is_none());
    }
}

#[cfg(test)]
mod verify_after_release_tests {
    use super::*;

    fn slot_in(dir: &Path) -> tokio::sync::Mutex<Option<Context>> {
        tokio::sync::Mutex::new(Some(Context::init(dir).unwrap()))
    }

    /// Catches: the integrity probe running while the daemon's store mutex is
    /// held, so every hook that arrives during a watcher reindex waits out a
    /// full-file `quick_check` in the context phase (#201-481e, rows of 377 to
    /// 916 ms on a 733 MB store).
    #[test]
    fn the_probe_runs_after_the_store_slot_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let slot = slot_in(dir.path());
        let mut slot_free_during_probe = false;
        let outcome = run_mutation_verify_after_release(
            &slot,
            "test",
            |_| -> Result<()> { Ok(()) },
            |_| {
                slot_free_during_probe = slot.try_lock().is_ok();
                Ok(())
            },
        );
        assert!(matches!(outcome, Some(Ok(()))));
        assert!(slot_free_during_probe, "the probe held the store slot");
    }

    /// Catches: a probe that finds corruption after the slot moved out of the
    /// lock leaving the long-lived connection (and its live lock) open, which
    /// is what blocks the quarantine.
    #[test]
    fn corruption_found_by_the_probe_closes_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let slot = slot_in(dir.path());
        let db_path = slot.try_lock().unwrap().as_ref().unwrap().db_path.clone();
        let outcome = run_mutation_verify_after_release(
            &slot,
            "test",
            |_| -> Result<()> { Ok(()) },
            |path| {
                // The probe passes and records its verdict; the corruption
                // arrives after, so closing must also forget that verdict.
                crate::store::heal::verify_and_mark_unadmitted(path)?;
                assert!(crate::store::heal::has_process_probe(path));
                Err(ErrorKind::IndexCorrupt {
                    path: path.to_path_buf(),
                }
                .into())
            },
        );
        assert!(matches!(outcome, Some(Err(ref e)) if e.is_index_corrupt()));
        assert!(slot.try_lock().unwrap().is_none());
        assert!(!crate::store::heal::has_process_probe(&db_path));
    }

    // After admission, a same-path replacement must force retry before mutation.
    // A corrupt verdict for the replacement actually mutated must close it.
    #[cfg(target_os = "linux")]
    #[test]
    fn replacement_during_writer_admission_closes_the_generation_actually_mutated() {
        use std::os::unix::fs::MetadataExt;
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let first = Context::init(dir.path()).unwrap();
        let db_path = first.db_path.clone();
        let replacement = Context::open(dir.path()).unwrap();
        assert_eq!(replacement.db_path, db_path);
        assert_ne!(replacement.generation, first.generation);
        let replacement_generation = replacement.generation;
        let slot = Arc::new(tokio::sync::Mutex::new(Some(first)));
        let writer = crate::store::mutation_lock::acquire_writer(&db_path, "test blocker").unwrap();
        let metadata =
            std::fs::metadata(crate::store::mutation_lock::writer_lock_path(&db_path)).unwrap();
        let identity = (
            libc::major(metadata.dev() as libc::dev_t),
            libc::minor(metadata.dev() as libc::dev_t),
            metadata.ino(),
        );
        let (result_tx, result_rx) = mpsc::channel();
        let worker_slot = Arc::clone(&slot);
        let worker_db = db_path.clone();
        let worker = std::thread::spawn(move || {
            let mut mutated_generation = None;
            let mut verified = false;
            let outcome = run_mutation_verify_after_release(
                &worker_slot,
                "generation replacement",
                |ctx| {
                    mutated_generation = Some(ctx.generation);
                    ctx.conn.execute_batch(
                        "CREATE TABLE admission_generation_effect(value INTEGER);
                         INSERT INTO admission_generation_effect VALUES(1);",
                    )?;
                    Ok(())
                },
                |path| {
                    assert_eq!(path, worker_db.as_path());
                    verified = true;
                    // Typed probe input, not a claim of physical file corruption.
                    Err(ErrorKind::IndexCorrupt {
                        path: path.to_path_buf(),
                    }
                    .into())
                },
            );
            let _ = result_tx.send((outcome, mutated_generation, verified));
        });
        // Real blocked FLOCK record measured on Linux run20261115T230040Z-65760.
        // /proc/locks reports process PID, not TID. This fixture's private inode
        // identifies its only waiter, which already snapshotted the first ctx.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let records = std::fs::read_to_string("/proc/locks").expect("read kernel lock records");
            let blocked = records.lines().any(|record| {
                let fields: Vec<_> = record.split_whitespace().collect();
                let [_, "->", "FLOCK", "ADVISORY", "WRITE", pid, key, ..] = fields.as_slice()
                else {
                    return false;
                };
                let mut parts = key.split(':');
                let major = parts
                    .next()
                    .and_then(|value| u32::from_str_radix(value, 16).ok());
                let minor = parts
                    .next()
                    .and_then(|value| u32::from_str_radix(value, 16).ok());
                let inode = parts.next().and_then(|value| value.parse::<u64>().ok());
                pid.parse::<u32>().ok() == Some(std::process::id())
                    && major == Some(identity.0)
                    && minor == Some(identity.1)
                    && inode == Some(identity.2)
                    && parts.next().is_none()
            });
            if blocked {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "mutator never blocked on the fixture's writer-lock inode {identity:?}"
            );
            std::thread::yield_now();
        }
        let previous = slot
            .try_lock()
            .expect("writer admission waiter must release the serving slot before waiting")
            .replace(replacement)
            .unwrap();
        drop(previous);
        drop(writer);
        // Inverted revalidation can loop forever: fail here, not at the runner's
        // whole-process timeout. No join is attempted until a result arrives.
        let (outcome, mutated_generation, verified) = result_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("mutation must finish after writer release and generation revalidation");
        worker.join().unwrap();
        assert_eq!(mutated_generation, Some(replacement_generation));
        assert!(verified);
        assert!(matches!(outcome, Some(Err(ref e)) if e.is_index_corrupt()));
        let conn =
            Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let writes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM admission_generation_effect",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(writes, 1);
        assert!(
            slot.try_lock().unwrap().is_none(),
            "corruption verdict left the generation actually mutated in the slot"
        );
    }

    /// Catches: an empty slot (nothing opened yet) reported as a finished
    /// mutation, or probed.
    #[test]
    fn an_empty_slot_runs_nothing() {
        let slot = tokio::sync::Mutex::new(None);
        let outcome = run_mutation_verify_after_release(
            &slot,
            "test",
            |_| -> Result<()> { panic!("no context to mutate") },
            |_| panic!("nothing to verify"),
        );
        assert!(outcome.is_none());
    }
}

pub mod search_contract;
pub mod source_search;
pub mod text_search;

#[cfg(test)]
mod slot_hold_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn routine_slot_holds_stay_below_warn_and_only_long_ones_warn() {
        // Catches: every warm open and hook read (100 ms and up) logged at WARN,
        // which filled daemon.log with 27k lines in 15 days (#064-3c58).
        assert!(!slot_hold_is_warning(Duration::from_millis(223)));
        assert!(!slot_hold_is_warning(
            SLOT_HOLD_WARN - Duration::from_millis(1)
        ));
        assert!(slot_hold_is_warning(SLOT_HOLD_WARN));
        assert!(slot_hold_is_warning(Duration::from_secs(9)));
    }
}
