---
id: root-collection-indexes-top-level-only
title: "The _root collection is *.md, not **/*.md"
entry_type: problem
source_type: user_statement
status: active
tags: [collections, indexing, measured, silent-failure]
created_at: 1789991253
updated_at: 1789991253
---

Found by the mdkb-fleet-audit agent 2026-09-21, verified here by reading the collections table of LS/maccollect: the _root collection is path='.' with pattern '*.md', NOT '**/*.md'. It indexes top-level markdown only, so every subdirectory needs its own registration and nothing says so. This is the mechanism behind two fleet-scale gaps. First, 25 stores register only '.' and therefore cannot see anything in a subdirectory - maccollect indexed 2 of its 25 markdown files, codex_vs_claude 5 of 1228, straus.it 3 of 85 with all 69 real files under content/. Second, 13 stores have ZERO registered collections and can index nothing at all; of those, six are genuinely orphaned with no nested store covering them - LS/schema-registry-actions 67 files, LS/jevclassifier 55, brainstorming/work/people/hr/OTR 24, LS/tenancy-deployments 22, brainstorming/work/people/hr/recruit 19, personal/P42 16, so 203 files total. CAUTION ON THE HEADLINE: CC_Playground/brainstorming looks like 739 unindexed files but 410 are under work/ and 302 under home/, each covered by one of 13 nested stores; only about 4-8 are truly orphaned. What is real there is that a registered, daemon-known store with zero collections answers every query with nothing - a root='*' fan-out includes it and gets silence rather than an error. Same family as the other silent failures found today. Fixing a store means mdkb collection add per subdirectory, which triggers an index and therefore also a v20-to-v30 migration with data-mutating steps - see story 139-cd2f.
