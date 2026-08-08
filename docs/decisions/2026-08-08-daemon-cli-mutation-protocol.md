# Daemon CLI Mutation Protocol

## Problem

The daemon is intended to be the only process that opens a store for writing,
but the CLI currently routes only `update` and `memory rm`. Every other command
classified as a mutation falls through to the in-process match and opens its own
write-capable connection. Adding one unrelated JSON-RPC method per CLI spelling
would close the immediate gap while creating a second public API with roughly
twenty-five methods whose argument and result semantics must remain synchronized
with Clap and the core services.

The audit also found that several commands classified as reads still opened a
write-capable context. Some read handlers performed telemetry updates or schema
initialization. Routing mutations cannot establish a sole-writer invariant while
the read side can still write.

## Decision

The hook socket exposes one internal method, `cli.mutate`. Its request is a
tagged, typed mutation enum and its response is a tagged, typed result enum. The
enum is exhaustive over CLI mutations and is converted explicitly from the Clap
command. The daemon executes each variant through the existing core operation;
the CLI alone renders the returned result.

This is an internal same-version protocol, not another public tool surface. The
daemon already retires when its executable is replaced, so the client and daemon
share the enum version. Existing MCP methods such as `memory_write`,
`memory_confirm`, and `update` remain supported for their public callers, but
their implementation and `cli.mutate` call the same core operations.

`init` remains local and is classified as such. It creates the store required to
register a daemon handle, so routing it through that handle would introduce a
circular bootstrap protocol without improving concurrency safety. Read commands
open `Context::open_read_only`; read handlers do not update access telemetry,
initialize schemas, repair databases, or open code storage read-write.

## Alternatives

### One JSON-RPC method per CLI mutation

Rejected. It duplicates the CLI surface, multiplies dispatch boilerplate, and
makes partial semantic drift likely. Several existing MCP methods could be
reused, but they do not cover the full CLI result contract.

### Send raw argv and run the CLI inside the daemon

Rejected. It couples the daemon to terminal parsing and printing, makes results
unstructured, and would either require unsafe global stdout capture or retain
daemon-side output.

### Serialize the entire Clap command

Rejected. Local and read-only commands do not belong on the mutation protocol,
and deriving a wire contract from parser types makes unrelated CLI changes alter
the daemon protocol implicitly.

## Trade-offs

The explicit mutation and result enums contain one variant per behavior, so a
new mutating command requires a deliberate protocol decision and fails the
exhaustiveness checks until mapped. This is more code than an untyped JSON bag,
but less code and less public surface than separate RPC methods. Long-running
commands return one final result; incremental progress is deferred because the
current one-response framing cannot stream without a separate protocol.

## Failure Semantics

A request rejected before dispatch may run in-process as the documented daemon
outage fallback. Once the complete request has been delivered and dispatch may
have begun, timeout, disconnect, and execution errors never trigger a second
write. The CLI reports that the daemon may still be writing. Result decoding is
also post-dispatch and therefore never licenses a retry.

## Ownership and Lifecycle

The daemon owns all write-capable `Context` and code-index connections after a
store exists. Core operations own behavior and return data. The CLI owns parsing
and presentation. The hook socket owns framing and delivery evidence. Adding a
mutation requires updating the typed conversion, daemon executor, result
renderer, and exhaustiveness test in the same change.
