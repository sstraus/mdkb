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
- **UserPromptSubmit** — match the user's prompt against the memory FTS
  index and inject the top-N entries before the assistant replies; when the
  prompt names a document, also inject its 1-hop frontmatter doc-graph
  neighbors.
- **PreToolUse** — intercept `Grep`/`Bash` searches; on a definition search for
  an indexed symbol inject the real `file:line` from the code index, otherwise
  suggest `mdkb search` / `mdkb code` CLI commands. Works without MCP.
- **PostToolUse** — when `Edit` / `Write` / `MultiEdit` / `NotebookEdit`
  touches a file, append it to `.mdkb/reindex-queue.jsonl` so the next
  `mdkb update` pass picks it up.

All hooks are fire-and-forget: internal errors are logged to stderr and
swallowed — the host CLI is never blocked by mdkb.

## Install

```bash
# Claude Code, project-scoped (writes .claude/settings.local.json)
mdkb setup hooks claude --scope local

# Claude Code, user-scoped (writes ~/.claude/settings.json)
mdkb setup hooks claude --scope user

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

Valid values: `session-start`, `user-prompt-submit`, `pre-tool-use`, `post-tool-use`.

### Dry run

```bash
mdkb setup hooks claude --dry-run
```

Prints the merged settings JSON to stdout without writing.

## Event contracts

Every handler reads the event JSON from stdin and writes a JSON object
to stdout. Exit code is always 0. When a hook has nothing to contribute,
it returns `{}` (empty object).

### SessionStart

Input: any JSON (ignored).

Output (when memory is non-empty):

```json
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "## mdkb memory warmup\n\n- [topic] …\n- [decision] …\n"
  }
}
```

### UserPromptSubmit

Input:

```json
{ "prompt": "how does the hook dispatcher work?" }
```

Empty or wrap-up prompts (`/clear`, `/compact`, `/exit`, `/quit`,
`/wrapup`) are skipped. The handler tokenizes the prompt, strips
stopwords and sub-3-char fragments, and runs an FTS5 OR query against
the memory index.

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

## Configuration

All toggles live under `[hooks]` in `.mdkb/config.toml`:

```toml
[hooks]
session_start_enabled = true
user_prompt_submit_enabled = true
pre_tool_use_enabled = true
post_tool_use_enabled = true

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

## Opt out

Three ways, in order of granularity:

1. **Per-project file marker** — create an empty `.mdkbignore-hooks`
   file at the repo root. All three hooks return `{}` immediately for
   any working directory under that marker. Useful for one-off repos
   where you do not want mdkb to participate even if hooks are
   globally installed.
2. **Per-event config toggle** — set
   `session_start_enabled = false` (or the other two) in
   `.mdkb/config.toml`.
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

- `search_entries_fts` requires at least one indexed memory entry. Run
  `mdkb memory list` and confirm the DB is populated.
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

PostToolUse sends edited paths to the daemon's watcher channel. If
the daemon is not running, the path is lost. Restart the daemon with
`mdkb daemon restart` or run `mdkb update` for a full differential
reindex.

## Automated verification

The hook contract is covered end-to-end by `tests/e2e_hooks.rs`,
which spawns the real `mdkb` binary and asserts that warmup, recall,
and reindex-queue output matches the spec.
