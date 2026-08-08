//! Reconciling the database with the markdown entry projection.
//!
//! One responsibility: keeping `memory_entries` and `.mdkb/memory/entries/*.md`
//! agreeing with each other, in both directions. Nothing here parses an argument
//! or formats output, which is why it does not belong in the command-line
//! adapter — the MCP layer and the daemon watcher both drive this same
//! reconciliation, and they were reaching through `cli::handlers` to get at it.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use crate::core::Context;
use crate::error::Result;
use crate::store::memory::{self, EntryStatus, MemoryEntry};

/// Memory index.json structure.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryIndex {
    pub updated: String,
    pub entries: Vec<String>,
}

/// Generate and save memory index.json file.
pub fn generate_memory_index(ctx: &Context) -> Result<()> {
    let index = memory::get_warmup_index(&ctx.conn, 50)?;
    let now = chrono::Utc::now().to_rfc3339();

    let memory_index = MemoryIndex {
        updated: now,
        entries: index,
    };

    let index_path = ctx.memory_dir().join("index.json");
    let json = serde_json::to_string_pretty(&memory_index)?;
    std::fs::write(index_path, json)?;

    Ok(())
}

/// Load memory index from index.json (fast warmup).
pub fn load_memory_index(ctx: &Context) -> Result<Option<MemoryIndex>> {
    let index_path = ctx.memory_dir().join("index.json");
    if !index_path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(index_path)?;
    let index: MemoryIndex = serde_json::from_str(&content)?;
    Ok(Some(index))
}

/// Save a memory entry to disk as markdown, returning the hash of the bytes
/// written.
///
/// Delegates to the shared YAML frontmatter serializer so export and per-add
/// backups stay byte-identical — otherwise a re-import would lose the fields
/// the hand-rolled version omitted (`source_type`, `created_at`, timestamps,
/// reminder `due_at`, etc.).
fn save_entry_to_disk(ctx: &Context, entry: &MemoryEntry) -> Result<String> {
    let entry_path = ctx
        .memory_dir()
        .join("entries")
        .join(format!("{}.md", entry.id));
    let content = crate::store::memory_file::to_markdown(entry);
    std::fs::write(entry_path, &content)?;
    Ok(crate::code::indexing::hasher::content_hash(&content))
}

/// Write an entry's markdown file **and** record the projection in one step.
///
/// The two must never drift: a `projected_at` without the hash of the bytes it
/// describes cannot answer "did the file change since?", and the next
/// reconciliation would have to guess. Every DB → file write goes through here.
pub(crate) fn project_entry(ctx: &Context, entry: &MemoryEntry, now: i64) -> Result<()> {
    let hash = save_entry_to_disk(ctx, entry)?;
    memory::set_projection(&ctx.conn, &entry.id, now, &hash)
}

/// Outcome of [`sync_memory_files`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MemorySyncSummary {
    /// Entries written DB → file (backfilled, or re-projected after a DB edit).
    pub projected: usize,
    /// Files with no DB row at all, inserted file → DB. A colleague's entry
    /// arriving in a checkout lands here.
    pub imported: usize,
    /// Existing entries updated file → DB because only the file changed.
    pub adopted: usize,
    /// Entries where both sides changed and the newer `updated_at` won; the
    /// loser is preserved in `memory_revisions`.
    pub conflicts: usize,
    /// Archived entries restored to active because their file reappeared.
    pub revived: usize,
    /// Entries whose (previously-projected) file was deleted and were
    /// therefore archived (file deletion → DB archive).
    pub archived: usize,
    /// Archival candidates NOT archived because too many projected files vanished
    /// at once (suspected bulk loss — git checkout/clean, backup restore — rather
    /// than deliberate per-entry deletion). See [`MEMORY_SYNC_BULK_ARCHIVE_CAP`].
    pub archive_skipped: usize,
    /// Files left untouched because they are not safely readable: unresolved
    /// merge markers, unparseable frontmatter, or an `id` disagreeing with the
    /// filename. Never imported, never used to overwrite the DB.
    pub quarantined: usize,
    /// Set to the count when this pass imported more than
    /// [`MEMORY_SYNC_BULK_ARCHIVE_CAP`] files at once. Not a veto — see the
    /// reasoning at the check itself — but never silent either.
    pub bulk_import_reported: Option<usize>,
    /// Set when a `.gitignore` above the store excludes `.mdkb/` wholesale,
    /// which makes `.mdkb/.gitignore` inert — git never descends into an
    /// excluded directory, so the entry projection cannot be tracked.
    pub gitignore_shadowed: Option<String>,
}

