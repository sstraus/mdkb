# Hooks

mdkb ships a hook dispatcher (`mdkb hook <event>`) that plugs into Claude
Code and Codex CLI lifecycle events. When registered, it injects relevant
memory into context automatically — no tool call required — and keeps the
code index fresh after edits.

## Why hooks

Without hooks, the assistant may ignore `mcp__mdkb__search` and answer
from stale training data. Hooks make recall proactive:

- **SessionStart** — inject a warmup block listing recently-accessed
  memory entries as soon as a session opens.
- **UserPromptSubmit** — hybrid-rank memory and documents against the user's
  prompt and inject only entries above the final-score floor; when the prompt
  names a document, also inject its 1-hop frontmatter doc-graph neighbors.
- **PreToolUse** — intercept `Grep`/`Bash` searches; on a definition search for
  an indexed symbol inject the real `file:line` from the code index, otherwise
  suggest `mdkb search` / `mdkb code` CLI commands. Works without MCP.
- **PostToolUse** — when `Edit` / `Write` / `MultiEdit` / `NotebookEdit`
  touches a file, send it directly to the daemon watcher for targeted reindex.
- **Stop** — trigger pending embedding backfill and, when explicitly enabled,
  distill the completed episode into a reusable behavioral prior in the
  background.

All hooks are fire-and-forget: internal errors are logged to stderr and
swallowed — the host CLI is never blocked by mdkb.

## Find labels and other literal values

Use `code grep --literals` before grep when you know text from the UI:

```bash
mdkb code grep 'Fork from here' --literals
mdkb code grep 'fork from here' --literals --ignore-case --root tuicommander
mdkb code grep 'Fork from here' --literals --path src/ --limit 30
```

The code-index parser stores AST string literals, JSX text, and JSON values with
file:line locations and the smallest enclosing symbol when one exists.
Matching is a case-sensitive fixed substring, not a regular expression;
`-i` uses Unicode lowercase matching. Use `mdkb code grep VALUE --literals` to
search them. `mdkb search` does not include literal hits; `mdkb code search`
finds definitions rather than values. `--root` uses the same repository
selector as other CLI searches.

Run `mdkb update --code` once to populate literals in an older index. Search reads
the index, not source files: the existing post-edit hook and daemon watcher
refresh changed files. Human output and JSON report the oldest file indexing
timestamp so a caller can identify stale data and request a refresh.
JSON for `--scope text` has `hits`, `truncated`, and `indexed_at`; default search
adds this result under `text`, beside the existing `documents` and `memory` keys.
A repository with no literal-capable code index has `text: null` in default JSON.
The MCP search scopes are unchanged; these commands describe the CLI surface.

Literal discovery uses the code index's ignore rules and nested-store boundaries.
JSON now participates in code indexing for its values, not its property keys.
To bound index growth, literals shorter than 4 characters or longer than 1024
bytes are omitted. Each file contributes at most 4096 literals and 256 KiB of
literal content. Generated markers in the first five lines, `.min.js`, `.min.ts`,
`.generated.ts`, and any source line longer than 4096 bytes suppress literal
extraction for that file. The existing 1 MiB file-size cap still applies.
Interpolated templates contribute static fragments, not a fabricated runtime
value. Double-quoted JSON-compatible strings decode escapes; other spellings
retain source escapes. This is not a full-text scan: use grep for comments,
short strings, generated files, or files outside the code index's language set.

## Install

```bash
# Claude Code, project-scoped (writes .claude/settings.local.json)
mdkb setup hooks claude --scope local

# Claude Code, user-scoped (writes ~/.claude/settings.json)
mdkb setup hooks claude --scope user

# Terminal 1: serve native HTTP hooks. The server token and the token exposed
# to Claude's allowed environment must have the same value.
export MDKB_TOKEN='replace-with-a-secret'
export MDKB_HOOK_TOKEN="$MDKB_TOKEN"
mdkb serve --http --bind 127.0.0.1:8080 --token "$MDKB_TOKEN"

# Terminal 2: write the matching Claude registration.
mdkb setup hooks claude --scope local --http-url http://127.0.0.1:8080

# Codex CLI (writes ~/.codex/hooks.json)
mdkb setup hooks codex
```

Restart the host CLI after setup. Re-running is idempotent: existing
hook entries are replaced, unrelated settings are preserved.

Codex handlers carry `statusMessage` (`mdkb session-start`, `mdkb stop`, …),
which Codex `/hooks` shows as the hook title. Codex hashes the whole handler for
trust, so after setup adds or changes that label, Codex marks the mdkb hooks as
modified. Review them once in `/hooks`; they do not run until you trust them.

### ego generic lifecycle hooks

