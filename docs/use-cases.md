# mdkb use cases

This guide is organised by job, not by command. Each section says when to use
it, which commands to run, what real output looks like, and what can mislead
you. The examples come from a scratch repository with three ADRs, a runbook,
a person page and three Python files, run on 2026-11-21 with main after the
command-audit fixes (mdkb 3.14.0 and older differ where
[the audit](command-audit-2026-11-21.md) lists a defect).

For a compact model-facing version, read [`llms.txt`](https://mdkb.tuicommander.com/llms.txt). For every
flag, run `mdkb <command> --help`.

Contents:

1. [Find something](#1-find-something)
2. [Correlate: who connects to what](#2-correlate-who-connects-to-what)
3. [Deduplicate code and memory](#3-deduplicate-code-and-memory)
4. [Consolidate: retire, merge, supersede](#4-consolidate-retire-merge-supersede)
5. [Organise the index](#5-organise-the-index)
6. [Keep memory honest](#6-keep-memory-honest)
7. [Measure retrieval](#7-measure-retrieval)
8. [Work across repositories](#8-work-across-repositories)
9. [Maintain the installation](#9-maintain-the-installation)

---

## 1. Find something

Pick the command by what you want back. A wrong command prints one line that
names the command that works.

| What you want back | Command | Reads |
|---|---|---|
| Source lines that contain a string now | `mdkb code grep TEXT` | live files |
| A string literal, JSX text, i18n value | `mdkb code grep TEXT --literals` | AST literal index |
| A symbol definition by (part of) its name | `mdkb code search NAME` | code index |
| Code that does something you can describe | `mdkb code search WORDS --semantic` | symbol embeddings |
| Documents | `mdkb search WORDS --docs` | markdown index |
| Memory entries | `mdkb memory search WORDS` | memory table |
| Documents and memory | `mdkb search WORDS` | both, two groups |

`search` returns documents and memory as two groups, each with its own
`--limit`. It does not search code. If the query looks like a code name, or
finds nothing, it prints the `code search` command on stderr.

### Exact text in code

```
$ mdkb code grep total --path src/
src/billing.py:2:5:     total = 0
src/billing.py:4:9:         total += amount * i
...
$ mdkb code grep -e "def charge" -e "def refund"
src/billing.py:1:1: def charge(card, amount):
src/refund.py:3:1: def refund(card, amount):
2 source-line hits; coverage: complete (8 files scanned, 1276 bytes); truncated: false
```

`code grep` is the right tool when the index can be stale: it reads the
files. The footer says how much it scanned and whether it stopped early.

### A user-visible string

```
$ mdkb code grep "Payment failed" --literals
src/notes.py:2:12: Payment failed, please retry
1 indexed literal hits; coverage: complete; freshness: current; truncated: false
```

This finds the literal as a value in the AST, so it does not match comments or
identifiers. Use `-i` for case-insensitive matching.

### A symbol

```
$ mdkb code search charge
  sym#4 Function charge in src/billing.py:1  sig: def charge(card, amount)
```

`code search` is fuzzy and lists the symbols named exactly `charge` first. Add
`--exact` to keep only those, `-k function` to filter by kind, and
`--path src/` to filter by path prefix. Every `path:line` in text output is
1-based; JSON keeps the 0-based tree-sitter `range`.

### A concept in code

```
$ mdkb code search "how do we retry failed charges" --semantic
  sym#2 Function refund in src/refund.py:3  similarity: 0.304
```

Read the similarity. On a small repository, the best match can be weak; 0.30
is not a confident answer.

### Documents and decisions

```
$ mdkb search "payment retries" --docs
[1] docs:adr/0001-queues.md - Use a queue for payment retries (score: 1.00)
[2] docs:adr/0002-queues-v2.md - Replace retry queue with outbox (score: 0.98)
[4] docs:people/alice.md - Alice (score: 0.48)
[3] docs:runbook.md - Runbook: card charge failures (score: 0.48)
```

- `[N]` is the document id, not the rank.
- Scores are normalised to the best hit. The two 0.48 lines do not mention
  retries; they are the tail, not evidence.
- `-c COLLECTION` narrows to one collection. An unknown name fails and lists
  the known collections.

Memory works the same way and accepts a type filter:

```
$ mdkb memory search "card retries" --entry-type decision
[retry-backoff] Payment retries use exponential backoff (decision) #billing #retry - 0 accesses
```

`mdkb search "card retries" --memory` is the same command.

### Read what you found

```
mdkb get adr/0001-queues               # without .md
mdkb get docs/adr/0002-queues-v2.md --lines 1:4
mdkb get 'adr/*'                       # glob: same as mget
mdkb get 'adr/0001-queues.md,runbook.md'
```

---

## 2. Correlate: who connects to what

Search tells you which documents mention a term. Correlation tells you which
things are *connected*, by an author (frontmatter, wikilinks, memory links),
by the code (call edges), or by history (co-changes).

### Document graph

Edges come from frontmatter keys on `graph.frontmatter_relations` (default
`owner`, `stakeholders`, `themes`, `related`) and from `[[wikilinks]]`.

```
$ mdkb graph links adr/0002-queues-v2
  adr/0002-queues-v2.md --related--> adr/0001-queues.md (frontmatter)
  adr/0002-queues-v2.md --owner--> @alice (frontmatter)
  adr/0002-queues-v2.md --links_to--> people/bob (wikilink)

$ mdkb graph neighbors adr/0001-queues --depth 2
  reliability (depth 1, via themes)
  billing (depth 1, via themes)
  people/alice.md (depth 1, via links_to, owner)
  adr/0002-queues-v2.md (depth 1, via related)
  people/bob (depth 2, via links_to)

$ mdkb graph path people/alice adr/0002-queues-v2
people/alice.md -> adr/0001-queues.md -> adr/0002-queues-v2.md
```

Give documents an identity so edges can name them by id or alias:

```yaml
---
id: person:alice
aliases: ["@alice"]
---
```

Every graph command resolves `person:alice`, `@alice`, `people/alice` and
`docs/people/alice.md` to the same document, so `graph backlinks` by any of
them returns both ADRs.

Use cases:

- *Ownership map*: `mdkb graph hubs --relation owner` ranks owners by how many
  docs name them.
- *Impact of a doc change*: `mdkb graph backlinks D` lists docs that point at D.
- *Gaps*: `mdkb graph dangling` lists targets with no document — a person
  without a page, a renamed file, a typo in a link.
- *Vocabulary*: `mdkb graph relations` measures which frontmatter keys hold
  values that name real documents. In `semi` mode, `--apply` adds them to the
  allowlist. The `Detected` column is the measurement; the `Extracted as
  edges` line lists the allowlist keys, which become edges either way.
- *Another repository*: `mdkb graph --root NAME links D` reads one other
  known repository.

The memory graph (`memory link`, `--relates`) has its own edges:

```
mdkb graph links outbox-choice --scope memory
  outbox-choice --derived_from--> adr/0002-queues-v2.md (doc)
mdkb graph backlinks adr/0002-queues-v2.md --scope memory
```

`--scope memory` covers `links` and `backlinks`, as the MCP `graph` tool does.

### Code graph

```
$ mdkb code callers send
Called by for Function send (src/billing.py:9)
  - Function refund (src/refund.py:3)
  - Function charge (src/billing.py:1)

$ mdkb code calls charge
Calls for Function charge (src/billing.py:1)
  - Function send (src/billing.py:9) [tier 4, unique]
  1 call(s) on a receiver whose type is not indexed: range

$ mdkb code impact send            # transitive, default depth 3
```

The tier says how the call was resolved. Tiers 1-2 are confident; higher tiers
are name matches. A call on a receiver whose type is not indexed has no edge,
so `callers` can miss real callers.

### Documents that drift from their code

`coupling --docs` pairs an indexed document with an indexed source file that
changes in the same commits, and marks the pair `drifted` when the source had
commits after the document last changed. Changelogs, `llms.txt`, todo files,
stories, plans and the memory projection are skipped.

```
$ mdkb graph coupling
docs/hooks.md  <->  src/main.rs   10 co-changes   drifted (source +3 commits)
```

On mdkb's own history (2026-11-21): 37 pairs, 14 drifted, 3 judged noise by a
reader (a document paired with a test or an eval fixture).

### Hidden coupling (history x code graph)

`coupling` lists file pairs that change in the same commits but have no
confident call edge between them — a dependency the code does not show.

```
$ mdkb code coupling
# Hidden Coupling

1 pair co-change without a call-graph edge.

## 1. `src/billing.py` ↔ `src/notes.py`

6 co-changes, no code-graph edge either way.
```

- Default floor: 5 shared commits in the last 12 months. Lower it with
  `--min-cochanges 3` on a young repository.
- Only files the code index parsed count. Docs, lockfiles and changelogs are
  dropped on purpose, so "No hidden coupling found" can also mean there was
  nothing to compare.
- Commits that touch more than 100 files are ignored.

Follow up a finding with `mdkb code callers` on a symbol in each file, then
decide: extract the shared knowledge, add the missing import, or accept it.

### Memory to documents and code

```
mdkb memory link outbox-choice derived_from adr/0002-queues-v2.md --doc
mdkb memory add retry-backoff ... --content "... see src/billing.py charge()"
```

A path or symbol in memory content is what `memory audit` checks for drift
(section 6). A `derived_from` link keeps the entry next to its source in
recall.

---

## 3. Deduplicate code and memory

### Code clones

```
$ mdkb code dup
# Duplication

1 cluster, 7 duplicated lines.

## 1. `charge` — de87e7ff6e98

2 copies across 2 modules · public · 7 duplicated lines · same shape, 6 bits apart

- `src/billing.py:1-7` — `billing::charge`
- `src/refund.py:3-9` — `refund::refund`
```

| Mode | Command | Cost |
|---|---|---|
| Whole repository, structural | `mdkb code dup` | seconds |
| Only clusters your change touched | `mdkb code dup --since main` | seconds |
| Clones of one file or folder | `mdkb code dup --file src/refund.py` | seconds |
| Renamed or reordered clones too | `mdkb code dup --semantic [--threshold 0.9]` | minutes, loads a model |
| Smaller bodies | `mdkb code dup --min-nodes N` | more noise |

`--since` and `--file` narrow the report, never the comparison: the copy of a
body in `src/refund.py` lives in another file, so the whole index is always
compared. A `--file` prefix with no indexed symbol says so instead of
reporting "no clusters".

Apply the rule of three before you extract: two copies are tolerated unless
they encode one fact that must change together.

### Duplicate memory entries

Three layers stop or find duplicates:

1. On write, an identical title is rejected:
   ```
   $ mdkb memory add x1 --title "Outbox replaces retry queue" --content dup
   Error: ... Near-duplicate entry exists: "Outbox replaces retry queue" (id: outbox-choice, identical title). Update that entry instead ...
   ```
   The command exits non-zero and nothing is written. `--dry-run` runs the
   same check and fails the same way.
2. On write, a semantically close entry is rejected when an embedding model
   with a calibrated threshold is active. `--on-conflict contradicts` writes it
   anyway and links the two with a `contradicts` edge.
3. Later, `mdkb memory audit` reports near-duplicate pairs among active entries.

Paraphrased entries ("Payment retries use exponential backoff" vs "Retries use
exponential backoff for payments") pass the title check. Search before you
write:

```
mdkb search "the claim in plain words" --memory
```

Then update the existing slug with `memory add SAME_ID ...` (it is an upsert).

---

## 4. Consolidate: retire, merge, supersede

### Replace a document

```
$ mdkb evolve link adr/0002-queues-v2.md supersedes adr/0001-queues.md --reason "outbox replaces queue"
Created evolution relationship #1: adr/0002-queues-v2.md supersedes adr/0001-queues.md

$ mdkb evolve current adr/0001-queues.md
Current version: [2] docs:adr/0002-queues-v2.md - Replace retry queue with outbox

$ mdkb search "payment retries" --docs
[2] docs:adr/0002-queues-v2.md - Replace retry queue with outbox (score: 1.00)
...
```

The superseded ADR leaves default search and recall. Only `supersedes` and
`retracts` change the older document's status (`store/evolution.rs`); the
other relations are recorded for `history` only. `--include-superseded`
brings it back.

| Relation | Meaning | Hides the older doc |
|---|---|---|
| `supersedes` | NEW replaces OLD | yes |
| `retracts` | NEW withdraws OLD | yes |
| `updates` | NEW changes part of OLD (`--scope` says which) | no |
| `corrects` | NEW fixes an error in OLD | no |
| `extends` | NEW adds to OLD | no |

- Paths resolve as in `get`: with or without `.md`, collection-prefixed, or by
  `id:`.
- A cycle is refused: `A supersedes B` followed by `B supersedes A` fails with
  `evolution cycle: ...`, because it would hide both documents.
- To undo an edge, read its id in `mdkb evolve history DOC` (`#N`) and run
  `mdkb evolve unlink N`. The older document becomes current again unless
  another `supersedes` or `retracts` edge still hides it.
- `mdkb doctor` reports cycles written by older binaries.

### Retire a memory entry

```
mdkb memory link outbox-choice supersedes retry-backoff
mdkb memory list --status superseded
```

`memory list` shows active entries only. `--status all` shows every entry
with its status marker, and `memory show ID` prints status, confidence,
provenance, and the edges in both directions.

### Merge prior clusters

Priors are behavioural patterns mined from sessions. When the curator has
reviewed several clusters that say the same thing:

```
mdkb memory propose                       # cluster-less user priors with proposals
mdkb memory curate-priors CLU_A CLU_B     # merge into the oldest cluster
```

### Reorganise collections

```
mdkb collection add meetings notes -p '**/*.md'
mdkb update
mdkb collection update meetings --path minutes -p '**/*.md'   # keeps matching embeddings
mdkb collection rename meetings minutes   # documents and embeddings move with it
mdkb collection rm minutes
```

---

## 5. Organise the index

- `mdkb init` creates `.mdkb/` and a `config.toml` with every setting as a
  commented line, and detects conventional folders as collections.
- `mdkb update` indexes docs and code, embeds new docs and backfills memory
  embeddings. Its code-index counts (`Files reindexed`, `Symbols written`)
  are this run's changes; `mdkb status` prints totals.
- `mdkb update --files a.md src/b.rs` reindexes only those files.
- `mdkb update --force` rebuilds documents; `mdkb update --code --force` rebuilds
  code.
- `mdkb update --collection claude_sessions` embeds a collection that is
  excluded by default.
- `mdkb update --sessions` indexes Claude Code transcripts for this project.
- `mdkb memory import --journal PATH` turns journal entries (one file, or a directory of them) into memory.
- Non-markdown sources enter through the daemon RPC in
  [external-ingest.md](external-ingest.md).

---

## 6. Keep memory honest

Memory gains trust by evidence, not age.

```
mdkb memory confirm ID --outcome confirmed   # it was right again
mdkb memory confirm ID --outcome refuted     # it was wrong
mdkb memory confirm ID --outcome misfired    # true, but recalled out of context
mdkb memory history ID                       # revisions, also those a conflict replaced
```

`memory audit` selects entries to re-read and decides nothing:

```
$ mdkb memory audit --dry-run --format json
{ "scanned": 3, "candidates": [], "near_duplicate_checked": true,
  "near_duplicate_coverage": { "embedded": 3, "scanned": 3 }, "source_drift_checked": true }
```

It reports:

- dead references: a path in the content that git knows but that is gone (a
  path git never saw counts as prose, so `src/gone.py` in a fresh entry is not
  flagged);
- dead symbols: a backticked `Type::method` or `function()` that the code
  index no longer defines and that no tracked source file mentions any more
  (a renamed function). The pass needs a code index newer than HEAD and a git
  repository; otherwise `symbol_check_skipped` says why it did not run;
- drifted source: the cited file changed after the entry was written;
- near-duplicate and contradicting pairs;
- expired or aged entries.

Read `near_duplicate_coverage`: `embedded: 0` means the duplicate pass did not
run.

Lifecycle:

```
mdkb memory prune --days 90 --dry-run   # expired entries; reminders, priors, handoffs unread for 90 days
mdkb memory export                      # write missing entries to .mdkb/memory/entries/
mdkb memory sync                        # reconcile database and files both ways
mdkb memory import DIR_OR_JSON          # load entries, --skip-duplicates
mdkb memory warmup                      # compact list for session start
```

---

## 7. Measure retrieval

| Question | Command |
|---|---|
| Is recall finding anything | `mdkb metrics quality --period 7` (zero-result rate, re-search rate, score bands) |
| Is it fast | `mdkb metrics latency --period 7` |
| Does a retrieval change help | `mdkb eval recall --fixture F.json --k 5 --mode all [--min-recall 0.8]` |
| Do answers contain the support | `mdkb eval judge --fixture F.json` |

Telemetry is local and off by default: `mdkb setup developer` enables it,
`mdkb metrics purge --yes` deletes it. Prompt text is never stored.

A high zero-result rate is not always missing content. The hook's relevance
floor rejects weak candidates on purpose, and story 065 tracks a regression in
that path since 2026-11-15.

---

## 8. Work across repositories

```
mdkb repos list                       # every repo the daemon knows, with health
mdkb search "outbox" --root ego       # one repo by name
mdkb search "outbox" --root '*'       # every repo in the caller's scope
mdkb search "outbox" --root 'scope:home'
```

Only `search` fans out. `get`, `graph` and `code` need one repository. A store
older than the binary is skipped by `*`; `mdkb repos refresh --only outdated`
migrates them after a backup copy.

`mdkb repos prune --dry-run` lists registry rows whose folder is gone or holds
no store; `mdkb repos prune` removes them. Only the registry row goes, never a
folder.

---

## 9. Maintain the installation

```
mdkb doctor            # problems with the fix for each
mdkb status            # one page: index, memory, code, hooks, daemon, telemetry; problems end with their fix
mdkb daemon restart    # after a binary upgrade
mdkb compact           # vacuum both databases
mdkb compact --prune-sessions --older-than 90d --export DIR
mdkb doctor --full       # verify MCP and hook registration
```

`mdkb status` shows `INDEX QUARANTINED` while a quarantined copy exists. A
copy whose salvage is proven is removed 15 days after quarantine. A copy
written by 3.8 or 3.9 shows `kept: report cannot prove the salvage; delete by
hand`: check `mdkb memory list --status all`, then delete the file named in the
banner (story 066).