/// Standing disagreement between the entry projection and the database.
///
/// Reported by `mdkb stats` and at session start, NOT only by the run that
/// caused it: 387 orphan files accumulated on a real store because the only
/// place the number ever appeared was the output of an `mdkb update` nobody
/// re-read. A count that lives in a health check is the difference between
/// finding this in a day and finding it by accident months later.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ProjectionDrift {
    /// Files in `entries/` that reconciliation refuses to absorb — unresolved
    /// merge markers, unparseable frontmatter, an `id` disagreeing with the
    /// filename, or content failing entry validation. These are inert: never
    /// imported, never searched, and they do not self-heal.
    pub files_unreadable: usize,
    /// Non-archived entries with no file on disk. They exist only in a database
    /// that is deliberately untracked and unbacked-up.
    pub entries_unprojected: usize,
}

/// The cheap drift check: how many `.md` files sit in `entries/`, and how many
/// non-archived entries the database holds.
///
/// One `read_dir` and one `COUNT(*)`, with no file contents read and nothing
/// parsed, because this runs on the session-start hook path — [`projection_drift`]
/// opens and parses every file, which is fine for a command someone typed and
/// far too much for a hook that fires on every session against a corpus of
/// thousands.
///
/// Equal counts do not prove the two sides agree (a file could be unreadable
/// while an unrelated entry is unprojected, netting to zero), so this is a
/// *smoke* signal: it points at `mdkb stats`, which does the real classification.
pub fn projection_file_and_row_counts(ctx: &Context) -> Result<(usize, usize)> {
    let entries_dir = ctx.memory_dir().join("entries");
    let files = std::fs::read_dir(&entries_dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
                .count()
        })
        .unwrap_or(0);
    let rows: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM memory_entries WHERE status != 'archived'",
        [],
        |r| r.get(0),
    )?;
    Ok((files, rows as usize))
}

/// Measure projection drift without changing anything.
///
/// Shares [`read_entries_dir`] with the reconciliation pass, so "unreadable"
/// means exactly what reconciliation means by it — a second implementation
/// would drift from the first and report a number nobody could act on.
pub fn projection_drift(ctx: &Context) -> Result<ProjectionDrift> {
    let entries_dir = ctx.memory_dir().join("entries");
    let mut files_unreadable = 0;
    let mut on_disk = read_entries_dir(&entries_dir, &mut files_unreadable);

    let mut entries_unprojected = 0;
    for row in memory::list_projection_state(&ctx.conn)? {
        if on_disk.remove(&row.entry.id).is_none() && row.entry.status != EntryStatus::Archived {
            entries_unprojected += 1;
        }
    }

    // Whatever is left parses but has no row. It is about to be imported, so it
    // is not drift — unless it would fail validation, in which case it is inert
    // for the same reason as the rest.
    for disk in on_disk.values() {
        if memory::validate_entry_input(
            &disk.file.meta.id,
            &disk.file.meta.title,
            &disk.file.meta.tags,
            &disk.file.content,
        )
        .is_err()
        {
            files_unreadable += 1;
        }
    }

    Ok(ProjectionDrift {
        files_unreadable,
        entries_unprojected,
    })
}

/// Detect a `.gitignore` above the store that excludes `.mdkb/` wholesale.
///
/// Git never descends into an excluded directory, so such a rule makes
/// `.mdkb/.gitignore` inert and the entry projection untrackable — silently.
/// Reported, never repaired: rewriting a `.gitignore` at the repo root, which
/// mdkb does not own, is not something a tool should do behind your back.
pub(crate) fn gitignore_shadow(root: &Path, entries_dir: &Path) -> Option<String> {
    let git_root = crate::git::find_git_root(root)?;
    let probe = entries_dir.join("probe.md");
    let rel = probe.strip_prefix(&git_root).ok()?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(&git_root)
        .args(["check-ignore", "-v", "--no-index", "--"])
        .arg(rel)
        .output()
        .ok()?;
    // Exit 1 with no output = not ignored, which is the healthy state.
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let rule = line.split('\t').next()?.trim();
    if rule.is_empty() || rule.starts_with(".mdkb/.gitignore") {
        return None;
    }
    Some(format!(
        "`{}` excludes the memory projection ({rule}) — git never descends into an \
         excluded directory, so .mdkb/.gitignore is inert and entries cannot be tracked. \
         Replace the blanket rule with `.mdkb/*` plus `!.mdkb/memory/`, or drop it and \
         let .mdkb/.gitignore do the work.",
        git_root.join(".gitignore").display()
    ))
}