```bash
mdkb setup hooks ego --dry-run
mdkb setup hooks ego
mdkb setup remove hooks ego
```

This targets ego's generic hook contract from story 718-0496, not the removed
`mdkb_prompt_command` bridge. Upgrade ego to a build with that contract before
setup. Setup does not probe or require an installed ego binary.

Setup merges six `[[hooks]]` entries: `SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, and `Stop`. Each command handler
uses the installed mdkb binary, exact `args = ["hook", "<event>"]`, and a ten-second
timeout. Both post-tool events call `mdkb hook post-tool-use`; the input
`hook_event_name` distinguishes failure. Setup also adds the binary to
`exec_paths` and five exact `exec(/path/to/mdkb hook <event>)` rules to
`permissions.allow`. These rules admit only those hook commands; they do not
change ego's sandbox or other permission boundaries.

The config is `$EGO_HOME/config.toml`, or `~/.ego/config.toml` when `EGO_HOME`
is unset. Setup finds an installed mdkb binary instead of registering an ephemeral
build artifact. It removes any legacy `mdkb_prompt_command` key. It preserves
unrelated TOML values, comments, and config symlink targets. Repeated setup or
removal does not add duplicates. A user-modified hook (for example, one with a
matcher or a different timeout) is not replaced or removed. Removal deletes only
the exact generated hooks and matching execution allowances for the selected
mdkb binary. `--force` remains accepted but is not needed: setup merges entries.
`--dry-run` prints the merged TOML without writing it. Restart ego after setup.

For version-1 hook input, MDKB returns `hookSpecificOutput.additionalContext`
only for `SessionStart`, `UserPromptSubmit`, and `PostToolUse`, with an exact
`hookEventName` match. `PreToolUse` and `PostToolUseFailure` observations use
`version = 1`, `decision = "continue"`, and `feedback` instead: ego accepts these
as evidence, not model context. Internal reindex queue fields are not returned
to this closed protocol. Empty results produce no stdout. Claude Code, Codex and
Grok keep their existing output contract; Antigravity has its own (see
[Grok and Antigravity](#grok-and-antigravity)).

Ego's `Stop.transcript` is plain inline text, not a Claude transcript-file path
or MDKB's `episode_jsonl` format. The Stop hook settles without claiming prior
mining from that text. Hosts that need mining must provide the documented
[inline episode contract](#inline-episodes-ego-and-other-hosts-without-transcript-files).

### Disable individual events at install time

```bash
mdkb setup hooks claude --disable post-tool-use
mdkb setup hooks claude --disable user-prompt-submit,post-tool-use
```

Valid values: `session-start`, `user-prompt-submit`, `pre-tool-use`,
`post-tool-use`, `stop`.

### Dry run

```bash
mdkb setup hooks claude --dry-run
```

Prints the merged settings JSON to stdout without writing.

## Event contracts

Outside a project that can hold an mdkb store, lifecycle commands are successful
no-ops. They still consume the host's stdin payload before exiting, so a large
event does not fail with a broken pipe merely because no store is available.

Command handlers read the event JSON from stdin and write a JSON object to
stdout. Exit code is always 0. Native HTTP handlers send the same event object
to `POST /hook/{method}` and receive the same compact JSON-RPC envelope used by
the Unix hook socket. `cwd` becomes the repository `root` when the host does not
supply an mdkb-specific field. Dispatch errors, including malformed JSON, use
HTTP 200 with a JSON-RPC error; missing or invalid bearer credentials are
rejected before the body is parsed.

`mdkb setup hooks claude --http-url <base>` installs HTTP handlers for
UserPromptSubmit, PreToolUse, PostToolUse, and Stop. SessionStart remains a
command handler because Claude Code does not support HTTP for that event. Codex
setup remains command-based.

### SessionStart

Input: any JSON (ignored).

Output always includes a compact power-feature hint in an initialized
repository. The payload can contain, in order: a `## mdkb doctor` block, the
latest project-scoped handoff in full, due
reminders, ranked memory, an unextracted-relation notice, and the one-line
feature map. The map is present even when the memory index is empty; disabled
hooks and uninitialized repositories remain silent.

**Doctor block.** SessionStart runs the cheap `mdkb doctor` checks against the
store the session opened: hook registrations, a `config.toml` that does not
load, index quarantines, memory projection drift, shadow recall that records
nothing, and prior mining without a distiller. Each error or warning is one
line with its fix, for example
``- [error] hooks.drift: … → fix: `mdkb setup hooks claude` ``. The block lists
at most 5 findings and then points at `mdkb doctor`. It is absent when nothing
is wrong. `info` findings, such as "recall runs only on `*` prompts", appear
only in `mdkb doctor`. The live distiller probe runs only with
`mdkb doctor --full`. Keys in `.mdkb/config.toml` that nothing reads (removed
or misspelled) appear as a `config.unknown_keys` warning that names each key
and says to delete it; the daemon logs the same keys once per repository per
start. The daemon logs `slot held` at WARN only when a mutation keeps a store
slot for 1 second or more; shorter holds are DEBUG.

