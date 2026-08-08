---
id: collection-add-convention-race
title: collection add races convention auto-registration
entry_type: problem
source_type: user_statement
status: active
tags: [collections, race, conventions, idempotency, e2e-mcp-pty, daemon]
created_at: 1783240242
updated_at: 1783240242
---

SYMPTOM: intermittent 'UNIQUE constraint failed: collections.name' / 'mdkb collection add failed' at tests/common/mod.rs:82 under full parallel 'cargo test' (different e2e_mcp_pty test each run; passed in isolation and serial). ROOT CAUSE: conventions enabled by default; served 'mdkb serve' auto-registers a 'docs' collection via apply_conventions (server.rs) when it sees a docs/ dir, racing the test's explicit 'mdkb collection add docs'. store::collections::add_collection was a plain non-idempotent INSERT, so whichever writer lost the race hit the PK UNIQUE. FIX: made add_collection an idempotent upsert (INSERT ... ON CONFLICT(name) DO UPDATE, preserving created_at) — the store chokepoint both callers share. Updated 2 tests that asserted the old 'duplicate fails' contract (collections.rs, cli/handlers.rs). BEHAVIOR CHANGE: 'mdkb collection add <existing>' now upserts instead of erroring. GOTCHA: 'mdkb hook user-prompt-submit/session-start' route through the detached hook daemon (mdkb serve --daemon --detach) which caches code in memory — kill it after a rebuild or it serves stale recall logic. Also: hard wall-clock p95 timing assertions flake under parallel test load; assert min-over-iterations + O(1)-in-corpus invariant instead.
