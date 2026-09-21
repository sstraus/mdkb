<p align="center">
  <img src="https://raw.githubusercontent.com/sstraus/mdkb/main/assets/mdkb.png" alt="mdkb logo" width="420">
</p>

# mdkb

**Repository memory for coding agents.**

mdkb gives Claude Code, Codex, and other MCP clients one local retrieval layer
for the repository: durable project memory, Markdown documentation, source
symbols, and call relationships.

It combines hybrid search, knowledge graphs, code intelligence, and lifecycle
hooks so an agent can recover what the team decided, find what the docs say, and
trace what the code does without loading the repository into every prompt.

Storage and inference stay local. mdkb uses SQLite, FTS5, tree-sitter, and a
local ONNX embedding model; no memory API, hosted vector database, or LLM
extraction service is required. The embedding model is downloaded on its first
use and then runs on-device.

## Why mdkb

- **Repository-first, not conversation-first** — docs, decisions, solved
  problems, symbols, and dependencies are searchable as one project context.
- **Memory designed to age well** — durable topics, problems, and decisions stay
  valid until explicitly superseded, refuted, or expired; lifecycle records
  (reminders, priors, and handoffs) still decay with age. Typed entries also
  carry provenance, revisions, confirmation signals, and explicit relations.
- **Recall is not dependent on a lucky tool call** — hooks inject a compact
  session warmup, provide opt-in prompt recall with a leading `*` by default,
  redirect code searches to indexed symbols, and reindex after edits. Always-on
  prompt recall is configurable.
- **It learns from its own sessions** — an opt-in Stop hook distils the
  episode that just ended into a behavioral prior, promotes lessons that recur
  across sessions, injects them where their trigger fires, and settles each
  injection as confirmed or refuted at the next Stop.
- **Code intelligence is structural** — tree-sitter indexes 14 languages and
  persists symbols and call relationships, so callers, callees, and transitive
  impact do not require repeated multi-file grep.
- **Local and inspectable** — queries, indexes, and embeddings stay on the
  machine. Durable authored memory is projected to reviewable Markdown for Git,
  while machine-local counters and SQLite state stay untracked.
- **Low operational weight** — one Rust binary owns CLI, MCP, hooks, watching,
  and repair. On Unix, an auto-started local daemon shares indexes and serializes
  writes; there is no separate LLM, vector, or graph service to provision.

## How it differs from other memory systems

"AI memory" covers products with very different jobs. mdkb deliberately
optimizes for software repositories rather than trying to be a general-purpose
personalization or conversation-memory platform.

| Memory approach | Usually optimized for | mdkb's difference |
|---|---|---|
| Conversation-memory SDKs | Extracting user facts and preferences for an application | Works as an installed repository tool; no application integration, extraction LLM, or hosted service is required |
| Markdown knowledge bases | Portable notes and human-editable knowledge graphs | Adds typed engineering-memory lifecycle, source indexing, symbol search, and a persistent call graph |
| Session-recording plugins | Capturing tool activity and AI-compressing past conversations | Prioritizes curated project truth and retrieves it alongside docs and code; it does not require a second AI process to summarize memory |
| Temporal knowledge graphs | Evolving entities, events, and point-in-time facts | Uses a lighter local stack and deterministic project relations; no graph database or ingestion LLM is required |

Choose mdkb when the repository is the memory boundary and source-code impact is
part of recall. A conversation-memory SDK is a better fit for end-user
personalization; a temporal graph is a better fit for bi-temporal entity facts;
and a session recorder is a better fit when automatic transcript compression is
the primary requirement.

## What it does

Five capabilities, each with its own section below.

### 1. Recall that reaches the agent without a tool call

