# todo.md — deferred opportunities

Captured during the graph-augmented-recall work (plan: `plans/graph-augmented-recall.md`).
Each item is an idea that needed discussion, larger scope, or product validation, so it was
NOT implemented inline.

## From /wiz:review (deferred — pre-existing or refactor-scoped)

These were raised by the multi-agent review. The high-value, low-risk findings were fixed
inline (single-resolve `resolve_to_path`, static wiki-link regexes, redundant `ensure_handle_context`
removed, `path_like_tokens` non-markdown filter, error-visibility logging, E2E coverage for
permissionDecision/caps/wikilink). The following were deferred:

- **`resolve_ref_to_doc` is collection-unscoped (P2, data-safety).** `SELECT id FROM documents WHERE
  relative_path = ? LIMIT 1` ignores the `(collection, relative_path)` UNIQUE key, so in a
  multi-collection store two docs with the same relative path resolve non-deterministically. This is
  pre-existing graph behavior, now reachable from the UPS hot path. Proper fix threads a collection
  scope through `resolve_ref_to_doc`/`doc_graph_neighbors`. Single-collection stores (the common case)
  are unaffected. Complexity M. Priority P2.
- **`IndexFacade::open_existing` to harden the `code_index_hits` TOCTOU (P3).** `acquire_handle_code_index`
  uses `open_or_create`, so a delete racing the `.exists()` guard can create an empty DB. It's benign
  (empty → falls back to suggestion) and now documented, but an open-only variant would make the
  "never create on the hot path" contract enforced by the type rather than a guard. Complexity S. P3.
- **DRY the Bash pipeline parse (P3, arch/perf).** `bash_definition_symbol` duplicates the
  `unwrap_shell_c → pipeline_sources → tokenize_shell → extract_grep_pattern` skeleton from
  `classify_bash_search`, and a Bash definition search currently parses the command twice (once for the
  suggestion, once for the symbol). Extract `first_grep_pattern_from_command` and thread the parsed
  result so both the suggestion and the code-index symbol come from one parse. Pure string ops (no I/O),
  so this is cleanup not a bug. Complexity S. P3.
- **Push the `KIND_FRONTMATTER` filter into SQL (P3).** `doc_graph_neighbors` calls
  `get_outgoing(.., None)` and filters `source_kind` in Rust, pulling wikilink rows it discards. Add a
  `source_kind` filter to `get_outgoing` (or a sibling). Bounded by cap=3, so low impact. Complexity S. P3.
- **Consider moving `doc_graph_neighbors`'s query into `store::graph` (P3, arch).** The resolution loop is
  pure graph logic; only the `- path (relation)` formatting is a dispatch concern. Splitting query from
  formatting would let `canonical_key` stay private and keep the storage API cohesive. Complexity S. P3.

## 0. Pin the Rust toolchain so `cargo fmt --check` is deterministic

- **Problem:** There is no `rust-toolchain.toml`. CI uses `dtolnay/rust-toolchain@stable`,
  which floats to the latest stable. rustfmt `1.9.0` (2026-05-25) reformats code that earlier
  rustfmt versions left alone — so `cargo fmt --check` fails on `main` too (verified:
  `main:src/git.rs:421`, `main:src/store/memory.rs`), independent of any feature work. This
  branch normalized `git.rs` + `memory.rs` to 1.9.0's canonical form to make `make check` green,
  but the drift will recur on the next rustfmt bump.
- **Proposed solution:** Add a `rust-toolchain.toml` pinning a specific channel/version (with
  `components = ["rustfmt", "clippy"]`) so local and CI format identically and deterministically.
- **Benefits:** `cargo fmt --check` becomes reproducible; no more "works on my machine" fmt
  churn; contributors don't fight rustfmt version skew.
- **Trade-offs:** Requires periodic intentional bumps of the pinned version (which is the point).
- **Complexity:** S.
- **Priority:** P2 (infra hygiene; prevents recurring red `make check`).

## 1. Code-index path base is inconsistent (subdir vs root indexing)

- **Problem:** `mdkb code index src/` stores `code_symbols.file_path` relative to the indexed
  argument (`lib.rs`), while `mdkb code index` (from root) stores it root-relative
  (`src/lib.rs`). The new PreToolUse `file:line` injection surfaces whatever is stored — so a
  project indexed via a subdir argument emits hints that are NOT openable from the repo root.
- **Proposed solution:** Always store `file_path` relative to the project root regardless of the
  indexing argument (canonicalize the indexed path against root, then strip the root prefix).
- **Benefits:** Hints (and `code search`/`code find` output) are always openable from root;
  removes a latent foot-gun; one source of truth for paths.
- **Trade-offs:** Touches the indexing pipeline and requires a one-time reindex; needs an audit of
  all `file_path` consumers (`delete_by_file`, mtime compare, FTS).
- **Complexity:** M.
- **Priority:** P2 (the production hint path indexes from root, so it's correct today; this is
  defense-in-depth + DX).

## 2. Doc-graph recall only resolves explicit paths, not doc titles/slugs

- **Problem:** `path_like_tokens` intentionally detects only `.md` tokens, `/`-paths, and
  `[[wikilinks]]`. A prompt that names a doc by its human title ("the authentication design doc")
  won't trigger neighbor injection. Bare-word resolution was excluded to avoid resolving every
  noun to a document.
- **Proposed solution:** Add an optional fuzzy pass: when a prompt contains a multi-word phrase
  that strongly matches a single document title (high-confidence FTS over `documents.title`),
  treat it as a doc reference and inject its neighbors.
- **Benefits:** Graph value reaches natural-language prompts, not just path-literal ones.
- **Trade-offs:** False positives risk noise on the recall hot path; needs a confidence gate and
  measurement before enabling.
- **Complexity:** M.
- **Priority:** P3 (validate demand via telemetry first).

## 3. Memory→memory graph edges (the original option B)

- **Problem:** Memories are not nodes in the doc graph (`edges.source_doc_id` FKs `documents.id`;
  memory ids are TEXT slugs with no `documents` row), so 1-hop memory→memory expansion in recall
  is impossible without new storage. Marked `DEFERRED (2026-06-30)` in
  `hook_user_prompt_submit_impl`.
- **Proposed solution:** A `memory_edges` table (memory_id ↔ memory_id, typed relation) populated
  from explicit cross-references in memory content, plus a post-recall 1-hop expansion step.
- **Benefits:** Recall can surface related decisions/problems even when FTS misses them.
- **Trade-offs:** Low yield at the current ~12-entry corpus; adds write-path complexity and an
  edge-maintenance burden. Second opinion concurred it's premature.
- **Complexity:** M–L.
- **Priority:** P3 (revisit when the memory corpus is large enough that hybrid recall stops being
  near-total).
