---
id: dedup-near-duplicate-bar-is-near-verbatim
title: "The 0.32 dedup bar is near-verbatim, not paraphrase"
entry_type: decision
source_type: user_statement
status: active
tags: [memory, dedup, embeddings, measured]
created_at: 1789649840
updated_at: 1789649840
---

NEAR_DUPLICATE_DISTANCE = 0.32 (L2 over unit vectors, cosine ~0.949) is the bar every memory write path now rejects on. Measured 2026-09-17 with all-MiniLM-L6-v2: a reworded restatement of one sentence scores cosine 0.99 and IS refused; a genuine paraphrase of the same lesson ('One writer connection serialises all mutations' vs 'Serialise every write behind a single connection') scores 0.917 / L2 0.408 and is NOT refused - it falls in the advisory SIMILAR_ENTRY_DISTANCE band (0.55, cosine 0.85). So the reject bar catches near-verbatim repeats only. The two thresholds were deliberately NOT collapsed into one: anything inside 0.32 is already refused, so a warning at the same distance could only ever fire for a write that said on_conflict=contradicts, and merging them would delete the 'Similar entry exists' advisory entirely. Story 089-7466, plan Step 7.