**Unextracted relations.** Under `graph.relations = "semi"` only, and only when
the detector found frontmatter keys that point at indexed documents and are not
being extracted, one line names them with their hit counts:

```
graph: 5 frontmatter keys point at indexed documents but are not extracted
(org 43, attendees 34, themes 28, +2). `mdkb graph relations` to see them,
`--apply` to extract. Set graph.relations="auto" to stop asking.
```

It is one line, it names at most three keys and counts the rest, and `auto` and
`manual` never emit it — `auto` because it already extracts them, `manual`
because being asked is the thing that mode opts out of.

```json
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "## mdkb memory warmup\n\n- [topic] …\n- [decision] …\n\n**mdkb:** `* query` = recall | `mdkb cheatsheet` = search/code/graph/audit/memory\n"
  }
}
```

### UserPromptSubmit

Input:

```json
{ "prompt": "how does the hook dispatcher work?" }
```

Empty or wrap-up prompts (`/clear`, `/compact`, `/exit`, `/quit`,
`/wrapup`) are skipped. Every other prompt is searched (automatic recall).
A leading `*`, for example `* how does writer recovery work?`, asks for an
answer explicitly and searches at a lower floor. Set
`user_prompt_submit_require_sigil = true` to opt out: a prompt without `*` then
passes through unchanged. The handler strips the sigil before search and
telemetry, then removes stopwords and sub-3-character fragments,
then ranks memory through hybrid BM25 and local-vector retrieval. Matching
documents reuse the same query embedding, avoiding a second ONNX inference pass.

The sigil selects a **cosine floor**, not a feature: a sigil prompt is admitted
at `search.memory.min_recall_cosine` (default 0.40 for MiniLM, 0.70 for Granite)
and — while `user_prompt_submit_require_sigil` is `false`, the default — a plain
one at `hooks.recall_auto_min_cosine` (default 0.50 for MiniLM, 0.782 for Granite),
because an injection nobody asked for is
charged on every turn after it. The floor is absolute, measured against the
query embedding. Automatic memory recall with an embedding must clear it;
incidental lexical overlap cannot bypass it. Explicit sigil recall and recall
without an embedding retain the strong lexical admission arm. Explicit threshold
overrides, including 0.0 to disable the semantic gate, remain unchanged when the
model changes. The memory setting also gates explicit memory search on the
CLI and MCP; it is not limited to the hook. Granite's 0.70 default is provisional,
selected on the English memory fixture, and does not establish a general
false-admission rate. Explicit floors are independent and can reverse the
default ordering. Confidence orders results and never admits them. When
nothing passes either admission arm, nothing is injected.

Output (when matches are found):

```json
{
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "## mdkb: relevant context\n\n- [hooks-topic] Hook dispatcher architecture — The mdkb hook dispatcher reads stdin and writes JSON to stdout …\n"
  }
}
```

**Doc-graph neighbors.** When the prompt names a document — a `.md` token, a
`/`-path, or a `[[wikilink]]` — the handler resolves it and appends up to 3
one-hop **frontmatter** graph neighbors that resolve to real documents, as a
compact `## mdkb: related docs` block (paths + relation labels only, no bodies).
Soft body-wikilink edges and non-document targets (e.g. `themes`, `owner` tags)
are skipped, and neighbors already surfaced as memory results are de-duplicated.
Controlled by `doc_graph_in_recall` (default `true`).

```
## mdkb: related docs

- data-model.md (related)
- auth-design.md (related)
```

### PreToolUse

Input (either the `Grep` tool or a `Bash` command):

```json
{
  "tool_name": "Grep",
  "tool_input": { "pattern": "handleAuth", "path": "src/" }
}
```

```json
{
  "tool_name": "Bash",
  "tool_input": { "command": "grep -rn handleAuth src/" }
}
```

Both `Grep` and `Bash` are intercepted (via the `matcher` field in
settings). Agents search code through `Bash` (`grep`/`rg`) far more than
the `Grep` tool, so matching `Bash` is where the redirect actually
reaches them. For `Bash`, the handler parses the command (quote-aware)
and only considers the *source* stage of a pipeline — `… | grep x`
filters stdout and is left alone, since mdkb cannot replace it. A bare
`grep PATTERN` with no `-r` and no path reads stdin and is likewise
skipped.

The extracted pattern is then classified:

