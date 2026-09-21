# Knowledge graph

mdkb keeps two graphs. They share the `graph` command and the `graph` MCP tool,
and are told apart by scope:

| Graph | Nodes | Edges come from | Scope |
|---|---|---|---|
| Document graph | indexed markdown documents and the slugs they name | frontmatter keys on the allowlist, body `[[wikilinks]]` | `doc` (default) |
| Memory graph | memory entries, and documents a memory points at | `relates` on `memory_write`, `mdkb memory link` | `memory` |

Search answers "which documents mention this?". The graph answers "which
documents did the author connect to this, and how?" — a relation an author
typed is evidence; a term two documents happen to share is not.

## Ownership and reorganization

Humans and agents own meaning: they write frontmatter, wikilinks, and memory
relations. MDKB owns mechanical consistency: indexing extracts document edges,
re-indexing replaces the changed document's edges, collection reconciliation
assigns each document to the most specific collection, and `supersedes` updates
memory status atomically with its edge.

MDKB does not infer a taxonomy, move files, rewrite prose, or automatically act
on centrality. `graph dangling` and `graph hubs` are read-only gardening reports;
`dup` and `coupling` are separate read-only structural audits. This boundary
keeps repository organization reviewable instead of letting a ranking heuristic
silently reshape source material.

## The document graph

### How edges are created

Edges are extracted during indexing (`mdkb update`), from two sources.

**Frontmatter keys on the allowlist** become typed edges. The key is the
relation name:

```yaml
---
title: Payments rewrite
owner: alice
stakeholders: [bob, carol]
themes: [billing, reliability]
related: architecture/queues.md
---
```

With the default allowlist that document gets six outgoing edges: `owner→alice`,
`stakeholders→bob`, `stakeholders→carol`, `themes→billing`,
`themes→reliability`, `related→architecture/queues.md`.

A key holds either a string or a list of strings. Values are trimmed; empty ones
are dropped. Any other YAML type (a number, a map, a nested list) yields no edge
— `owner: {name: alice}` is silently ignored, so keep relation values flat.

Only keys named in `graph.frontmatter_relations` become edges. Everything else
in the frontmatter is still indexed and searchable, it just is not an edge.
`supersedes`, `updates`, `corrects`, `extends` and `retracts` are owned by the
evolution subsystem, not the graph — do not add them to the allowlist.

**Body wikilinks** become soft edges. Every `[[target]]` in the body produces one
edge with the relation `links_to`. Turn them off with `include_wikilinks = false`
if a repository uses `[[...]]` for something other than references.

Edges are replaced wholesale on every re-index of a document, so re-indexing is
idempotent and deleting a link from the file deletes the edge.

### Targets are stored verbatim, resolved at query time

An edge points at the string the author wrote. It is not required to name an
indexed document — a reference to a page that does not exist yet, or to a person
who will never have one, is a first-class edge. This is what makes indexing
order irrelevant: `alice.md` picks up its backlinks the moment it is indexed,
without touching the documents that pointed at it.

When a reference *is* resolved, these forms are tried in order, first match wins:

1. the reference verbatim
2. with a leading `./` or `/` stripped
3. with `.md` added, or removed if already present
4. all of the above again with a leading collection name stripped
   (`map/people/alice.md` resolves like `people/alice.md`)

So `people/alice`, `people/alice.md`, `./people/alice.md` and
`map/people/alice` all reach the same document. When nothing matches, the error
lists every form that was tried instead of just saying "not found".

### Configuration

`.mdkb/config.toml`, section `[graph]`. Shown with the defaults:

```toml
[graph]
enabled = true
frontmatter_relations = ["owner", "stakeholders", "themes", "related"]
include_wikilinks = true

# Recall expansion — how much graph context rides along with a recalled memory
expand_seeds = 2       # top recall seeds whose neighbors are surfaced
expand_neighbors = 3   # hard cap on neighbors surfaced across all seeds
doc_neighbor_cap = 3   # cap on neighbor lines when a prompt names a document
```

Add your own vocabulary to `frontmatter_relations` — `depends_on`, `replaces`,
`team`, whatever the repository already writes. A plain `mdkb update` will not
apply the change to documents it has already seen: it skips files whose mtime
has not moved, and their edges with them. Run `mdkb update --force` once after
editing the allowlist.

Which keys are relations is a decision only the repository can make: a relation
target is any string or list of strings, so `type: person` and `org: [org:acme]`
are indistinguishable by value. The README explains what auto-detection would do
to `graph hubs`. See also
[cross-folder-flows.html](cross-folder-flows.html) for how a store is chosen for a
working directory and how the graph boundary follows the store boundary.

