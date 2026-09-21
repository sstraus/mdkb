---
id: graph-relation-detector-self-edge
title: A key resolving to its own document is not a relation
entry_type: problem
source_type: user_statement
status: active
tags: [graph, detector, measured, false-positive]
created_at: 1789989675
updated_at: 1789989675
---

Measured 2026-09-21 on brainstorming/work. The relation-key detector reported 18 relation keys where manual analysis of the same corpus had found 17. The extra one was github: in all four occurrences 'github: @handle' resolved to the document carrying it, because those people list their own handle in aliases:. Four self-loops would have entered the edges table and distorted every degree in graph hubs. Fix: a value that resolves to the source document does not count as a hit - a relation points at something else. After the fix the live corpus gives 17 relation keys all at 1.0 and 18 metadata keys all at 0.0, with distinct scores exactly [0.0, 1.0] - no middle band on real data, which is the measurement RELATION_THRESHOLD was named for. Zero self-loops in the edges table. Lesson: the detector's fixture agreed with the implementation because both were written by the same reasoning; only the live corpus disagreed.
