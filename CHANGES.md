# Changelog

## Unreleased

### Fixed

- **A change the watcher dropped can no longer sit unrecovered.** The watcher
  drops events when its 100-slot channel fills and sets a flag that schedules a
  full rescan — measured on 2026-09-17, a 500-file burst produced 793 failed
  `try_send` calls, and the rescan did cover every file. But the flag was only
  read in the flush arm, which runs only when a batch, a doc update or a memory
  sync is already pending. A burst whose delivered events all route nowhere —
  the shape of a `cargo build`: hundreds of artifacts, not one of them a source
  file — left the loop blocked with the flag set and nothing scheduled to read
  it, so an edit dropped in that same burst stayed out of the index until some
  later routed change happened to arrive. The flag is now read on every
  delivered event, and a drop alone is enough to arm the flush. Separately, the
  recovery rescan used to do nothing at all, and say nothing, when a corrupt
  database had closed the index connection — the flag that scheduled it was
  already spent, so that gap was permanent. It reopens the index first.

  Related, and worth knowing before diagnosing this again: `code.sqlite` is in
  WAL mode, so under a daemon that holds the connection open its mtime sits at
  the last checkpoint. `ls -l` on it can read days old over a perfectly current
  index. `mdkb stats` reports the real state.

- **`code impact` no longer walks through a call it could not place.** An
  arrival at tier 7 means the caller wrote this name and no rule could say it
  meant this symbol. The walk expanded it anyway, so everything behind a bare
  name match was reported as impacted — for a common name, most of the index.
  Reported and expandable are now two states: every arrival is reported, only
  placed ones are walked through, and a symbol first reached ambiguously is
  still expanded if a placed call reaches it later (with the budget of the path
  that qualified it, not of the one that named it). The answer splits into the
  actionable radius and an ambiguous frontier, and carries a coverage note
  saying how many arrivals the walk stopped at — a truncated radius used to
  read as exhaustive. `mdkb code impact --format json` is one document with
  `targets`, `ambiguous` and `stopped_arrivals`.

- **A behavioural prior stops being injected when its lesson is gone or
  contradicted.** The 30-day TTL lives on the memory entry, never on the
  cluster, and the injection path read `prior_clusters` alone: a prior whose
  entry had lapsed, been archived or been deleted kept firing a lesson that no
  `mdkb memory` command would serve any more — the reader could not even look
  it up. The promoted list now joins the entry and applies the same liveness
  rule as every other read, so an expired prior leaves the injection path the
  moment `mdkb update` archives it, and comes back if the entry does.
  Separately, the `refuted` state the schema always named and nothing ever
  wrote is now written: after at least three settlements, a cluster the errors
  contradict more often than a person confirms is demoted out of injection, and
  a human verdict on the projection is what brings it back.

- **Every memory write path runs the same duplicate check.** The near-duplicate
  gate lived inside `memory add` and the MCP write, and it only ran when the
  caller happened to supply an embedding. Every import — a directory of files,
  a single file restore, a JSON bundle, and the git sync — went straight to the
  insert, so a file could restore an entry the interactive path would have
  refused, and the same memory could sit in a store twice under two ids. The
  check is now one function with one named threshold
  (`NEAR_DUPLICATE_DISTANCE`), and a write that brought no vector gets one.
  It has two arms: an identical title, which needs no model and so still works
  on a cold store, and meaning, which catches a reworded restatement
  (measured: cosine 0.99 refused, 0.917 not — the bar is near-verbatim). A git
  sync never fails the whole pass on one repeated file; it reports
  `duplicates_skipped` and leaves the file alone.

- **`--entry-type` narrows memory search instead of replacing it.** Asking
  `search --scope memory --entry-type decision` dropped to a token-AND
  full-text query with no vector leg, so a paraphrase the same search recalled
  perfectly without the flag returned nothing with it. The filter now runs
  inside both legs of the one hybrid search, and it applies to the memory half
  of an unscoped search too, where it used to be discarded in silence.
  Not-yet-due reminders are hidden from a typed search, as they already were
  from every other search and listing.
- **`search --format json` emits one JSON document.** Searching the default
  scope printed two arrays under `## Documents` and `## Memory Entries`
  markdown headings, so `mdkb search q --format json | jq` failed on the first
  line and every caller fell back to parsing prose. The combined scope now
  prints a single object with `documents` and `memory` keys. `--format csv`
  no longer carries markdown headings either; its two tables stay told apart
  by their header rows.
- **`get` accepts the paths `graph` accepts and the one `search` prints.**
  `search` displays a hit as `collection:path`, and pasting that back into
  `get` failed, as did `stories/archive/x.md` and a path without `.md` — while
  `graph links` resolved all of them. Both commands now share one resolver, so
  a path is a path whichever command reads it.
- **`graph dangling -c <collection>` scopes the report.** On a store that
  indexes session transcripts alongside documents, the real gaps in one
  collection were buried under another's noise. The filter applies to the
  edge's source document.

