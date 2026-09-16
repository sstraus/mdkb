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
repository. The payload can contain, in order: an outstanding quarantine or
projection-drift warning, the latest project-scoped handoff in full, due
reminders, ranked memory, and the one-line feature map. The map is present even
when the memory index is empty; disabled hooks and uninitialized repositories
remain silent.

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
`/wrapup`) are skipped. By default, recall also requires a leading `*` opt-in
sigil, for example `* how does writer recovery work?`. The asterisk is explicit
consent to search the current repository's context. Without it, the prompt
passes through unchanged. The handler strips the sigil before search and
telemetry, then removes stopwords and sub-3-character fragments,
then ranks memory through hybrid BM25 and local-vector retrieval. The configured
floor applies to the final relevance-plus-confidence score, not confidence
alone. Matching documents reuse the same query embedding, avoiding a second
ONNX inference pass.

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
  suggest"); otherwise falls back to suggesting `mdkb search --scope symbols`.
  Controlled by `code_hits_in_pretooluse` (default `true`).
- **Pure identifier** (e.g. `handleAuth`) → suggests `mdkb search --scope symbols`
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
mdkb setup check    # runs the configured CLI once, prints the prior or the failure
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

# Keep normal prompts untouched unless they begin with `*`.
user_prompt_submit_require_sigil = true

# Warmup is bounded by both entry count and tokens.
warmup_limit = 10
warmup_token_budget = 300
warmup_min_confidence = 0.25

# Max recall results injected on UserPromptSubmit.
recall_limit = 5

# Max matching documents injected alongside the memory recall, from
# the same hybrid engine as `mdkb search --scope docs`. 0 = memory only.
recall_docs_limit = 3

# Latency budget in milliseconds. If a hook exceeds this,
# the overrun is appended to .mdkb/hook-slow.jsonl and the
# output may be truncated with a notice.
latency_budget_ms = 200

# Minimum hybrid score for a recall result to be injected.
min_recall_score = 0.3

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
mdkb metrics status
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

Query telemetry has no denominator because it records only activated recalls.
Use `mdkb stats` for engagement: the Hooks row for `user_prompt_submit` reports
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

- Recall requires a leading `*` by default. Use `* your prompt`, or set
  `user_prompt_submit_require_sigil = false` for always-on recall.
- Hybrid recall requires at least one indexed memory entry. Run `mdkb memory
  list` and confirm the DB is populated.
- Conversational prompts with only stopwords (e.g. "what is this?")
  produce no tokens and are skipped by design.

### Slow hooks

Any hook that exceeds `latency_budget_ms` logs a line to
`.mdkb/hook-slow.jsonl`:

```json
{"event":"session-start","elapsed_ms":412,"budget_ms":200,"ts":…}
```

Use this to tune the budget or diagnose cold-start issues.

### Edited files not reindexing

Command hooks auto-start the daemon and fall back in-process unless
`daemon_required = true`; check `mdkb daemon status` if delivery fails. Native
HTTP hooks require the configured HTTP/HTTPS server to remain running and
`MDKB_HOOK_TOKEN` to match its bearer token. `mdkb update` remains the safe full
differential fallback.

## Automated verification

The hook contract is covered end-to-end by `tests/e2e_hooks.rs`,
which spawns the real `mdkb` binary and asserts that warmup, recall,
and immediate watcher delivery match the spec. HTTP/Unix parity, authentication,
root admission, and JSON-RPC errors are covered in `src/mcp/common.rs` and
`tests/e2e_mcp_http.rs`.
