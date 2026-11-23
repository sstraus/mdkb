# Configuration keys

`mdkb init` writes every key of `.mdkb/config.toml` with its default,
commented out. This page explains the keys that the README and the topic
guides do not cover. Uncomment a key only to change it. `src/config.rs` is
the reference for types and validation. Machine-wide settings live in
`~/.mdkb/daemon.toml`: see [daemon.md](daemon.md).

## Documents and search

| Key | Default | Effect |
|---|---|---|
| `[chunking] max_tokens` | 512 | Largest chunk a document is split into. |
| `[chunking] overlap_tokens` | 64 | Tokens repeated between adjacent chunks. |
| `[chunking] include_header_path` | true | Prefix each chunk with its heading path. |
| `[search] auto_embed_docs` | true | `mdkb update` embeds changed documents, so hybrid search does not fall back to BM25 only. |
| `[search] auto_embed_sessions` | false | Also embed the `claude_sessions` collection. Transcripts are large; embed them on demand with `mdkb update --collection`. |
| `[search] auto_embed_memory` | true | Embed memory entries on write. When false, the entry stays pending and `mdkb update` embeds it later. |
| `[search.memory] access_recency_weight` | 0.2 | Rank weight for entries that `get` read often and recently. 0 turns it off. `search` never counts as an access. |
| `[search.memory] recency_half_life_secs` | 2592000 | Half-life of that recency signal (30 days). |
| `[mcp] truncate_with_ellipsis` | true | Mark truncated MCP content with an ellipsis. |

## Memory audit

| Key | Default | Effect |
|---|---|---|
| `[memory.audit] stale_after_days` | 180 | `memory audit` reports an entry untouched this long when a path it cites changed. |
| `[memory.audit] aged_lifecycle_days` | 90 | Age at which an unread reminder, prior or handoff is listed. Same default as `memory prune`. |
| `[memory.audit] near_duplicate_similarity` | 0.9488 | Cosine at or above which two active entries are reported as near-duplicates; the threshold at which the write path refuses a duplicate. |
| `[priors] injection_enabled` | true | Inject promoted priors where their trigger fires. Independent of `mining_enabled`. |
| `[hooks] recall_holdout_rate` | 0.0 | Fraction of automatic recalls that also inject one entry recall did not select, tagged `holdout` in the recall ledger, to measure false negatives. |
| `[telemetry] retention_days` | 30 | Maximum age of query events; applied on every telemetry write. |

## Code index

| Key | Default | Effect |
|---|---|---|
| `[code.indexing] ignore_patterns` | (list) | Globs the code walker skips. |
| `[code.indexing] batch_size` | 500 | Files per indexing commit. |
| `[code.indexing] debounce_ms` | 300 | File-watcher events inside this window count as one change. |
| `[code.indexing] batch_idle_ms` | 30000 | Quiet time after the last change before an incremental reindex runs. Lower is fresher and costs more embedding passes. |
| `[code.semantic_search] enabled`, `threshold` | true, 0.3 | Embedding-based code search (`search --scope semantic-code`) and its minimum score. |
| `[code.duplication] similarity_threshold` | 0.7 | Minimum cosine for a clone pair that the structural pass did not already group. |
| `[code.duplication] hamming_threshold` | 6 | Bits two structural fingerprints may differ by and still group. |
| `[code.duplication] min_nodes` | 30 | Smallest body (named AST nodes) that `dup` reports. |
| `[db] optimize_interval_calls` | 200 | Run `PRAGMA optimize` every N persistent tool calls; 0 turns it off. |
