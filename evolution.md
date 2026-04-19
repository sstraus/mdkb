---
status: completed
approved_at: "2026-04-18T08:03:57.034Z"
updated: "2026-04-18T14:28:35.747Z"
started_at: "2026-04-18T08:10:41.925Z"
completed_at: "2026-04-18T14:22:26.248Z"
archived_at: "2026-04-18T14:28:35.746Z"
---
# Implementation Plan: OpenWolf + claude-hooks Gap Analysis → mdkb Hook Layer

**Created:** 2026-04-18
**Status:** Complete
**Estimated Effort:** L (4–6 focused sessions)
**Branch:** TBD (suggest `poc/hook-layer` or Jira issue prefix)
**Depends on:** `plans/strategic-vision.md` (Phase 3B instruction rewrite ideally first), `plans/plan-get-claude-code-to-actually-use-mdkb-tools-2.md` (CLAUDE.md injection pattern)

---

## Summary

mdkb today is a **passive MCP server**. Claude must choose to call `search`, `memory_write`, `code_graph`. When Tool Search mode activates (inevitable with 20+ aggregate MCP tools), adoption collapses — which is exactly the pain Boss reports: *"Claude usa poco mdkb"*.

OpenWolf (`cytostack/openwolf`, TypeScript, AGPL-3.0) and claude-hooks (`mann1x/claude-hooks`, Python, MIT) both prove the missing layer: **lifecycle hooks that push context into Claude's workflow instead of waiting to be pulled**. OpenWolf reports 65.8% token reduction and ~85–90% corrections compliance across 20+ projects; claude-hooks reports 524+ test cases with deterministic recall and explicit Stop Guard compliance patterns.

This plan adopts only the ideas that are **non-redundant with mdkb's existing capabilities** and **buildable in Rust without dragging Node/Python/Ollama into the binary**. Concretely:

1. **Bundled hook layer in mdkb** — `mdkb hook <event>` subcommands registered by `mdkb setup mcp claude` into `.claude/settings.json`. Thin-client over unix socket to the long-running `mdkb serve` process (mono-process: same binary hosts MCP stdio AND hook socket in parallel tasks, sharing DB/embedder state). See §Architecture: hook transport below.
2. **Two foundational hooks: SessionStart + UserPromptSubmit** — transform mdkb from pull to push on the highest-leverage events.
3. **Token ledger visibility** — expose the existing `src/metrics/` via a new `usage` tool, plus per-file token estimates in `code_files`.
4. **Explicit handoff paragraph for wiz-agents** — Stop Guard, HyDE, Safety Scan, Instinct extraction deferred to the sibling project. See §7 for the paragraph to paste into wiz-agents.

Everything else in OpenWolf (anatomy, cerebrum, buglog) and claude-hooks (HyDE, attention decay, Safety Scan, Stop Guard, Instinct extraction) is either **already solved by mdkb** (code_graph, memory confidence model, Ebbinghaus decay, semantic dedup) or **out of scope** for a knowledge base.

---

## Research Findings

### OpenWolf (cytostack/openwolf) — TypeScript, AGPL-3.0

Hook-based "second brain" for Claude Code. Six Node.js lifecycle hooks fire on every file read / code write / session completion, writing to a `.wolf/` directory:

| File | Purpose | Overlap with mdkb |
|---|---|---|
| `anatomy.md` | Project file map with token estimates | **Redundant** — mdkb `code_graph` + `search(scope="symbols")` does more; but token estimate per file is new value |
| `cerebrum.md` | Learning memory: do-not-repeat lists, preferences, corrections | **Redundant** — mdkb memory + confidence model (Phase 2A) covers this better |
| `memory.md` | Chronological action log | **Redundant** — mdkb indexes Claude session JSONL files already (`src/domain/sessions.rs`) |
| `buglog.json` | Bug memory preventing re-discovery | **Redundant** — `memory_write(entry_type="problem")` already does this |
| `token-ledger.json` | Lifetime + session token tracking | **Net new value** — mdkb has `src/metrics/` but nothing exposes it |
| Design QC | Full-page screenshot evaluation | **Out of scope** for a knowledge base |

**Architectural lesson:** The *intelligence* mdkb already has (code graph, hybrid search, confidence). The *delivery mechanism* (hooks firing on every Claude action) is missing.

### claude-hooks (mann1x/claude-hooks) — Python, MIT

Four event-driven hooks with rich side-channels (Qdrant vector DB, Ollama LLM, optional proxy):

