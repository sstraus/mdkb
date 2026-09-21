---
id: legacy-root-pattern-is-frozen-by-skip-by-name
title: 74 stores carry a fixed bug that can never reach them
entry_type: problem
source_type: user_statement
status: active
tags: [collections, indexing, migration, fleet, measured]
created_at: 1789996835
updated_at: 1789996835
---

Replaces my entry root-collection-legacy-vs-convention, whose mechanism was wrong. Corrected by the mdkb-fleet-audit agent and verified at source 2026-09-21. detect_conventions in src/domain/conventions.rs does NOT skip when a store has collections; it skips PER COLLECTION NAME - existing_names.contains(name). Proof from my own data: ai-governance already had a plans collection and still got _root auto-created. So a store self-heals for every convention name it is MISSING. The permanently stuck case is narrower: a store that already owns the NAME _root with the legacy non-recursive pattern '*.md'. Detection sees the name, skips, and the bad pattern is frozen forever. The source comment at conventions.rs:54-64 documents this as issue #8 - '*.md indexed only the two or three files beside the README and silently ignored the other few hundred, so a fresh mdkb init produced a store that knew nothing' - and the fix was to make _root recursive. THE FIX SHIPPED IN CODE AND CANNOT REACH DISK. Counted by python over 102 stores with a database outside the temp tree: 74 carry _root = '*.md', 17 have no _root (these self-heal on the next update), 11 are recursive. Same family as hooks.warmup_limit = 50 frozen in 32 configs: a default improved in code that a store which already materialised the old value never receives. Remedy per store is one command, mdkb collection update _root --pattern '**/*.md' followed by mdkb update - measured on copies: maccollect 6 to 26 documents, SecBrowserExt 30 to 129, openrouter-keys-manager 5 to 38, ls-pr-reviewer 49 to 690 (of which 602 are test fixtures, so the sweep is not always wanted). Reverting is one command, collection update back to '*.md', because update changes the pattern in place without dropping the collection. jevclassifier is the separate case: zero top-level .md so has_root_markdown_files is false and no docs/ or archive/ dir, so detection cannot fire at all and it needs a manual add.
