# Ideas

## Session Knowledge Indexing

**Problem:** Claude Code sessions contain valuable institutional knowledge (decisions, problem solutions, patterns discovered) but it's lost after each conversation. Journal entries are inconsistent and depend on discipline.

**Idea:** Automatically summarize and index Claude Code sessions into mdkb as searchable knowledge.

### Approach A: Hook-based (real-time)

Use Claude Code's `Stop` hook to trigger summarization at session end:

1. `Stop` hook fires when assistant finishes
2. Script locates the session's JSONL conversation file
3. Pipes through `claude -p` with a condensation prompt to extract:
   - Problems encountered and solutions
   - Architectural decisions and rationale
   - Patterns/anti-patterns discovered
   - What was built/changed and why
4. Writes summary as markdown to an mdkb-watched directory
5. mdkb indexes on next reindex or via `update` trigger

**Open questions:**
- Does `Stop` fire per-turn or per-session? If per-turn, needs debounce logic
- What env vars are available in hooks? Need session ID and JSONL path
- Race condition: is JSONL fully flushed when hook fires?

### Approach B: Batch cron (simpler, more robust)

Skip hooks entirely. Run a nightly cron job:

```bash
find ~/.claude/projects/*/conversations/ -name "*.jsonl" -newer .last_indexed |
  while read f; do
    claude -p "Summarize decisions, problems solved, insights" < "$f" > summaries/$(basename "$f" .jsonl).md
  done
touch .last_indexed
```

**Pros:** No dependency on hook internals, easier to debug, can reprocess
**Cons:** Not real-time, requires cron setup

### Common considerations

- **Cost:** Each summarization call costs API tokens. Could be significant for many sessions
- **Pre-filtering:** Strip tool results and file contents from JSONL before summarizing to reduce token usage and noise
- **Deduplication:** Track processed sessions by filename/hash to avoid reprocessing
- **Collection:** Index summaries as a dedicated mdkb collection (e.g., `sessions`) separate from docs and code
- **Prompt quality:** The condensation prompt needs iteration to produce high-signal summaries vs generic fluff

### Recommendation

Start with Approach B (cron). It's simpler, doesn't depend on undocumented hook behavior, and can be built without researching Claude Code internals. Migrate to hook-based once we verify the hook API supports it well.