| Hook | Function | Fit for mdkb |
|---|---|---|
| UserPromptSubmit (HyDE + recall + decay + inject) | Expand query via LLM, recall from vector DB, inject context before Claude sees prompt | **Core idea yes, LLM dep no** — mdkb can do the recall+inject part in Rust against its own SQLite. HyDE defers to wiz (needs Ollama). |
| Stop (classify + dedupe + store + instincts) | Store turn summary, extract reusable rules from error→fix patterns | **Defer to wiz-agents** — `plans/wiz-auto-memory-hooks.md` already owns this |
| SessionStart (compact recall) | Re-inject full memory after context compaction | **Core idea yes** — mdkb already computes warmup index; just needs a hook to emit it on compaction recovery |
| PreToolUse (safety scan + rtk rewrite + stop guard) | Block dangerous bash, rewrite verbose commands, block premature stops | **Out of scope for mdkb** — Stop Guard belongs in wiz-agents (generic CC workflow enforcement); safety scan belongs in security tooling |

**Architectural lesson:** All hooks exit 0 even on failure (non-blocking recall). PreToolUse uses `permissionDecision: "ask"` for safety, never auto-denies. Opt-out via `.claude-hooks-disable` marker. These patterns are worth mirroring.

### mdkb codebase (current state, 2026-04-18)

- **Passive MCP server, zero hooks** — confirmed by exploration (`Agent` exploration, see session research).
- **9 MCP tools**: `search`, `get`, `status`, `update`, `memory_write`, `memory_write_batch`, `memory_delete`, `memory_list`, `code_graph`. All pull-only.
- **BASE_INSTRUCTIONS** at `src/mcp/server.rs:2423` (~350–400 tokens). Currently uses documentation framing that assumes tools are pre-loaded — breaks under Tool Search. **Already planned to rewrite** in `plan-get-claude-code-to-actually-use-mdkb-tools-2.md`.
- **Warmup index** (`get_warmup_index`) already injects top-N memories into BASE_INSTRUCTIONS at startup. This is the closest thing mdkb has to a push mechanism — but it only fires on MCP `initialize`, not on compaction recovery or mid-session.
- **Session indexing** (`src/domain/sessions.rs`) reads Claude session JSONL — mdkb knows where session state lives. A hook can write back to trigger reindex.
- **Metrics exist but hidden**: `src/metrics/` has `UsageMetrics` and token counting; no tool exposes it.
- **Setup command**: `mdkb setup mcp claude` writes MCP config to `.claude/`. Natural home to also register hook entries.

### Gaps Identified

1. **No push channel.** mdkb cannot inject content into a turn; it can only respond when called. This is the root cause of low adoption.
2. **No SessionStart awareness.** After `/clear` or auto-compaction, warmup memories disappear from Claude's context and the next MCP `initialize` may happen much later (or never, if tools are deferred).
3. **No per-turn recall.** Every user prompt is a fresh start; relevant memories go unseen unless Claude proactively searches.
4. **No token economy visibility.** Boss complains about waste; mdkb measures but doesn't surface.
5. **Rust-only delivery constraint.** Adopting OpenWolf/claude-hooks directly would pull Node/Python into the install story, violating mdkb's "single binary, zero runtime deps" identity (`strategic-vision.md` design principles).

---

## Comparative Feature Matrix

Legend: ✅ = have it, ⚠️ = partial / not exposed, ❌ = missing, N/A = out of scope

| Capability | mdkb | OpenWolf | claude-hooks | Decision |
|---|---|---|---|---|
| Hybrid BM25 + vector search | ✅ docs + memory (after Phase 1A) | ❌ | ⚠️ Qdrant only | **keep mdkb's** |
| Code call graph + impact | ✅ 13 languages | ❌ | ❌ | **unique mdkb advantage — amplify** |
| Memory confidence / decay | ✅ Ebbinghaus + Bayesian | ⚠️ cerebrum.md (no scoring) | ⚠️ attention decay | **keep mdkb's** |
| Semantic dedup on write | ✅ (0.32 L2 ≈ 0.95 cos) | ❌ | ⚠️ 0.85 cos Qdrant | **keep mdkb's** |
| Session JSONL indexing | ✅ | ⚠️ memory.md only | ⚠️ episodic-server opt-in | **keep mdkb's** |
| SessionStart push | ❌ | ✅ anatomy + cerebrum | ✅ compact recall | **BUILD in mdkb** |
| UserPromptSubmit recall injection | ❌ | ❌ | ✅ HyDE + vector recall | **BUILD in mdkb (no HyDE)** |
| PostToolUse reindex trigger | ❌ | ✅ on write | ⚠️ claudemem detached | **BUILD in mdkb (small)** |
| Stop Guard (premature abandonment) | ❌ | ❌ | ✅ regex on assistant output | **DEFER to wiz-agents** |
| Safety Scan (bash PreToolUse) | ❌ | ❌ | ✅ pattern match + `ask` | **DEFER to wiz-agents** |
| HyDE query expansion | ❌ | ❌ | ✅ Ollama LLM | **DEFER to wiz-agents (LLM dep)** |
| Instinct extraction (auto-rules) | ❌ | ❌ | ✅ error→fix mining | **DEFER to wiz-agents** |
| Token ledger / usage stats | ⚠️ internal only | ✅ lifetime + session | ✅ proxy-based | **EXPOSE mdkb's metrics** |
| File token estimates | ❌ | ✅ anatomy.md | ❌ | **BUILD (column + search metadata)** |
| Bug memory | ✅ via memory_write | ✅ buglog.json | ⚠️ observations | **no action — document pattern in CLAUDE.md template** |
| Directive tool descriptions (Serena-style) | ❌ | N/A | N/A | **BUILD — already in sibling plan** |
| `.claude/rules/mdkb.md` injection | ❌ | ❌ | ❌ (managed by users) | **BUILD — already in sibling plan** |
| Opt-out marker (`.claude-hooks-disable`) | ❌ | ❌ | ✅ per-directory | **BUILD — mirror pattern as `.mdkbignore-hooks`** |

