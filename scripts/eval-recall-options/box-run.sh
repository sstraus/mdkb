#!/usr/bin/env bash
# Run on the rb box (arm64 Linux) through `rb <worktree> -- bash scripts/eval-recall-options/box-run.sh`.
# Needs eval-data-private/ (untracked: corpus.json it_set.json sample.json translations.json pool.json empty.json).
# Every raw JSON goes to stdout between `@@BEGIN <name>` and `@@END <name>` markers; the caller extracts them
# from the rb log. Build and model noise goes to stderr.
set -uo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
export FASTEMBED_CACHE_DIR=$HOME/Gits/.tmp/fastembed-mdkb HF_HOME=$HOME/Gits/.tmp/fastembed-mdkb
mkdir -p "$FASTEMBED_CACHE_DIR"
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
