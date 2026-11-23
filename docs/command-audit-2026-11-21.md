# Command audit, 2026-11-21

Scope: every top-level command of mdkb 3.14.0 (installed binary), run against
this repository (read-only commands) and against a scratch repository with
documents, frontmatter identities, wikilinks, Python code with a clone pair,
and git history (every write command). Source references are to `main` at
`94ce14d4` plus the uncommitted work of stories 050 and 058.

Method: each finding below was reproduced by running the command; a cause is
named only where the code shows it directly. Findings that need a code change
are tracked as stories; none were fixed in this pass, because `src/main.rs`,
`src/mcp/dispatch.rs` and the store modules carry uncommitted work of active
stories.

Use-case documentation: [use-cases.md](use-cases.md). Model-facing summary:
[`llms.txt`](../llms.txt).

## Fix status

Fixed on main the same day, each with a regression test (unit tests in the
touched module plus `tests/cli_command_audit.rs`): 069, 070, 071, 072, 073,
074, 075, 076, 077, 078, 080, and the banner criterion of 066 and the
`daemon status` criterion of 067.

Still open:

- 079 (overlapping commands) and 081 (doc and memory correlation with code)
  need Boss's approval of a design note before code changes.
- 066: removing the two legacy quarantine copies of this repository is Boss's
  call.
- 067: test runs still add temporary roots to the user registry, and
  `repos prune` still keeps NoStore roots.
- 065: the recall zero-result regression is not part of this audit.

Narrowed while fixing:

- 070: collections live only in the database, never in `config.toml`, so
  rename has no config to update.
- 080: `update` labels its counts as this run's and points at `mdkb code info`
  for totals instead of printing them, because the routed result carries no
  store handle.
- 077: the source already ran the duplicate check in dry-run (the installed
  3.14.0 did not); the fix makes a refused preview exit non-zero.

## Defects

| Story | P | Command | Observed | Cause (where shown) |
|---|---|---|---|---|
| 069-cf5c | P1 | `evolve` | `A supersedes B` then `B supersedes A` both succeed; both docs leave default search; `current` returns each doc as its own current version | no cycle check before `update_document_status` (`store/evolution.rs`) |
| 070-0da0 | P2 | `collection rename` | `FOREIGN KEY constraint failed` once the collection has documents | `UPDATE collections SET name` with `documents.collection` still pointing at the old name (`store/collections.rs:100`) |
| 071-eb7b | P2 | `code *`, `search --scope symbols/semantic-code`, MCP `code_graph` | `def send` on line 9 printed as `src/billing.py:8`; line 1 printed as `:0` | raw tree-sitter row printed in `path:line` form (`main.rs` symbol printers, `mcp/dispatch.rs` `format_symbol`); literals, source and dup print 1-based |
| 072-e8e9 | P2 | every daemon-routed write | validation errors read `invalid search query: ... may still be writing`; `collection remove nope` and `search -c nope` exit 0 | errors wrapped as InvalidQuery; transport-loss suffix appended to returned errors |
| 073-504b | P2 | `dup --file` | `dup` finds billing/refund clone; `dup --file src/refund.py` finds nothing | `--file` filters candidates before clustering, unlike `--since` |
| 074-55bc | P2 | `evolve`, `history`, `current`, `superseded-by` | `adr/0001-queues` (no `.md`) not found, although `get` and `graph` resolve it; `superseded-by` prints `Doc #2 (supersedes` | exact-path lookup instead of the graph resolver; formatter at `main.rs:4266` |
| 075-b14e | P2 | `graph backlinks` | `backlinks people/alice` returns nothing; `backlinks person:alice` returns only the verbatim edge; `hubs` counts both | backlinks matches the stored target string; hubs and path resolve identities |
| 076-7514 | P2 | `memory list`, `memory show` | superseded entry listed with no marker by default; `show` hides status, confidence, edges | `status: None` lists every status (`core/memory.rs:558`) |
| 077-1b6c | P3 | `memory add --dry-run` | dry run says `validated`; the real write fails on the identical title | dedup arm not run in dry-run |
| 078-fba4 | P3 | `graph` | CLI has no `--scope memory` and no `--root`; MCP `graph` has both | CLI/MCP parity gap not listed by `mdkb surface` |
| 080-4dae | P3 | `update`, `search --scope literals`, `graph relations` | code-index counts are deltas under total labels; index age in raw Unix seconds; `themes` reported `Relation: no` while extracted | report wording |
| 066-bbd4 (criterion added) | P3 | `stats` | banner promises removal 15 days after quarantine; the 2026-09 copies are kept because their 3.8/3.9 reports lack `salvage_succeeded` | banner text ignores the sweep's keep rule (`cli/stats_render_report.rs:86`) |
| 067-5c87 (criterion added) | P3 | `daemon status`, `repos prune` | `daemon status` prints all 177 registry rows; `repos prune` removes only gone roots | registry pollution already tracked |

