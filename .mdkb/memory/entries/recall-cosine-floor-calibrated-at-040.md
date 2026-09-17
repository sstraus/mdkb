---
id: recall-cosine-floor-calibrated-at-040
title: Recall cosine floor calibrated at 0.40
entry_type: decision
source_type: auto_extracted
status: active
tags: [mdkb, retrieval, recall, calibration, eval]
created_at: 1789572932
updated_at: 1789572932
---

Story 083. min_recall_cosine = 0.40, read off the precision-recall curve over 36 held-out queries and 40 new labelled in-domain negatives (hybrid mode, AllMiniLML6V2). Curve: tau 0.00 -> recall 1.000 / precision 0.474; 0.30 -> 0.861/0.705; 0.35 -> 0.722/0.839; 0.40 -> 0.583/1.000; 0.45 -> 0.444/1.000. Rule: the lowest floor that admits no labelled negative, dictated by criterion 9 ('a query unrelated to every stored entry injects nothing'), NOT by F1 (which peaks flat at 0.30-0.35). The test asserts the rule, not the number, so a fixture change reopens the choice. Price: hybrid/embedding recall@5 fell 1.000 -> 0.583 and bm25 0.167 -> 0.000, because with no embedding there is no distance arm and only a strong lexical match can admit. Recovering the 15 lost queries needs a second lower threshold for explicitly asked queries (sigil or search tool) - story 096, not a reason to lower this one.
