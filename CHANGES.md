# Changelog

## Unreleased

### Added

- **The test suite now compiles and runs clean on Windows.** Three test
  targets used `std::os::unix` ungated, so `cargo test` stopped at compile
  time and a Windows contributor could not run a single test. Tests that are
  unix-only by design (daemon, `flock`, unix sockets, `mdkb hook`) are now
  gated `#[cfg(unix)]`, each with a comment naming the reason. Tests that
  were merely written with unix-shaped inputs now run on Windows too: path
  assertions compare components instead of `/`-strings, the outside-root
  security tests build their escape path from a real tempdir, and the
  git-sync test hands git a non-verbatim path. Test code only; no shipped
  behavior changes. Running the suite on Windows surfaced three real
  platform defects, documented at the gate sites: the live-connection lock
  probe errors with os error 33 (heal/quarantine misbehaves), `mdkb schema`
  crashes with a main-thread stack overflow, and `mdkb hook` exits nonzero
  against the exit-zero host contract.

## 3.7.16 (2026-08-18)

### Fixed

- **On Windows, `mdkb mcp` now serves MCP in-process instead of exiting.** The
  daemon is a unix-socket singleton, so `mdkb mcp` — the entry `mdkb setup mcp`
  writes into the MCP client config — exited with `Daemon proxy requires Unix`.
  MCP clients surface that as an opaque `CONNECTION_CLOSED` at session start,
  with no cause, which made mdkb unusable on Windows out of the box. Every other
  Windows path already runs in-process, so the default now falls back to the same
  in-process global stdio server `MDKB_NO_DAEMON=1` selects. An explicit
  `--socket` on a platform without the daemon still refuses and names the flag:
  ignoring a typed flag would hide a misconfiguration. Unix behavior is
  unchanged. Reported and fixed by Steve Muchow (@smuchow1962).

- **Outside a git repo, project root resolution no longer adopts a container
  directory's store.** `resolve_project_root` bounded its upward search by the
  git root, but only on the branch that found one. A cwd that is not inside a
  repo — a directory that merely holds repos, such as `~/Gits`, `~/Gits/LS`, or a
  worktree container like `~/Gits/LS/agent2__wt` — still walked up unbounded and
  adopted the nearest stray `.mdkb/`. The daemon then anchored the whole
  container tree and indexed every sibling repo, `target/` and `node_modules/`:
  3.99 GB of `code.sqlite` in 15 minutes, followed by an embedding run that held
  every core at 100% for 20 minutes and did not answer SIGTERM. The upward search
  is kept — a non-git project must still find its own store from a sub-path — but
  it now refuses a store that would anchor far more than a project: a directory
  holding git repositories among its children, or `$HOME` and above. This
  completes the fix shipped earlier for the same failure inside a git repo, whose
  acceptance criterion covered only that branch.

- **Embedding no longer nests two per-core thread pools.** fastembed parallelises
  batches with `par_chunks` on rayon's global pool, while every ONNX session it
  builds sets `with_intra_threads(available_parallelism())` — a knob `InitOptions`
  does not expose. On an N-core host that is N rayon workers issuing concurrent
  `Session::run()` into a single N-thread ORT pool; ORT's pool spin-waits, so the
  contention burned every core instead of blocking. The rayon global pool is now
  capped to one worker before the model is created, leaving parallelism to ORT,
  which parallelises a single inference. This does not cause runaway indexing on
  its own — it decides whether an accidental one costs a slow minute or an
  unusable machine.

## 3.7.12 (2026-08-09)

### Fixed

- **On Unix, every CLI store mutation now executes in the daemon.** A single internal
  typed `cli.mutate` protocol covers the complete mutating command surface and
  returns structured results for CLI-side formatting. `init` remains the local
  bootstrap operation; `MDKB_NO_DAEMON=1` remains the explicit direct-write
  escape hatch. The old partial `routing_gap()` and its misleading proof were
  removed. Windows, where the Unix-socket daemon is unavailable, keeps the
  direct path; the same project writer-admission lock serializes it with MCP,
  watcher, telemetry, and schema writers.

- **Corruption detection now releases the daemon context and actually triggers
  recovery.** Memory/document reads, hook telemetry, persistent call telemetry,
  watcher mutations, and daemon mutations close their long-lived `Context` as
  soon as SQLite reports corruption, allowing the next open to quarantine,
  salvage, and rebuild. Successful markers are no longer trusted when the DB or
  WAL is newer, and post-write checks use a fresh connection rather than the
  daemon pager cache. Hook and MCP telemetry use the same universal writer lock
  as direct CLI commands instead of remaining an uncoordinated hot writer.

- **The `memory_write` tool schema now advertises the valid relation values.**
  `relates[].relation` and `relates[].target_kind` were plain strings in the
  JSON Schema, with the accepted values mentioned only in prose, so MCP clients
  guessed relations outside the closed set and had the whole write rejected at
  runtime. Both fields now emit a JSON Schema `enum` generated from the domain
  enums themselves, so the advertised vocabulary cannot drift from the one the
  server enforces. Server-side validation and its error message are unchanged.

- **Commands classified as reads are now actually read-only.** Search, stats,
  collection and memory reads, metrics, experiment inspection, and code-index
  queries no longer initialize schemas, update access telemetry, run repairs,
  create a missing index, or open SQLite read-write. Regression coverage checks
  that neither `index.sqlite` nor `code.sqlite` gains WAL/SHM sidecars. Direct
  CLI reads therefore no longer increment memory `access_count` or
  `last_accessed`; those per-clone signals now move only on daemon-owned paths.

- **The repository's own memory projection is no longer shadowed by its root
  `.gitignore`.** `.mdkb/memory/entries/*.md` can now be committed as designed,
  while databases, locks, WAL files, caches, and archives remain ignored. The
  README now documents the bidirectional `memory sync` workflow introduced by
  schema v19 instead of the superseded external export directory.

- **A routed mutation no longer becomes the second writer it was meant to
  remove.** The routing gave the daemon 30 seconds and then ran the mutation
  in-process regardless — so a `mdkb update` the daemon was still working on had
  the CLI open its own write connection alongside it, on the longest write in the
  program. The client now classifies by *execution evidence*, not by symptom: the
  only question is whether the daemon can have started writing. A request that
  never arrived, one cut short mid-frame, or one the daemon refused before
  dispatch (unknown method, missing `root`, a repo outside its whitelist) is
  proof that nothing ran, and the CLI finishes the job itself. Anything after the
  last request byte — silence, a dropped connection, a failure from inside a
  dispatched method — leaves the outcome unknown, and the CLI fails loudly
  naming the daemon rather than guessing (invariant I3). The boundary is a
  contract, not an inference: `daemon::ipc_server::DISPATCHED_ERROR_CODE` is the
  single code emitted post-dispatch, and a daemon-side test fails if any
  admission refusal starts wearing it. Mutations also stopped borrowing the hook
  deadline, which is sized so an editor keystroke never stalls; a write gets an
  hour, because a full `update` walks the tree, embeds and reindexes the code
  graph.

- **Routed `update` kept its arguments.** Over the daemon, `--force` was dropped
  and `--files` reindexed the whole tree: the RPC carried neither, so a targeted
  update silently became a full one and a forced update silently became a no-op.
  Both now travel with the request, and the daemon returns counts rather than
  printed text, so `--format` is honoured on the routed path the same way it is
  in-process. A targeted in-process update whose code phase fails now says so on
  stderr instead of only tracing it.

### Added

- **Store mutations route through one typed daemon protocol on Unix.** `mdkb mcp` and `mdkb hook`
  already did; the plain CLI never adopted the pattern, so every `mdkb memory
  add` was an independent writer process — its own connection, its own migration
  run, its own virtual-table init — racing the long-lived daemon on one file.
  Commands are now classified as mutation, read or local, with no wildcard: a new
  command that nobody classified fails to *compile* rather than defaulting to a
  direct write. Every mutation is represented by the exhaustive `cli.mutate`
  request and result enums and goes over the hook socket;
  an unreachable daemon falls back to writing in-process, because a routing layer
  that turns a daemon outage into a broken CLI is worse than no routing.
  `MDKB_NO_DAEMON=1` remains the explicit Unix escape hatch. Windows continues
  to use the direct path because the daemon transport is Unix-only, protected
  by the same cross-surface writer-admission lock as MCP and watcher activity.

- **A read-only store path, so a read stops being a write.** Opening the store
  ran migrations, created the FTS and vector virtual tables and initialized the
  stats schema — on *every* open, including the ones that only wanted to answer a
  query. Every one-shot CLI read was therefore another writer process against the
  file the long-lived daemon is also writing. `mdkb get`, `mget`, `graph`,
  `history`, `current` and `superseded-by` now open with `SQLITE_OPEN_READ_ONLY`
  and skip initialization entirely: they cannot even create a `-wal`/`-shm` pair
  on a store that had none, because creating those files is itself a write. A
  schema mismatch in either direction is an error naming both versions and the
  remedy, rather than a migration — migrating on a read would put the writer
  straight back. `mdkb stats` and `mdkb metrics` deliberately keep a write
  connection: they record telemetry, so they are writers by design until that
  telemetry routes through the daemon.