## 3.9.0 (2026-09-14)

Two questions the index could not answer before: *what does this repository say
twice?* and *what changes together without any edge saying why?*

Read the duplication report knowing what it is worth. Measured on this
repository, 66% of the lines it claims sit at exactly the threshold, and a
hand-classified sample of that bucket was 5 false positives out of 8. The
trustworthy core is the clusters at 0–3 bits apart. The numbers are under *The
structural threshold* below, because a reader who does not have them will
over-trust the headline.

This release also resolves the concrete defects tracked in issues #5–#11 and
adds the missing collection update command. CI now includes a native
`windows-latest` test job instead of relying only on cross-compilation and
portable unit tests.

### Added

- **Session start advertises the power features it can activate.** Every
  initialized repository receives one compact instruction for `* <prompt>`
  recall and `mdkb cheatsheet`, even before the memory index has entries. The
  cheatsheet now includes collection updates and the developer telemetry
  commands that were previously absent.
- **Privacy-minimized developer telemetry profile.** `mdkb setup developer`
  enables repository-local recall measurements with bounded retention (30 days
  by default), preserves unrelated config and comments, and creates a private
  256-bit key at `.mdkb/telemetry.key`. Query correlation now uses keyed
  HMAC-SHA-256 instead of an unsalted digest; prompt text is still never stored.
  `mdkb metrics status` exposes activation, retention, key presence, and event
  count without exposing the key, while `mdkb metrics purge --yes` routes
  deletion through the daemon's single-writer protocol. End-user defaults and
  the prompt-recall sigil remain unchanged.

- **Native lifecycle hooks over HTTP and HTTPS.** Both network transports now
  expose `POST /hook/{method}` through the same dispatcher, repository registry,
  and shutdown work gate as Unix hook IPC. Claude Code setup accepts
  `--http-url`; UserPromptSubmit, PreToolUse, PostToolUse, and Stop become native
  HTTP handlers with bearer authentication, while SessionStart remains a
  command hook because that event does not support HTTP handlers.
- **Complete MCP tool annotations.** All 12 advertised tools declare read-only,
  destructive, idempotent, and open-world hints so an agent can reason about
  side effects before calling them.
- **`mdkb dup` reports duplicated code**, and `search scope="duplicates"` asks
  the same question over MCP. One handler serves both surfaces, so the CLI and
  the MCP server cannot disagree about the same repository, and there is no
  thirteenth MCP tool: the audit is a scope on the search tool that already
  exists. `--file` scopes the sweep, `--min-nodes` sets how big a body must be
  to be worth comparing, `--threshold` overrides the semantic floor.
- **Review mode: `mdkb dup --since <ref>`**, and `since` on the MCP search.
  The whole-repository sweep is the audit; the daily question is narrower —
  *what did **this change** duplicate against code that already existed?* The
  distinction that makes it work is where the narrowing happens. `--file`
  narrows the **candidates**, deciding what gets fingerprinted at all. Review
  mode must not, because the code your change duplicated is by definition code
  your change did not touch, so narrowing the candidates first deletes the very
  symbols the answer is made of. The whole index is fingerprinted and clustered
  as always, and only the **report** is narrowed, to clusters with at least one
  member among the changed files. On this repository a sweep of 690 clusters
  came back as 230 against an uncommitted working tree, and the unchanged twin
  is still named in each one — which is the finding. (The exact pair moves with
  the tree; what does not is that the twin survives the narrowing.) Note that a
  worktree shares its main worktree's store by design, so `--since` compares
  the tree the store is anchored to, not the checkout you typed the command in.
  Two behaviours a test pins: an unresolvable ref is an
  error and never an empty report, because an empty report reads as "your change
  duplicated nothing"; and untracked files count, because `git diff --name-only`
  lists tracked work only while a brand-new file duplicating existing code is
  the archetypal finding.
- **`mdkb coupling` reports files that change together with no edge between
  them.** Two files with shared commits in git history and no
  `Calls`/`Uses`/`Expands`/`Implements` edge in the code graph — an implicit
  contract, a config kept in two places, a test that knows the implementation.
  No new column and no new table: `git log --name-only` joined against the index
  that already exists. Defaults are 5 shared commits over 12 months; `--ref`,
  `--since` and `--min-cochanges` override them. Two filters decide whether the
  output is a report or noise, both added because the unfiltered run produced
  noise: only files the index parsed are paired, since a path with no symbols
  can carry no edge by construction (without it, 51 of 61 pairs here were
  `Cargo.lock` ↔ `Cargo.toml` and friends); and commits touching over 100 files
  are dropped whole rather than truncated, because a repo-wide sweep is one
  event and not evidence about any two of its files — it is also where the O(n²)
  pair expansion would go, a 500-file commit alone contributing ~124,750 pairs.
  15 pairs on this repository, in 0.11s of CPU after the index is open. CLI
  only; there is no MCP scope for it yet.

