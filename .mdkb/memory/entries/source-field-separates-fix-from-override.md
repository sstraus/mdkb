---
id: source-field-separates-fix-from-override
title: The source field is what makes a default upgradable
entry_type: decision
source_type: user_statement
status: active
tags: [conventions, migration, design, measured]
created_at: 1789999154
updated_at: 1789999154
---

Story 142, decided and shipped 2026-09-21. mdkb had a convention pattern it wrote (_root = '*.md'), corrected it to '**/*.md' after issue #8, and could never deliver the correction: detect_conventions skips by collection NAME, so a store already holding _root keeps the old pattern forever. Measured: 70 of 102 fleet stores. THE DESIGN QUESTION was how to upgrade a stale default without overriding a deliberate human choice, and the two are indistinguishable from the pattern alone. The answer already existed in the schema: collections carry a source column, manual or convention or sessions. Only source=convention is eligible, so mdkb corrects its own past output and never touches what a person wrote. Measured on the fleet, all 70 affected are source=convention and zero are manual, so no real choice was at stake - but ls-pr-reviewer going 49 to 690 documents when its pattern widened shows what a blind upgrade would have cost if someone HAD narrowed it deliberately to keep 602 test fixtures out. IMPLEMENTATION: SUPERSEDED_PATTERNS is a const naming (collection, what mdkb used to write, what it writes now) explicitly rather than inferring - a correction the code cannot express is one it cannot apply. Applied by apply_conventions during update (a deliberate write), reported through UpdateResult::pattern_upgrades and printed with the revert command, once rather than every run. GENERALISABLE: any shipped default that a store materialises on disk has this problem. hooks.warmup_limit = 50 frozen in 32 configs is the same shape and still unsolved; the source-field trick does not apply there because config has no provenance column.