Already tracked and observed again: story 065-9a07 (recall zero-result rate;
`metrics show` reports 72.8 % over 7 days in this repository).

## Per-command verdict

`ok` = behaved as its help says in this run. `defect` = see the table above.
`not run` = needs a model download, network, or an external host.

| Command | Verdict | Notes |
|---|---|---|
| `init` | ok | Detects `docs/` as a convention collection. |
| `collection add/update/remove/list` | ok | `remove` of a missing name exits 0 (072). |
| `collection rename` | defect | 070 |
| `dup` | ok | Structural pass in seconds; `--semantic` not run. |
| `dup --file` | defect | 073 |
| `coupling` | ok | Code files only, by design; floor 5 co-changes, 12 months, commits over 100 files skipped. |
| `search` | ok, with gaps | Scopes `source`, `literals`, `symbols`, `semantic-code`, `docs`, `memory`, `knowledge` all answer. `-c UNKNOWN` returns memory (072). `[N]` is a doc id, not a rank. |
| `get`, `mget` | ok | `get` already accepts globs and comma lists, so `mget` is redundant (079). |
| `update`, `embed` | ok | Output wording (080). |
| `serve`, `mcp` | not run | Used daily through Claude Code. |
| `daemon status/stop/restart` | ok | `status` dumps the registry (067). |
| `repos list/refresh/prune` | ok | `prune` keeps NoStore rows (067). |
| `doctor` | ok | `--full` clean on the scratch repo. |
| `stats` | defect | quarantine banner (066). |
| `metrics *` | ok | Advice lines are generic; see 065 for the numbers. |
| `eval recall/judge` | not run | Needs the ONNX model for embedding and hybrid modes. |
| `memory add/show/list/search/link/confirm/rm` | defect | 076, 077, 072 |
| `memory audit/propose/warmup/prune/history` | ok | Audit treats a path git never recorded as prose. |
| `memory export/sync/import` | ok | Dry-run export and sync in scratch repo. |
| `memory curate-priors` | not run | Needs reviewed prior clusters. |
| `evolve *`, `history`, `current`, `superseded-by` | defect | 069, 074 |
| `graph links/neighbors/path/dangling/hubs/relations` | ok | `relations` wording (080). |
| `graph backlinks` | defect | 075 |
| `experiment *` | ok | `list` only; no experiment created. |
| `journal *`, `session index` | not run | Help reviewed only. |
| `setup *`, `hook *` | partly run | `hook status` and `hook search` answered; setup not re-run. |
| `code init/index/search/find/calls/callers/impact/info/parse` | defect | 0-based lines (071); otherwise ok. |
| `compact`, `cheatsheet`, `surface`, `schema` | ok | Cheatsheet uses the old scope names `text` and `code` (079). |

## Improvement proposals (compatibility not required)

Tracked as story 079-c50c; the list needs Boss's approval before code changes.

1. **One retrieval verb.** Fold `mget` into `get`, `memory search` into
   `search --scope memory`, and `code search` into `search --scope symbols`.
   Keep `code find` as the exact lookup.
2. **One status report per audience.** `doctor` for problems and fixes,
   `stats` for the operator report, MCP `status` for agents. Drop
   `hook status` from the human surface; make `daemon status` print a registry
   summary and leave the table to `repos list`.
3. **Group evolution reads under `evolve`.** `evolve history|current|superseded-by`
   next to the writers, sharing the document resolver used by `get` and
   `graph` (074).
4. **One path filter.** `search` has `--path` (prefix) and `--file`
   (substring); `dup` has `--file` (prefix filter). Use `--path` everywhere,
   always as an anchor, never as a candidate filter (073).
5. **One line convention in text.** 1-based in every human and MCP text
   output; JSON names its base (071).
6. **Errors carry their kind.** Validation, not-found, conflict and transport
   loss get distinct messages and non-zero exit codes (072).
7. **Graph parity.** CLI `graph --scope memory --root` (078) and backlinks that
   resolve identities (075).
8. **Make correlation discoverable.** `coupling` could optionally include
   documents (doc ↔ code co-change shows docs that drift with their code);
   `memory audit` could report memory entries that cite symbols `code find`
   no longer knows. Both are proposals, not defects, and need a design note (story 081-1642).

## Open questions

- Does the 72.8 % zero-result rate in `metrics show` count hook recall
  queries that the relevance floor rejects on purpose? Story 065 measures
  this; until then the metric cannot be read as "content is missing".
- `memory audit` and write-time semantic dedup skip the near-duplicate pass
  for Granite in the uncommitted work of story 058. The text output of
  `memory audit` should then say the pass was skipped; check when 058 lands.