- **Definition search** (e.g. `fn handle_auth`, `struct RepoHandle`) → if the
  symbol is in the code index, injects the real `file:line` hits ("act, not
  suggest"); otherwise falls back to suggesting `mdkb code search`.
  Controlled by `code_hits_in_pretooluse` (default `true`).
- **Pure identifier** (e.g. `handleAuth`) → suggests `mdkb code search`
- **Callsite pattern** (e.g. `handleAuth(`) → suggests `mdkb code callers`
- **Other patterns** (regex, alternation, single-file) → no suggestion (returns `{}`)

Output (definition search, symbol indexed — the "act" case):

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "additionalContext": "mdkb code index — `handle_auth` defined at:\n- src/auth.rs:42 (Function)\nRead the definition directly instead of grepping.\n"
  }
}
```

Context-only responses deliberately omit `permissionDecision`. Codex accepts
`"allow"` only when the hook also supplies `updatedInput` to rewrite the tool
call; including it here makes the hook fail validation.

The code-index lookup only fires for definition-classified searches and is
skipped entirely when `.mdkb/code.sqlite` is absent, so non-symbol searches
never pay for a DB open. The binary path is resolved via `current_exe()` so
fallback suggestions work regardless of installation location.

### PostToolUse

Input:

```json
{
  "tool_name": "Edit",
  "tool_input": { "file_path": "/abs/path/to/file.rs" }
}
```

Only `Edit`, `Write`, `MultiEdit`, `NotebookEdit` are tracked. For
notebooks the handler also reads `tool_input.notebook_path`.

Effect: sends the edited file path to the daemon's watcher channel
(`reindex_tx`) for immediate reindex. The path is first validated
via `canonicalize_under_cwd()` to reject traversal attempts.

Output: `{"queued": true}` on success, `{}` when skipped or on error
(PostToolUse must never return `additionalContext`).

### Stop

Stop is an end-of-episode signal. It always schedules a best-effort background
backfill for memory entries whose embedding could not be generated on their
write path. Behavioral-prior mining is separate and disabled by default. When
`[priors] mining_enabled = true` and `distiller_program` is configured, mdkb
reads a bounded transcript tail and launches the external distiller without
holding up the host. The hook itself returns `{}` immediately.

#### Inline episodes (ego and other hosts without transcript files)

`mdkb hook stop` reads a JSON object from stdin. The daemon method is
`hook.stop`. Send `session_id` (the real host session ID) and one episode source:

- `transcript_path`: the existing host transcript file path. mdkb reads its last
  800 lines in the background, as before.
- `episode_jsonl`: a string of complete Claude-shaped JSONL records, at most
  **800 records and 262144 UTF-8 bytes**. This is inline data, not a file path or
  ego's public session export. mdkb owns the payload before it returns.

Each inline record has `type` (`user` or `assistant`) and
`message.content` (a string or an array of blocks). Supported blocks are
`{"type":"text","text":"..."}`,
`{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"..."}}`,
and `{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"..."}`.
Tool input can also include `file_path`. Tool uses belong to assistant records;
results and real prompt text belong to user records. A record can include an
RFC 3339 `timestamp`; preserve the actual completion time on tool results so
settlement can distinguish failures before and after a prior was shown.

Example stdin (the newlines inside the string are JSON escapes):

```json
{"session_id":"ego-session","episode_jsonl":"{\"type\":\"user\",\"message\":{\"content\":\"No, edit the generator template instead.\"}}\n"}
```

Inline data uses the same episode parser, prior settlement, recall settlement,
and optional miner as file data. Settlement does not require mining to be on.
If `episode_jsonl` is present, it takes precedence over `transcript_path`.
A non-string, malformed record, unsupported block, or over-limit payload is
refused as a whole with a logged reason. It is never truncated or replaced by
the file source. Empty inline text is an empty episode. Blank JSONL records are
malformed. The hook still exits 0 and emits no host instructions (`{}` at RPC).
Embedding backfill remains independent of episode acceptance.

Delivery is best-effort, not exactly-once: `session_id` correlates observations,
but Stop has no persistent occurrence deduplication. Inline data can be sent to
an external or remote-backed `distiller_program` only when the operator has
configured mining; this input form does not enable it.

#### The distiller contract

The distiller is any CLI, and the contract is three rules:

- **The prompt goes on stdin**, so it never lands in argv or a process listing.
  A CLI that reads its prompt from an argument instead opts out by putting the
  literal `{prompt}` in `distiller_args`: it is substituted there and stdin is
  closed, so a CLI that would block on an unwritten pipe cannot hang mining.
- **stdout must contain one JSON object** matching the schema the prompt states.
  Everything before the first `{` and after the last `}` is discarded, so a
  ` ```json ` fence or a prose preamble is fine. Output with no braces at all is
  rejected — no agent CLI reliably prints bare JSON, but none of them should be
  able to pass off an apology as a prior either.