---

## Questions to Resolve

### Critical (P1 — blockers)

1. **Which hook events does Claude Code 2026 actually expose, and what's their contract?**
   - Default: assume `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `PreCompact` per claude-hooks source. Verify against current Claude Code docs before Step 1. If contract changed, adjust hook signatures.

2. **Hook output injection mechanism.** UserPromptSubmit must return context that ends up in Claude's prompt — format TBD (stdout JSON? `additionalContext` field?). Verify from Claude Code settings.json hook schema.
   - Default: follow claude-hooks convention (`exit 0` + JSON on stdout with `context` field).

3. **Hook latency budget.** Each hook runs synchronously in Claude's critical path. SessionStart on a cold mdkb can take 100–500ms for embedding model load.
   - Default: lazy-load embedding model; hooks must complete in <200ms hot path or bail and return empty context. Log slow runs.

### Important (P2 — affects implementation)

4. **Where do hook scripts live in the Rust binary?** `mdkb hook session-start` subcommand vs. separate `mdkb-hook` binary?
   - Default: subcommand on the main `mdkb` binary. Same artifact, routed by `clap`. Keeps install story single-binary.

5. **Config surface for hooks.** New `[hooks]` section in `.mdkb/config.toml` with per-event enable flags and limits (e.g. `user_prompt_recall_limit = 5`). Or inherit from `[memory]`/`[search]` defaults?
   - Default: new `[hooks]` section, explicit enable flags, defaults that match current warmup behavior.

6. **Should `mdkb setup mcp claude` auto-register hooks, or require `--with-hooks`?**
   - Default: auto-register with a visible summary ("Registered 3 hooks: SessionStart, UserPromptSubmit, PostToolUse") and `--no-hooks` to opt out. Boss wants adoption; silent opt-in is appropriate.

7. **Idempotency tag in settings.json.** claude-hooks uses `_managedBy: "claude-hooks"` to make updates safe. Mirror as `_managedBy: "mdkb"`.
   - Default: yes, mirror. Critical for `setup` to re-run without duplicating entries.

---

## Architecture: hook transport (mono-process)

Claude Code fires hooks ~70–160 times per active session. Spawning a fresh `mdkb` process for each is unacceptable: cold start loads SQLite, mimalloc, tokio, and potentially the 384-dim ONNX embedder (~80–250ms per invocation, wasted because `mdkb serve` is already running as CC's MCP subprocess with that state warm).

**Design: mono-process, dual transport.** `mdkb serve` hosts two tokio tasks sharing the same `Context` (DB pool, embedder, config):

1. **MCP stdio loop** (existing) — JSON-RPC over stdin/stdout for Claude Code tool calls.
2. **Hook socket listener** (new) — `UnixListener` on `$XDG_RUNTIME_DIR/mdkb-<repo-hash>.sock` (fallback: `/tmp/mdkb-<repo-hash>.sock`), accepts thin-client connections, reads one JSON request, writes one JSON response, closes.

**Thin client**: `mdkb hook <event>` reads CC's event JSON from stdin, connects to the socket, forwards the request, prints the response to stdout, exits 0. Budget: connect + round-trip < 20ms warm.

**Fallback on missing daemon**: `ECONNREFUSED` or socket absent → `Command::new(current_exe()).arg("serve").arg("--hook-only").spawn()` detached, retry connect with backoff 50ms×3. If still absent after retries, exit 0 silently (never block CC). `--hook-only` is a new `serve` flag that skips MCP stdio and only hosts the socket — used for standalone daemon when no MCP server is running.

**Failure isolation**: socket handler panics are caught with `tokio::spawn` + `JoinHandle::is_panicked` check; MCP stdio loop is unaffected. No shared mutable state outside `Arc<Context>` (read-heavy, short writes).

**Scope for story 014**: this section describes the target. Story 014 implements only the `mdkb hook <event>` thin-client CLI scaffolding with stubbed response (`{}`). Socket transport ships with 015 (SessionStart) when there is actual payload to deliver.

---

## Implementation Order (TDD)

### Step 1: Hook dispatch skeleton

- **Test:** `tests/hooks/dispatch_test.rs` — invoking `mdkb hook session-start` with mock stdin produces exit code 0 and valid JSON on stdout.
- **Implement:**
  - `src/cli/hooks.rs` — new module with `handle_hook_session_start`, `handle_hook_user_prompt_submit`, `handle_hook_post_tool_use`. Each reads JSON event from stdin, writes JSON response to stdout, exits 0 on error (non-blocking).
  - `src/cli/mod.rs` — add `Hook { event: HookEvent }` subcommand with clap.
  - `src/config.rs` — add `[hooks]` section with `session_start_enabled`, `user_prompt_submit_enabled`, `post_tool_use_enabled`, `recall_limit: usize = 5`, `latency_budget_ms: u64 = 200`.
- **Validation:** `mdkb hook session-start < /dev/null` returns `{}` (empty valid JSON), exits 0.

### Step 2: SessionStart hook — warmup on compaction

- **Test:** `tests/hooks/session_start_test.rs` — given a populated mdkb store, hook returns top-N warmup entries formatted for Claude context injection. Returns empty context if `.mdkbignore-hooks` marker exists.
- **Implement:**
  - `cli/hooks.rs::handle_hook_session_start` — call `get_warmup_index` (already in `src/store/memory.rs`), format as markdown block with header `## mdkb memory warmup` + bulleted `[type] id: title` lines.
  - Opt-out: walk from CWD looking for `.mdkbignore-hooks` → return empty.
  - Latency guard: if elapsed > `latency_budget_ms`, log to `.mdkb/hook-slow.jsonl` and truncate output.