- **`mdkb surface` maps each MCP tool to its CLI equivalent.** The two surfaces
  expose overlapping capability under different names — the MCP tool is
  `memory_write`, the command is `mdkb memory add`, and `mdkb memory-write` does
  not exist at all — and nothing said so. The inventory is checked rather than
  trusted: the tool names come from the MCP server's own generated router and the
  command paths from clap's own parser, so a tool added on one side and forgotten
  on the other fails the test suite, and a tool with no CLI equivalent must carry
  a reason. The map also ships in the MCP server instructions, so an agent
  holding a tool name can find the command without leaving MCP.

### Changed

- **The shared application layer moved out of `cli::handlers` into `core`.** The
  MCP layer and the daemon reached into the command-line adapter for the work
  they do, which made the CLI the de-facto core of the program and inverted the
  dependency direction of every layer above it. `cli::handlers` is now 57 lines
  of re-exports; the logic lives in `core::indexing`, `core::memory`,
  `core::memory_sync`, `core::search`, `core::sessions`, `core::graph`,
  `core::code` and `core::ops`, each with a header stating why it cannot live
  behind a command-line entry point. A test fails if anything under `src/mcp` or
  `src/daemon` names `cli::handlers` again. No behaviour changed.


### Added

- **`mdkb memory import <file>.md` restores a single entry, timeline intact.**
  `mdkb memory add` stamps `created_at`/`updated_at` with now() and has no flag
  to preserve them, so restoring a corpus of entry files flattened months of
  history into one day and destroyed recency ranking. The only alternative was a
  raw `sqlite3 INSERT` against `index.sqlite` — which skips the connection
  pragmas the store depends on (`busy_timeout`, WAL, `synchronous = NORMAL`) and
  the `.mutation.lock` protocol. Doing exactly that against a live store
  corrupted `memory_fts_data` (`Rowid out of order`, `2nd reference to page
  12862`). The restore runs on the ordinary `Context` connection, so the
  pragmas, the lock and the FTS/embedding triggers all apply. A directory or a
  `.json` file keeps the existing bulk semantics. An existing id is an explicit
  conflict, never a silent overwrite; a frontmatter id disagreeing with the
  filename is refused with both spellings named. Restore preserves the file's
  counters, where a git sync deliberately resets them — the same file means
  different things depending on whose history it records.

### Fixed

- **The daemon hook fallback was decorative and is now real.** The generated
  wiring was `if ! mdkb hook <event>; then MDKB_NO_DAEMON=1 mdkb hook <event>;
  fi`, live in both global settings files. It could never fire: the hook client
  returns success on every failure by contract, because the host hook must exit
  0, so the `if !` branch was unreachable and a dead daemon meant hooks silently
  did nothing while the settings file advertised a rail that did not exist. The
  fallback now runs **in-process** when the daemon cannot answer, and the shell
  conditional is gone from the generated wiring, so the settings file describes
  what actually happens. The exit-0 contract is unchanged. New `MDKB_NO_SPAWN=1`
  reports the daemon unreachable immediately instead of waiting out the spawn
  backoff — for sandboxes and CI runners that must not leave a background
  process behind. Distinct from `MDKB_NO_DAEMON`, which bypasses the daemon
  entirely.
- **A binary refuses to open a store newer than it understands, and a daemon
  retires when its executable is replaced.** Measured on one machine: a daemon up
  for two days while `target/release/mdkb` was rebuilt underneath it, with two
  schema versions landing in between — so one-shot CLI writers and the daemon
  were different builds writing one file. Opening a store recorded newer than the
  running binary used to fall through and carry on: no migration runs, but the
  binary then reads and writes tables whose shape it does not know, and
  `SCHEMA_SQL` has already re-run by that point, leaving anything the newer
  version redefined as whichever definition the older binary carries. Both
  versions are now named in the refusal. Separately, the daemon polls the
  executable it was launched from and stands down gracefully when it changes, so
  the next call spawns a matching build.
- **The v11 → v12 prior purge no longer leaves its markdown behind (schema
  v20).** The migration deletes legacy behavioural priors and the delete cascades
  through triggers, but nothing touched `.mdkb/memory/entries/<id>.md` — 113
  files on one store, all with `status: active` frontmatter and no row. Since
  bidirectional sync that is not litter but a correctness bug: a file with no row
  is imported, so the purge would undo itself on the next `mdkb update`. Disposal
  is now one shared rule (`mdkb memory rm` already had it; the migration did not,
  and that duplication *was* the bug). Two halves, because they reach different
  stores: the v12 purge disposes at source, and a new v20 sweep archives every
  orphaned legacy prior projection — the heal for files already on disk. Files
  are archived, never deleted.
- **A quarantine no longer wipes collection registrations — this was the cause
  of the "collection vanished" reports.** Autoheal rebuilds a corrupt index empty
  and salvaged `memory_entries` and `memory_edges` out of the old file, and
  nothing else. `collections` went with it, so the next `mdkb update` found no
  collection registered, indexed only the repo root, printed a success line and
  exited 0. On one store that turned 2046 indexed documents into 3; it was blamed
  on an unrelated `.mdkb/config.toml` edit and found by accident several runs
  later, when a spot-check query failed. The rule now applied is whether a table
  can be re-derived from files still on disk: `documents`, `content` and `edges`
  can, so a reindex rebuilds them; `collections` records the *decision* that a
  directory is a collection and exists nowhere else. Also salvaged:
  `memory_revisions` (edit history, and since v19 the losing side of every
  file/DB conflict) and the mined behavioural priors. `evolution` is deliberately
  excluded — its foreign keys point at `documents`, which the rebuild wipes.
- **`mdkb update` reports per-collection counts and names a collection that
  disappeared.** The old single total could not distinguish a healthy re-index
  from one collection dropping to zero while another grew. A collection that held
  documents on a previous run and is no longer registered is now named in the
  output *and* pushed into `errors`, so a caller that only checks `errors` — every
  hook, and the MCP layer — stops treating the run as clean. A store with no
  collection registered at all says so instead of printing "Docs: 0 indexed".
  Detection uses a `.mdkb/collections.snapshot.json` sidecar, because neither
  in-database trace works: `documents.collection` cascades on delete, so
  unregistering erases the evidence in the same statement, and a quarantine wipes
  both tables together.

### Added

- **Projection drift is reported by the standing health check, not only by the
  run that caused it.** On one store, 387 entry files drifted away from the
  database — 265 of them carrying unique decision/problem knowledge — and were
  found by accident months later, because the only place the number ever
  appeared was the output of an `mdkb update` nobody re-read. `mdkb stats` now
  reports two counts, each shown only when non-zero so the line is never
  wallpaper: entry files reconciliation refuses to absorb (merge markers, bad
  frontmatter, id/filename mismatch, failed validation — these are inert and do
  not self-heal), and non-archived entries with no file on disk. Session start
  carries a cheaper version: one `read_dir` and one `COUNT(*)`, no file
  contents read, because that is a hook path against a corpus of thousands. It
  is a smoke signal by design and points at `mdkb stats` for the breakdown.
  A bulk import — more than ten files with no database row — announces itself
  but is **not** capped: the largest import there is, a fresh clone of the whole
  corpus, is the reason the projection is tracked at all, and blocking it would
  put a flag on the one command a new checkout must run unattended. Archiving
  keeps its cap, because archiving is destructive and importing is additive.

### Fixed

- **Enum-valued CLI flags now publish their accepted values.** `--entry-type`,
  `--source-type` and `mdkb memory link`'s `<RELATION>` named a closed set
  without listing it, so the only way to learn the values was to read the source
  — and a wrong guess failed at runtime with a Rust debug payload
  (`Error { kind: InvalidQuery("Invalid entry type: pattern"), .. }`) that named
  neither the flag nor the alternatives. They are now derived from `EntryType`,
  `SourceType` and `MemoryRelation`, so `--help` prints
  `[possible values: ...]` and a bad value fails as a clap usage error listing
  the set. Two drifted help strings went with it: `mdkb hook memory-write`
  advertised `pattern`, which has never been a variant, and `mdkb search
  --entry-type` omitted `handoff`. A test asserts no help text hand-lists a
  closed set, so the copy cannot drift from the enum again.

### Changed

- **Reading a memory entry no longer rewrites the full-text index (schema v18).**
  `get_entry` bumps `access_count` and `last_accessed` on every read, and the
  `memory_au` trigger was an unscoped `AFTER UPDATE` — so each read deleted and
  reinserted the entry's FTS5 segments, making *reads* the store's single
  heaviest writer against the blob-heavy `memory_fts_data` shadow table, one of
  the three tables recurring field corruption keeps damaging. The trigger is now
  scoped to the columns the index actually stores (`id`, `title`, `content`,
  `tags`). **This changes behaviour on every existing store**: the v17 → v18
  migration drops and recreates `memory_au` in place. No reindex is needed and
  no index content changes — only the write amplification disappears.
