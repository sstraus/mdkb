---
id: root-selector-wildcard-two-holes
title: Replacing a special case with a parser left two holes
entry_type: problem
source_type: auto_extracted
status: active
tags: [mcp, root-selector, regression, false-negative, testing]
created_at: 1789931667
updated_at: 1789931667
---

Story 126 replaced the explicit root=="*" arms in src/mcp/server.rs with a RootSelector parser. Two pre-existing tests the story never ran then failed, and only the full suite found them.

1. search() checked the selector ONLY when a registry existed. In standalone mode root="*" fell through to the single local handle and answered one store as if it were every repo - the same false-all-clear class as 106/113/125/127/128.
2. resolve_handle() refused the wildcard only through its n>1 arm, so with 0 or 1 known repos the wildcard produced 'No repos registered' or would have been accepted silently.

Rule: a special case deleted in favour of a general parser must be re-checked at the EDGES the special case covered by accident - the zero case, the one case, and the mode where the general machinery is absent. Refuse a selector for what it MEANS, not for how many things it happens to resolve to.

Also: cargo nextest stops at the first failure and cancels the rest. The first run reported 1 failure and skipped 1066 tests; the second hole only appeared with --no-fail-fast. Batch verification uses --no-fail-fast.
