---
id: recall-gate-is-relative-top-hit-always-passes
title: "Recall gate is relative: top hit always passes"
entry_type: problem
source_type: auto_extracted
status: active
tags: [recall, hooks, ranking, min-recall-score]
created_at: 1789564891
updated_at: 1789564891
---

Measured 2026-09-16 on the live mdkb store. hybrid::normalize_scores divides every fused score by the max, so the top memory result always has rrf_norm 1.0 and final_hybrid_score >= 0.7 (RELEVANCE_WEIGHT 0.7) against min_recall_score 0.3. The gate cannot reject the best hit, and vector distance is only used for rank, never as an absolute threshold. Proof: '* credentials rejected by the provider' injected 5 entries, 3 unrelated (cerebro org graph, wiz primary backend, agent-agnostic HTTP). Fix direction: an absolute relevance signal (cosine floor or BM25 hit requirement) before normalization, or normalize against a fixed scale.