- **Store namespaces, so a consumer's test suite cannot pollute the store its
  sessions warm up from.** `MDKB_NAMESPACE=<name>` points a process at
  `.mdkb/namespaces/<name>/` — its own index, projection and locks — which no
  read of the default store can see and the store-level `.gitignore` never
  commits. A process carrying a test runner's marker (`NODE_TEST_CONTEXT`,
  `VITEST`, `JEST_WORKER_ID`, `PYTEST_CURRENT_TEST`) is routed to the `test`
  namespace without asking; `MDKB_NAMESPACE=default` opts back out. Namespaced
  processes never use the daemon, and the daemon refuses to start in one. The
  file watcher reconciles the projection of the store it opened, namespaced or
  not. Motivated by three `wiz-bridge-test-<timestamp>` entries and a `retest-001`
  found active in a live store.

- **Warmup eligibility is an allow-list.** The pool admits topics, problems,
  decisions and priors (the last only for the reserved confidence-gated slot).
  Handoffs, reminders and net-refuted entries (`corrections > confirmations`)
  never compete for a slot; the newest handoff and due reminders still arrive
  through their own queries.

- **`mdkb collection update <name> [--path P] [--pattern G]`** changes a
  collection in place. Changing a pattern used to mean `collection remove` +
  `collection add`, which drops every indexed document and forces a full
  re-embed of files whose content never changed. The update keeps `created_at`
  and `source`, validates the new path and pattern before writing, and refuses
  a call that names neither. The saving applies to `--pattern`: a document is
  keyed by its path relative to the collection's base, so `--path` moves the
  base out from under the existing rows and the new one is indexed — and
  embedded — from scratch. *(#11, reported by Stefano Straus (@sstraus))*

- **[docs/graph.md](docs/graph.md)** — how graph edges are created, how a
  reference resolves (and what is tried before it is called dangling), what each
  query answers, and when to reach for the graph instead of search.

### Changed

- **Prompt recall is gated by the final hybrid score.** Memory recall now returns
  its BM25/vector relevance fused with confidence, and `min_recall_score`
  filters that result instead of filtering on age-derived confidence alone.
  Topics, problems, and decisions no longer decay merely because they are old;
  reminders, priors, and handoffs retain lifecycle decay.
- **Call resolution understands receiver types.** Method calls retain their
  written qualifier and an inferred receiver type; the resolution cascade and
  coupling audit share that evidence instead of treating every same-named
  symbol as connected.
- **rmcp upgraded from 0.14 to 3.3.** The HTTP/HTTPS MCP surface keeps the same
  12-tool semantic contract while adopting current JSON Schema conventions.
  Bearer auth and constant-time token checks protect both MCP and HTTP hook
  routes; MCP additionally validates the Host header against an allow-list.
  RUSTSEC-2026-0189 is no longer present.
- **Duplication clusters use complete linkage.** Similarity is not transitive.
  Union-find closed it transitively anyway, and on this repository that produced
  one component of 3209 symbols across 241 modules whose widest pair sat 47 bits
  apart against a threshold of 12 — half the report was noise. A member now
  joins a group only if it is admissible with **every** member already in it, so
  the group's diameter is bounded by the threshold by construction rather than
  by hope. Seeding is ordered by simhash and not by row id, so editing a file
  above a symbol cannot renumber the groups an ignore-list is keyed on. 21
  clusters became 758; the widest pair went from 47 bits to 12; the largest
  cluster from 3209 members to 26. 19 of the 21 pre-existing cluster hashes were
  unchanged, so accepted-duplication decisions survived the fix — the two that
  moved are the two that were out of contract.
- **The structural threshold is 6 bits, was 12.** The clustering fix bounded the
  groups but not the cut. At 12 bits, 497 of 706 clusters sat at exactly 12 and
  another 105 at 11 — 85% of the mass pressed against the boundary. A threshold
  that finds real duplication has its mass near 0; one whose mass sits on its
  own cut is reporting whatever fits. Two competing explanations were tested and
  one was ruled out: the share at the cut held at every body size (70% for
  bodies over 100 AST nodes, 63% over 200), so it was the threshold and not an
  entropy floor on small bodies. Reported lines halve, 38931 to 19200.

  The distribution did **not** come off the boundary. One sweep of this
  repository, 690 clusters claiming 22727 lines, broken out by the distance
  that admitted each one (`mdkb dup --format json` is where these come from):

  | bits apart | clusters | lines claimed | share of lines |
  |---:|---:|---:|---:|
  | 0 | 40 | 922 | 4.1% |
  | 1–3 | 38 | 854 | 3.8% |
  | 4 | 57 | 1397 | 6.1% |
  | 5 | 124 | 3619 | 15.9% |
  | **6 (the cut)** | **371** | **15034** | **66.2%** |
  | semantic (cosine, no distance) | 60 | 901 | 4.0% |

  Halving the threshold moved the cliff; it did not remove it. The shape belongs
  to simhash over shingles, not to the number 12, so 6 is an improvement and not
  a settled answer. Hand-classifying a random sample says the same: 8 clusters
  from the 6-bit bucket gave 1 clearly worth extracting, 2 true but marginal and
  5 false positives, while 6 drawn from the 0-bit bucket were 6 for 6 genuine.
  Treat the ≤3-bit clusters as the report's core.
- **`mdkb dup` reports by bucket, not as one number, and ranks bucket-first.**
  The table above was one sweep, printed once, in this file. Every run now
  prints its own: after the headline, a table of clusters and duplicated
  lines per bucket — `0`, `1-3`, `4`, `5`, `at cut`, `cosine` — so a reader
  calibrates without re-deriving the table by hand. `--format json` carries
  the same `buckets` summary next to `evidence.hamming`; CSV is unchanged,
  since it is already one row per member and a repeated bucket column would
  not read as a table there. `rank()` used to order by module spread, which
  put the noisiest cut-band clusters on top — the ones the hand sample called
  wrong two times in three. It orders by bucket first now, then the same
  spread-before-reach tie-breakers, so a single-module 0-bit finding outranks
  a twelve-module 6-bit one instead of losing to it.
- **The semantic pass is opt-in: `mdkb dup --semantic`.** `dup` runs a
  structural pass over fingerprints and a semantic pass that embeds every
  body. On this repository the second one took **817 s of an 818 s run** to
  add 69 of 767 clusters — 4% of the findings for 99.8% of the time. It is now
  off unless asked for: `--semantic` or any `--threshold` override turns it on
  for one run, and `semantic = true` under `[code.duplication]` is the standing
  opt-in (the key was `enabled` earlier in this cycle, and never shipped under
  that name). Over MCP there is no new field — passing `threshold` to
  `search(scope="duplicates")` is the opt-in, which is what the parameter's
  schema now says. A default `dup` never constructs the embedder at all, so a
  machine with no model on disk and no network gets the structural report in
  seconds instead of a download; a model that is configured but will not load
  still degrades the run rather than failing it.
- **The semantic pass costs a third and survives a Ctrl-C.** Measured on 2323
  bodies, cold cache, release build: **479 s and 13.3 GB peak before, 168 s and
  4.6 GB after** — 2.85× less wall time, 2.9× less memory. Three changes, none
  of which touch the structural half:
  - Input is cut at 256 tokens rather than fastembed's 512. Attention is
    quadratic in sequence length, so the bodies long enough to be truncated are
    exactly the expensive ones.
  - Bodies are sorted by length before batching. fastembed pads every batch to
    its longest text, and candidates used to arrive in file order, so each short
    body paid the token cost of its longest neighbour.
  - Vectors are written every 256 bodies instead of once at the end. A pass
    interrupted at minute 12 used to lose all twelve minutes; it now loses the
    current chunk. This is also what caps the memory: the old code held every
    vector until the last body was embedded.

  `dup.sqlite` gains a `meta` table recording `model:dimensions:max_tokens`. A
  vector is only comparable with vectors computed the same way, and nothing else
  would notice a change — the key is the body hash and the body did not change.
  A mismatch drops the embeddings and keeps the simhashes, which cost no model.
  The first run after this release drops the whole embedding cache, because what
  produced it was never recorded.

  Structural cluster hashes are byte-identical before and after, on both corpora
  measured. The `cosine` bucket is not: 53 of 59 clusters survive, 6 drop and 8
  appear, all fourteen scoring between 0.7019 and 0.8140 against a 0.70 floor.
  That is threshold-boundary churn rather than a loss — several are the same
  finding with different membership — and it lands in the bucket the report
  already ranks last.
- **A semantic sweep from the CLI runs at background priority.** It still costs
  the same CPU-seconds; it stops taking a core away from the editor and the
  build that are running next to it. macOS gets `PRIO_DARWIN_BG`, which throttles
  disk I/O along with CPU — right for a sweep that reads the index once and then
  sits in ONNX for minutes; other unix gets nice 19. Never the daemon: it answers
  interactive searches from a long-lived process, and backgrounding that would
  make every search pay for an audit nobody asked it to run.
- **Thirteen language parsers share one parse-and-collect helper, and their
  walks are data.** `mdkb dup` found the same four lines — parse, bail quietly
  on unreadable source, allocate, hand over the root — written out 50 times.
  They are now `CachingParser::collect`. Exactly 13 sites keep the old shape and
  they are all `parse_symbols`, whose walk is a `&mut self` call that cannot be
  made while `self.parser` is borrowed by `collect`: a real borrow conflict, one
  per language. That alone reduced no duplication — 606 clusters before, 606
  after — because the 13 `find_calls_impl` methods got shorter, not fewer.
  `LanguageParser` now takes the walk as data instead: a plain `fn` pointer
  returned by `calls_walk`, `uses_walk`, `defines_walk` and three siblings, with
  the six `find_*` methods sharing one body in the trait. The trait stays object
  safe, which it must, since it is used as `Box<dyn LanguageParser>`.
  Duplication in `src/code/parsing` went from 7143 lines to 7051 at an unchanged
  113 clusters, and the three clusters the work was started for are gone. It
  shrank rather than vanished: a `calls_walk` cluster now stands where
  `find_calls_impl` stood, over the same 11 modules, at 50 duplicated lines
  against 80. Thirteen trait implementations must exist; what a refactor can
  remove is how much each one has to say.

### Fixed

- **Windows test coverage follows the current architecture.** The native CI
  suite keeps Unix-socket-only writer tests on Unix while retaining portable
  singleton coverage on Windows, exercises directory aliases when Developer
  Mode permits them, and compares native paths by component so separators and
  spaces in executable paths do not create false failures. HTTP/HTTPS hook
  dispatch and in-flight work draining now live outside the Unix-socket module,
  so the portable servers compile and run on Windows too.
- **Every memory-writing surface uses one mutation pipeline.** CLI, MCP, batch,
  import, and hook-driven writes now share duplicate admission, embeddings,
  edges, revisions, and Markdown projection behavior.
- **Shutdown and configuration are explicit.** HTTP/HTTPS accept loops stop
  before in-flight work drains, detached daemon stderr goes to its log file,
  and dead configuration knobs have been removed so unknown keys are reported
  by dotted path instead of pretending to work.
- **`mdkb dup` and `mdkb coupling` honour `--format`.** The flag is declared
  `global = true`, so both commands advertised `json`, `csv` and `markdown` in
  their own `--help` — and printed the prose report whatever was asked for. A
  flag a program accepts and then ignores is worse than one it rejects. Both now
  render the findings they already ranked: JSON carries `evidence.hamming`,
  which the prose only spells out in a sentence, so a caller can bucket the
  findings by distance — the thing the section above says a reader has to do.
  CSV writes one row per member under a repeated cluster key, and quotes any
  field holding a comma. `text` and `markdown` stay one surface, because the
  prose report is markdown already. One deliberate exception: a repository with
  no code index prints its prose in *every* format, since
  `{"clusters": 0, "findings": []}` is indistinguishable from "nothing is
  duplicated here" — the one answer an unindexed repository must not be able to
  give.
- **`mdkb init` no longer freezes its defaults into the config it writes.**
  `Config` and all 20 of its sections are `#[serde(default)]`, so an absent key
  takes the value in the code — but `init` was writing every default out as live
  TOML, and a **present** key beats the code. Every store ever created was
  pinned to whatever the defaults were on its creation date, which is why
  lowering the duplication threshold reached nobody until this was found. `init`
  now writes the same defaults commented out: the options stay discoverable, the
  code stays the single place a default lives, and uncommenting a line is what
  it looks like — a deliberate override.

- **An MCP `memory_write` projects its entry to disk, like `mdkb memory add`
  always did.** The MCP path wrote the row and stopped; the `.md` appeared only
  when the daemon's watcher, `mdkb update` or `mdkb memory sync` next ran, so
  the git-tracked projection silently diverged from the index after every MCP
  write. `memory_delete` had the mirror defect: the file stayed in `entries/`
  and the next reconciliation re-imported the deleted entry. Both doors now
  share the CLI's post-write and post-delete steps. The watcher recognises the
  store's own projections by their recorded hash and skips the reconciliation
  pass for them, so a write no longer costs a whole-directory scan.

- **Hook telemetry and the quarantine banner follow the store the process
  opened.** A namespaced hook appended `hook-events.jsonl` to the default
  `.mdkb/`, and a namespaced session reported the default store's quarantine
  markers instead of its own. Nothing escapes a namespace now, telemetry
  included; the code index stays deliberately shared, with the reasoning
  recorded at the site.

- **`mdkb memory prune` no longer archives durable knowledge for want of a
  signal nothing writes.** It selected every active entry whose
  `last_accessed` was older than `--days`, or NULL with an old `created_at`.
  `search` — the dominant read path — deliberately records no access, so
  `last_accessed` was NULL for 41 of 41 entries in a live store, and
  `prune --days 90` would have archived every topic, problem and decision older
  than 90 days regardless of how often it was consulted. Topics, problems and
  decisions are now retired only by an explicit `--ttl`; age applies to
  reminders, priors and handoffs alone, sparing the newest handoff (the next
  session's thread) and reminders not yet due. `--dry-run` lists exactly the
  set a real run archives. Help text and the cheatsheet now describe what the
  command does rather than a guarantee it could not keep.

- **A live-connection lock probe no longer mistakes contention for a real I/O
  error.** The probe keyed on `ErrorKind::WouldBlock`, which is what a
  contended `flock` produces on Unix. Windows returns
  `ERROR_LOCK_VIOLATION` (os error 33), and Rust does not map it to
  `WouldBlock` — so a store that was merely busy read as broken, and heal,
  quarantine and salvage ran against a database nothing was wrong with. The
  check now asks the platform (`fs4::lock_contended_error()`) rather than one
  error kind, and the daemon singleton uses the same predicate so both agree on
  what "already held" means. *(#5, reported by Steve Muchow (@smuchow1962))*

- **`mdkb schema` no longer overflows the main-thread stack in debug builds.**
  Windows gives the main thread 1 MiB, against 8 MiB on Linux and macOS, and a
  debug build of the clap command tree does not fit. All CLI work now runs on a
  thread this program sizes itself (8 MiB), so the stack no longer depends on
  which platform started the process. The report blamed the recursive
  `command_to_json` serializer; measuring it on macOS aarch64 showed
  `Cli::command()` alone needs between 768 and 896 KiB and the serializer adds
  nothing measurable — making the serializer iterative would have fixed nothing.
  *(#6, reported by Steve Muchow (@smuchow1962))*

- **`mdkb hook` runs on platforms without a daemon instead of exiting 1.** Host
  hooks are contractually exit-zero, and the whole hook client was compiled out
  where unix sockets do not exist, so every lifecycle event failed loudly and
  Claude Code surfaced the noise. Only the socket transport is unix-specific
  now: elsewhere the same commands take the in-process route
  `MDKB_NO_DAEMON=1` already selected. `hooks.daemon_required = true` still
  refuses, with a message that says the platform has no daemon rather than
  blaming an environment variable the user did not set. *(#7, reported by Steve
  Muchow (@smuchow1962))*

- **`mdkb init` indexes the whole tree, not just its top level.** The implicit
  `_root` collection was created with the non-recursive pattern `*.md`, so a
  repository whose documentation lives in subdirectories was indexed as almost
  empty. It is `**/*.md` now. Because that pattern overlaps every collection
  below it, a document is claimed by the collection with the most specific path
  — adding `docs/` to a store no longer leaves its files indexed twice, and
  removing it hands them back to `_root`. The single-file update path applies
  the same rule as the full walk, so incremental and full indexing cannot
  disagree about who owns a file. Two notes for an existing store: it keeps the
  `_root` pattern it was created with — nothing rewrites a collection behind the
  operator's back — so fix it with `mdkb collection update _root -p '**/*.md'`;
  and the ownership rule applies to *every* overlapping pair on the next
  `update`, so a document currently indexed under both an outer and an inner
  collection loses its row (and embedding) in the outer one. That is the
  intended de-duplication, but it is a data change, not just a query change.
  *(#8, reported by Stefano Straus (@sstraus))*

- **An empty or whitespace-only query returns no rows instead of an FTS5 parser
  error.** FTS5 answers an empty `MATCH` with `fts5: syntax error near ""`,
  which reached the caller verbatim through search, memory search, bm25 and the
  hybrid paths. All of them share one guard now. The hybrid path stops before
  the vector half too: ranking a corpus against the embedding of an empty string
  returns arbitrary neighbours, which is worse than returning nothing. The MCP
  hint on an empty result set now names the empty query instead of advising the
  caller to reach for Grep over a query it never supplied. *(#9, reported by
  Stefano Straus (@sstraus))*

- **A long `update` on the daemon is no longer cut off five seconds into
  shutdown.** In-flight requests were drained under the grace period meant for
  idle sockets — five seconds, against the hour the CLI itself budgets for a
  mutation. On any shutdown (a signal, or the 30-second watcher that retires the
  daemon when its executable is replaced by `cargo install`) the client saw
  `io: early eof` or `Connection reset by peer` while the daemon went on to
  finish the write as its runtime drained the blocking pool: a failure reported
  for work that succeeded. Requests that have started executing are now drained
  on their own budget before the socket-level grace. A second signal exits the
  process outright: tokio keeps its handler installed after the first one, so
  the operator's second Ctrl-C would otherwise go nowhere — and merely
  abandoning the wait would not end it either, because dropping the runtime
  waits for the very blocking write being interrupted. SQLite's WAL recovers an
  interrupted writer on the next open, exactly as after the SIGKILL this
  replaces. *(#10, reported by Stefano Straus (@sstraus))*

### Removed

- **21 configuration keys nothing read.** They parsed, some were validated,
  four were round-trip-tested, and no code path looked at the value:
  `[indexing]` `default_pattern`, `debounce_ms`, `parse_frontmatter`,
  `parse_wikilinks`, `index_headings`; `[search]` `default_limit`, `min_score`,
  `rrf_k`, `bm25_weight`, `vector_weight`; `[memory]` `enabled`, `directory`,
  `title_max_chars`, `order_by`, `track_access`; the whole `[models]` table;
  `[mcp] include_token_count`; `[code] index_path`; `[code.indexing]
  parallelism`; `[code.semantic_search] model`; `[hooks]
  recall_half_life_secs`. The worst was `[code] index_path`: the index has
  always lived at `.mdkb/code.sqlite`, a path resolved in over a hundred
  places, so a user who set the key believed the index had moved when nothing
  had. The `MDKB_SEARCH_DEFAULT_LIMIT`, `MDKB_INDEXING_DEBOUNCE_MS` and
  `MDKB_MEMORY_WARMUP_LIMIT` environment overrides go with them: the function
  that read them had no caller. In their place, `mdkb update` warns once per
  key it does not know, naming the key by its dotted path — which also covers
  the two `[models]` embedding keys that had a warning of their own before.


## 3.8.0 (2026-08-29)

Seventy-two commits, most of them in the code index. 3.7.18 was prepared but
never tagged, so its entry below ships here too.

Two things change behaviour on upgrade, both described under *Changed*: the
schema migrates 21→22, and `mdkb update` now archives entries whose TTL has
lapsed.

### Changed

- **`mdkb update` reclaims entries past their `expires_at`.** Expiry had always
  filtered reads — an expired entry was never served to warmup, handoff, search
  or list — but nothing reclaimed the row, so it stayed `active` and kept its
  projected file for as long as the store lived. The only thing that archived
  was `mdkb memory prune`, a command a human types. The sweep's scope is exactly
  the scope of the filter that was already hiding them: `expires_at IS NOT NULL
  AND expires_at < now`. NULL means permanent, so no `decision`, `problem` or
  `topic` can be reached by it. Archived, not deleted — the row keeps its
  content and the file is renamed into `memory/archive/`. The newest handoff is
  spared whatever its age, because the session it was written for can start
  after the TTL.
- **A mined prior now ages out like a written one.** Written through the MCP
  tool a prior got 30 days; mined from a session it got none, and was re-read by
  every warmup for as long as the database lived. Both writers read one
  constant now. Schema 21→22 dates the priors already written undated, from
  `created_at` rather than from now, so one mined two months ago reads as
  expired immediately. Priors a human stated are left alone.
- **Macro invocations are `Expands` edges, not calls.** Counted as calls,
  `assert!` and `println!` were 4 921 edges of this repository's own index
  pointing at functions that do not exist.
- **An unresolved call says what it is.** `CallTarget` distinguishes a call
  placed in the index, one naming a module the index does not contain
  (`std::fs::write`), and a bare name with no candidate. 31 503 of this
  repository's 44 202 `Calls` edges used to answer "no callers", which reads as
  "nothing calls this" and was wrong for nearly all of them.

### Added

- **Every symbol has an address, and calls resolve by scope.** Call sites keep
  their qualifier, and `code_graph` reports how many callers arrived on a bare
  name rather than folding them into the resolved count.
- **Imports collected by the parsers are stored.**
- **Construction is a call edge** in Java, C#, C++ and PHP.
- **Inheritance and type usage** are recorded in GDScript, PHP and Lua.
- **Wider symbol coverage across the parsers**: unions, macros, enum members,
  namespaces and aliases in C and C++; unions, trait associated items and extern
  declarations in Rust; records, annotation types and enum bodies in Java;
  events, delegates, indexers, operators and local functions in C#; protocol
  requirements, enum cases and unnamed members in Swift.
- **`code_graph` answers the hook socket with resolved symbols**, not only prose.
  `result.symbols` carries the same row shape as `symbols_in_file` for the
  callers/calls/impact set, alongside the unchanged `result.text`. "No callers"
  is `symbols: []`, never an absent field, so a client can tell an empty result
  from a daemon too old to carry it. *(prepared as 3.7.18)*

### Fixed

- **An unreadable vector store is no longer read as an empty one.** The
  incremental embedder adds what it just embedded to what it read back and
  writes the sum as the whole store, so one unreadable store turned a one-file
  update into a delete of every other symbol's vector.
- **Visibility is read as each language declares it**: Rust and C as written, Go
  unexported names at package level, Java package-private and C# file scope on
  their own levels, Swift `package` and C# composite levels distinct, and a C#
  top-level type defaulting to its assembly rather than to private.
- **Doc comments are recovered** where they were being dropped: above a wrapped
  C or C++ declaration, on a decorated TypeScript method, on a TypeScript
  namespace, on every GDScript declaration that has one, and the way Go writes
  one.
- **Members are reached through their declarators** in C and C++ rather than
  through fields the grammar does not always provide, and types declared inside
  a C++ class body are walked.
- **Names no longer recurse without bound.** A qualified name or receiver chain
  deeper than the AST limit aborted the process with a stack overflow; both
  walks are iterative now, as is the PHP type walk.
- **Language-specific corrections**: Swift extensions no longer emit a phantom
  type symbol and actors are not reported as classes; GDScript signals are not
  reported as variables and dotted calls are recorded; Kotlin companion objects
  get a symbol and extensions their receiver; PHP enum cases, promoted
  properties, `readonly` and namespaces are recorded; Python's three underscore
  conventions read as three, and class members are named through their class;
  Go embedded fields are indexed under the name they are reached by, and types
  come from the grammar rather than a hand-written list; Lua assignment targets
  each get their own value and `local` reads as visibility; a C# record reports
  whether it is a struct or a class, an enum member has the reach of its enum,
  and a qualified call stores its bare member name; Java records and enums
  delegate to their own constructors; TypeScript typed declarations get their
  symbols and a literal stays unnamed.
- **Stored paths are keyed from the project root**, not from the argument given
  to the indexer.
- **A qualifier's suffix is matched without `LIKE`'s wildcards**, so a name
  containing `%` or `_` no longer matches more than itself.
- **An index run reports every number it collected**, and a migration says how
  many files it marked rather than only how many changed.
- **The daemon publishes its IPC sockets already at 0600** and never hangs on a
  bind failure.

### Performance

- **One impact hop is one query** instead of one per candidate.
- **Embeddings are carried across a reparse** rather than regenerated, and old
  indexes migrate to the form that allows it.
- **The vector store is swept without materialising it.** Deciding which entries
  to keep no longer turns every one of them into its own `Vec<f32>` first. The
  saving is proportional to the fraction dropped; there is no wall-clock
  difference on a 2.3 GB store, where the run is bound by reading the file.
- **Orphaned vectors are swept** when the symbols behind them go.

### Removed

- Three parser capabilities nothing ever called, and `CallTarget::resolved`.


## 3.7.18 (2026-08-26)

### Added

- **`code_graph` now answers the hook socket with resolved symbols, not only
  prose.** `result.symbols` carries the same row shape as `symbols_in_file`
  (`name`, `kind`, `file_path`, `line_start`, …) for the callers/calls/impact set,
  alongside the unchanged `result.text`. The prose is written for agents to read
  and has never been JSON, so a programmatic client — an editor's "find
  references" — had no way to get locations except to scrape it. Both halves come
  out of one traversal; the MCP tool still returns `text` alone, so nothing an
  agent sees changes. "No callers" is `symbols: []`, never an absent field, so a
  client can tell an empty result from a daemon too old to carry it.

### Testing

- The hook socket's code-intelligence methods now have their **response shapes**
  pinned end to end, against a real daemon and a real code index — not just
  "the call succeeded". The three shapes differ on purpose (`symbols_in_file` a
  bare array, `code_find` a `{total, showing, symbols}` envelope, `code_graph`
  prose plus `symbols`) and a client that assumes one shape for all of them
  deserializes another into an empty list without erroring, on both ends. The
  0-based range convention — the opposite of `symbol_at_position`'s 1-based
  `line` *input* — is pinned in the same place.

## 3.7.17 (2026-08-19)

### Fixed

- **Outside a git repo, a store is no longer CREATED on a directory whose store
  would be refused.** The 3.7.16 guard rejected *adopting* a container's
  `.mdkb/`, but the fallback that picks the anchor when there is nothing to adopt
  was never checked. The two disagree exactly when it matters: a session started
  with `cwd = ~/Gits`, which holds every repo and is not one. No store existed
  yet, so there was nothing to refuse; the walk returned nothing, no
  `CLAUDE_PROJECT_DIR` was set, and `cwd` was handed back verbatim — anchoring a
  brand-new store on the very directory adoption rejects. `code.sqlite` reached
  2.9 GB in nine minutes and the daemon held ten of fourteen cores at 100% for
  24 hours, silent (no log line after the first minutes), with the watcher
  looping on `File watcher channel full — scheduling a full rescan to recover`.
  The over-anchoring test now gates every anchor outside a repo — discovered,
  hinted, or `cwd` — and `resolve_project_root` returns `Option`, so a refusal is
  a refusal rather than a fallback: the CLI errors and names the directory, hooks
  and MCP roots are skipped. `mdkb init` still bypasses resolution entirely, so a
  store on a container remains possible when it is what you actually want.

  The guard is deliberately absent from the git branch: inside a repo the anchor
  is already bounded by the repo root, and `holds_git_repos` cannot distinguish a
  container of projects from one repo that vendors submodules.

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

### Added

- **A unit test now pins that `--socket` overrides the default daemon socket
  path.** The behavior already worked: when the daemon proxy runs, a typed
  `--socket` path wins over `socket_path` in `~/.mdkb/daemon.toml` and the
  `~/.mdkb/daemon.sock` fallback. But the mode resolver reduced the flag to a
  boolean, so no test could see whether the path survived resolution. The
  resolver now carries the path in `McpRunMode::DaemonProxy`, and a test
  asserts the typed path comes back intact. Explicit testing of
  already-working behavior, added to prevent future drift.

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