/// A markdown file found in `entries/`, with the hash of its exact bytes.
struct DiskEntry {
    hash: String,
    file: crate::store::memory_file::MemoryFile,
}

/// Lines that mean a merge was left unresolved. A file containing them still
/// parses — the markers land in the body — so without this check an unresolved
/// merge would be imported as content, with both sides concatenated.
fn has_conflict_markers(text: &str) -> bool {
    text.lines()
        .any(|l| l.starts_with("<<<<<<< ") || l == "=======" || l.starts_with(">>>>>>> "))
}

/// Read `entries/`, keyed by entry id.
///
/// Anything not safely readable is counted as quarantined and omitted: it is
/// then invisible to reconciliation, which is the point — an unreadable file
/// must neither overwrite the DB nor look like a deletion. Because the entry
/// keeps its `projected_at`, the *next* pass retries it unchanged.
fn read_entries_dir(dir: &Path, quarantined: &mut usize) -> BTreeMap<String, DiskEntry> {
    let mut out = BTreeMap::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("memory sync: {}: read error: {e}", path.display());
                *quarantined += 1;
                continue;
            }
        };
        if has_conflict_markers(&text) {
            tracing::warn!(
                "memory sync: {} contains unresolved merge markers — skipped. \
                 Resolve the merge and re-run.",
                path.display()
            );
            *quarantined += 1;
            continue;
        }
        let file = match crate::store::memory_file::from_markdown(&text) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("memory sync: {}: parse error: {e}", path.display());
                *quarantined += 1;
                continue;
            }
        };
        if file.meta.id != stem {
            tracing::warn!(
                "memory sync: {} declares id `{}` — skipped. Rename the file to `{}.md` \
                 or fix the frontmatter; mdkb will not guess which one is authoritative.",
                path.display(),
                file.meta.id,
                file.meta.id
            );
            *quarantined += 1;
            continue;
        }
        out.insert(
            stem.to_string(),
            DiskEntry {
                hash: crate::code::indexing::hasher::content_hash(&text),
                file,
            },
        );
    }
    out
}

/// What reconciliation decided to do with one entry.
enum SyncAction {
    /// DB → file (backfill, or the DB side won).
    Project(String),
    /// File → DB insert; this machine has never seen the entry.
    Import(Box<MemoryEntry>),
    /// File → DB update; only the file changed.
    Adopt {
        entry: Box<MemoryEntry>,
        updated_at: i64,
    },
    /// Both sides changed. Winner decided by `updated_at`, loser preserved.
    Conflict {
        id: String,
        file_entry: Box<MemoryEntry>,
        file_updated_at: i64,
    },
    /// An archived entry's file came back.
    Revive { id: String, updated_at: i64 },
}

/// Maximum entries `sync_memory_files` will archive in a single pass **on the
/// evidence of the filesystem alone**. Above this, a vanished *set* of projected
/// files is far more consistent with directory loss (a `git checkout`/`stash`/
/// `clean`, a partial restore, a mount blip) than with a human deliberately
/// deleting that many entries by hand — so we refuse to archive and warn loudly
/// instead of silently retiring a large slice of the memory corpus (DATA-2). A
/// human who really wants a bulk prune can re-run; nothing is lost by waiting,
/// everything is lost by archiving a corpus that's about to reappear.
///
/// Deletions git can *prove* were intentional bypass the cap — see
/// [`committed_deletions`].
pub const MEMORY_SYNC_BULK_ARCHIVE_CAP: usize = 10;

