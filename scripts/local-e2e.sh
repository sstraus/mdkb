#!/usr/bin/env bash
# Full local acceptance run. Needs a Rust toolchain, Python 3 and the ONNX model.
set -euo pipefail
cd "$(dirname "$0")/.."

# Preflight: the eval command may return success with "skipped" when the model
# cannot load. Check the report before trusting any vector-related result.
report="$(cargo run --all-features --quiet -- eval recall --mode embedding --download --format json)"
python3 -c 'import json,sys; rows=json.loads(sys.argv[1]); sys.exit(0 if len(rows)==1 and rows[0].get("mode")=="embedding" and "report" in rows[0] else "embedding model unavailable: " + sys.argv[1])' "$report"

# One full suite, including CLI, MCP wire, daemon, watcher and hook tests.
cargo test --all-features
# These model-backed tests are ignored by the default Cargo invocation.
for target in e2e_llm memory_embeddings embed_on_update e2e_memory_graph memory_file_import; do
    cargo test --all-features --test "$target" -- --ignored
done
# Focused model-backed library checks; `--lib -- --ignored` also includes
# manual experiments that need inputs other than the production ONNX model.
for test in committed_fixture_embedding_baseline_holds committed_fixture_hybrid_baseline_holds \
    test_semantic_search_basic test_semantic_search_threshold \
    incremental_update_reembeds_only_changed_symbols; do
    cargo test --all-features --lib "$test" -- --ignored
done

# A green process exit is insufficient when an eval mode was skipped: demand
# both a real report and the published recall/precision floors for each mode.
for mode in embedding hybrid; do
    report="$(cargo run --all-features --quiet -- eval recall --mode "$mode" --min-recall 0.58 --min-precision 1.0 --format json)"
    python3 -c 'import json,sys; rows=json.loads(sys.argv[2]); sys.exit(0 if len(rows)==1 and rows[0].get("mode")==sys.argv[1] and "report" in rows[0] else "eval mode skipped: " + sys.argv[2])' "$mode" "$report"
done
cargo run --all-features --quiet -- eval recall --mode bm25
cargo run --all-features --quiet -- eval judge