- **stderr is never parsed**, because that is where codex writes its progress.
  It is not thrown away: when a run fails with an empty stdout, stderr is what
  the failure line quotes — a rejected model reports its HTTP status there and
  nowhere else.

A distiller that cannot be spawned, exits non-zero, or prints no JSON object is
logged at **warn** with its exit code and the first 200 characters of what it
said. A well-formed answer the validator turns down stays at debug: most
episodes teach nothing, and warning about those would bury the other kind.

Configure it in `~/.mdkb/daemon.toml` under `[priors]` (global base) or in a
repo's `.mdkb/config.toml` (override), then verify it:

```bash
mdkb doctor --full    # runs the configured CLI once, prints the prior or the failure
```

Every option below was checked that way on 2026-09-16. Times are `setup check`
wall-clock, so they include process start; all four returned a valid prior.

| CLI | Args | Time | Note |
|---|---|---|---|
| `codex` | `exec --ignore-user-config -m gpt-5.6-luna -c model_reasoning_effort="low" -s read-only --skip-git-repo-check` | 8-9s | Default. `--ignore-user-config` is the only working way to skip MCP startup; `-c 'mcp_servers={}'` is a silent no-op. `gpt-5.4-mini` is rejected (HTTP 400) on a ChatGPT account. |
| `ollama` | `run gemma4:12b-mlx --think=false --hidethinking --nowordwrap --format json` | 18s cold, 2-5s warm | Local, no quota. `gemma4:e4b-mlx` invents trigger kinds; do not use it. |
| `claude` | `-p --model claude-haiku-4-5-20251001 --setting-sources "" --strict-mcp-config --tools "" --no-session-persistence` | 19s | Subscription login. Wraps the JSON in a fence. Never `--bare`: it drops the login. |
| `grok` | `--no-auto-update -p {prompt} -m grok-4.5 --tools "" --no-subagents --no-plan --deny 'mcp__*'` | 33s | Reads the prompt from argv only, hence `{prompt}`. `--deny 'mcp__*'` keeps MCP protocol prose out of the answer. |

## Configuration

All toggles live under `[hooks]` in `.mdkb/config.toml`:

```toml
[hooks]
session_start_enabled = true
user_prompt_submit_enabled = true
pre_tool_use_enabled = true
post_tool_use_enabled = true

# Automatic recall is the default (false). Set true to opt out and keep
# normal prompts untouched unless they begin with `*`.
user_prompt_submit_require_sigil = false

# The cosine floor for a prompt that carries no sigil. A `*`-prefixed
# prompt uses the lower `search.memory.min_recall_cosine` (MiniLM default 0.40,
# Granite default 0.70; automatic Granite default 0.782). Explicit values win.
# The sigil selects a threshold, it does not switch recall on. Unused when
# require_sigil is true, except in shadow mode.
recall_auto_min_cosine = 0.50

# Run the always-on path on the prompts the sigil gate skips, record
# what it would have injected in .mdkb/hook-events.jsonl, inject
# nothing. Only meaningful with require_sigil = true.
user_prompt_submit_shadow = false

# Warmup is bounded by both entry count and tokens.
warmup_limit = 10
warmup_token_budget = 300
warmup_min_confidence = 0.25

# Max recall results injected on UserPromptSubmit.
recall_limit = 5

# Max matching documents injected alongside the memory recall, from
# the same hybrid engine as `mdkb search --docs`. 0 = memory only.
recall_docs_limit = 3
recall_docs_min_cosine = 0.57   # a doc must score this cosine or be quoted by the prompt (Granite default 0.77)

# Latency budget in milliseconds. A run over budget is copied to
# .mdkb/hook-slow.jsonl. Nothing is truncated: the value decides
# what gets flagged, not what gets emitted.
latency_budget_ms = 200

# Hard deadline for UserPromptSubmit in milliseconds. Unlike the budget it cuts
# the work short: past it the hook answers with nothing and logs
# outcome = "deadline". The hook client waits this plus 250 ms, so raising it
# raises the wait the host sees. 0 disables the daemon-side cut; the client then
# waits the default (1000) plus 250 ms.
user_prompt_submit_deadline_ms = 1000

# Embeddings are selected per store in [embedding], outside this [hooks] table.
# model = "minilm" (default, English only) or "granite107m" (multilingual FP32).
# Use granite107m when memory, docs or prompts use any language other than
# English: MiniLM does not match a translation to its source, and recall can
# inject both language copies of one entry.
# Serving requests use cached weights only. A background migration worker can
# fetch missing weights when the daemon encounters an incompatible writable store.
# Model changes trigger background regeneration; lexical retrieval stays available.
# `mdkb update --reembed` remains available for an explicit full rebuild or retry.
# All hooks.recall_rerank_* keys have been removed and are configuration errors.

# Require daemon delivery instead of using the in-process fallback.
daemon_required = false

# PreToolUse: inject real code-index file:line hits for definition
# searches (fn/struct/…) instead of a suggestion. Falls back to the
# suggestion when the symbol is not indexed.
code_hits_in_pretooluse = true

# UserPromptSubmit: inject up to 3 one-hop frontmatter doc-graph
# neighbors when the prompt names a document.
doc_graph_in_recall = true
```