/// Reconcile the markdown projection with the DB, in both directions.
///
/// The DB owns local telemetry (access counts, confirmations); the markdown file
/// is the durable, git-tracked projection that clones share. Neither is
/// unconditionally authoritative, so every entry is classified on two
/// independent signals before anything is written:
///
/// * `db_changed`   — `updated_at` moved past `projected_at`;
/// * `file_changed` — the file's bytes no longer hash to `projected_hash`.
///
/// Content hashing, not mtime: git stamps every file it writes during a
/// checkout or merge with the checkout time, so after any `git pull` every
/// pulled file — changed or not — has an mtime newer than `projected_at`, and
/// the whole directory would look hand-edited.
///
/// Classification runs over the whole directory before any mutation, so the bulk
/// circuit breaker below sees the archival candidates as a set.
pub fn sync_memory_files(ctx: &Context) -> Result<MemorySyncSummary> {
    let entries_dir = ctx.memory_dir().join("entries");
    std::fs::create_dir_all(&entries_dir)?;
    crate::core::ensure_store_gitignore(&ctx.memory_dir())?;

    let now = chrono::Utc::now().timestamp();
    let mut summary = MemorySyncSummary {
        gitignore_shadowed: gitignore_shadow(ctx.root(), &entries_dir),
        ..Default::default()
    };

    let mut on_disk = read_entries_dir(&entries_dir, &mut summary.quarantined);
    let mut actions: Vec<SyncAction> = Vec::new();
    let mut to_archive: Vec<String> = Vec::new();

    for row in memory::list_projection_state(&ctx.conn)? {
        let id = row.entry.id.clone();
        let disk = on_disk.remove(&id);
        match (disk, row.entry.status) {
            // Archived and still gone — the retirement stands.
            (None, EntryStatus::Archived) => {}
            // Never projected (DB-only) → backfill the markdown file. Never
            // archive: that distinction is exactly what `projected_at` records.
            (None, _) if row.projected_at.is_none() => actions.push(SyncAction::Project(id)),
            // Was projected, file now gone → archival candidate (subject to the cap).
            (None, _) => to_archive.push(id),
            // A file for an archived entry means it came back (branch switch
            // back, restore). Revive rather than leave a tracked file shadowing
            // a dead row — otherwise the entry could never return. Content
            // differences settle on the next pass, once the row is active again;
            // reviving and rewriting at once would make the revive itself look
            // like a durable edit.
            (Some(_), EntryStatus::Archived) => actions.push(SyncAction::Revive {
                id,
                updated_at: row.entry.updated_at,
            }),
            (Some(disk), _) => {
                // No recorded hash: the projection predates schema v19, so the
                // file's bytes are unknown and "did it change?" is unanswerable.
                // Re-project from the DB rather than guess — guessing "yes" here
                // would fabricate a conflict for every pre-existing entry.
                let Some(projected_hash) = row.projected_hash.as_deref() else {
                    actions.push(SyncAction::Project(id));
                    continue;
                };
                let db_hash = crate::code::indexing::hasher::content_hash(
                    &crate::store::memory_file::to_markdown(&row.entry),
                );
                let file_changed = projected_hash != disk.hash;
                let db_changed = projected_hash != db_hash;
                let file_updated_at = disk.file.meta.updated_at.unwrap_or(row.entry.updated_at);
                match (file_changed, db_changed) {
                    (false, false) => {}
                    (false, true) => actions.push(SyncAction::Project(id)),
                    (true, false) => {
                        // A hand-edit does not touch `updated_at`; without this
                        // the entry would keep a stale timestamp and lose every
                        // future conflict it should have won.
                        let updated_at = if file_updated_at > row.entry.updated_at {
                            file_updated_at
                        } else {
                            now
                        };
                        actions.push(SyncAction::Adopt {
                            entry: Box::new(disk.file.into_fresh_entry()),
                            updated_at,
                        });
                    }
                    // Both moved — but if they moved to the same place (the same
                    // edit applied on both sides) there is nothing to arbitrate:
                    // just record the new projection.
                    (true, true) if disk.hash == db_hash => {
                        memory::set_projection(&ctx.conn, &id, now, &disk.hash)?;
                    }
                    (true, true) => actions.push(SyncAction::Conflict {
                        id,
                        file_entry: Box::new(disk.file.into_fresh_entry()),
                        file_updated_at,
                    }),
                }
            }
        }
    }

    // Whatever is left on disk has no DB row at all: a colleague's entry arriving
    // in a checkout, or a hand-authored file dropped into the directory.
    for (_, disk) in on_disk {
        match memory::validate_entry_input(
            &disk.file.meta.id,
            &disk.file.meta.title,
            &disk.file.meta.tags,
            &disk.file.content,
        ) {
            Ok(()) => actions.push(SyncAction::Import(Box::new(disk.file.into_fresh_entry()))),
            Err(e) => {
                tracing::warn!("memory sync: {}: {e} — not imported", disk.file.meta.id);
                summary.quarantined += 1;
            }
        }
    }

    // DATA-2 guard, now split by what git can prove. A deletion recorded in
    // reachable history is intent and must propagate however large it is —
    // otherwise a colleague's `git rm` never reaches anyone. A file that HEAD
    // still lists, or that history never saw deleted, is unexplained loss and
    // keeps the cap.
    let (intentional, suspect) = partition_deletions(ctx.root(), &entries_dir, to_archive);
    let mut to_archive = intentional;
    if suspect.len() > MEMORY_SYNC_BULK_ARCHIVE_CAP {
        tracing::warn!(
            "memory sync: {} projected entry files are missing at once (> cap {}) with no \
             recorded deletion — suspected bulk loss (git checkout/clean, backup restore), \
             NOT archiving. Restore .mdkb/memory/entries/ or re-run `mdkb update` once the \
             files return.",
            suspect.len(),
            MEMORY_SYNC_BULK_ARCHIVE_CAP,
        );
        summary.archive_skipped = suspect.len();
    } else {
        to_archive.extend(suspect);
    }

    // A bulk import is NOT capped. Archiving is destructive and reversible only
    // by hand, so its cap buys time; importing is additive, and the largest
    // import there is — a fresh clone of the whole corpus — is the primary
    // reason the projection is tracked at all. Blocking it would need a flag on
    // the one command a new checkout must run unattended.
    //
    // What 015-2dc2 actually cost was silence: 387 files drifted for weeks
    // because nothing said so. So the size is announced, and the import runs.
    let incoming = actions
        .iter()
        .filter(|a| matches!(a, SyncAction::Import(_)))
        .count();
    if incoming > MEMORY_SYNC_BULK_ARCHIVE_CAP {
        summary.bulk_import_reported = Some(incoming);
        tracing::warn!(
            "memory sync: importing {incoming} entry files with no database row (> cap {}). \
             Expected on a fresh checkout; unexpected on a store that was already in sync, \
             where it means the database lost rows the projection still has.",
            MEMORY_SYNC_BULK_ARCHIVE_CAP,
        );
    }

    for action in actions {
        apply_sync_action(ctx, action, now, &mut summary)?;
    }
    for id in &to_archive {
        memory::set_status(&ctx.conn, id, EntryStatus::Archived)?;
        summary.archived += 1;
    }

    if summary.projected > 0
        || summary.imported > 0
        || summary.adopted > 0
        || summary.conflicts > 0
        || summary.revived > 0
        || summary.archived > 0
    {
        generate_memory_index(ctx)?;
    }
    Ok(summary)
}