- **Depends on:** Step 1
- **Validation:** fresh session in a test fixture project sees warmup memories in the initial system context; `/clear` + new turn still sees them via hook re-fire.

### Step 3: UserPromptSubmit hook — auto-recall injection

- **Test:** `tests/hooks/user_prompt_submit_test.rs` — given a stored memory for "auth jwt refresh" and a user prompt "how did we handle token expiration", hook returns the memory in the injected context block. No injection on empty-result or below-threshold.
- **Implement:**
  - `cli/hooks.rs::handle_hook_user_prompt_submit` — extract `prompt` from event JSON, call `search(query, scope=None, limit=hooks.recall_limit)`, filter by min_score (config `hooks.min_recall_score = 0.3`), emit as `## mdkb: relevant context` markdown block with `[ID] title — short snippet` entries.
  - Must respect opt-out marker.
  - Must skip injection if prompt matches `/wrapup`, `/clear`, `/compact` user-intent markers (mirror claude-hooks stop-guard respect pattern, inverted — don't inject on wrap-up).
- **Depends on:** Step 1
- **Validation:** manual — in a live Claude Code session with mdkb registered, ask a question related to a known memory; confirm Claude references it without explicit search call. Compare before/after adoption on the same question.

### Step 4: PostToolUse hook — reindex queue

- **Test:** `tests/hooks/post_tool_use_test.rs` — after Edit/Write on a source file, hook enqueues the path in `.mdkb/reindex-queue.jsonl`; daemon (or next `update`) drains the queue. Hook itself returns immediately (no blocking reindex).
- **Implement:**
  - `cli/hooks.rs::handle_hook_post_tool_use` — on `tool_name in {Edit, Write, NotebookEdit}`, append `{"path": "...", "at": <unix_ts>}` to queue file.
  - `cli/handlers.rs::handle_update` — drain queue first, then do differential reindex.
- **Depends on:** Step 1
- **Validation:** edit a `.rs` file, observe queue entry; next `mdkb update` processes it without rescanning the whole tree.

### Step 5: Setup commands for Claude and Codex

**Design: split MCP registration from hook registration, mirror both CLI hosts.**

| Command | Target | Scope | Notes |
|---|---|---|---|
| `mdkb setup mcp claude` | `claude mcp add` CLI | `--scope local\|user` | EXISTS, unchanged |
| `mdkb setup mcp codex` | `~/.codex/config.toml` → `[mcp_servers.mdkb]` | global only | NEW. TOML merge, idempotent |
| `mdkb setup hooks claude` | `.claude/settings.local.json` (local) or `~/.claude/settings.json` (user) | `--scope local\|user` | NEW. JSON merge with `_managedBy: "mdkb"` tag |
| `mdkb setup hooks codex` | `~/.codex/hooks.json` | global only | NEW. Checks `codex_hooks = true`; emits warning if missing and offers to set with confirm |

**Codex event coverage caveat:** Codex supports `PreToolUse`, `PostToolUse`, `Stop`. `UserPromptSubmit` (the killer auto-recall hook) is **not confirmed** available in Codex as of 2026-04. Implementation should probe at registration time and skip unsupported events gracefully. Codex `SessionStart` support is likewise unverified — register optimistically, log if rejected.

**Common shared flags:**
- `--yes` — skip confirmation prompt
- `--dry-run` — print the resulting settings blob without writing
- `--disable <csv>` — comma-separated event names to skip (e.g., `--disable session-start,post-tool-use`)

**Idempotency contract:**
- Every hook entry written by mdkb carries `_managedBy: "mdkb"` on the inner command object
- On re-run, scan existing arrays, drop any `_managedBy == "mdkb"` entries, then append the current set. Zero diff on second run with same args.
- Unrelated hook entries (other tools, user-custom) are preserved untouched.

**Clap shape (mod.rs):**
```
SetupCommand::Mcp(SetupMcpCommand {
  Claude { scope, yes },               // EXISTS
  Codex { yes, dry_run },              // NEW
})
SetupCommand::Hooks(SetupHooksCommand {
  Claude { scope, yes, dry_run, disable: Vec<String> },  // NEW
  Codex { yes, dry_run, disable: Vec<String> },          // NEW
})
```

**Tests:**
- `tests/cli/setup_mcp_codex_test.rs` — clean `config.toml` gets `[mcp_servers.mdkb]`. Existing `config.toml` with other MCP servers preserved. Re-run idempotent.
- `tests/cli/setup_hooks_claude_test.rs` — clean settings gets 3 hook entries with `_managedBy: "mdkb"`. Pre-existing unrelated hooks preserved. `--disable session-start` skips that event. `--scope user` targets `~/.claude/settings.json`.
- `tests/cli/setup_hooks_codex_test.rs` — clean `~/.codex/hooks.json` gets `PostToolUse` + `Stop` entries. Missing `codex_hooks = true` triggers warning. `UserPromptSubmit` silently skipped if unsupported.

**Implement:**
- `cli/setup.rs::handle_setup_mcp_codex` — load TOML via `toml_edit`, merge `[mcp_servers.mdkb]`, preserve other sections, write atomically.
- `cli/setup.rs::handle_setup_hooks_claude` — resolve target path by scope, load JSON, strip `_managedBy: "mdkb"` entries, merge new set, write.
- `cli/setup.rs::handle_setup_hooks_codex` — same pattern for `~/.codex/hooks.json`. Probe `codex_hooks` flag in `~/.codex/config.toml`.
- Shared helper `build_hook_entries(binary_path, disable) -> Vec<HookEntry>` used by both Claude/Codex variants to keep the command template in one place.

**Depends on:** Steps 1–4

**Validation:** on a fixture project with pre-existing unrelated hooks in both Claude and Codex, all four setup commands run clean, idempotent, preserve external entries, and produce working hook registrations. Manual dogfood: register both, start a Claude Code session AND a Codex session in the same repo, confirm each fires its events.

### Step 6: `usage` tool — token ledger exposure

- **Test:** `tests/mcp/usage_tool_test.rs` — tool returns JSON with session tokens (in/out, cached), tool call counts per tool, top-5 most-called tools, lifetime totals.
- **Implement:**
  - `src/mcp/tools.rs` — add `UsageParams { session_only: bool, root: Option<String> }`.
  - `src/mcp/server.rs` — new `usage` handler wrapping `src/metrics/` types.
  - Update BASE_INSTRUCTIONS line to mention `usage` in the "Memory" section: `- `usage`: audit token economy when output feels expensive.`
- **Depends on:** none (parallel with hook work)
- **Validation:** fresh session → `status` then `usage` → counts are coherent, not zero.

### Step 7: File token estimates in code index

- **Test:** `tests/code/file_tokens_test.rs` — after `update`, every `code_files` row has `token_estimate > 0`. Search results for `scope="symbols"` include file token count in metadata.
- **Implement:**
  - Schema migration v10→v11: add `token_estimate INTEGER` to `code_files`.
  - `src/code/indexing/pipeline.rs` READ stage: compute token estimate using existing token counter (`src/metrics/`).
  - `src/mcp/server.rs::search` symbols formatter: append ` (file: ~Ntok)` when known.
- **Depends on:** Step 6 (reuses token counter)
- **Validation:** `search("*", scope="symbols", file="src/main.rs")` includes a coherent token estimate on results.

### Step 8: Integration tests + dogfood

- **Test:** `tests/e2e/hooks_integration_test.rs` — drive a synthetic Claude Code session transcript through the hook dispatcher end-to-end, assert memory warmup present in SessionStart output, relevant memory injected on UserPromptSubmit, reindex queued on PostToolUse.
- **Implement:** no code — glue test harness only.
- **Depends on:** Steps 1–7
- **Validation:** full test suite passes. Run `mdkb setup mcp claude` on this repo itself, restart Claude Code, ask questions whose answers live in existing memories, verify Claude surfaces them without an explicit `search` call. Capture before/after session in `plans/archive/hooks-dogfood-log.md`.

### Final: Documentation + release notes

- [ ] Update `README.md` with hook overview and opt-out instructions.
- [ ] Update `CHANGELOG.md` / `docs/release-notes.md` — new `[hooks]` config section, new `usage` tool, new `--no-hooks` flag on setup.
- [ ] Write `docs/hooks.md` explaining each hook's contract, latency budget, opt-out, and troubleshooting.
- [ ] Draft `CLAUDE.md` template snippet emitted by `mdkb setup mcp claude` (reuse pattern from `plans/mdkb-claude-adoption.md`).
- [ ] Update `BASE_INSTRUCTIONS` in `src/mcp/server.rs` to mention `usage` tool — defer the full rewrite to the sibling plan already in flight.
- **Validation:** `cargo test`, `cargo clippy`, lint clean.

---

## Acceptance Criteria

- [x] `mdkb hook <event>` subcommand exists and handles SessionStart, UserPromptSubmit, PostToolUse non-blockingly (exit 0 on error).
- [x] `mdkb setup mcp claude` registers MCP server via `claude mcp add` (unchanged behavior).
- [x] `mdkb setup mcp codex` registers MCP server in `~/.codex/config.toml` under `[mcp_servers.mdkb]`, preserving other servers.
- [x] `mdkb setup hooks claude` writes hook entries to `.claude/settings.local.json` (scope=local) or `~/.claude/settings.json` (scope=user) with `_managedBy: "mdkb"` tag, idempotent across re-runs.
- [x] `mdkb setup hooks codex` writes hook entries to `~/.codex/hooks.json` with same idempotency tag. Probes `codex_hooks` feature flag and warns if missing. Gracefully skips unsupported events (UserPromptSubmit may not be supported).
- [x] `--disable <csv>` flag on hook setup commands skips named events.
- [x] `--dry-run` prints the merged settings blob without writing.
- [x] `.mdkbignore-hooks` marker disables hooks for the containing directory tree.
- [x] SessionStart hook emits warmup index including due reminders; measured latency p50 < 100ms, p99 < 200ms on a 1000-entry store.
- [x] UserPromptSubmit hook injects top-K relevant memories+docs (configurable `recall_limit`, default 5) when score ≥ `min_recall_score` (default 0.3).
- [x] PostToolUse hook enqueues file paths after Edit/Write tool use; next `mdkb update` drains the queue and performs differential reindex.
- [x] New `usage` MCP tool reports session + lifetime token stats with per-tool breakdown.
- [x] `code_files.token_estimate` populated; `search(scope="symbols")` results surface it.
- [x] All new code has tests; `cargo test` green; `cargo clippy` clean; `cargo fmt` applied.
- [x] Dogfood verification: same question pre-adoption vs post-adoption in Claude Code shows measurably higher mdkb tool invocation or memory referencing (log captured in `plans/archive/hooks-dogfood-log.md`).
- [x] Release notes updated; `docs/hooks.md` exists.
- [x] Handoff paragraph for wiz-agents delivered (see §7).

---

## Security Considerations

- **Path traversal in hook input.** UserPromptSubmit receives user-typed prompts; inject them into search queries but never into filesystem paths or shell commands. Use existing `search` API which already parameterizes SQL.
- **Hook command registered in settings.json** uses absolute path to the `mdkb` binary — if that binary is replaced by an attacker, the hook runs the replacement. Same trust model as any installed CLI; not a new exposure. Document it in `docs/hooks.md`.
- **Opt-out marker respects project boundaries.** `.mdkbignore-hooks` check walks from CWD upward to the project root; never crosses home directory.
- **Injected context is trusted input.** mdkb's own memory content is author-controlled and injected verbatim — same trust model as CLAUDE.md. No new risk surface beyond what mdkb already carries.
- **Non-blocking failure.** Hooks exit 0 even on internal errors (mirror claude-hooks pattern). A crashing hook never blocks Claude Code. Failures go to `.mdkb/hook-errors.jsonl` with rotation (reuse existing log rotation if present, else 30-day cap).

## Performance Considerations

- **Cold embedding model load** (~200–500ms for AllMiniLM-L6-V2 ONNX). SessionStart warmup does NOT need embeddings (it reads pre-computed index). UserPromptSubmit DOES — mitigate with warm cache (`get_cached_service`, already exists) and fall back to BM25-only if model not ready within `latency_budget_ms`.
- **SQLite lock contention.** MCP server and hook may both open `.mdkb/index.sqlite`. Use `WAL` mode (verify it's on — `PRAGMA journal_mode`). Read-only hooks should open with `read_only=true` connection flag.
- **Hook on every user prompt** multiplies recall queries. Cache last N queries + results within a short TTL (e.g. 10s) — if user re-sends nearly the same prompt, skip recall. Simple LRU in `cli/hooks.rs` state file.
- **PostToolUse fires on every Edit/Write.** Keep hook body to: parse event → append line to queue file → exit. No reindex work inline.

---

## Related Files

Files touched:

| File | Change |
|---|---|
| `src/cli/mod.rs` | `Hook { event }` subcommand, clap enum |
| `src/cli/hooks.rs` | NEW — all hook handlers |
| `src/cli/setup.rs` | NEW handlers: `handle_setup_mcp_codex`, `handle_setup_hooks_claude`, `handle_setup_hooks_codex`; shared `build_hook_entries` helper |
| `src/cli/handlers.rs` | Drain reindex queue in `handle_update` |
| `src/config.rs` | `[hooks]` section, defaults, env overrides |
| `src/mcp/server.rs` | `usage` tool handler, BASE_INSTRUCTIONS minimal tweak |
| `src/mcp/tools.rs` | `UsageParams` struct |
| `src/store/schema.rs` | v10→v11 migration (`code_files.token_estimate`) |
| `src/code/indexing/pipeline.rs` | Compute token_estimate in READ stage |
| `tests/hooks/*` | New test module |
| `tests/cli/setup_hooks_test.rs` | Setup idempotency tests |
| `tests/e2e/hooks_integration_test.rs` | End-to-end |
| `docs/hooks.md` | NEW — hook contract documentation |
| `docs/release-notes.md` | 1.5.0 section |
| `README.md` | Hooks overview |

Files deliberately NOT touched (already planned elsewhere):

- `BASE_INSTRUCTIONS` full rewrite → `plans/plan-get-claude-code-to-actually-use-mdkb-tools-2.md`
- CLAUDE.md injection → `plans/mdkb-claude-adoption.md`
- MCP Resources → `plans/mcp-resources.md`
- Directive tool descriptions → same sibling plan

---

## Explicitly NOT Doing (and why)

| Feature (source) | Why not |
|---|---|
| Ollama HyDE query expansion (claude-hooks) | Violates single-binary Rust constraint. Defer to wiz-agents where Ollama is already in the stack. |
| Stop Guard — block premature "good stopping point" (claude-hooks) | Not knowledge management; it's workflow enforcement. Belongs in wiz-agents hooks layer. |
| Safety Scan — bash PreToolUse pattern matching (claude-hooks) | Security tooling, not knowledge management. Belongs in wiz-agents or a dedicated security plugin. |
| Instinct extraction — auto-generate rules from error→fix (claude-hooks) | Overlaps with `memory_write` + confidence model. Defer until Phase 2A is live to avoid competing paths. Can then be implemented in wiz-agents using mdkb as backend. |
| `anatomy.md` file map (OpenWolf) | `code_graph` + `search(scope="symbols")` covers this with more detail. Only the **token estimate** dimension is new — captured as Step 7. |
| `cerebrum.md` learning memory (OpenWolf) | Redundant with mdkb memory + confidence model (Phase 2A of strategic-vision). |
| `buglog.json` bug memory (OpenWolf) | Redundant with `memory_write(entry_type="problem")`. Instead, document the pattern in the CLAUDE.md template emitted by `mdkb setup mcp claude`. |
| Design QC screenshot evaluation (OpenWolf) | Out of scope for a knowledge base. |
| Proxy-based rate-limit observability (claude-hooks) | Adds network proxy, heavy. Out of scope for mdkb. |
| RTK command rewriter (claude-hooks) | External Rust CLI; not mdkb's problem to solve. |

---

## §7 — Handoff Paragraph for wiz-agents

Paste this into `wiz-agents` as a new plan (`plans/wiz-hooks-from-mdkb-gap-analysis.md`) or an issue:

> **Context:** During the mdkb OpenWolf + claude-hooks gap analysis (see `mdkb/plans/openwolf-claudehooks-gap-analysis.md`), four capabilities were deliberately excluded from mdkb because they are workflow/enforcement concerns rather than knowledge management, or because they require external LLM dependencies that break mdkb's single-binary Rust identity. These belong in wiz-agents, which already has the hook infrastructure and Ollama dependency paths to host them.
>
> **To implement in wiz-agents hooks layer:**
>
> 1. **Stop Guard (PreToolUse / Stop hook).** Regex-scan assistant output for premature-abandonment patterns — "good stopping point", "pre-existing issue", "out of scope for this session", permission-seeking mid-task. On match, emit `decision: block` with a correction forcing continuation. Respect user wrap-up markers (`/wrapup`, `/clear`, "compact context") so intentional stops still pass. Reference implementation: `mann1x/claude-hooks` Stop Guard (`claude_hooks/hooks/pre_tool_use.py`). Directly solves the recurring Boss complaint that "Claude doesn't respect commands and abandons tasks".
>
> 2. **HyDE query expansion for mdkb recall.** The mdkb UserPromptSubmit hook does BM25+vector recall against its own SQLite. For higher-quality semantic hits on conceptual queries, wiz-agents can layer a HyDE step: expand the user prompt via Ollama (qwen2.5:2b or similar small local model) into a hypothetical answer, then call `mdkb search` with the expanded query. Skip expansion if raw recall already has ≥ N hits above threshold (grounded HyDE pattern). Reference: `mann1x/claude-hooks` UserPromptSubmit hook.
>
> 3. **Safety Scan (PreToolUse bash guard).** Pattern-match dangerous bash commands anywhere in the command string (after pipes, in subshells, `find -exec`), not just prefix. Emit `permissionDecision: "ask"` so the user always decides — never auto-deny. Log matches as JSONL with rotation. Reference: `mann1x/claude-hooks` safety scanner.
>
> 4. **Instinct extraction (Stop hook).** Mine assistant turns for error→fix patterns (tool error followed by a successful edit). When detected with confidence, emit a draft markdown rule to `~/.claude/instincts/` that the user can promote to CLAUDE.md. Use mdkb as the persistence backend: `mdkb memory_write(entry_type="decision", source_type="auto_extracted")` with a tag like `#instinct-candidate`. Reference: `mann1x/claude-hooks` Stop hook instinct extraction.
>
> **Integration contract with mdkb:** wiz-agents hooks should invoke `mdkb` via its CLI (`mdkb search`, `mdkb memory_write`) rather than reimplementing search or persistence. mdkb owns the knowledge layer; wiz-agents owns the workflow layer. The UserPromptSubmit hook mdkb ships itself (see `mdkb/plans/openwolf-claudehooks-gap-analysis.md` Step 3) handles the *recall* side; wiz-agents HyDE would layer *query expansion* on top by calling mdkb twice (raw recall first, then expanded recall if needed).
>
> **Don't duplicate:** mdkb already ships SessionStart, UserPromptSubmit (recall-only), and PostToolUse hooks. wiz-agents should not re-register these events with mdkb-equivalent behavior. Check `_managedBy: "mdkb"` tags in `.claude/settings.local.json` before writing and use a different tag (`_managedBy: "wiz-agents"`) for idempotency.

---

## Next Steps

**MANDATORY FLOW: plan → stories → work.**

1. `/wiz:stories create` — **REQUIRED NEXT STEP** — create stories from this plan (one per Step, with the Step's tests as acceptance criteria). Suggested IDs: `hooks-skeleton`, `hooks-session-start`, `hooks-user-prompt-submit`, `hooks-post-tool-use`, `hooks-setup-wiring`, `usage-tool`, `file-token-estimates`, `hooks-e2e-dogfood`, `hooks-docs-release`.
2. `/wiz:work` — execute the stories (only after stories exist).
- `/wiz:deepen-plan` — drill deeper into any single Step (e.g. exact JSON contract for each hook event, idempotency algorithm for settings.json merge).
- `/wiz:brainstorming` — if Boss wants to revisit the scope decisions in §"Explicitly NOT Doing".
