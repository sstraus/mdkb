---
id: dup-cluster-hash-breaks-ignore-list
title: dup cluster hash breaks the ignore list
entry_type: problem
source_type: auto_extracted
status: active
tags: [code-intel, duplication, ignore-list, review-2026-09-16]
created_at: 1789565733
updated_at: 1789565733
---

cluster_hash (src/code/duplication/report.rs:210) is the first 8 hex of SHA-256 over the sorted full membership. One member joining or leaving changes the id, so an ignore-list entry silently stops matching and the cluster reappears as new. 32 bits also invites collisions in a large report. Story 099-6061, plan Step 17.