/// Execute one classified action. Split out so classification above reads as a
/// decision table rather than a decision table interleaved with I/O.
fn apply_sync_action(
    ctx: &Context,
    action: SyncAction,
    now: i64,
    summary: &mut MemorySyncSummary,
) -> Result<()> {
    match action {
        SyncAction::Project(id) => {
            if let Some(entry) = memory::get_entry_without_tracking(&ctx.conn, &id)? {
                project_entry(ctx, &entry, now)?;
                summary.projected += 1;
            }
        }
        SyncAction::Import(entry) => {
            memory::add_entry(&ctx.conn, &entry)?;
            // Re-project rather than trust the incoming bytes: the file may have
            // been hand-written with fields in another order, and the recorded
            // hash must describe what *we* would write, or the very next pass
            // reads it back as a local edit.
            project_entry(ctx, &entry, now)?;
            summary.imported += 1;
        }
        SyncAction::Adopt { entry, updated_at } => {
            adopt_file_version(ctx, &entry, updated_at, now)?;
            summary.adopted += 1;
        }
        SyncAction::Conflict {
            id,
            file_entry,
            file_updated_at,
        } => {
            let Some(db_entry) = memory::get_entry_without_tracking(&ctx.conn, &id)? else {
                return Ok(());
            };
            // Ties go to the file: it is the side that just arrived from a merge,
            // and the DB copy is always recoverable from this machine's history.
            if file_updated_at >= db_entry.updated_at {
                memory::save_conflict_snapshot(
                    &ctx.conn,
                    &id,
                    &crate::store::memory_file::to_markdown(&db_entry),
                    "file",
                )?;
                adopt_file_version(ctx, &file_entry, file_updated_at, now)?;
            } else {
                memory::save_conflict_snapshot(
                    &ctx.conn,
                    &id,
                    &crate::store::memory_file::to_markdown(&file_entry),
                    "database",
                )?;
                project_entry(ctx, &db_entry, now)?;
            }
            summary.conflicts += 1;
        }
        SyncAction::Revive { id, updated_at } => {
            // The entry's content did not change, so `updated_at` is carried
            // through unchanged — stamping it now would make the returning file
            // immediately look stale against the DB.
            memory::set_status_at(&ctx.conn, &id, EntryStatus::Active, updated_at)?;
            if let Some(entry) = memory::get_entry_without_tracking(&ctx.conn, &id)? {
                project_entry(ctx, &entry, now)?;
            }
            summary.revived += 1;
        }
    }
    Ok(())
}

