#!/usr/bin/env bash
# Light-reranker benchmark on the rb box (arm64 Linux): `rb <worktree> -- bash scripts/eval-recall-options/box-run-light.sh <control|cands>`.
# Needs eval-data-private/ (untracked). Same @@BEGIN/@@END stdout protocol as box-run.sh; model noise goes to stderr.
# `control` reproduces jina-v2 fp32 (builtin fastembed file); `cands` runs the lighter files. Every model is scored over
# pool.json at all cores, then latency-only (empty pool) pinned to 8 cores.
set -uo pipefail
mode=${1:?control|cands}; shift; only=("$@")   # optional: restrict `cands` to these reranker keys
export PATH="$HOME/.cargo/bin:$PATH"
export FASTEMBED_CACHE_DIR=$HOME/Gits/.tmp/fastembed-mdkb HF_HOME=$HOME/Gits/.tmp/fastembed-mdkb
mkdir -p "$FASTEMBED_CACHE_DIR"
# The box TLS chain fails hf-hub's rustls roots (UnknownIssuer) but not curl: pre-populate the hf-hub cache
# (snapshots/<commit>/<file> + refs/main, pinned revisions).
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
JV2=jinaai/jina-reranker-v2-base-multilingual JV2C=9cfeff2df7d40d1b78e75e5e9cebec92a99813c9
case $mode in
  control) fetch $JV2 $JV2C $TOK onnx/model.onnx; keys=(jina-v2-ml) ;;
  cands)
    fetch $JV2 $JV2C $TOK onnx/model_int8.onnx onnx/model_fp16.onnx
    fetch cross-encoder/mmarco-mMiniLMv2-L12-H384-v1 1427fd652930e4ba29e8149678df786c240d8825 $TOK onnx/model.onnx onnx/model_qint8_arm64.onnx
    fetch jinaai/jina-reranker-v1-turbo-en b8c14f4e723d9e0aab4732a7b7b93741eeeb77c2 $TOK onnx/model_int8.onnx
    bash scripts/eval-recall-options/export-mmarco-l6.sh >&2 || echo 'mmarco-l6 export failed' >&2   # L6 has no ONNX on HF
    keys=(jina-v1-turbo-int8 mmarco-q8arm jina-v2-int8 mmarco-fp32 mmarco-l6-q8arm mmarco-l6-fp32 jina-v2-fp16)
    [ ${#only[@]} -eq 0 ] || keys=("${only[@]}") ;;
  *) echo "unknown mode $mode" >&2; exit 64 ;;
esac
D=eval-data-private; F=assets/eval/memory-recall.json
cargo build --release --example recall_options_eval >&2 || { echo "@@BUILD FAILED"; exit 1; }
B=target/release/examples/recall_options_eval
echo "@@BEGIN env-$mode"; echo "loadavg=$(cat /proc/loadavg) nproc=$(nproc)"; lscpu | grep -E 'Model name'; free -m | sed -n 2p
find "$FASTEMBED_CACHE_DIR" -name '*.onnx' -printf '%s %p\n'; echo "@@END env-$mode"
emit() { name=$1; shift; echo "@@BEGIN $name"; "$@"; echo "@@END $name exit=$?"; }
for k in "${keys[@]}"; do emit "rerank-$k" $B rerank $k $D $F $D/pool.json; done
for k in "${keys[@]}"; do emit "lat8-$k" taskset -c 0-7 $B rerank $k $D $F $D/empty.json; done