Defaults are safe for interactive use; tune `recall_limit` and
`recall_docs_limit` higher if you want more context, lower if the
assistant is getting too much noise on every prompt.
`code_hits_in_pretooluse` and `doc_graph_in_recall` independently kill
the two graph/index injectors if you want the plain suggestion /
memory-only behavior.

### Measuring recall in a development repository

```bash
mdkb setup developer
mdkb status
mdkb metrics quality --period 7
```

The developer profile enables `[telemetry] query_events` with 30-day retention
by default. It records result count, final top score, and latency, but never the
prompt text. The correlation identifier is an HMAC-SHA-256 value keyed by a
random repository-local `.mdkb/telemetry.key`, so an exported database does not
permit an offline dictionary attack without that key. Use
`mdkb metrics purge --yes` to remove every stored query event. Restart the
daemon after changing the profile because repository configuration is cached by
the active daemon handle.

Query telemetry is a choice of the developer, so it can also be enabled for
every repository on the machine, without a change to a tracked
`.mdkb/config.toml`. Set it in `~/.mdkb/daemon.toml` and restart the daemon:

```toml
[telemetry]
query_events = true
retention_days = 30   # optional, 1 to 365
```

The order of precedence is: built-in default, then `daemon.toml`, then the
`[telemetry]` section of the repository, key by key. A repository that sets
`query_events = false` explicitly stays disabled. A repository that does not
set the key uses the `daemon.toml` value. `mdkb status` reports the
merged value as it reads the files now. A running daemon reads `daemon.toml`
only at startup, so it records with the new value only after a restart.

Query telemetry has no denominator because it records only activated recalls.
Use `mdkb status` for engagement: the Hooks row for `user_prompt_submit` reports
all calls, fired recalls, and Hit%. Use `mdkb metrics quality` for retrieval
quality after activation. Low Hit% points to discoverability or opt-in friction;
high zero-result rate or low score bands point to retrieval/index quality.
These are technical proxies: a high ranking score is not a user judgment, and
cross-language prompts can still return irrelevant high-scoring matches. MDKB
does not claim helpfulness without an explicit feedback signal.

`mdkb cheatsheet` is the compact command inventory intended for agents. It
covers search and batch reads, durable memory/provenance, code relationships,
knowledge graph navigation, duplication/coupling audits, collection mutation,
developer telemetry, maintenance, daemon control, and the machine-readable CLI
schema.

## Opt out

Three ways, in order of granularity:

1. **Per-project file marker** — create an empty `.mdkbignore-hooks`
   file at the repo root. All hooks return `{}` immediately for
   any working directory under that marker. Useful for one-off repos
   where you do not want mdkb to participate even if hooks are
   globally installed.
2. **Per-event config toggle** — disable SessionStart, UserPromptSubmit,
   PreToolUse, or PostToolUse in `.mdkb/config.toml`. Stop mining has its own
   `[priors] mining_enabled` switch and is off by default.
3. **Uninstall** — `mdkb setup remove hooks claude --scope local|user`
   or `mdkb setup remove hooks codex`. Or remove the `_managedBy: "mdkb"`
   entries manually from the settings file.

The `.mdkbignore-hooks` marker is looked up by walking ancestor
directories up to `$HOME`; it is never searched above the user home
directory.

## Troubleshooting

### Hooks aren't firing

1. Restart the host CLI after `mdkb setup hooks …`.
2. Verify the settings file contains an `mdkb hook <event>` entry for
   the relevant event.
3. Run the dispatcher manually:

   ```bash
   echo '{}' | mdkb hook session-start
   echo '{"prompt":"test"}' | mdkb hook user-prompt-submit
   ```

   Both should print a JSON object to stdout and exit 0.

### Recall is empty

- If `user_prompt_submit_require_sigil = true` is set, recall requires a leading
  `*`. Use `* your prompt`, or remove the key (default `false`) for automatic
  recall at the stricter `recall_auto_min_cosine` floor.
- Hybrid recall requires at least one indexed memory entry. Run `mdkb memory
  list` and confirm the DB is populated.
- Conversational prompts with only stopwords (e.g. "what is this?")
  produce no tokens and are skipped by design.
