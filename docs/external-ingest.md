# External document ingest (JSON-RPC)

External owners push documents to the daemon's existing `daemon-hook.sock`.
Transport: JSON-RPC 2.0, each UTF-8 JSON message prefixed with its byte length
as a little-endian u32. The existing 4 MiB request limit and root whitelist apply.
These are daemon methods, not MCP tools or CLI write commands.

## Identity and ordering

The identity is `(canonical root, source, id)`. `source` and `id` are opaque,
non-empty strings (maximum 256 and 1024 UTF-8 bytes). `source` is generic;
`tuic-stories` is the first consumer. Neither string is interpreted as a path.
`revision` is an integer from 0 through 9223372036854775807. The owner increments
it on every change, including deletion. Revisions must remain monotonic across
owner restarts; timestamps are not used to order writes.

A request with a greater revision applies atomically. Equal revisions are
idempotent no-ops, even if their payload differs. Older revisions are no-ops.
Deletion retains a tombstone, so a delayed upsert cannot resurrect a deleted
record. A greater revision can intentionally recreate it. No tombstone expiry.

## Methods

All params include `root`: an absolute repository path.

### `external.upsert`

Params:

```json
{"root":"/absolute/repo","source":"tuic-stories","id":"2333-b498","revision":7,"title":"Index native stories","body":"Story Markdown content","metadata":{"status":"in_progress","type":"feature","updated_at":"2026-10-25T16:00:00Z"}}
```

`title` and `body` are required strings. `metadata` is an optional JSON object,
default `{}`; it stores owner fields unchanged, including status, type and
updated_at. The server owns the reserved `external` metadata field, which holds
`source`, `id`, `revision`, and `read_only: true`. Owner status does not change
the index's document lifecycle status: completed stories remain searchable.

Result:

```json
{"id":"2333-b498","revision":7,"document_id":123,"deleted":false,"applied":true}
```

`document_id` is the numeric ID accepted by CLI/MCP get. A stale or equal request
returns the current record (including its current revision and deletion state)
with `applied: false`; `document_id` is null for a tombstone.

### `external.delete`

Params: `root`, `source`, `id`, `revision`, with the same types as upsert.
Result has the same shape as upsert. An accepted delete returns `deleted: true`
and `document_id: null`, including when the ID was not previously present.

### `external.list`

Params: `root`, `source`.
Result, sorted by external ID:

```json
{"documents":[{"id":"2333-b498","revision":8,"document_id":null,"deleted":true}]}
```

Includes live records and tombstones, not bodies. The client compares revisions
and pushes missing/newer changes. A client must also delete IDs it no longer
owns, using a revision greater than the listed revision. Listings are a single
store snapshot; subsequent pushes can race and are protected by revisions.

## Errors

Standard JSON-RPC errors: `-32700` invalid JSON, `-32600` invalid envelope,
`-32601` unknown method, `-32602` invalid params or root admission failure,
`-32603` store/dispatch failure. Parameter errors do not mutate documents.
A disconnected client can retry the identical revision safely. After a store
error, resync through list: do not assume that a request did not commit.

## Store and read behavior

Reuse collections, documents, content-addressed bodies, and their FTS triggers:
this keeps external content on the existing CLI/MCP search and numeric get paths,
including embeddings. A dedicated external-record table maps opaque owner IDs
to document IDs and retains revisions/tombstones. Each source has a reserved,
non-filesystem collection. Filesystem indexing and watcher rescans skip these
collections. No Markdown mirror or source file is created.

Search/get label these documents read-only and show the source. JSON search
results add `source` and `read_only: true` for external records; ordinary document
results retain their existing shape. JSON get exposes these fields under
`document.metadata.external` together with the owner ID and revision. Collection and
document mutation paths refuse external ownership; only these ingest methods
can change owner content or remove records. A full store deletion or corruption
recovery that rebuilds the entire database still requires client resync; ordinary
update/full reindex does not remove external records.
