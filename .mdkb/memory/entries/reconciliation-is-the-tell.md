---
id: reconciliation-is-the-tell
title: A number that does not reconcile is the cheapest defect detector
entry_type: topic
source_type: user_statement
status: active
tags: [method, audit, measured]
created_at: 1790002019
updated_at: 1790002019
---

Named by the fleet-audit teammate closing a long session on 2026-09-21, and worth carrying. Two of its own errors that day came from read-only sqlite paths that silently answered a different question than the one asked: sqlite3 -readonly returning CANTOPEN(14) on 26 of 101 WAL stores, counted as 'no _root' and producing a miscounted census; and 'no such module: vec0' making embedding coverage unreadable, which nearly produced a fabricated 9888-chunk backlog. Both times the tell was the same and neither was a code inspection: A NUMBER THAT DID NOT RECONCILE WITH SOMETHING ALREADY KNOWN. 62 legacy _root stores against my 74 from a different instrument. 4738 embeddings against 14626 chunks in a store that had just reported a clean drain. The reconciliation step - take any count from a second instrument, or check it against a figure you already trust - is cheaper than either mistake and caught both. Generalises past sqlite: the same session had a grep -r returning 45 of 146 files, a rate projected as a duration and wrong by 6x, and an elapsed time read as 1h03m when it was 1m03s. Every one would have been caught by asking 'does this agree with anything else I know?' before reporting it.
