---
id: root-collection-legacy-vs-convention
title: _root is *.md only in legacy stores; update auto-creates **/*.md
entry_type: problem
source_type: user_statement
status: active
tags: [collections, indexing, measured, correction]
created_at: 1789993793
updated_at: 1789993793
---

Replaces my entry root-collection-indexes-top-level-only, which generalised from a single store. Measured 2026-09-21 on four rsync copies, running a bare mdkb update on each. RESULT: LS/schema-registry-actions 0 to 41 documents, personal/P42 0 to 14, ai-governance 0 to 58 of 66 - in each case update AUTO-CREATED collections named docs and _root with pattern **/*.md, recursive, and the only files left out were dotdir internals (.claude, .codex, .github) and a CLAUDE.md symlink, which is exactly the right exclusion. LS/jevclassifier 0 to 0 - convention detection recognised nothing in its layout of four sibling project directories, so it genuinely needs collection add. LS/maccollect 6 to 6 - it already HAS collections, including a legacy _root with pattern '*.md' (non-recursive), and convention detection does not fire on a store that already has any collection, so update can never widen it. CONCLUSIONS: (1) _root is not uniformly '*.md'; that is a legacy shape, and current convention detection creates '**/*.md'. (2) A store with ZERO collections is often fixed by mdkb update alone - no collection add needed. (3) The real trap is the opposite of what I recorded: a store that already has a legacy non-recursive _root is the one that can never self-heal, because detection is skipped whenever any collection exists. (4) mdkb collection add defaults to --pattern '**/*.md' (verified from --help), so one collection per top-level directory covers its subtree. Every one of these updates also migrated the store to v30, which is why the batch is gated on story 139.