- **Quarantine reports now record the damage, not just the loss.** Every previous
  post-mortem stalled at "the index is malformed" with no record of *how*. A
  `.report.json` sidecar now also carries the `PRAGMA quick_check` rows, the
  tables owning the damaged b-trees (resolved through `sqlite_master.rootpage`),
  the database and WAL sizes at quarantine time, and the pid and version of the
  process that *detected* the corruption — which is explicitly not a claim about
  what caused it. All of it is read through `immutable=1`, best-effort: a file
  too damaged to answer a question contributes nothing rather than failing the
  quarantine. Nothing is collected on a healthy store.
- **A store is refused under a second spelling of its own path.** Every
  cross-process guarantee is keyed on the database path as a *string* — the
  `.mutation.lock` and `.live.lock` sidecars, and the `-wal`/`-shm` files SQLite
  derives itself — so two spellings that reach one inode give two lock domains
  and two WAL indexes over a single database: writers that cannot see each other,
  allocating the same pages twice, which is the `2nd reference to page N` damage
  seen in the field. `Context::open` already canonicalized, ruling out case
  folding on APFS, symlinks and `..`; it cannot rule out aliases canonicalization
  does not resolve, such as macOS firmlinks. The first process to open a store now
  records the spelling it used, and a later process arriving with a different
  spelling for the same inode is refused with both paths named. A store that was
  moved or copied is adopted, not refused.

### Added

- **Memory entries are git-tracked markdown, reconciled in both directions
  (schema v19 `projected_hash`).** `.mdkb/` was excluded from git wholesale, so a
  project's memory died with the machine: it could not be shared, reviewed, or
  restored. The markdown projection existed but ran one way — the sync loop
  iterated DB rows, so a file arriving from `git pull` was structurally invisible
  and an edited file was ignored outright.
  - **`mdkb init` writes `.mdkb/.gitignore`**, an allow-list (`*` then
    re-includes) rather than an enumeration, so a sidecar added later cannot leak
    into git. Only `.gitignore` and `memory/entries/*.md` are tracked; the sqlite
    indexes and their `-wal`/`-shm`/lock/integrity sidecars, `vectors.bin`, hook
    telemetry, backups, quarantined corrupt databases, the regenerated warm-up
    index and the per-machine archive stay out.
  - **Frontmatter is split into durable and local.** The file carries `id`,
    `title`, `entry_type`, `source_type`, `status`, `tags`, `created_at`,
    `updated_at`, `source_path`, `superseded_by`, `expires_at`, `due_at`.
    `access_count`, `last_accessed`, `confirmations` and `last_confirmed_at` stay
    in the DB: they move on every read, so projecting them meant a diff per
    lookup and a merge conflict per pull. Consequence worth knowing —
    **confidence is now per-clone**, since all of its inputs are local.
  - **File → DB reconciliation.** A file with no DB row is imported; a file whose
    bytes changed updates its entry. Change detection compares content hashes on
    both sides, never mtime: git stamps every file it writes during a checkout
    with the checkout time, so mtime would report the whole directory as
    hand-edited after any pull.
  - **Conflict rule.** When both sides moved, the newer `updated_at` wins (ties to
    the file, which just arrived from a merge) and the loser is preserved as a
    full markdown snapshot in `memory_revisions` — deliberately not through
    `save_revision`, which stores nothing at all for `auto_extracted` entries and
    records only a content diff, losing a title or tag change entirely.
  - **The bulk-loss circuit breaker now asks git.** A colleague's committed
    deletion of twenty entries and a broken checkout are identical on the
    filesystem. A deletion recorded in reachable history is intent and archives
    however large; a file HEAD still lists, or one history never saw deleted,
    keeps the cap of 10. Outside a git repo the behaviour is unchanged. A file
    that reappears revives its archived entry.
  - **Unsafe files are never absorbed.** Unresolved merge markers, unparseable
    frontmatter, or an `id` disagreeing with the filename leave the entry
    untouched and are reported, not guessed at.
  - **`mdkb memory sync`** runs the reconciliation without reindexing documents;
    `mdkb update` still runs it and now reports imports, adoptions, conflicts,
    revivals and quarantines.
  - **The daemon watcher reconciles automatically.** A change under
    `.mdkb/memory/entries/` is a third watcher route, so a `git pull` is picked up
    without anyone remembering to run `mdkb update`. The watcher event is only a
    *trigger*: the debounced flush re-reads the whole directory, because the
    bulk-loss breaker and the git deletion discriminator are set-level decisions
    that twelve per-file events would defeat. Reconciliation writing into the
    directory it watches costs exactly one extra no-op pass, since the recorded
    hashes then already match.

### Fixed

- **The daemon watched nothing when code indexing was disabled.**
  `watcher.watch(&root)` is the only call that registers the repo root and it sat
  behind `if code_enabled`, so with `[code] enabled = false` no code, no
  documents — and none of the new memory routing — were ever seen, while every
  log line still reported a running watcher. Registering the root is what makes
  routing possible and no longer depends on any single sink; whether a change is
  acted on remains a per-sink decision.
  - **A parent `.gitignore` excluding `.mdkb/` is detected and reported.** Git
    never descends into an excluded directory, so such a rule makes
    `.mdkb/.gitignore` inert silently. mdkb names the offending rule and the fix;
    it never rewrites a `.gitignore` it does not own.
  - **Migration.** Existing projections have telemetry frontmatter and no recorded
    hash. The first run re-projects them once — a single mechanical commit
    stripping the local fields — and explicitly does not read the unknown bytes as
    a conflict. Nothing is lost: those values live in `memory_entries`.

## 3.7.11 (2026-08-03)

### Fixed

- **Daemon-backed memory mutations now participate in index recovery.** MCP
  memory write, batch, delete, and confirm operations previously wrote directly
  through the long-lived repository context. They neither held the
  cross-process mutation lock nor invalidated and rechecked the integrity
  marker, so a corrupt connection could retain its live lock and block the
  quarantine intended to repair it. These operations are now serialized,
  verified through a fresh SQLite connection, and release the repository
  context immediately on corruption so the next call can quarantine, salvage,
  and rebuild. Salvage also reports rows skipped by `INSERT OR IGNORE` instead
  of presenting its inserted-row count as complete recovery.

- **The daemon ping identifies the running mdkb version.** Integrators can now
  detect a detached daemon left behind by a binary upgrade instead of silently
  sending work to an older process. The local-release script terminates every
  matching MCP proxy and detached daemon before starting the rebuilt daemon,
  rather than relying on one PID-file owner to represent all stale runtimes.

## 3.7.10 (2026-08-02)

### Changed

- **Session-start warmup is scoped to the project the session works in.** One
  `.mdkb` store routinely anchors a family of sibling projects, and warmup was
  project-blind: `get_warmup_entries` ranked purely by access count, and
  `take_newest_handoff_body` picked the newest handoff in the whole store —
  whose full body is injected verbatim and exempt from the token budget. A
  session was warmed with unrelated projects' entries and handed another
  project's session state as its anchor. The missing signal was the session
  cwd, which already rides the hook wire and was simply never consumed:
  `hook_session_cwd` accepts `params.cwd` only when absolute and, canonicalized,
  under the store root (it is client-supplied, so it is validated rather than
  trusted), and `project_scope_token` takes the first path segment below the
  root only when a collection of that name is registered — collections are
  created one per subproject, so they are the store's own statement of what a
  project is. The handoff selector now takes the newest handoff *in scope* and
  injects nothing when there is none, because another project's session state is
  worse than no anchor. Scope affinity is a leading term in the existing ranking
  comparator, constant 0 when unscoped, so it is a bias and not a filter:
  out-of-scope entries still reach every project while budget remains, and there
  is no second code path to drift.

### Fixed

- **Index recovery is serialized across processes, and a corrupt in-use index no
  longer hands back a usable connection.** `Context::open` treated
  `Heal::CorruptInUse` as a warning and continued, so a read command became
  another holder of a database already known to be corrupt — `open` initializes
  schemas and ordinary reads update access statistics, so it both wrote into the
  malformed generation and extended the live-lock veto that blocks recovery. It
  now returns the typed `ErrorKind::IndexCorruptInUse`, which
  `is_index_corrupt` recognises alongside `IndexCorrupt`, so the daemon's
  close-on-corruption path catches it too. Two adjacent holes are closed with
  it: `Context::init` canonicalizes the store directory and takes the mutation
  lock before creating config and virtual tables, so the several hook/MCP
  processes that can enter auto-init at once no longer race; and `Store::open`
  — public, and used as a low-level disk-backed opener — takes the same shared
  live lock as `Context`, so it can no longer keep an invisible connection alive
  while autoheal renames and recreates the database underneath it.

