#!/usr/bin/env bash
# Run on the rb box (arm64 Linux) through `rb <worktree> -- bash scripts/eval-recall-options/box-run.sh`.
# Needs eval-data-private/ (untracked: corpus.json it_set.json sample.json translations.json pool.json empty.json).
# Every raw JSON goes to stdout between `@@BEGIN <name>` and `@@END <name>` markers; the caller extracts them
# from the rb log. Build and model noise goes to stderr.
set -uo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
export FASTEMBED_CACHE_DIR=$HOME/Gits/.tmp/fastembed-mdkb HF_HOME=$HOME/Gits/.tmp/fastembed-mdkb
mkdir -p "$FASTEMBED_CACHE_DIR"
# The box TLS chain fails hf-hub's rustls roots (UnknownIssuer) but not curl: pre-populate the hf-hub cache
# (snapshots/<commit>/<file> + refs/main, revisions pinned to the ones the Mac runs used).
fetch() { # repo commit file...
  repo=$1 commit=$2; shift 2
  dir=$FASTEMBED_CACHE_DIR/models--${repo//\//--}
  mkdir -p "$dir/refs"; printf %s "$commit" > "$dir/refs/main"
  for f in "$@"; do
    [ -s "$dir/snapshots/$commit/$f" ] && continue
    mkdir -p "$(dirname "$dir/snapshots/$commit/$f")"
    curl -sSfL --retry 3 -o "$dir/snapshots/$commit/$f.part" "https://huggingface.co/$repo/resolve/$commit/$f" >&2 && mv "$dir/snapshots/$commit/$f.part" "$dir/snapshots/$commit/$f" || echo "fetch failed: $repo $f" >&2
  done
}
TOK="config.json special_tokens_map.json tokenizer_config.json tokenizer.json"
fetch Qdrant/all-MiniLM-L6-v2-onnx 5f1b8cd78bc4fb444dd171e59b18f3a3af89a079 $TOK model.onnx
fetch intfloat/multilingual-e5-small 614241f622f53c4eeff9890bdc4f31cfecc418b3 $TOK onnx/model.onnx
fetch jinaai/jina-reranker-v2-base-multilingual 9cfeff2df7d40d1b78e75e5e9cebec92a99813c9 $TOK onnx/model.onnx
fetch BAAI/bge-reranker-base 2cfc18c9415c912f9d8155881c133215df768a70 $TOK onnx/model.onnx
fetch rozgo/bge-reranker-v2-m3 fbd57b17b4db111a9d16813bb08b4c804fac18e9 $TOK model.onnx model.onnx.data
D=eval-data-private; F=assets/eval/memory-recall.json
cargo build --release --example recall_options_eval >&2 || { echo "@@BUILD FAILED"; exit 1; }
B=target/release/examples/recall_options_eval
env_note() { echo "@@BEGIN env-$1"; echo "loadavg=$(cat /proc/loadavg) nproc=$(nproc) taskset=$(taskset -p $$ | cut -d: -f2)"; grep -m1 -E 'model name|Model' /proc/cpuinfo; lscpu | grep -E 'Model name|Thread|Core|Socket' ; free -m | sed -n 2p; echo "@@END env-$1"; }
emit() { name=$1; shift; env_note "$name"; echo "@@BEGIN $name"; "$@" 2>&2; echo "@@END $name exit=$?"; }
for m in minilm-l6 e5-small; do emit "embed-l-$m" $B embed $m $D $F; done
emit lex-l $B lex $D $F
for m in jina-v2-ml bge-base bge-v2-m3; do emit "rerank-$m" $B rerank $m $D $F $D/pool.json; done
# Constrained latency: same reranker calls, empty pool, pinned to 8 cores (Rust available_parallelism honours affinity).
for m in jina-v2-ml bge-base bge-v2-m3; do emit "lat8-$m" taskset -c 0-7 $B rerank $m $D $F $D/empty.json; done
