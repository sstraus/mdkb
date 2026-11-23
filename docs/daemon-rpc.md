# Daemon JSON-RPC contract

The mdkb daemon (`mdkb serve --daemon`) answers JSON-RPC 2.0 requests on a local
Unix socket. The CLI hook path, `mdkb status`, `mdkb repos prune` and
external clients such as editor integrations use this socket. This page is the
contract for those clients. The external ingest methods have their own page:
[external-ingest.md](external-ingest.md).

Responses quoted on this page come from a live 3.15.0 daemon.

## Transport

The daemon binds two sockets in `~/.mdkb` (directory mode `0700`, socket mode `0600`):

| Socket | Protocol | Use |
|---|---|---|
| `daemon.sock` | MCP over newline-delimited JSON | `mdkb mcp` forwards the bytes of an MCP client to this socket |
| `daemon-hook.sock` | JSON-RPC 2.0, length-prefixed | everything on this page |

The daemon always binds in `~/.mdkb`, the directory of its PID file. No
configuration key moves the sockets (see [Daemon configuration](daemon.md#daemon-configuration-mdkbdaemontoml)).

Framing on `daemon-hook.sock`: every message is a 4-byte little-endian unsigned
length followed by that many bytes of UTF-8 JSON. Responses use the same framing.
One connection can carry many request/response pairs in sequence. A length of 0 or
more than 4 MiB closes the connection. The daemon accepts at most 64 hook
connections at the same time.

Minimal client (Python):

```python
import json, os, socket, struct

def call(method, params):
    s = socket.socket(socket.AF_UNIX)
    s.connect(os.path.expanduser("~/.mdkb/daemon-hook.sock"))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    s.sendall(struct.pack("<I", len(body)) + body)
    n = struct.unpack("<I", s.recv(4, socket.MSG_WAITALL))[0]
    data = b""
    while len(data) < n:
        data += s.recv(n - len(data))
    s.close()
    return json.loads(data)
```

`mdkb serve --http` also exposes `POST /hook/{method}` with the same dispatcher.
That route reaches only the `hook.*` methods: the path segment is normalised
(`-` becomes `_`, and a `hook.` prefix is added) and a body `cwd` is used as
`root` when `root` is missing. It requires the bearer token like every other HTTP
route except `GET /health`.

## Request rules

- `params.root` is required, and must be an absolute repository path, for every
  method except `ping`, `daemon.repos` and `daemon.repos_prune`.
- The daemon opens the repository through its registry, so `whitelist_dirs`
  applies: a root outside it is refused with `-32602`.
- A request that cannot be parsed, has no `method`, or misses `root` is refused
  before any store is opened.

## Errors

| Code | Meaning | Did it write? |
|---|---|---|
| `-32700` | body is not JSON | no |
| `-32600` | `method` missing | no |
| `-32601` | unknown method (`unknown tool: <name>`) | no |
| `-32602` | invalid params, missing or relative `root`, root outside the whitelist | no |
| `-32010` | the store refused the request before writing: unknown id, duplicate title, evolution cycle, invalid field. Final: retrying gives the same answer | no |
| `-32603` | the method started and then failed | **maybe** |

`-32603` is the only code a method can return after it starts. Every other code
means nothing was written, so a client can safely run the operation another way
(the CLI falls back to a local write). After `-32603`, do not retry a write
blindly.

## Daemon methods (no `root`)

### `ping`

```json
{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}
→ {"id":1,"jsonrpc":"2.0","result":{"pong":true,"version":"3.15.0"}}
```

Use it as a liveness check and to compare the daemon version with the client version.

### `daemon.repos`

Lists the repositories the daemon knows, without walking the disk.
`mdkb status` and `mdkb repos list` render the same rows.

Result: `{"roots": [path, ...], "rows": [row, ...]}`. A row:

```json
{"name": "mdkb", "path": "/abs/path", "kind": "dir", "health": "Empty",
 "docs": 0, "memory": 0, "symbols": 25, "schema": 35, "stale": false,
 "docs_indexed_at": null, "code_indexed_at": 1794450761, "head_at": null,
 "last_opened_at": 1794450756}
```

Timestamps are UTC Unix seconds, or `null` when not known.

### `daemon.repos_prune`

Removes Gone roots from the daemon's repository map. Params:
`{"paths": [absolute path, ...], "dry_run": bool}`. If `paths` is missing or
empty, every Gone root is selected. A relative path or a non-boolean `dry_run`
is refused with `-32602`.

```json
{"jsonrpc":"2.0","id":1,"method":"daemon.repos_prune","params":{"dry_run":true}}
→ {"id":1,"jsonrpc":"2.0","result":{"dry_run":true,"refused":[],"removed":[]}}
```

## Repository methods

Every method below takes `root` and returns `result` as an object with at least
`text` (string) and `tokens` (the token count of `text`).

### MCP tool mirrors

These methods take the same params as the MCP tool of the same name; the MCP
JSON Schema (`tools/list`) is the reference for them. `text` is the text the MCP
tool returns.

| Method | Extra result fields |
|---|---|
| `search` | `count` |
| `get` | `count`, `truncated` |
| `status` | — |
| `update` | `outcome` |
| `memory_write` | — |
| `memory_write_batch` | `count` |
| `memory_delete` | — |
| `memory_confirm` | — |
| `memory_list` | `count` |
| `graph` | — |
| `code_graph` | `symbols` |
| `usage` | — |

### Symbol lookups (editor integration)

For these three methods, `text` holds a **JSON document encoded as a string**.
Parse `result.text` a second time to read it.

Symbol objects use **0-based** `line_start`/`line_end` (tree-sitter rows) and
0-based columns. Add 1 to show a line number to a person.

#### `code_find`

Exact symbol name lookup. Params: `name` (required), `kind` (for example
`Function`, `Struct`, `Constant`), `file` (substring of the path), `limit`
(default 50).

`text` decodes to `{"total": n, "showing": m, "symbols": [symbol, ...]}`.
`total` counts every match. `showing` counts the matches after the `limit`.

```json
{"total":1,"showing":1,"symbols":[{"name":"check_whitelist","kind":"Method",
 "file_path":"src/daemon/config.rs","line_start":388,"line_end":433,
 "col_start":4,"col_end":5,
 "signature":"pub fn check_whitelist(&self, path: &Path) -> Result<()>",
 "scope_context":"ClassMember { class_name: Some(\"DaemonConfig\") }"}]}
```

#### `symbols_in_file`

Every symbol in one file, ordered by position. Params: `file` (repo-relative
path). `text` decodes to an array of symbol objects, the same shape as in
`code_find`. The first entry can be the synthetic `<module>` symbol at row 0.

#### `symbol_at_position`

The innermost symbol that contains a position. Params: `file` (repo-relative),
`line` (**1-based**, as an editor shows it), `col` (0-based, optional).
`text` decodes to one object, or to `null` when no symbol contains the position.
The object has `module_path` instead of `scope_context`:

```json
{"name":"check_whitelist","kind":"Method","file_path":"src/daemon/config.rs",
 "line_start":388,"line_end":433,"col_start":4,"col_end":5,
 "signature":"pub fn check_whitelist(&self, path: &Path) -> Result<()>",
 "module_path":"crate::daemon::config"}
```

If the repository has no code index, all three methods fail with
"code index not available — run `update` first".

### Hook lifecycle

`hook.session_start`, `hook.user_prompt_submit`, `hook.pre_tool_use`,
`hook.post_tool_use` and `hook.stop` carry the hook payloads of `mdkb hook <event>`.
Their result is the hook output that the CLI prints to the agent.
[hooks-architecture.md](hooks-architecture.md) describes their behaviour. They are
internal to the hook path and their result shape can change between releases.

### Other

- `cli.mutate`: CLI write commands routed through the daemon. See
  [decisions/2026-08-08-daemon-cli-mutation-protocol.md](decisions/2026-08-08-daemon-cli-mutation-protocol.md).
  Internal to the CLI.
- `external.upsert`, `external.delete`, `external.list`: see
  [external-ingest.md](external-ingest.md).
