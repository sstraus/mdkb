---
id: plain-sqlite-cannot-audit-embedding-coverage
title: sqlite3 cannot read embedding coverage at all
entry_type: problem
source_type: user_statement
status: active
tags: [audit, embedding, sqlite, measured]
created_at: 1790001946
updated_at: 1790001946
---

Measured 2026-09-21 on CC_Playground/itview after a full drain. A plain sqlite3 read of the store suggests a large backlog and is wrong. 'SELECT count(*) FROM embeddings' returns 4738 while document_chunks holds 14626, which reads as 9888 chunks unembedded. It is not: the embeddings table holds ONE ROW PER DOCUMENT, and the chunk-level vectors live in the vec_documents and vec_chunks sqlite-vec virtual tables, which the sqlite3 CLI cannot open at all - it errors with 'no such module: vec0' because the extension is not loaded. So any audit performed from outside the binary is STRUCTURALLY BLIND to embedding coverage and will either under-report or invent a backlog. I hit this myself trying to read drain progress, and the fleet-audit agent nearly filed a false 9888-chunk backlog on the strength of it. THE AUTHORITATIVE CHECK is to re-run the command and read what it says it skipped: 'mdkb embed' printing 'Generated: 0, Skipped: 4738' means full coverage. Same class as the CANTOPEN(14) trap: a read-only sqlite path that silently answers a different question than the one asked.
