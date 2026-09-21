---
id: reconciliation-is-the-tell
title: "Six wrong numbers, none caught by whoever produced it"
entry_type: topic
source_type: user_statement
status: active
tags: [method, audit, collaboration, measured]
created_at: 1790002072
updated_at: 1790002072
---

From a long two-agent session on 2026-09-21. Six numbers were reported wrong and every one was a figure nobody had checked against a second figure: 62 legacy stores against 74 from another instrument; 4738 embeddings against 14626 chunks in a store that had just reported a clean drain; a grep -r returning 45 files against find's 146; a 739-file headline against 712 already indexed by nested stores; a rate projected as a duration and wrong by 6x; an elapsed time of 1m03s read as 1h03m. THE TWO PROPERTIES THAT MATTER, and the second is the actionable one. (1) None needed code inspection to catch - each was visible as an arithmetic disagreement with something already known. (2) NONE OF THE SIX WAS CAUGHT BY THE PERSON WHO PRODUCED IT. Self-checking did not find any of them; cross-checking found all of them. That is why the reconciliation step has to be performed by someone other than the author, or against an instrument the author did not choose. AND THE CHECKS PAID OFF PRECISELY BECAUSE THEY WERE NOT CONFIRMATIONS: I re-ran a teammate's ai-governance falsifier instead of accepting its prediction of 4 documents and got 58; the teammate re-measured my warmup_limit count instead of conceding my correction and got 29 roots against my 1. Both went the OPPOSITE way to what the checker expected, which is the only reason checking was worth the time. A reconciliation you expect to confirm is not one.