Lifecycle hooks for Claude Code and Codex inject context at the moments it is
useful: a ranked **session warmup** with the latest handoff and due reminders at
SessionStart; **prompt recall** that ranks memory and documents against the
prompt with the same hybrid BM25-plus-vector score as search, expands one hop
through typed memory edges, and flags entries whose supporting memory was
superseded; **code-index hits** that replace a `grep` for a definition with the
real `file:line`; and **post-edit reindexing** so the index follows the edit.
The same dispatcher serves command hooks, a Unix socket, and native HTTP. See
[Hooks](#hooks-optional-recommended) and [docs/hooks.md](docs/hooks.md).

### 2. Memory that ages by evidence, not by the calendar

`topic`, `problem` and `decision` entries stay valid until superseded, refuted
or expired; `reminder`, `prior` and `handoff` entries decay. Every entry carries
provenance, a source-authority weight, a Bayesian confirmation signal
(`memory_confirm`), up to three revision diffs, and typed edges (`supports`,
`contradicts`, `supersedes`, `derived_from`, `relates_to`). Near-duplicates are
rejected at write time, or linked as a contradiction on request. Durable
entries are projected to `.mdkb/memory/entries/*.md` for review in Git; local
usage counters never are. See [Memory](#memory).

### 3. A self-learning loop over the agent's own sessions

At Stop, mdkb reads the episode that just ended, detects an error that was
fixed or a user correction, and asks a configured local or remote CLI
(`codex`, `claude`, `ollama`, `grok`) to distil one falsifiable lesson with a
machine-matchable trigger. Lessons that recur across sessions are promoted to
**priors** and injected exactly where their trigger fires: before a tool call,
after one, or on a matching prompt. Each injection is settled at the next Stop
as confirmed or refuted, and `mdkb stats` shows what mining did. Off by
default. See [Priors](#priors).

### 4. Structural code intelligence, with audits built on it

tree-sitter indexes 14 languages into persistent symbols and typed edges
(`Calls`, `Uses`, `Implements`, `Expands`, `Defines`). Calls are resolved
through a tiered cascade that keeps the written qualifier and the inferred
receiver type, so `store.write()` and `cache.write()` are different edges and
an unresolved call says so instead of inventing one. Two audits read the same
index: `mdkb dup` reports what the repository says twice, bucketed by how
trustworthy each finding is, and `mdkb coupling` reports files that change
together in Git with no confidently resolved `Calls` edge between them. See
[Code Intelligence](#code-intelligence).

### 5. Retrieval you can measure

`mdkb eval` scores memory search against a held-out fixture and fails CI below
a floor; `mdkb stats` reports hook activation, hit rate, latency and mining
outcomes; the opt-in developer telemetry profile records recall quality without
storing prompt text. Numbers in this README and in `CHANGES.md` come from those
commands, run on this repository. See [Retrieval eval](#retrieval-eval),
[Stats](#stats) and [Developer Telemetry Profile](#developer-telemetry-profile).

Also included: two knowledge graphs (frontmatter and wikilink relations for
docs, typed relations for memory), 12 annotated MCP tools with a CLI twin for
each (`mdkb surface`), self-maintaining indexes with integrity checks and
repair, and store namespaces so a consumer's test suite cannot pollute its own
memory.

See [CHANGES.md](CHANGES.md) for release history.

## Installation

### Homebrew (macOS/Linux)

```bash
brew install sstraus/tap/mdkb
```

### From source

```bash
cargo install --path .
```

### Pre-built binaries

Download from [Releases](https://github.com/sstraus/mdkb/releases) — macOS (arm64/x64), Linux (arm64/x64), Windows (x64).

## Quick Start

```bash
cd your-project
mdkb init
mdkb collection add docs ./docs
mdkb update
```

### Connect to Claude Code

```bash
# Project-scoped (recommended)
mdkb setup mcp claude --scope local

# Or user-scoped (global)
mdkb setup mcp claude --scope user
```

Restart Claude Code after setup. The MCP server auto-indexes on startup and watches for file changes.

### Hooks (optional, recommended)

MCP gives the assistant tools; hooks make it use them. Hooks also work standalone without MCP — the `PreToolUse` Grep interceptor suggests CLI commands via `current_exe()`, and `SessionStart` points to `mdkb cheatsheet` for the full command reference.

Register the lifecycle dispatcher so Claude gets a memory warmup at session
start, prompt recall when requested, and Grep-to-mdkb suggestions — without
having to call `search` first:

```bash
# Claude Code, project-scoped (writes .claude/settings.local.json)
mdkb setup hooks claude --scope local

# Claude Code, user-scoped / global (writes ~/.claude/settings.json)
mdkb setup hooks claude --scope user

# Terminal 1: serve native HTTP hooks (SessionStart remains a command hook)
export MDKB_TOKEN='replace-with-a-secret'
export MDKB_HOOK_TOKEN="$MDKB_TOKEN"
mdkb serve --http --bind 127.0.0.1:8080 --token "$MDKB_TOKEN"

# Terminal 2: write the matching Claude registration
mdkb setup hooks claude --scope local --http-url http://127.0.0.1:8080

# Codex CLI (writes ~/.codex/hooks.json)
mdkb setup hooks codex

# Preview the merged settings JSON without writing
mdkb setup hooks claude --scope local --dry-run

# Disable specific events at install time
mdkb setup hooks claude --disable post-tool-use
mdkb setup hooks claude --disable user-prompt-submit,post-tool-use
```

Restart the host CLI after setup. Re-running is idempotent: existing hook
entries are replaced and unrelated settings are preserved. Events:
`session-start`, `user-prompt-submit`, `pre-tool-use` (Grep/Bash interceptor),
`post-tool-use`, and `stop`. Full command and HTTP contracts, configuration,
and opt-out behavior are in [docs/hooks.md](docs/hooks.md).

Session start includes a compact power-feature reminder and points to
`mdkb cheatsheet`. Per-prompt recall is quiet by default: prefix a prompt with
`*` to inject matching memory, documents, and graph hints. MDKB removes the
asterisk before search and before telemetry; it is an activation signal, not
part of the query. Without it, the prompt passes through unchanged. Session
warmup and the other enabled hooks do not require the sigil.

#### The two recall floors

The sigil selects a **threshold**, not a feature. Both settings run the same
retrieval over the same text; they differ in what a candidate has to score to be
injected, because an injection nobody asked for is charged on every turn after
it while a miss on a sigil prompt costs one search.

| `[hooks]` / `[search.memory]` key | Default | What it gates |
| --- | --- | --- |
| `search.memory.min_recall_cosine` | `0.40` | The floor for a `*`-prefixed prompt. Lowest floor admitting no labelled negative on the eval fixture. |
| `hooks.recall_auto_min_cosine` | `0.50` | The floor for a prompt with no sigil. The recall plateau above `0.40` — see [docs/retrieval-eval.md](docs/retrieval-eval.md). |
| `hooks.user_prompt_submit_require_sigil` | `true` | When `true`, a prompt without `*` retrieves nothing at all. Set `false` for always-on recall at the `0.50` floor. |
| `hooks.user_prompt_submit_shadow` | `false` | Runs the always-on path on the skipped prompts, records the result, injects nothing. |

**`require_sigil` is still `true`, and the way to change that is to measure
first.** This repo logged 1716 UserPromptSubmit calls over 72 days and injected
on 8 of them (0.47%); flipping the default turns the other 1708 into retrieval
attempts, and the eval fixture cannot say how many of those are worth the turn —
it scores precision 1.000 at every floor from 0.40 up, so it cannot rank them.

Set `user_prompt_submit_shadow = true`, leave it for a week, then read
`.mdkb/hook-events.jsonl`. Shadow rows carry `"outcome": "shadow"` and a
`shadow` object:

```json
{"ts":1789659256,"event":"user_prompt_submit","outcome":"shadow","elapsed_ms":41,
 "shadow":{"session":"…","entries":["writer-recovery-protocol"],"docs":1,
           "related":0,"top_cosine":0.62,"floor":0.5}}
```

The counters to decide on, all four together — no one of them is the release
criterion on its own:

- **injection rate** — shadow rows with a non-empty `entries`/`docs`/`related`, over all `user_prompt_submit` rows. How noisy always-on would be.
- **precision** — read the `entries` ids and judge them. This is why the row names entries instead of counting them, and why fixture precision cannot stand in.
- **repetition rate** — the same entry id recurring across rows of one `session`. An entry injected on every turn is worse than one never injected.
- **P95 `elapsed_ms`** — shadow runs the full retrieval, so its latency is the real cost of the always-on path.

Shadow mode deliberately does **not** touch the per-session dedup map or the
behavioural-prior injection counters: writing to either would change what a
later real injection does and corrupt the counters above.

SessionStart keeps discovery compact and operational:

- restores the latest project-scoped handoff;
- surfaces due reminders, ranked memory, quarantine, and projection drift;
- emits `* query = recall` and the executable `mdkb cheatsheet` command even
  when the memory index is empty.

The cheatsheet is the complete AI-facing command map: hybrid search and batch
reads, durable memory and provenance, code callers/calls/impact, knowledge
graph navigation, duplication and hidden-coupling audits, collection updates,
developer telemetry, maintenance, daemon control, and machine-readable schema.

### Binary path caveat

`mdkb setup mcp …` and `mdkb setup hooks …` hard-code the absolute path of the binary that ran the setup. If you later move or rebuild the binary, the recorded command breaks. For stable global installs, first run `cargo install --path .` (binary lands in `~/.cargo/bin/mdkb`), then run setup from that binary.

For local development builds that back active MCP/hooks, prefer:

```bash
scripts/local-release.sh
```

It builds `target/release/mdkb`, stops stale `mdkb mcp` processes, restarts the daemon, and reports which process holds the rebuilt binary.

### Uninstalling

```bash
# Remove all Claude Code registrations (MCP + hooks)
mdkb setup remove claude --scope local   # per-project
mdkb setup remove claude --scope user    # global

# Remove individually
mdkb setup remove mcp claude --scope local
mdkb setup remove mcp codex
mdkb setup remove hooks claude --scope local
mdkb setup remove hooks codex
```

Soft alternatives before uninstalling: create an empty `.mdkbignore-hooks` marker at the repo root to silence hooks for that working tree, or toggle `session_start_enabled` / `user_prompt_submit_enabled` / `post_tool_use_enabled` in `.mdkb/config.toml`.

### Manual MCP Setup

Add to your Claude Code MCP config (`.claude/mcp.json` or `~/.claude/mcp.json`):

```json
{
  "mcpServers": {
    "mdkb": {
      "type": "stdio",
      "command": "/path/to/mdkb",
      "args": ["mcp"]
    }
  }
}
```

The `mcp` subcommand connects to the daemon via unix socket (auto-spawning it
if needed). Each Claude Code session runs a lightweight proxy instead of a
full in-process server, sharing one daemon for file watching and indexing.

## MCP Tools (12)

| Tool | Description |
|------|-------------|
| `search` | Hybrid search across docs+memory (default), or scoped to `docs`, `memory`, `code`, `symbols`, `duplicates`. `scope="memory"` accepts `min_confidence` to filter decayed entries; `scope="duplicates"` accepts `since` for review mode |
| `get` | Retrieve by ID, path, memory slug, glob pattern, or comma-separated list |
| `code_graph` | Call graph queries: `calls`, `callers`, or `impact` (transitive) |
| `graph` | Knowledge-graph queries over frontmatter + wikilink edges: `links` (outgoing), `backlinks` (incoming), `neighbors` (adjacent, each annotated with the `via` relation), or `path` (shortest path to `to`). Edge endpoints render as document paths, never numeric ids |
| `status` | Index health, collections, and code index stats |
| `update` | Differential reindex of all collections and source code |
| `memory_write` | Create or update a memory entry (supports `ttl`, `due_in` for reminders, near-duplicate rejection) |
| `memory_write_batch` | Create or update multiple memory entries at once (max 20) |
| `memory_confirm` | Atomic Bayesian signal without rewriting content — `outcome="confirmed"` bumps `confirmations` and `last_confirmed_at`; `"refuted"` bumps `corrections`, stamps `last_refuted_at`, and stops the entry being injected unasked until it is reconfirmed |
| `memory_delete` | Delete a memory entry |
| `memory_list` | List memory entries sorted by recency, popularity, or creation date |
| `usage` | Session and lifetime token ledger (per-tool call counts, token totals, truncation stats) |

Every advertised tool includes MCP annotations for read-only, destructive,
idempotent, and open-world behavior. These hints describe effects; server-side
validation and write admission remain authoritative.

### Search Scopes

| Scope | What it searches |
|-------|-----------------|
| _(omit)_ | Docs + memory combined (default) |
| `docs` | Hybrid BM25 + semantic over markdown documents |
| `memory` | Hybrid BM25 + semantic over memory entries, identical on every surface — the CLI, the MCP tool and the recall hook build the same OR-expanded query and apply the same absolute relevance floor. `--entry-type` narrows the corpus both legs draw from; it does not select a different engine |
| `symbols` | Exact symbol lookup by name, filterable by `kind` and `file` |
| `code` | Semantic code search across indexed symbols |
| `duplicates` | Clusters of near-identical bodies. `since="<ref>"` narrows the report to clusters your change touched |

### Memory

Persistent AI knowledge that survives across sessions — decisions, patterns, solved problems:

- **Confidence scoring** — topics, problems, and decisions do not lose trust
  merely because they are old; reminders, priors, and handoffs decay using age,
  access count, and source authority. Explicit TTL, supersession, and refutation
  still retire durable knowledge.
- **Duplicate detection** — near-duplicate entries are rejected before writing
- **Revision tracking** — manual entries track up to 3 revision diffs
- **TTL (time-to-live)** — pass `ttl` (seconds) to `memory_write` for auto-expiring entries. Expired entries are filtered from searches and listings but remain accessible via `get(id)` with an `[EXPIRED]` marker, so they can be inspected or renewed. `mdkb update` then archives them and moves their file to `memory/archive/` — archived, never deleted, so a renewal is always possible. Only entries given a TTL are ever reached: omit `ttl` and the entry is permanent, which is what `topic`, `problem` and `decision` are by default.
- **Provenance** — `memory_write` records the authoring session and (optional) `agent`; both surface in `get(id)` and via `mdkb memory link ... --agent <name>`.

Entry types: `topic` (concepts), `problem` (solutions), `decision` (architectural choices), `reminder` (time-bound — see below), `prior` (behavioral patterns — 30-day TTL default, excluded from default searches), `handoff` (session handover — no default TTL).

#### Memory graph (typed edges)

Memory entries are graph nodes: a `memory_edges` table (schema v14) records typed relations between memories, or from a memory to a document. Relations are a closed set — `supports`, `contradicts`, `supersedes`, `derived_from`, `relates_to` — and unknown values are rejected with the valid set listed.

- **Create edges at write time** — pass `relates` to `memory_write`: `relates=[{relation, target, target_kind}]` (up to 10, `target_kind` is `memory` (default) or `doc`). The entry and its edges are written in one transaction. Or link an existing entry from the CLI: `mdkb memory link <id> <relation> <target> [--doc] [--agent <name>]`.
- **`supersedes`** keeps the scalar `superseded_by` and `superseded` status in lockstep with the edge (single write path).
- **Traverse** — `graph(entity, direction="links"|"backlinks", scope="memory")` (MCP) walks the memory graph; targets are dangling-tolerant and resolved at query time, mirroring the document graph.
- **`on_conflict="contradicts"`** — when a `memory_write` hits the near-duplicate gate, instead of rejecting it writes the new entry and links it to the similar one with a `contradicts` edge (returning both ids). Omitting `on_conflict` keeps the default rejection.
- **Recall expansion** — a recalled entry's active 1-hop neighbors are surfaced (capped), annotated `(via <relation>)`.
- **`[STALE-DEP]` marker** — at injection time, an entry whose `derived_from`/`supports` target is superseded or net-refuted is prefixed `[STALE-DEP]` in warmup and recall. This is a read-only flag — it never mutates stored confidence.

#### Graph introspection & gardening (CLI)

- **`mdkb graph dangling`** — lists references (with source doc + relation) that resolve to no indexed document. Full-table scan, explicit command only (never runs in hooks).
- **`mdkb graph hubs [--relation R] [--limit N]`** — entities ranked by degree centrality (in/out-degree) with a per-relation breakdown. Full-table scan, explicit command only.
- **`mdkb collection list`** — name, path, pattern, and document count per collection (`--format json` for stable output).
- Graph refs accept collection-prefixed paths (`map/people/x.md` resolves like `people/x`); an unresolved ref lists the forms it tried.

#### Reminders

Create with `memory_write(id, title, content, entry_type="reminder", due_in=<seconds>)` (or `mdkb memory add --entry-type reminder --due-in N`). While `due_at > now` the reminder is hidden from searches and listings. Once due, it appears in the session warmup index prefixed `[reminder:DUE] {id}: {title}` so the MCP client sees it on the next turn. The AI is instructed to ask for confirmation before deleting and to snooze via `memory_write` with a new `due_in` (same `id` updates the record).

#### Priors

A prior is a behavioral lesson mined from the agent's own sessions: *do not edit
generated files, change the generator*. mdkb mines, promotes, injects and
settles them itself; the loop is off until a distiller is configured.

1. **Mine.** At Stop, with `[priors] mining_enabled = true` and a
   `distiller_program`, the hook reads the tail of the transcript, looks for an
   error that was fixed or a user correction, and sends tool names, the
   redacted error signature and the correction to the configured CLI. The CLI
   must answer with one JSON object: a falsifiable lesson (160 characters, no
   hedging), a trigger kind (`prompt`, `pre_tool`, `post_tool`) with a
   machine-matchable pattern, a scope, and the failure/fix evidence. Anything
   else is rejected. `mdkb setup check` runs the configured CLI once and prints
   the prior or the failure; four tested configurations are in
   [docs/hooks.md](docs/hooks.md).
2. **Cluster and promote.** Candidates with the same trigger, or a lesson within
   0.85 cosine of an existing cluster, merge. A cluster seen in two distinct
   sessions is promoted to a `prior` memory entry with a 30-day TTL.
3. **Inject.** A promoted prior is injected only where its trigger fires: a
   `pre_tool` lesson before the matching tool call, a `post_tool` lesson after
   it, a `prompt` lesson when the prompt contains its pattern. At most one per
   hook by default (`max_injected_per_hook`). Priors also take one reserved slot
   in the session warmup when their confidence clears 0.7.
4. **Settle.** At the next Stop, every prior injected in the session is marked
   confirmed if its error signature did not recur, refuted if it did.
   `mdkb memory confirm <id> --outcome confirmed|refuted` records a human
   verdict on the same counters. The belief score gates future injection.

`mdkb stats` shows mining outcomes for the last seven days (gated, distilled,
promoted, rejected, failed, with the last reason). Priors are excluded from
default searches and listings; query them with `--entry-type prior`. A prior
can also be written by hand with `entry_type="prior"`, and receives the same
30-day TTL.

#### Handoffs

Session context transfer entries. Create with `memory_write(id, title, content, entry_type="handoff")` or `mdkb memory add <id> --entry-type handoff`. Use `--file <path>` (CLI) or `source_file` (MCP) to read content from a file — saves tokens when agents write handoffs to the filesystem. The file path is persisted as `source_path` metadata. Handoffs have no default TTL; confidence decay handles relevance naturally. If a caller gives one an explicit `ttl`, the newest handoff is still never archived by the expiry sweep — the session it was written for can start after the TTL runs out.

Source types control confidence weighting:

| Source Type | Multiplier | Use Case |
|-------------|-----------|----------|
| `official_docs` | 1.0 | Verified documentation |
| `user_statement` | 0.85 | Human-stated facts (default) |
| `auto_extracted` | 0.70 | Automated knowledge capture |
| `inference` | 0.65 | AI-inferred knowledge |

## Code Intelligence

Tree-sitter parsing for **14 languages**: Rust, Go, TypeScript, JavaScript, Python, Java, Kotlin, C, C++, C#, PHP, Swift, Lua, and GDScript.

- **Substring search** — find symbols by partial name (FTS5 trigram, works from 3 characters)
- **Semantic code search** — find conceptually similar code using embeddings
- **Persistent call graph** — function calls, callers, and transitive impact radius survive restarts
- **Scope-resolved calls** — every symbol carries an address, and a call site keeps the qualifier it was written with, so `Store::write` and `Cache::write` are not the same edge
- **Receiver-type inference** — Rust method receivers are reduced through local
  bindings, parameters, constructors, `Self`, and return values before the call
  cascade resolves the target. Ambiguous bare-name matches remain candidates,
  not invented edges. **This pass is Rust-only.** In the other 13 languages a
  method call carries no receiver type, so it resolves on its written name
  alone — the unplaced tier, or no candidate at all. `code_graph` says so in
  the answer instead of letting a TypeScript result read as authoritative as a
  Rust one.
- **A call the index cannot place says so** — the graph distinguishes a call resolved inside the index, one naming a module the index does not contain (`std::fs::write`), and a bare name with no candidate. None of the three is reported as "no callers"
- **Macro invocations are their own edge kind** — `assert!` and `println!` are expansions, not calls to functions that do not exist
- **Imports, inheritance, type usage and construction** are recorded as edges, not only definitions

Hidden directories (`.git/`, `.vscode/`, etc.) are excluded by default.
To force-index files inside a hidden directory, annotate your `.gitignore`:

```gitignore
# mdkb:index
.generated/**/*.rs
```

Generate semantic embeddings (downloads ~30MB ONNX model on first run):

```bash
mdkb embed
```

`mdkb embed` lowers its own scheduling priority by `search.embed_nice` (default
15) so a backfill over a large corpus yields to the editor and the hooks running
beside it. Unix only, and one-way — an unprivileged process may lower its own
priority and may not raise it back, which is why only this command does it and
never the daemon. Set it to `0` to leave the priority alone.

### Audits: duplication and hidden coupling

Two audits read the same index. `dup` reports what the repository says twice;
`coupling` reports files that change together in git history with no confidently
resolved `Calls` edge between them. It uses the same callable-kind and
resolution-tier cascade as the call graph, so a coincidental bare name cannot
hide coupling. `Calls` at tiers 1–2 is the only edge that suppresses a pair: a
pair joined solely by `Uses`, `Implements`, `Expands` or `Defines` is still
reported, because those kinds say the two files are related, not that they must
change together.

```bash
mdkb dup                          # sweep the repository
mdkb dup --file src/code/parsing  # scope the candidates
mdkb dup --since HEAD             # review mode: only clusters your change touched
mdkb dup --semantic               # add the embedding pass (minutes, not seconds)
mdkb coupling                     # 5+ shared commits over the last year
mdkb coupling --since 6.months --min-cochanges 3
mdkb dup --format json               # findings with their distance, for bucketing
```

`dup` runs two passes. The structural one compares fingerprints, needs no
model, and finishes in seconds. The semantic one embeds every body and is
**off by default**: measured on this repository it took 817 s of an 818 s run
to add 69 of 767 clusters. Turn it on for a single run with `--semantic` or
any `--threshold` override, or standing with `semantic = true` under
`[code.duplication]` in `.mdkb/config.toml`. Over MCP, passing `threshold` to
`search(scope="duplicates")` is the opt-in. A model that will not load
degrades the run to the structural pass rather than failing it.

Read `dup` knowing where its signal is: the report says so itself. After the
headline, a bucket table breaks the clusters down by structural distance
(`0`, `1-3`, `4`, `5`, `at cut`) and the semantic pass (`cosine`) — the
clusters at 0–3 bits are the trustworthy core, the ones at the cut are mostly
false positives. Clusters are ranked bucket-first, so a trustworthy finding
outranks a noisy one regardless of how far it spreads. `--format json` carries
the same `buckets` summary alongside `evidence.hamming` per cluster. See
`CHANGES.md` for the measured distribution.

## CLI Reference

### Search

```bash
mdkb search "authentication flow"
mdkb search "handler" --scope symbols --kind function
mdkb search "auth handler" --scope code
```

### Collections

```bash
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection remove <name>
mdkb collection rename <old> <new>
```

### Document Retrieval

```bash
mdkb get <id|path|slug>
mdkb get 42 --lines 10:50
mdkb get "docs/*.md"
```

### Code Commands

```bash
mdkb code index
mdkb code search "handler" --kind fn
mdkb code calls main
mdkb code callers handle_get
mdkb code impact init --depth 5
```

### Developer Telemetry Profile

Use the developer profile on repositories where mdkb itself is being evaluated.
It enables local per-recall measurements while the shipped default remains off
for end users:

```bash
mdkb setup developer                       # 30-day retention
mdkb setup developer --retention-days 14
mdkb metrics status
mdkb metrics show --period 7
mdkb metrics latency --period 7
mdkb metrics quality --period 7
mdkb metrics purge --yes                   # delete every query event
```

Prompt text is never stored. Repeated queries are correlated with HMAC-SHA-256
using a random 256-bit key unique to the repository at
`.mdkb/telemetry.key`; the key is outside Git and owner-readable only on Unix.
Every recorded recall deletes events older than the configured retention window.
`setup developer` preserves unrelated TOML settings and comments and supports
`--dry-run`. Restart the daemon after enabling the profile so its cached
repository configuration is reloaded. This profile does not make prompt recall
always-on: the `*` opt-in sigil remains a separate content-selection choice.

The two reports answer different questions:

- `mdkb metrics quality/latency` measures recalls after `*` activated them:
  result count, score bands, repeated-query rate, and latency.
- `mdkb stats` measures engagement. In the Hooks table, `user_prompt_submit`
  `Calls` is the denominator, `Fired` is successful activation, and `Hit%` is
  the activation rate. A low Hit% indicates that the opt-in instruction may be
  missed; it does not by itself mean retrieval quality is poor.

Score bands are ranking diagnostics, not human relevance labels. A high score
can still be unhelpful, especially when the prompt language differs from the
indexed corpus. The current profile proves activation, performance, result
shape, and repeated use; it does not infer helpfulness without explicit user
feedback.

### Knowledge Graph

Typed edges are extracted during indexing from allowlisted frontmatter keys
(strong) and body `[[wikilinks]]` (soft). Configure via the `[graph]` section.
Repository authors and agents define relationships; MDKB maintains them. A
re-index replaces one document's extracted edges atomically, so removing a link
removes the edge and repeated updates do not duplicate it. Memory edges are
written transactionally with `memory_write` or explicitly with `memory link`.
MDKB never invents a taxonomy or rewrites documents from graph analysis.

#### Why you no longer have to declare `frontmatter_relations`

Frontmatter holds identity, metadata and relations in one map, and a relation
target is any string, or any list of strings — that is the whole rule. In this
node, six keys carry a value of that shape and only two of them are relations:

```yaml
id: person:arnaud-tauveron          # identity
type: person                        # metadata
name: Arnaud Tauveron               # metadata
aliases: ["@ArnaudTurn-pro", arnaud.tauveron@lansweeper.com]
role: Data Scientist (@Lansweeper/cloud)
org: ["org:lansweeper"]             # relation
```

Nothing about the *value* separates them. What separates them is whether the
value names something the index knows, and that is a measurement MDKB can make:
the share of a key's values that resolve to a document, by path or by the
identity that document declares. Free text can never score, so metadata cannot
enter the graph. Measured on a 404-document corpus, 17 relation keys scored
1.0 and 23 metadata keys — `type`, `name`, `date`, `role`, `status`, `github`,
`slug`, `horizon`, `source` and the rest — scored 0.0, with no middle band.

This is why naive auto-detection was the wrong answer and measured detection is
the right one. Guessing from shape alone would have made `type: person` an edge:
one node with degree 29 in a repository with 29 people, outranking every real
entity in `graph hubs`, with `source: slack`, every `date:` and every free-text
`role:` becoming nodes beside it.

`graph.relations` defaults to `"auto"`: the detected keys are unioned with
`frontmatter_relations` on every index, and **your `config.toml` is never
written to**. Derivation re-reads the corpus, so it follows the repository
instead of freezing a snapshot you then maintain by hand.

```bash
mdkb graph relations            # every key with hits, total and score
mdkb graph relations --apply    # write them into the allowlist (semi/manual)
```

Set `relations = "semi"` to keep extracting only what you declared while
SessionStart names, in one line, the keys it is not extracting. Set
`"manual"` to hear nothing.

The stakes for getting this wrong are quiet: a repository writing keys outside
the allowlist gets a partial graph and nothing reports it, because edges that
were never extracted cannot appear in `graph dangling` or `graph hubs`.
Measured on a 62-node operational graph, the four default keys extracted 56
edges; the twelve keys the repository actually wrote took it to 182 over the
same files. Nothing was missing from the documents — the reader was configured
for someone else's vocabulary. That is the failure `auto` removes.

`supersedes`, `updates`, `corrects`, `extends` and `retracts` belong to the
evolution subsystem. They are never derived and config validation rejects them
in the allowlist.

Editing the allowlist by hand still works and no longer needs `--force`: edges
are rebuilt as a pass over the store after indexing, so the change reaches
documents no file touched.

```bash
mdkb graph links project.md                 # outgoing edges (owner, themes, links_to, ...)
mdkb graph links project.md --relation owner # filter by relation
mdkb graph backlinks alice                   # who points at this entity (works on dangling slugs)
mdkb graph neighbors project.md --depth 2    # adjacent entities, undirected
mdkb graph path project.md guide.md          # shortest path between two entities
mdkb graph dangling                          # broken references / missing pages
mdkb graph hubs --relation owner             # central nodes for one relation
```

Use search to discover relevant content; use the graph after finding an entity
to inspect impact, dependencies, ownership, and paths. Graph neighbors also
enrich prompt recall within strict caps. `supersedes` retires old memory in the
same transaction, while `[STALE-DEP]` marks recalled knowledge whose supporting
memory was superseded or refuted. `dangling` and `hubs` are read-only gardening
reports: they identify reorganization work but never mutate the repository.

**[docs/graph.md](docs/graph.md)** — how edges are created, how references
resolve, what each query is for, and when to reach for the graph instead of
search.

**[docs/cross-folder-flows.html](docs/cross-folder-flows.html)** — every
cross-folder flow in one page: how a store is chosen for a working directory,
how a directory that merely holds repositories is served rather than refused,
how collections scope folders inside one store, and how cross-repo search fans
out and states its coverage.

### Cross-repository search

One daemon answers about every repository it knows. The map of known roots
lives in `repos.json`, is seeded from `[[repos]]` in `daemon.toml`, is extended
by every store the daemon opens, and survives a restart. `mdkb daemon status`
lists the known and the discoverable roots separately.

The MCP `root` parameter says which repositories a call means:

| `root` | Means |
| --- | --- |
| omitted | the workspace the client declared, and every store nested beneath it |
| `/abs/path` | that repository |
| `mdkb` | the known root with that last path component; an ambiguous name is refused by naming the candidates |
| `a,b` | those repositories |
| `*` | every known repository — accepted by `search` only |

`*` is search-only on purpose: fanning out a read is meaningful, fanning out a
write is not. A fan-out always states its coverage — what it read, out of what
is known, and what it skipped and why — because a repository that could not be
opened is not an empty repository. `max_active_repos` in `daemon.toml`
(default 5) bounds how many stores are held open at once.

### Memory

```bash
mdkb memory add auth-patterns -t "OAuth2 PKCE Flow" -T topic --tags auth,security \
  -c "Always use PKCE for public clients..."
mdkb memory add pay-bill -t "Pay electricity bill" -T reminder --due-in 86400 \
  -c "Monthly utility payment"
mdkb memory list
mdkb memory search "authentication"
mdkb memory history auth-patterns

# Which stored entries deserve a fresh look. Selects from signals the store
# already holds and decides nothing; --dry-run does not even stamp them.
mdkb memory audit
mdkb memory audit --format json

# Export all entries to .mdkb/memory/entries/ (one .md file per entry)
mdkb memory export
mdkb memory export --dir ./memories --include-expired --overwrite

# Import from a markdown folder (auto-detected) or legacy JSON file
mdkb memory import .mdkb/memory/entries --skip-duplicates
mdkb memory import entries.json --dry-run --skip-duplicates
```

#### Team sync (git)

`mdkb init` keeps the SQLite indexes and machine-local state ignored while
allowing `.mdkb/memory/entries/*.md` into Git. Memory writes update that durable
projection, and `mdkb memory sync` reconciles changes arriving from a pull:

```bash
# Reconcile and commit the tracked projection
mdkb memory sync
git add .mdkb/.gitignore .mdkb/memory/entries/
git commit -m "chore(memory): sync team knowledge"

# Teammate, after pulling:
mdkb memory sync
```

Automate both ends with git hooks so nobody has to remember the manual steps:

```bash
# .git/hooks/pre-commit — reconcile before every commit
#!/bin/sh
mdkb memory sync
git add .mdkb/memory/entries/

# .git/hooks/post-merge and post-checkout — pick up teammates' entries after a pull
#!/bin/sh
mdkb memory sync
```

Only `entries/*.md` is meant for version control. `index.json` and `archive/`
under `.mdkb/memory/` are regenerable caches — never commit those. Derived
counters (`access_count`, `last_accessed`, `confirmations`) reset to zero on
import; they track local usage, not authored knowledge, so they don't need
to round-trip.

### Retrieval eval

```bash
# recall@5 / MRR of memory search over a held-out fixture, one line per mode
# (bm25, embedding, hybrid); model modes are skipped when the ONNX model is not cached
mdkb eval recall
mdkb eval judge
mdkb eval recall --mode hybrid --min-recall 0.9   # exit 1 below the floor
```

Baseline numbers, the fixture authoring rule and what CI enforces: [docs/retrieval-eval.md](docs/retrieval-eval.md).

### Stats

`mdkb stats` is the unified diagnostic dashboard introduced in 2.0.0 (replaces the former `mdkb status` — not aliased, it was removed).

```bash
# Unified ASCII diagnostic dashboard
mdkb stats

# Machine-readable JSON output (safe for pipes and scripts)
mdkb stats --format json

# Plain text (no ANSI color, no Unicode box-drawing)
mdkb stats --no-color
```

The report is stacked: header (repo, version, db size, last update) → index health → collections → memory (by entry type, reminders DUE / upcoming 7d) → code (by language, top files by tokens) → sessions (totals, top tools) → hooks (invocations, hit rate, latency, prior mining, registration drift). Output auto-detects whether stdout is a TTY; the JSON format is stable for scripting.

#### Sessions

The sessions row comes from Claude Code session JSONL files under
`~/.claude/projects`, indexed per project for token usage and tool-call counts:

```bash
mdkb session index
mdkb session index --sessions-path /path/to/sessions --project-root /path/to/project
```

The same data backs the `usage` MCP tool: `usage(session_only=true)` for the
current session, `usage(session_only=false)` for lifetime aggregates.

## Configuration

Configuration lives in `.mdkb/config.toml`:

```toml
[indexing]
# When true, the doc/collection walker honors .gitignore.
# When false (default), it reads .mdkbignore instead.
respect_gitignore = false

[code.indexing]
# When true (default), the code walker honors .gitignore.
# When false, it reads .mdkbignore instead.
respect_gitignore = true

[mcp]
max_response_tokens = 50000
max_document_tokens = 10000

[search]
# Scheduling priority `mdkb embed` gives up while it works. 0 disables it.
embed_nice = 15

[graph]
# How the extracted relation-key set is decided:
#   auto   — derive it from the corpus on every run, unioned with the allowlist
#   semi   — allowlist only, and report what derivation found
#   manual — allowlist only, and report nothing
relations = "auto"
# Frontmatter keys that declare what a document IS, not what it points at.
identity_keys = ["id", "aliases"]
```

`mdkb init` writes every setting with its default, commented out. A key mdkb does not read is ignored on load; `mdkb update` warns and names it by its dotted path.

### Controlling what gets indexed

Both the document walker (`mdkb update`) and the code walker (`mdkb code index`) share a unified ignore system:

| Mode                       | Files honored                                  | Use when                                                               |
| -------------------------- | ----------------------------------------------- | ---------------------------------------------------------------------- |
| `respect_gitignore = true` | `.gitignore` (+ `# mdkb:index` force-include)  | Your ignore rules are already correct for indexing.                    |
| `respect_gitignore = false`| `.mdkbignore` only                              | You want to index content that `.gitignore` excludes (e.g. `stories/`, generated sources), or you need a different ignore scope from git. |

**Defaults:**
- Code indexing: `respect_gitignore = true` — source trees usually want `.gitignore` honored (skip `target/`, `node_modules/`, etc.).
- Document indexing: `respect_gitignore = false` — project knowledge often lives in gitignored folders (plans, stories, drafts).

**`# mdkb:index` annotation** (only active when `respect_gitignore = true`):

Force-include a gitignored path by prefixing it with a `# mdkb:index` comment line in `.gitignore`:

```gitignore
# mdkb:index
generated/
# mdkb:index
docs/api/*.md
```

Blank lines between the annotation and the pattern are tolerated. The annotation is case-insensitive.

**`.mdkbignore`** (only active when `respect_gitignore = false`):

Uses the same syntax as `.gitignore`, including `!pattern` for re-inclusion. Place one at the repo root.

## Storage

Project state stays local in `.mdkb/`:

```
.mdkb/
├── config.toml
├── index.sqlite      # FTS5 + document metadata
├── code.sqlite       # Source code symbols + call graph
└── memory/           # Memory entries (markdown mirror + index.json cache)
```

The embedding model (AllMiniLML6V2, ~30MB ONNX) is downloaded on first use and cached in the platform's user cache directory.

`MDKB_NAMESPACE=<name>` points a process at `.mdkb/namespaces/<name>/` instead:
its own index, memory projection and locks, invisible to `memory list`, search
and the SessionStart warmup of the default store, and never committed. A process
under a test runner (`node --test`, vitest, jest, pytest) gets the `test`
namespace without asking, so a consumer's test suite cannot pollute the store
its sessions warm up from. `MDKB_NAMESPACE=default` opts back out. Namespaced
processes never use the daemon.

Keep `.mdkb/*` ignored at the repository root, then re-include
`.mdkb/.gitignore` and `.mdkb/memory/`. The generated store-level ignore file
allows only `memory/entries/*.md` to be tracked; indexes and machine-local state
remain ignored. See [Team sync (git)](#team-sync-git).

## License

MIT