/// Write a file's version into the DB, preserving the authored timestamp, and
/// re-project so the recorded hash describes canonical bytes.
fn adopt_file_version(
    ctx: &Context,
    file_entry: &MemoryEntry,
    updated_at: i64,
    now: i64,
) -> Result<()> {
    let previous = memory::get_entry_without_tracking(&ctx.conn, &file_entry.id)?;
    if let Some(prev) = &previous {
        // Ordinary edit history, in addition to any conflict snapshot.
        memory::save_revision(
            &ctx.conn,
            &file_entry.id,
            &prev.content,
            &file_entry.content,
            prev.source_type,
        )?;
    }
    memory::update_entry_at(&ctx.conn, file_entry, updated_at)?;
    let persisted = memory::get_entry_without_tracking(&ctx.conn, &file_entry.id)?;
    if let Some(entry) = persisted {
        project_entry(ctx, &entry, now)?;
    }
    Ok(())
}

/// Split vanished projected files into (intentional, suspect).
///
/// The filesystem alone cannot tell a colleague's committed `git rm` of twenty
/// entries from a broken checkout — both are "twenty previously-projected files
/// are gone". Git can: a deletion reachable from HEAD is recorded intent, while
/// a file HEAD still lists is a worktree that does not match its own commit.
///
/// Everything is `suspect` when git cannot answer — not a repository, git
/// missing, entries never committed — which is exactly the pre-existing
/// behaviour.
fn partition_deletions(
    root: &Path,
    entries_dir: &Path,
    candidates: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    if candidates.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let Some((in_head, deleted)) = committed_deletions(root, entries_dir) else {
        return (Vec::new(), candidates);
    };
    candidates
        .into_iter()
        .partition(|id| !in_head.contains(id) && deleted.contains(id))
}

/// `(entry ids present in HEAD, entry ids deleted somewhere in reachable history)`
/// for the projection directory, or `None` when git cannot answer.
fn committed_deletions(
    root: &Path,
    entries_dir: &Path,
) -> Option<(HashSet<String>, HashSet<String>)> {
    let git_root = crate::git::find_git_root(root)?;
    let rel = entries_dir.strip_prefix(&git_root).ok()?;
    let pathspec = format!("{}/", rel.to_str()?);

    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&git_root)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };

    // No commits yet → history carries no evidence either way.
    run(&["rev-parse", "--verify", "HEAD"])?;

    let ids = |listing: String| -> HashSet<String> {
        listing
            .lines()
            .filter_map(|l| Path::new(l.trim()).file_stem()?.to_str().map(String::from))
            .collect()
    };
    let in_head = ids(run(&[
        "ls-tree",
        "-r",
        "--name-only",
        "HEAD",
        "--",
        &pathspec,
    ])?);
    // A branch that simply never had the file is NOT a deletion; requiring an
    // actual deletion commit is what keeps a branch switch from archiving
    // entries that still exist elsewhere.
    let deleted = ids(run(&[
        "log",
        "--diff-filter=D",
        "--name-only",
        "--format=",
        "HEAD",
        "--",
        &pathspec,
    ])?);
    Some((in_head, deleted))
}

/// Move an entry to archive directory.
pub(crate) fn archive_entry_on_disk(ctx: &Context, id: &str) -> Result<()> {
    // One disposal rule, shared with the migration path — see
    // `memory_file::archive_projection` for why a second copy was a bug.
    crate::store::memory_file::archive_projection(&ctx.memory_dir(), id)?;
    Ok(())
}