The three `expand_*` values are the only graph settings on a hot path. They cap
how much the graph is allowed to add to each prompt injection; raise them
deliberately.

## Querying

Both surfaces read the same edges.

| Question | CLI | MCP |
|---|---|---|
| What does this point at? | `mdkb graph links <entity>` | `graph(entity, direction="links")` |
| What points at this? | `mdkb graph backlinks <entity>` | `graph(entity, direction="backlinks")` |
| What is nearby? | `mdkb graph neighbors <entity> --depth 2` | `graph(entity, direction="neighbors", depth=2)` |
| How are these two connected? | `mdkb graph path <a> <b>` | `graph(entity, direction="path", to=<b>)` |
| Which references are broken? | `mdkb graph dangling` | — |
| What is central here? | `mdkb graph hubs` | — |

Every query takes `--relation` / `relation` to filter to one relation type.
Endpoints always render as document paths, never as numeric ids.

`links`, `neighbors` and `path` need their starting entity to be an indexed
document — you cannot list the outgoing edges of a slug that owns no file.
`backlinks` does not: asking who points at `alice` works whether or not
`alice.md` exists, which is usually the point.

`neighbors` is undirected and returns each entity once, with the depth it was
reached at and the relations it was reached through (`via`). Dangling targets
appear as leaves and are not expanded further. `path` is undirected too, with
`--max-hops` defaulting to 6.

`dangling` and `hubs` scan the whole edge table. They are gardening commands,
run on request; nothing in the hook path calls them.

## How to use it

**Start from a document, not from a query.** The graph is worth reaching for
when you already have one entity and want its context. If you do not yet know
which document matters, `mdkb search` first, then follow edges from the hit.

**Ask for backlinks before editing.** `mdkb graph backlinks payments.md` is the
blast radius of a change: every document that named this one, and under which
relation. Rename or retire a document without it and you create dangling
references you will find much later.

**Use `--relation` to separate structure from prose.** `links_to` is whatever
someone typed in a sentence; `owner` or `depends_on` is a deliberate assertion.
Mixing them makes a hub list meaningless. `mdkb graph hubs --relation owner`
answers "who carries the most", `mdkb graph hubs` answers "what gets mentioned
the most" — different questions.

**Let `dangling` drive the writing queue.** A reference pointing at no document
is either a typo or a page that ought to exist. Both are worth knowing; running
it after a batch of edits is the cheapest review there is.

**Keep depth small.** `neighbors --depth 2` on a well-linked store already
returns a lot; depth 3 usually returns most of the repository and tells you
nothing. If you need reach rather than breadth, use `path`.

**Do not expect the graph to infer.** Nothing is derived, weighted or guessed:
an edge exists because someone wrote a frontmatter key or a wikilink. A graph
that answers poorly is a repository whose frontmatter is thin — the fix is in
the documents, or in `frontmatter_relations`, not in the query.

### A worked pass over a repository

```bash
mdkb update                                  # extract edges from what changed
                                             # (--force after editing the allowlist)
mdkb graph hubs --limit 20                   # what everything points at
mdkb graph hubs --relation owner             # who carries the most
mdkb graph dangling                          # references with no document
mdkb graph backlinks architecture/queues.md  # who depends on this before changing it
mdkb graph path payments.md oncall.md        # is there a documented connection at all?
```

## The memory graph

Memory entries are graph nodes with their own closed set of relations —
`supports`, `contradicts`, `supersedes`, `derived_from`, `relates_to`. An
unknown relation is rejected, and the valid set is listed in the error.

```bash
mdkb memory link auth-v2 supersedes auth-v1
mdkb memory link auth-v2 derived_from docs/auth.md --doc
```

Or at write time, `memory_write(..., relates=[{relation, target, target_kind}])`
— up to 10 edges, written in the same transaction as the entry. `target_kind` is
`memory` (default) or `doc`.

Traverse with `graph(entity, direction="links"|"backlinks", scope="memory")`.
Targets are dangling-tolerant and resolved at query time, exactly like the
document graph.

Two behaviours are specific to memory: `supersedes` keeps the entry's
`superseded_by` field and `superseded` status in lockstep with the edge, and an
entry whose `derived_from`/`supports` target has been superseded or refuted is
prefixed `[STALE-DEP]` when injected. The prefix is a read-only flag — it never
mutates stored confidence.

See the memory sections of the [README](../README.md) for the entry lifecycle
these edges hang off.