- Nothing cleared the floor. That is the gate working, not a failure: recall is
  absolute, so a store with no answer injects nothing rather than its best
  guess. `mdkb search --memory "<prompt>"` shows what was there.

### Shadow mode

`user_prompt_submit_shadow = true` runs the always-on path on the prompts the
sigil gate skips, records what it *would* have injected, and injects nothing.
It measures the always-on path for a repo that opted out with
`require_sigil = true`. The default flipped on 2026-09-30 without that week,
and the eval fixture cannot stand in for it: it scores precision 1.000 at every
floor from 0.40 up and so cannot rank them.

```json
{"ts":…,"event":"user_prompt_submit","outcome":"shadow","elapsed_ms":41,
 "shadow":{"session":"…","entries":["writer-recovery-protocol"],"docs":1,
           "related":0,"top_cosine":0.62,"floor":0.5}}
```

`entries` names the memory ids rather than counting them, because the release
criterion is precision and a count cannot be judged after the fact. No prompt
text is recorded, as in every other row. The release criteria are all four
together — injection rate, precision, repetition rate and P95 `elapsed_ms` —
and the README's two-floor section says how to read each off these rows.

Shadow mode does not touch the per-session dedup map or the behavioural-prior
injection counters: a write to either would change what a later real injection
does, which would corrupt the counters it is there to produce.

### Slow hooks

Every hook run appends a row to `.mdkb/hook-events.jsonl`; a run that exceeds
`latency_budget_ms` is copied to `.mdkb/hook-slow.jsonl` as well. Exceeding the
budget truncates nothing — the value only decides what gets flagged.

`UserPromptSubmit` is the exception: `user_prompt_submit_deadline_ms` (default
1000, `0` disables) stops recall when it has not finished, answers with nothing
and logs `outcome = "deadline"`. The hook client waits that deadline plus a fixed
250 ms margin, so the deadline you set is the one the host sees.
`mdkb status` counts these as deadline hits.
Its rows carry a `phases` object (`prepare`, `context`, `embed`, `lock_wait`,
`search`, `enrich`, `prior`, `render`, and `between_phases`, in ms).
Durations include the open interval when cancellation interrupts work;
`search` includes blocking-pool queue time and execution, not just SQLite time.
`between_phases` collects intervals after a completed phase and before the next
starts. `unaccounted_ms` covers dispatcher overhead and millisecond rounding:
the numeric phase durations plus this residual equal `elapsed_ms` within 5 ms.
Deadline rows include `phases.deadline_phase`, the phase active when recall was
cut, even before the first phase completes. These are daemon wall-clock timings;
they do not measure the hook client's socket wait. Completed shadow rows retain
their `shadow` object; interrupted shadow runs carry both `shadow` and `phases`.

SessionStart rows carry the split of `elapsed_ms` across the six phases, in the
order they run:

```json
{"ts":…,"event":"session_start","outcome":"fired","elapsed_ms":494,
 "phases":{"context":132,"warmup":360,"handoff":0,"stale_deps":0,"relations":0,"code_check":0}}
```

`context` is opening the store (schema check included), `warmup` the ranked
entry query plus the document, collection and projection counts, `handoff` the
newest-handoff lookup, `stale_deps` the dependency graph check, `relations` the
stored-aggregate read behind the undetected-relation-keys notice, and
`code_check` the read-only open of `code.sqlite`.

**Measured 2026-09-17 on a 615-document store**: the first SessionStart against
a daemon that has not opened the repo yet costs 494 ms — 360 ms `warmup`, 132 ms
`context` — and every run after it costs **1 ms**. The budget is exceeded by
first-touch cost, not by the steady-state work, so a mean over all recorded runs
mostly measures how often the daemon was cold.

### Edited files not reindexing

Command hooks auto-start the daemon and fall back in-process unless
`daemon_required = true`; check `mdkb status` if delivery fails. Native
HTTP hooks require the configured HTTP/HTTPS server to remain running and
`MDKB_HOOK_TOKEN` to match its bearer token. `mdkb update` remains the safe full
differential fallback.

Watcher admission applies collection globs and Git/ignore-file rules, including
common Git excludes in linked worktrees. A shared exclusion union of code
patterns, document defaults and `.tmp` gates all routes. This is not full-walk
parity for custom patterns: code-only exclusions can suppress documents, and
force-included documents below excluded directories require `mdkb update`.
Ordinary edits index only delivered paths. Vanished files and subtrees remove
stored rows on an exact-path or slash-prefix match, without a tree walk.
Existing directories and relevant ignore-file changes still trigger full
membership reconciliation. Git and database churn is rejected before queue
admission; the active `memory/entries/*.md` projection remains an input.
Pending work is bounded to 1024 paths. Overflow and membership recovery are
coalesced per repository with a 30-second cooldown; ordinary targeted work does
not wait for that recovery cooldown. Failed document batches retain their paths
(or full request), warn with the repository root, and retry after 30 seconds.
Per-file errors also retain the batch. These bounds control recovery work, not
a promise that every change is indexed within 30 seconds.