- **`mdkb update` drops files deleted from disk.** The code index kept the
  symbols and relationships of deleted files forever: `update` walks the tree
  and hands the result to `reindex_files`, which computes deletions by testing
  the paths it was given for existence — and a file removed from disk never
  appears in a walk, so the deleted branch was unreachable on that path. Stale
  symbols kept answering `search`, `callers` and `calls` until someone ran a
  full `--force` reindex (agent2 was carrying four such files). `update` now
  prunes every indexed path absent from its walk before diffing, and reports the
  count as `Files removed`. The prune deliberately does NOT live in
  `reindex_files` or `index_directory`: both are also called with a subset of
  the tree (the watcher's changed paths, `mdkb code index <subdir>`), where
  "indexed but not in this batch" is the normal case, not a deletion.

- **A corrupt index is now released by the process that detects it, instead of
  being retried forever.** `verify_and_mark` runs after every index-wide
  mutation, but its failure was only logged. A one-shot CLI recovers anyway (it
  reopens, and the open path quarantines), while the daemon does not: it holds
  the `Context` — and with it the `.live.lock` that stops autoheal renaming the
  file — for the life of the repo handle, so every reopen found the file *in
  use* and declined to quarantine. The daemon was the holder blocking its own
  heal. `~/.mdkb/logs/daemon.log` records the result: `failed PRAGMA quick_check
  after mutation` 17153 times across four stores over 13 days (tuicommander from
  07-11, itview from 07-17), each retry writing into a malformed database. That
  is where tuicommander's 673 lost memory entries went — memory lives only in
  this database, and by the time a daemon restart finally allowed the
  quarantine, only what survived in the torn file could be salvaged.
  `verify_and_mark` now returns a typed `ErrorKind::IndexCorrupt` (and
  `Error::is_index_corrupt` also recognises SQLite's own `DatabaseCorrupt` /
  `NotADatabase`), and the daemon's four mutation sites run through
  `handlers::run_mutation`, which closes the context on that signal. The next
  open then quarantines, salvages memory, and schedules the rebuild — machinery
  that already existed and was simply unreachable while the handle stayed open.
  `tests/e2e_corrupt_recovery.rs` reproduces both halves against a real torn
  database: with the handle held nothing is ever quarantined, and with the
  release the reopen heals and the memory entry survives.

- **`code.sqlite` is probed after index-wide mutations too, on a throwaway
  connection.** It was checked only at open, so a daemon that opens once and
  runs for days could never notice damage — and the obvious fix, probing the
  caller's own connection, does not work: `quick_check` goes through the pager,
  so a long-lived connection answers out of its page cache and reports a file
  torn underneath it as sound. (The reproduction test caught exactly that: with
  the probe on the working connection the mutation reported success over a
  deliberately corrupted database.) `IndexFacade::{update, index_directory,
  reindex_files}` now call `heal::verify_and_mark_throttled`, which opens a fresh
  connection and is bounded to one scan per `CHECK_INTERVAL` (6h) by the same
  marker the open path uses — the watcher fires this constantly, and a code index
  can reach gigabytes. The daemon's three code-index mutation sites run through
  `indexing::run_code_mutation`, which closes the facade on corruption so the
  next open quarantines and rebuilds from source.

### Diagnosis notes

- **No store has gone corrupt since 2026-07-18, and the original cause remains
  unproven.** Onset dates from the daemon log are tuicommander 07-11, automa
  07-16, itview 07-17, agent2 07-18 — all before the 3.7.6/3.7.7/3.7.8 lock work,
  and agent2's four quarantines (07-22 → 07-28) are re-corruptions of a store
  already stuck in the loop. Everything logged after 3.7.9 shipped (2026-07-29
  07:43) is aftermath on already-corrupt stores, not a new onset: itview's
  07-30 23:40 write is a failed write into a file corrupt since 07-17, not the
  moment of damage. Post-mortem cannot go further — a quarantined file records
  no writer identity — but the two releases above change what a recurrence looks
  like: detection within one `CHECK_INTERVAL` instead of never, a quarantine
  timestamp that dates the damage rather than the discovery, and no window in
  which memory is written into a file that is already lost.

## 3.7.9 (2026-07-29)

### Fixed

- **A distiller that never reads its prompt is no longer an error.**
  `run_distiller_cli` propagated `EPIPE` from writing the prompt to the agent
  CLI's stdin, so a distiller that exited before reading it failed the call.
  Whether the write lands before the child exits is a scheduling race, which
  made the outcome platform-dependent — the same non-zero-exit stub returned
  `Ok` on macOS and `Err` on Linux, so 3.7.8's CI went red on a test that is
  green locally. A closed pipe means the child did not want the input; its
  stdout still decides the outcome, so `EPIPE` is swallowed while every other
  write error still propagates. The regression test writes 4 MiB to a child
  that never reads, making `EPIPE` a certainty rather than a coin flip.

## 3.7.8 (2026-07-28)

### Fixed

- **`mdkb stats` and `mdkb compact` no longer open `code.sqlite` without
  announcing themselves.** 3.7.6 closed the corruption loop by making every
  connection hold a shared `*.live.lock` so a quarantine can never rename the
  database out from under an open handle — but three `code.sqlite` opens
  bypassed it. Two of them write: `mdkb compact` runs `VACUUM`, and `mdkb
  stats` runs `run_repairs`, which issues `DELETE`s. A quarantine concurrent
  with either one recycled the path onto a fresh database, and SQLite derives
  `-wal`/`-shm` from the path, so those frames landed in the replacement's
  WAL — the same mechanism 3.7.6 set out to close. Both now take the live lock
  before opening. The third site (the hook staleness probe) is read-only and
  injects no frames, so it is left as is.

- **The quarantine banner no longer truncates its own remediation.** Every
  line of `⚠ INDEX QUARANTINED` exceeded the 72-column frame, so the only
  actionable part — how to clear the warning — was ellipsized away, leaving a
  healthy store nagging about a weeks-old file with no visible way out. The
  banner now prints one field per line and states the cleanup command
  (`rm .mdkb/*.corrupt-*`, matching the scan predicate, so it also clears a
  quarantined `code.sqlite`) once per store instead of once per file.

- **`Context::open` canonicalizes the store before deriving any lock.** Every
  cross-process identity was built from the caller's spelling of the path as a
  string: the `.mutation.lock` and `.live.lock` sidecars, and the `-wal`/`-shm`
  files SQLite names itself. Two spellings of one store therefore yielded two
  lock domains over a single inode — neither the open guard nor the live lock
  excluded the other writer, which produces exactly the doubly-referenced pages
  and freelist mismatch seen after each incident. On a case-insensitive volume
  (the APFS default) `Gits` and `GITS` are such a pair, resolving to the same
  file. Callers were expected to canonicalize and `main.rs` did, but with a
  silent `unwrap_or` fallback to the raw spelling; the invariant now lives
  where the locks are named, and a store whose path cannot be resolved is
  refused instead of opened.

### Known issues

- **One `index.sqlite` corruption remains unexplained.** A repo whose index
  passed `PRAGMA integrity_check` was found corrupt roughly an hour later, with
  the signature above. Three candidate mechanisms were ruled out by direct
  measurement: the live lock *was* held (probed with a non-blocking exclusive
  `flock` on the sidecar) and no rename had occurred; `auto_vacuum` reads
  `NONE`; no `mmap_size` is set anywhere and there is no `incremental_vacuum`
  caller. Path aliasing — closed above — was the fourth candidate and cannot be
  confirmed as the cause of *that* incident either, since `Path::canonicalize`
  does normalize case on APFS and the CLI already applied it. Distinguishing the
  remaining possibilities needs write-level tracing while corruption happens,
  which a post-mortem file cannot supply. Recovery, verified: memory is a 1:1
  markdown mirror in `.mdkb/memory/entries`, so stop the daemon in a poll loop
  until an exclusive `flock` on `index.sqlite.live.lock` succeeds, move the
  corrupt file aside, then `mdkb memory import .mdkb/memory/entries && mdkb
  update`.

## 3.7.7 (2026-07-28)

### Added

- **`*`-prefixed prompts now search documents, not just memory.** The
  UserPromptSubmit recall gained a documents leg backed by the same hybrid
  engine as `mdkb search --scope docs`, emitted as a `## mdkb: matching docs`
  block. It reuses the recall query and the embedding already computed for the
  memory leg, so the added cost is one BM25 pass plus one vector probe — no
  second inference, and no lock held across it. Tune with
  `[hooks] recall_docs_limit` (default `3`, `0` restores memory-only recall).
  A document reachable both by search and by the frontmatter graph is emitted
  once, under `## mdkb: related docs`, which carries the relation label.

### Fixed

- **A quarantine no longer seeds the next corruption.** Autoheal renamed
  `index.sqlite` (and `code.sqlite`) plus their `-wal`/`-shm` sidecars while
  other processes still had the database open — the daemon keeps per-repo
  handles alive for days. SQLite ties a connection to the inode but derives
  `-wal`/`-shm` from the *path*, so once the path was recycled onto a fresh
  database a surviving connection could land its frames in the replacement's
  WAL, which produces exactly the doubly-referenced pages seen after each
  heal. Every connection now holds a shared `*.live.lock` for its lifetime and
  quarantine only renames when it can take that lock exclusively; otherwise the
  corrupt file is left in place and reported (`Heal::CorruptInUse`,
  `Context::corrupt_in_use`) so the operator can close the daemon and let the
  next open rebuild it. The lock is a separate sidecar from the mutation lock,
  so a live connection never blocks an index-wide write.

## 3.7.6 (2026-07-22)

### Changed

- **Rust lint and API hardening.** Apply project-wide Clippy cleanups across
  parsers, storage, daemon, MCP, and tests, including explicit conversion,
  result-use, async-lock, and floating-point assertion handling. No runtime
  behavior changes are intended.

### Fixed

- **Quarantine artifacts are collision-safe and transactional.** Preserve every
  forensic copy when multiple recoveries share a timestamp, move WAL/SHM
  sidecars with the database, and roll back partial moves on filesystem errors.

## 3.7.5 (2026-07-22)

### Fixed

- **Lifecycle hooks no longer leak context across repositories.** Prefer the
  host-provided event working directory over the hook subprocess directory, so
  a SessionStart in one repository cannot surface another repository's warmup
  or quarantine banner.
- **Corrupt code indexes recover automatically.** Validate `code.sqlite` before
  opening it, retain malformed databases and WAL sidecars under
  `.mdkb/quarantine/`, and rebuild the reproducible code index from source.
- **MCP stdio survives daemon restarts.** Keep the client transport open after
  a daemon socket disconnect, fail only requests that were in flight, and
  reconnect by replaying the initialization handshake before the next request.
  The proxy no longer leaves detached stdin tasks and zombie processes behind.
- **Hook memory writes accept documented comma-separated tags.** Normalize the
  CLI string into the JSON array required by the daemon instead of returning an
  `invalid type: string, expected a sequence` protocol error.

## 3.7.4 (2026-07-21)

### Fixed

- **Recurring SQLite corruption under concurrent daemon/CLI writes.** Upgrade
  bundled SQLite from 3.46.0 to 3.51.3, which contains the upstream fix for the
  WAL-reset corruption race affecting concurrent writers and checkpointers.
- **Codex `PreToolUse` context injection no longer fails validation.** Context-only
  hook responses now omit `permissionDecision`; Codex reserves `"allow"` for
  responses that also rewrite the tool call through `updatedInput`.

## 3.7.3 (2026-07-11)

Graph navigation & DX (stories 075–082) plus a P1 autoheal data-safety fix (083).

### Fixed

- **Autoheal no longer silently loses memory.** `memory_entries`/`memory_edges`
  live only in `index.sqlite`; on quarantine they are now salvaged into the fresh
  database via `ATTACH ... immutable=1` (a table that cannot be read logs the row
  count lost). The event is surfaced loudly and never silently: an enriched stderr
  warning at heal time, a persistent banner in `mdkb stats` while a `*.corrupt-*`
  file remains, and a SessionStart warmup line (even when the rebuilt store is
  empty). Post-heal now triggers a full docs + sessions + code rebuild, not just
  code. `search`/`get` on an empty store append an actionable "run `mdkb update`"
  hint so a blank result no longer reads as "nothing matched".
- **Graph output no longer leaks numeric doc ids.** `links`/`backlinks` render the
  source document's path (`people/x --owner--> repos/mdkb`) across CLI
  (text/json/csv/markdown) and MCP, resolved in one batched query.
- **`mdkb update` reports honest doc counts.** Output leads with
  `Docs: N indexed (X new, Y changed, Z removed)` so an unchanged re-run reads as
  `N indexed`, not the misleading code-index `Files discovered: 0`.

### Added

- **`neighbors` carries relation labels.** Each neighbor is annotated with the
  `via` relation(s) it was reached through — you see WHY nodes connect, not just
  THAT they do. No extra queries (labels come from the adjacency rows).
- **Collection-prefixed graph refs.** `graph links map/people/x.md` resolves like
  `people/x`; an unresolved ref enumerates the forms it tried.
- **`mdkb collection list`** — name, path, pattern, and document count per
  collection (`--format json` stable).
- **`mdkb graph dangling`** — references that resolve to no indexed document
  (with source doc + relation). Full-table scan, explicit command only.
- **`mdkb graph hubs [--relation R] [--limit N]`** — entities ranked by degree
  centrality with a per-relation breakdown. Full-table scan, explicit command only.

### Changed

- **Recall expansion caps are configurable.** `[graph] expand_seeds`,
  `expand_neighbors`, and `doc_neighbor_cap` move from hardcoded constants into
  `GraphConfig`, with defaults (2/3/3) that keep existing behavior byte-identical.

## 3.7.2 (2026-07-07)

### Fixed

- **`index.sqlite` pointer-map corruption.** Dropped the `mmap` + `auto_vacuum`
  combination that could corrupt the SQLite pointer map on the code index, and
  added an autoheal path that detects and rebuilds a corrupted index on open
  instead of failing the session.

## 3.7.1 (2026-07-07)

Full-codebase audit remediation (stories 055–070) plus warmup/handoff and parser
hardening. No schema break; existing DBs gain the new index on next open.

### Security

- **Daemon root whitelist is now default-deny in global mode.** An empty
  `whitelist_dirs` in `~/.mdkb/daemon.toml` no longer means allow-all; it now
  confines the daemon to the user's home directory. A client can no longer point
  the `--global` daemon at an arbitrary path to force `.mdkb/` creation, indexing,
  or a file watcher. Set `whitelist_dirs` to widen or narrow the allowed roots.
  Single-repo (non-global) local usage is unaffected — it never consults the
  whitelist.
- **MCP `source_file` confined to the repo root** and **HTTP transport now
  enforces authentication**, closing a path-traversal / unauthenticated-read gap
  on the MCP boundary.

### Performance

- **Query embeddings computed off the context lock.** The per-turn semantic
  search no longer holds the context mutex while running ONNX — the single
  highest-impact per-turn latency fix.
- **`idx_files_rel_path` kills O(n²) indexing.** `insert_file` runs a legacy
  cleanup `DELETE ... WHERE rel_path = ?` per file; with `rel_path` unindexed
  each was a full table scan, making a full reindex O(n²). The new index makes it
  a lookup. Added via `CREATE INDEX IF NOT EXISTS`, so existing DBs gain it on
  open. Also speeds `run_repairs`.
- **Incremental reindex re-embeds only changed symbols** instead of the whole
  file's symbol set.
- **Frontmatter regexes cached** (compiled once) and single-chunk doc embedding
  batched.

### Fixed

- **Honest code-index errors.** A failed update/reindex no longer silently wipes
  the index; worker threads are joined, and parser failures are logged instead of
  swallowed.
- **`busy_timeout` + WAL set in the production `Context::open` path**, removing
  the most common `SQLITE_BUSY` that previously triggered the silent wipe.
- **RAII reindex guard** so a panic mid-reindex can no longer wedge the MCP
  handle.
- **Bounded daemon memory** — stale `hook_dedup` sessions are evicted (TTL + LRU).
- **Watcher backpressure visibility + graceful shutdown drain** on the daemon.
- **`get_document_status` errors surfaced**, and the recall stale-dependency check
  batched.
- **CLI `get`** returns a correct exit code, scans the collection once, and runs
  on a one-shot current-thread runtime.
- **Warmup handoff injection.** mdkb now owns handoff injection: the newest
  handoff body is injected and handoffs are excluded from the compact list, with
  a cap on warmup handoffs and noise tags filtered from warmup lines.

### Changed

- **Recursion-depth guards threaded through all recursive parser walks** (31
  walks across the tree-sitter language backends) via shared helpers
  (`node_range`, visibility extraction, doc-comment strip), removing the last
  unbounded-recursion paths in parsing. Deleted the dead `domain/traits.rs`.



## 3.7.0 (2026-07-06)

### Changed

- **UserPromptSubmit recall is now opt-in by default.**
  `[hooks] user_prompt_submit_require_sigil` now defaults to `true`: mdkb injects
  context (recall, related docs, priors, call-graph hint) only for prompts that
  begin with `*`. The `*` is stripped before recall and stopwords are already
  dropped from the FTS query, so suggestions key off the meaningful prompt terms.
  Set `user_prompt_submit_require_sigil = false` to restore the always-on behavior.

### Added

- **Non-aggressive auto-indexing & embedding backfill.** mdkb now self-heals its
  memory embeddings and stops umbrella stores from re-scanning sub-repos, without
  the user running `mdkb update` by hand:
  - **Automatic embedding backfill.** Pending memory embeddings left by a
    cold-model `memory_write` now drain in the background on the next
    session-start and stop hooks (`spawn_embedding_backfill`) — single-flight per
    repo, gated on a cheap count, ONNX off the async runtime. The "N pending
    embeddings — run `mdkb update`" warning clears on its own.
  - **Nested-`.mdkb` boundary.** The index walk (both code and doc/collection
    scanning) no longer descends into a subdirectory that owns its own `.mdkb`
    store — a sub-repo indexes its own files, so an umbrella/parent store stops
    re-walking every child. An explicitly configured collection rooted in a
    sub-repo is still scanned (the walk root is exempt).
  - **Config-driven watcher tunables.** `[code.indexing] debounce_ms` (default
    raised 100→300) and `batch_idle_ms` (default 30000, unchanged — each flush
    re-embeds changed code, so it stays coalesced) are now settable in
    `.mdkb/config.toml`; the hardcoded literals are gone.

- **mdkb×wiz synergy audit — self-learning loop revived, token economy, retention
  (schema v16/v17).** Fixes the audit findings where the self-learning loop was
  effectively dead and search silently degraded to BM25:
  - **Embeddings on every write path.** CLI/bridge `memory add` and both import
    paths now embed like the MCP path; `mdkb update` backfills any entry missing
    an embedding. `mdkb update` also auto-embeds changed documents (`[search]
    auto_embed_docs`, default on; `claude_sessions` excluded unless
    `auto_embed_sessions`). `mdkb embed --collection <name>` embeds one collection
    explicitly. Pending-embedding counts surface in `mdkb stats`.
  - **`memory add --source-type`** (`official_docs|user_statement|inference|
    auto_extracted`, default `user_statement`, preserved on re-write) so
    synthesized entries stop being over-trusted. `update_entry` now persists
    `source_type`.
  - **Daemon-less `mdkb memory confirm <id> --outcome confirmed|refuted`** — the
    confirm loop is reachable on every transport; the UPS recall nudge points at
    this command.
  - **Warmup token economy.** SessionStart warmup strips YAML frontmatter from
    recall snippets, suppresses empty auto-handoffs (keeps the newest), applies a
    confidence floor (`warmup_min_confidence` 0.25) and a ~300-token budget
    (`warmup_token_budget`); `warmup_limit` 50→10.
  - **`claude_sessions` retention.** `mdkb update` archives transcripts whose
    source jsonl is gone (still searchable via `--collection claude_sessions`);
    `mdkb compact --prune-sessions --older-than <dur> [--export <dir>]`
    hard-deletes only archived transcripts, exporting markdown first.
  - **Hook-call telemetry.** Hook invocations are counted under a reserved
    `hooks` pseudo-session (schema v16 `sessions.agent`); opt-in `[telemetry]
    query_events` records per-recall metrics and NEVER the query text.
  - **Memory storage reconciliation (schema v17 `projected_at`).** `mdkb update`
    projects every DB entry to a markdown file (DB is the source of truth); a
    manually deleted, previously-projected file archives its entry.
  - **Setup drift detection & prior-mining visibility in `mdkb stats`** — warns on
    duplicated / missing (Stop) hook registrations; shows mining enabled/disabled
    with reason using the effective merged (daemon.toml < repo) priors.
  - **Housekeeping & log rotation.** `mdkb update` removes vestigial artifacts
    (0-byte `mdkb.sqlite`, legacy `code-index/`, writer-less `reindex-queue.jsonl`)
    and warns on dead `[models]` embedding keys (now removed); `hook-events.jsonl`
    / `hook-slow.jsonl` are halved (newest kept) past 1 MiB.

- **Memory graph — typed edges between memory entries (schema v14).** A new
  `memory_edges` table records typed relations (`supports`, `contradicts`,
  `supersedes`, `derived_from`, `relates_to`) from a memory entry to another
  memory or a document. Targets are dangling-tolerant and resolved at query time,
  mirroring the document graph.
  - `memory_write` accepts `relates=[{relation, target, target_kind}]` (max 10) —
    entry and edges are written in one transaction. A `supersedes` memory edge
    keeps the `superseded_by` scalar and `superseded` status in lockstep (single
    write path).
  - `graph(entity, direction="links"|"backlinks", scope="memory")` traverses the
    memory graph. CLI: `mdkb memory link <id> <relation> <target> [--doc]
    [--agent <name>]`; invalid relations are rejected listing the closed set.
  - `memory_write(on_conflict="contradicts")` records a near-duplicate conflict as
    a `contradicts` edge to the similar entry instead of rejecting the write
    (default behavior unchanged when omitted).
  - **Authorship provenance** — `memory_write` records the authoring session and
    optional `agent`; both surface in `get(id)`.
- **Post-recall 1-hop expansion.** A recalled entry's active memory neighbors are
  surfaced (≤2 seeds, ≤3 neighbors), annotated `(via <relation>)`;
  superseded/expired/dangling neighbors are excluded.
- **`[STALE-DEP]` marker.** At injection time (warmup + recall), an entry whose
  `derived_from`/`supports` dependency is superseded or net-refuted is prefixed
  `[STALE-DEP]`. Read-only — it never mutates stored confidence.
- **AI-distilled behavioral priors (schema v13).** Replaces the mechanical
  tool-chain "prior" miner with a recurrence-gated, trigger-matched subsystem
  owned by mdkb. New `prior_candidates`/`prior_clusters` tables; a write-time gate
  rejects mechanical tool-chain priors.
  - **Mining (opt-in, kill-switched).** A new `Stop` hook feeds the end-of-episode
    transcript to a cheap no-LLM candidate detector (error→fix→clean, or explicit
    user correction). Only flagged episodes are distilled — by an external agent
    CLI (`[priors].distiller_program`, prompt piped on stdin, run off the hook
    budget in a detached task) into strict JSON (falsifiable ≤160-char lesson,
    machine-matchable trigger, scope, evidence). Untrusted transcript evidence is
    secret-redacted before it leaves the process. Off by default
    (`[priors].mining_enabled=false`, and inert without a configured distiller).
  - **Recurrence gate + promotion.** A distilled prior is clustered by canonical
    trigger key; a cluster promotes to a `memory_entries` prior only after
    recurring across ≥2 distinct sessions. Injection scoring
    (`recurrence × freshness × belief`) is decoupled from per-entry source
    authority, so an honestly-tagged AI prior can finally surface.
  - **Trigger-matched injection.** Promoted priors surface at PreToolUse
    (tool / path-glob / command match) and UserPromptSubmit (prompt match) — never
    unconditionally at SessionStart. `[priors].injection_enabled` (on) and
    `max_injected_per_hook` (1) bound the per-turn cost; the PreToolUse path reads
    only an already-warm context so it never opens a DB on the hot path.

### Fixed

- **Data-safety guards on auto-run paths** (from the 2026-07-06 multi-agent
  review + GPT-5.5 triage — none of these had shipped):
  - **Bulk-archive circuit breaker.** `mdkb update`'s memory→file sync refuses to
    archive when more than 10 previously-projected entry files vanish in one pass
    (a `git checkout`/`stash`/`clean` or backup restore, not deliberate deletion),
    warning loudly instead of silently retiring the corpus. `mdkb update` now also
    prints archived / archive-skipped counts in its default output.
  - **Nested-store validation.** The `.mdkb` walker boundary requires an
    *initialized* store (`.mdkb/index.sqlite`); a bare or half-created `.mdkb`
    directory no longer makes the parent hard-delete every previously-indexed doc
    under it.
  - **`compact --prune-sessions --export` never loses the only copy.** A transcript
    whose content body is missing is skipped (not deleted), and export filenames
    are collision-proof (`{stem}-{id}-{hash8}.md`) so two sessions can't overwrite
    each other's export.
  - **Overflow-checked retention.** `--older-than` parsing and the prune cutoff use
    checked arithmetic, so an oversized value is rejected rather than wrapping to a
    cutoff that over-deletes.
  - **Backfill no longer stalls on a poison row.** A single un-embeddable memory
    entry is skipped; only a cold model pauses the batch (previously one bad row
    starved every later entry).
- **`[search] auto_embed_memory`** (default on) — kill switch for embed-on-write on
  `memory add` / `memory import`; off leaves entries pending for `mdkb update`.
- **Performance.** Auto-embed / memory backfill / session indexing run off the
  async runtime via `spawn_blocking` (no longer holding the repo lock across ONNX
  work); the doc-embed pass replaces a per-document `has_embedding` query with one
  set lookup; new partial index `idx_sessions_agent` for the per-hook session
  lookup.

## 3.4.0 (2026-06-09)

### Added

- **Knowledge graph — typed edges from frontmatter + wikilinks.** A new `edges`
  table (schema v11) records typed relations from a document to entity slugs,
  derived during indexing from allowlisted frontmatter keys (strong) and body
  `[[wikilinks]]` (soft). Targets are stored verbatim and resolved to documents
  at query time, so cross-document links survive regardless of indexing order
  (dangling edges resolve once their target is indexed). Re-indexing replaces a
  document's outgoing edges idempotently.
  - CLI: `mdkb graph links <entity> [--relation T]` (outgoing),
    `mdkb graph backlinks <entity> [--relation T]` (incoming),
    `mdkb graph neighbors <entity> [--relation T] [--depth N]` (adjacent,
    undirected), and `mdkb graph path <a> <b> [--max-hops N]` (shortest path) —
    all honoring `--format json|text|csv|markdown`.
  - MCP: a single consolidated `graph` tool with
    `direction=links|backlinks|neighbors|path` (mirrors `code_graph`), keeping
    the always-on tool surface minimal.
  - Config: a `[graph]` section (`enabled`, `frontmatter_relations`,
    `include_wikilinks`) written into the default template by `init`.
- **`mdkb update --force`** reindexes every file regardless of modification time.
  Without it, `update` is mtime-incremental, so config changes (e.g.
  `graph.frontmatter_relations` or `include_wikilinks`) only reach documents
  that are subsequently edited; `--force` reapplies them to the whole index.

### Fixed

- **CLI memory-write upserts instead of failing** — `mdkb memory add` (and the
  bridge `memory-write` path) now updates an existing entry in place — saving a
  revision — rather than crashing with `UNIQUE constraint failed:
  memory_entries.id`. Matches the MCP `memory_write` behavior.
- **`setup hooks` replaces legacy untagged entries** — re-running hook setup
  removes prior `mdkb hook <event>` entries that predate the `_managedBy: mdkb`
  tag, instead of leaving a duplicate that fires mdkb twice.
- **`setup mcp claude` heals stale registrations** — it now removes an existing
  registration at the target scope before adding, so a legacy `mdkb serve`
  command is replaced by the `mdkb mcp` proxy instead of being reported as
  "already registered" and left untouched.

## 3.3.0 (2026-06-07)

### Added

- **PreToolUse redirects Bash `grep`/`rg` to mdkb** — the hook now intercepts
  `Bash` commands, not just the rarely-used `Grep` tool. Agents search code
  through `Bash` far more than the `Grep` tool, so this is where the redirect
  actually reaches them. The shell command is parsed quote-aware; only the
  source stage of a pipeline is considered (a `… | grep x` stdout filter is
  left alone), and bare `grep PATTERN` (stdin), single-file greps, and
  regex/alternation patterns are left to grep. `sh|bash|zsh -lc "…"` wrappers
  (used by Codex) are unwrapped first.
- **Redirect conversion telemetry** — a new `mdkb_invocation` hook outcome
  records when a `Bash` command actually runs mdkb. `mdkb stats` shows a `Conv`
  column per hook event so the PreToolUse redirect's hit rate is measurable.

### Changed

- **Slimmed MCP server instructions** — dropped the code-search syntax table
  that duplicated the tool JSON Schema. Kept the semantic-vs-literal routing,
  memory guidance, and reminder protocol. Fewer always-injected tokens per
  session.

## 3.2.0 (2026-06-03)

### Added

- **Automatic incremental `auto_vacuum` reclaim** — the maintenance pass now
  runs incremental `auto_vacuum` so `index.sqlite` releases freed pages instead
  of growing unbounded after deletes/reindexes.
- **Git worktrees share the main repo's `.mdkb/`** — secondary worktrees no
  longer get an isolated database; memory and index written in one worktree are
  visible from the others.
- **`symbols_in_file` and `symbol_at_position` MCP tools** — list the symbols
  defined in a file, or resolve the symbol at a `line:col` position.

### Changed

- **MCP registration routes through the daemon proxy** — `mdkb setup` now
  registers the server via the daemon proxy command instead of a direct binary
  invocation.
- **Instructions clarify mdkb is semantic search, not literal matching** — the
  server instructions and tool text state that exact strings, substrings, and
  regex belong to Grep, not mdkb.

### Fixed

- **Watcher bootstraps code index on startup** — in daemon/global mode, the
  file watcher now runs a full `index_directory` when `code.sqlite` is empty
  (file_count == 0). Previously, repos opened via the daemon had 0 symbols
  until a file change triggered the incremental watcher. Mirrors the standalone
  startup task behavior.
- **Standalone startup respects `code.enabled`** — the background code reindex
  task now checks `code.enabled` before indexing. Previously it always ran,
  ignoring the config flag that the CLI `init` path honored.
- **Watcher receives `respect_gitignore` config** — the file watcher now
  creates its `PipelineConfig` with the correct `respect_gitignore` setting
  from `code.indexing.respect_gitignore`, instead of relying on the default.
- **Hidden directories excluded from code index** — directories starting with
  `.` (`.git/`, `.vscode/`, `.idea/`, etc.) are now skipped by the file walker.
  Previously `hidden(false)` let the walker enter hidden directories, relying
  on `.gitignore` to filter them — which failed when `respect_gitignore` was
  false. Use `# mdkb:index` in `.gitignore` to force-include files inside
  hidden directories.
- **`_root` collection no longer recursively duplicates docs** — indexing the
  repo root stopped re-adding the same documents on each pass.
- **Duplicate `rel_path` entries prevented in the code index** — plus
  previously-silent repair failures are now surfaced.
- **Race between `ensure_context` and the `doc_reindex_active` flag eliminated.**

## 3.1.0 (2026-05-01)

### Added

- **Automatic code.sqlite repair on open** — idempotent integrity checks run
  every time the code index is opened. Detects and fixes: NULL kind rows,
  orphaned symbols (missing file), orphaned relationships (missing file or
  symbol), and desynced FTS5 index. Fixes are reported to stderr; clean
  databases have zero overhead beyond the integrity check queries.
  New module: `code::storage::repair`.

### Changed

- **Stats report opens code.sqlite read-write** — enables autofix on
  `mdkb stats` instead of silently logging a WARN nobody reads. Falls back
  to read-only if write access is unavailable.

## 3.0.3 (2026-04-26)

### Added

- **`handoff` entry type** — session handover entries for agent context
  transfer. No default TTL (use `--ttl` to set one). Handoffs are project
  history — confidence decay handles relevance naturally.
- **`--file <path>` on `memory add`** — reads content from a file instead
  of `--content` or stdin. Saves token overhead when agents write handoffs
  to the filesystem and want to register them in mdkb. Mutually exclusive
  with `--content`.
- **`source_file` on MCP `memory_write` / `memory_write_batch`** — server-side
  file read. The model passes only the path; mdkb reads the content. Mutually
  exclusive with `content`.
- **Source path metadata** — the file path is persisted in `source_path` and
  displayed in `memory show` (text and markdown formats).
- **Memory subcommand aliases** — hidden aliases for commands models commonly
  guess: `write`/`create` → `add`, `get` → `show`, `delete` → `rm`.

## 3.0.2 (2026-04-26)

### Fixed

- **`setup mcp claude/codex` registers `mdkb mcp` instead of `mdkb serve`** —
  the old registration spawned standalone server processes per Claude session,
  bypassing the singleton daemon. Now correctly proxies through the daemon.

## 3.0.0 (2026-04-25)

### Breaking Changes

- **Hook dispatch via daemon IPC** — all hook events (`session-start`,
  `user-prompt-submit`, `pre-tool-use`, `post-tool-use`) now dispatch
  through the daemon's Unix socket instead of running in-process. The CLI
  `mdkb hook <event>` connects to the daemon, auto-spawning it if needed,
  with exponential backoff. Falls back to in-process (`MDKB_NO_DAEMON=1`)
  if the daemon is unreachable.
- **`reindex-queue.jsonl` removed** — `PostToolUse` no longer appends to a
  file. Edited paths are sent directly to the daemon's watcher channel via
  `reindex_tx`, triggering immediate reindex. Any tooling that read or
  monitored `reindex-queue.jsonl` must be updated.
- **`hooks.rs` deleted** — the monolithic hook handler is replaced by
  `hook_logic.rs` (pure functions) + `hook_client.rs` (IPC client) +
  `dispatch.rs` (4 hook methods in the daemon dispatch layer).

### Added

- **Hook event logging** — every hook invocation is logged to
  `.mdkb/hook-events.jsonl` with event name, outcome (ok/empty/error),
  elapsed time, and latency budget. Replaces the old `hook-slow.jsonl`
  which only logged overruns.
- **Per-event configurable thresholds** — `latency_budget_ms` can now be
  set per event type in `[hooks]` config.
- **`mdkb hook` one-shot IPC client** — `mdkb hook <event>` reads stdin,
  sends a JSON-RPC call to the daemon socket, and prints the response. No
  in-process DB access on the primary path.
- **Agent DX CLI Scale** — imported evaluation rubric at
  `.agents/skills/agent-dx-cli-scale/SKILL.md` for scoring CLI
  agent-friendliness.

### Changed

- **`spawn_blocking` for CPU-bound hook work** — FTS tokenization and
  pattern classification moved to `tokio::task::spawn_blocking` to avoid
  blocking the async runtime.
- **Safe JSON serialization** — hook responses use checked serialization
  with fallback to `{}` on failure, preventing malformed output.

## 2.2.1 (2026-04-21)

### Changed

- **Silent hooks** — hooks that have nothing to report now produce no stdout
  output instead of an empty JSON object. Reduces noise for the host CLI.
- **`emit_response` graceful error handling** — serialization failures are
  logged to stderr instead of emitting a fallback `{}`.
- **File watcher ready signal** — `run_file_watcher_inner` accepts an optional
  `Notify` to signal readiness, replacing sleep-based synchronization in tests.
- **Watcher test determinism** — `e2e_daemon_watcher` uses `Notify`-based
  readiness instead of `sleep(500ms)`, eliminating flaky timing.

## 2.2.0 (2026-04-20)

### Added

- **`prior` entry type** — behavioral pattern entries for external analyzers
  (e.g., HUD stop hooks). 30-day default TTL. Excluded from all default
  searches; query with `--entry-type prior` or `search(scope="memory",
  entry_type="prior")` via MCP.
- **`mdkb cheatsheet`** — AI-friendly compact command reference with full
  binary paths via `current_exe()`. Eliminates trial-and-error CLI discovery.
- **`--entry-type` filter on `mdkb search`** — filter memory searches by
  entry type (topic, problem, decision, reminder, prior).
- **PreToolUse Grep interceptor suggests CLI commands** — works without MCP.
  Classifies Grep patterns (pure identifiers, definition searches, callsite
  patterns) and suggests `mdkb search`/`mdkb code` via Bash.
- **`mdkb setup remove`** — CLI removal of MCP and hook registrations.
  `setup remove mcp claude|codex`, `setup remove hooks claude|codex`,
  `setup remove claude --scope local|user` (MCP + hooks in one shot).

### Changed

- **Hook suggestions use CLI instead of MCP tool names** — `current_exe()`
  resolves the binary path dynamically. No daemon socket check required.
- **Optimized injected text** — ~185 fewer tokens per turn across
  BASE_INSTRUCTIONS, PreToolUse messages, and SessionStart tip.
- **SessionStart tip points to `mdkb cheatsheet`** instead of inline syntax.
- **Removed duplicated entry_type/ttl docs from BASE_INSTRUCTIONS** — already
  in JSON Schema `/// doc` comments.

## 2.0.0 (2026-04-18)

### Breaking Changes

- **`mdkb status` removed** — use `mdkb stats` instead. The old command
  prints an "unknown command" error from clap. No alias is provided.
- **`mdkb stats` signature changed** — `--sessions` / `--aggregate` flags
  removed. The command now accepts `--no-color` and `--format json|text`.

### Added

- **`mdkb memory export`** — dumps all memory entries to a folder of
  per-entry `.md` files with YAML frontmatter. Options: `--dir`,
  `--include-expired`, `--overwrite`, `--dry-run`. Default directory:
  `.mdkb/memory/entries/`.
- **`mdkb memory import` (directory mode)** — auto-detects whether the
  path argument is a directory; if so, scans `*.md` files and imports
  via the new `memory_file` YAML parser. JSON file path unchanged.
- **`mdkb stats` unified ASCII dashboard** — replaces both `mdkb status`
  and the old session-only `mdkb stats`. Sections: index health
  (document/memory counts, free-page ratio), collections table, memory
  bar-by-type with reminder due/upcoming counts, code symbols per language
  (when code.sqlite is present), session totals with top-tools bar chart,
  hooks slow events and reindex-queue pending count. Uses box-drawing
  characters and block-element bar charts. `--format json` serializes
  the full `StatsReport` struct.

### Internals

- `src/cli/memory_file.rs` — hand-written YAML frontmatter serializer
  and `gray_matter`-based parser for `MemoryEntry`. Round-trip preserves
  all authored fields; derived counters (`access_count`, `last_accessed`,
  `confirmations`) are reset on import.
- `src/cli/stats_render.rs` — `bar`, `sparkline`, `frame` ASCII primitives
  and a hand-rolled ANSI `style` module (no `owo-colors` dependency).
- `src/cli/stats_report.rs` — `collect_report` aggregator.
- `src/cli/stats_render_report.rs` — ASCII renderer for `StatsReport`.
- `src/store/memory.rs` — added `list_entries_all` (no expiry filter,
  used by export to include expired entries when requested).

## 1.5.0 (2026-04-18)

### Added

- **Lifecycle hook dispatcher** — `mdkb hook <event>` handles `session-start`, `user-prompt-submit`, and `post-tool-use`. SessionStart injects a `## mdkb memory warmup` block; UserPromptSubmit injects `## mdkb: relevant context` via an FTS5 OR query over the prompt tokens; PostToolUse appends edited paths to `.mdkb/reindex-queue.jsonl` so the next `mdkb update` pass picks them up. See `docs/hooks.md`.
- **Hook registration commands** — `mdkb setup hooks claude --scope local|user [--disable …] [--dry-run]` writes `.claude/settings.local.json` or `~/.claude/settings.json`; `mdkb setup hooks codex` writes `~/.codex/hooks.json`. Idempotent re-runs; preserves unrelated settings.
- **`mdkb setup mcp codex`** — registers mdkb in `~/.codex/config.toml` under `[mcp_servers.mdkb]` using `toml_edit` to preserve comments and formatting. Dry-run prints the merged config without writing. (#023)
- **`.mdkbignore-hooks` opt-out marker** — empty file at repo root suppresses all three hooks; ancestor lookup stops at `$HOME`.
- **`[hooks]` config section** — per-event enable toggles, `recall_limit`, `latency_budget_ms`, `min_recall_score`. Slow hooks log to `.mdkb/hook-slow.jsonl`.
- **`usage` MCP tool** — reports per-tool call counts and recent activity for the current session. (#019)
- **Memory confidence & access counters** — search ranks memories by `access_count × recency` as a third RRF signal (weight configurable via `[search.memory] access_recency_weight`); `get` is the only writer of `access_count` so `search` stays SELECT-idempotent. (#025–#027)
- **File token estimates in code index** — `files.token_count` populated from `cl100k_base`; surfaced via `search(scope="symbols")` and `get`. (#020)
- **Auto-optimize on drift** — startup VACUUM when free-page ratio > threshold, runtime `PRAGMA optimize` every `db.optimize_interval_calls` tool calls. (#028)

### Changed

- **E2E hook contract covered** — `tests/e2e_hooks.rs` spawns the real binary and verifies SessionStart warmup, UserPromptSubmit recall, PostToolUse queue, and `.mdkbignore-hooks` suppression. (#021-0ad9)

## 1.4.0 (2026-04-17)

### Added

- **Reminder entry type** — `memory_write(entry_type="reminder", due_in=<seconds>)` creates a time-bound memory entry. Future reminders (`due_at > now`) are hidden from `memory_list`, `search(scope="memory")`, and active-count stats. Once `due_at <= now`, the reminder surfaces in the warmup index with a `[reminder:DUE] {id}: {title}` prefix so the MCP client sees it on the next turn.
- **Reminder confirmation protocol in BASE_INSTRUCTIONS** — CC is instructed to ask the user before deleting a due reminder and to re-ask on ambiguous replies, preventing accidental deletion from incidental topic mentions.
- **Schema migration v9 → v10** — adds `due_at INTEGER NULL` column; non-destructive on existing DBs.
- **CLI support** — `mdkb memory add <id> --entry-type reminder --due-in <seconds> --title "..." --content "..."`.
- **Input hardening** — memory titles and tags now reject newlines and control characters to prevent prompt-injection via instruction-surface fields.

### Changed

- **BASE_INSTRUCTIONS rewritten** — tighter wording (token budget < 600 for empty-index), English-only affirmatives section, documented `memory_write` signature inline, `code_graph` direction values listed, Reminders protocol added as a numbered 4-step flow.

## 1.2.0 (2026-04-08)

### Fixed

- **Code index: duplicate symbols crash** — `UNIQUE constraint failed` on JS/TS files with same-line redeclarations (e.g., minified code, `var` re-declarations). Changed `INSERT` to `INSERT OR REPLACE` in symbol storage.
- **Startup reindex silent failure** — the above crash was logged but silently ignored, leaving the code index stale after server restart.

### Added

- **Shebang language detection** — extensionless scripts with shebangs (`#!/usr/bin/env node`, `#!/usr/bin/python3`, etc.) are now detected and indexed as their respective languages.
- **Semantic code search enabled by default** — `code.semantic_search.enabled` defaults to `true`. Embedding-based code search (`scope="code"`) works out of the box.

### Changed

- **MCP instructions rewritten** — removed "always use mdkb search before Grep" rule. New instructions clarify when to use mdkb (semantic queries, code_graph, memory) vs Grep (exact pattern matching). `code_graph` promoted to a primary workflow step.