Removed rerank keys refuse daemon admission, MCP startup, lifecycle-hook policy
loading and document watcher updates with the replacement in the logged error.
No legacy values are translated. Lifecycle hooks skip the event and exit 0;
they do not run with defaults. `doctor` remains available to report invalid
configuration. Other malformed files keep the existing logged whole-file fallback
at cold start. Malformed hot-reloads keep the daemon's last good configuration,
not factory defaults; removed rerank keys refuse new requests.

## Automated verification

The hook contract is covered end-to-end by `tests/e2e_hooks.rs`,
which spawns the real `mdkb` binary and asserts that warmup, recall,
and immediate watcher delivery match the spec. HTTP/Unix parity, authentication,
root admission, and JSON-RPC errors are covered in `src/mcp/common.rs` and
`tests/e2e_mcp_http.rs`.

## Triggered memory rules

`--trigger` attaches a JSON matcher to a `topic`, `problem` or `decision`
entry. The hooks then inject the entry exactly where it applies, at most once
per session, instead of waiting for recall to find it:

```bash
mdkb memory add use-mbx -t "Build through mbx" -T decision -c "Run cargo through the mbx shim." \
  --trigger '{"tool":"Bash","command_contains":"cargo"}'
mdkb memory add auth-rule -t "PKCE only" -T decision -c "..." \
  --trigger '{"prompt_contains":"oauth"}' --trigger '{"path_glob":"src/auth/**"}'
```

| Selector | Matches | Case |
|---|---|---|
| `tool` | the tool name, before or after the call | insensitive |
| `path_glob` | a glob over the call's repo-relative path | sensitive |
| `command_contains` | a substring of the shell command | sensitive |
| `error_contains` | a substring of a failed call's error text (after the call only) | sensitive |
| `prompt_contains` | a substring of the user's prompt | insensitive |
| `when` | free prose for readers; never matched | — |

The selectors in one matcher must all match. Repeat `--trigger` to give
alternatives, of which any one is enough. `prompt_contains` cannot be combined
with tool selectors, and a matcher needs at least one selector. An entry that is
expired, not active, or refuted since its last confirmation is not injected.
MCP `memory_write` takes the same matchers as `triggers`.

## Daemon client commands

`mdkb hook search`, `hook memory-write`, `hook memory-confirm` and
`hook reindex` send one request to the daemon over its hook socket and print
the answer. They exist for hook wiring and scripts that must not open the
store themselves; for interactive use, prefer `mdkb search`, `memory add`,
`memory confirm` and `update`. `hook memory-write` takes `--id`, `--title`, `--entry-type`, `--content`,
`--tags` and `--ttl` (seconds); `hook memory-confirm` takes `--id` and
`--outcome`.

## Grok and Antigravity

Both run mdkb through the same dispatcher; what reaches the model differs by
host. Measured on grok 1.0.89 and agy 1.5.10, 2026-11-23.

**Antigravity CLI (`agy`)** has its own hook protocol. `mdkb setup hooks agy`
adds an `mdkb` group to `~/.gemini/config/hooks.json` whose commands call
`mdkb hook agy <event>`, which translates agy's input and answers with
`injectSteps`:

| agy event | mdkb work |
|---|---|
| `SessionStart` | memory warmup, injected as a system message |
| `PreInvocation` | prompt recall, on the first model call of each prompt only (`invocationNum` 0) |
| `PostToolUse` | reindex of the file a write or replace tool changed |
| `Stop` | end-of-turn work; no prior mining, because agy's transcript is not the format mining reads |

No `PreToolUse` hook is registered: agy denies a tool call when a pre-tool hook
answers without a decision. Remove with `mdkb setup remove hooks agy`.

**Grok CLI** uses the Claude Code hook format, and by default it already runs
the hooks in `~/.claude/settings.json` (`[compat.claude] hooks` in
`~/.grok/config.toml`) and the MCP servers in `~/.claude.json`. Grok drops the
context of `SessionStart` and `UserPromptSubmit`, so warmup and prompt recall
cannot reach a Grok model through hooks; use the mdkb MCP tools there.
`mdkb setup hooks grok` writes `~/.grok/hooks/mdkb.json` with the events whose
context Grok delivers (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
`Stop`). It refuses while Grok already runs the mdkb hooks of Claude Code, which
would run each event twice; set `[compat.claude] hooks = false` or pass
`--force`.
