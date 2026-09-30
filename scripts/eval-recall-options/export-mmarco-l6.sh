#!/usr/bin/env bash
# Export nreimers/mmarco-mMiniLMv2-L6-H384-v1 (PyTorch only on HF) to ONNX fp32 + dynamic int8, into the hf-hub cache layout
# that recall_options_eval.rs reads. Runs on the rb box: `rb <worktree> -- bash scripts/eval-recall-options/export-mmarco-l6.sh`.
# Python deps go into a pip --target directory under ~/Gits/.tmp (nothing global). Prints package versions and max |logit| deviations.
set -euo pipefail
REPO=nreimers/mmarco-mMiniLMv2-L6-H384-v1 REV=4ceabf2d1e212e16da0d1fb94d5dea66a9a1cca0
ROOT=$HOME/Gits/.tmp/fastembed-mdkb
SNAP=$ROOT/models--${REPO//\//--}/snapshots/$REV
mkdir -p "$SNAP/onnx" "$ROOT/models--${REPO//\//--}/refs"; printf %s "$REV" > "$ROOT/models--${REPO//\//--}/refs/main"
[ -s "$SNAP/onnx/model.onnx" ] && [ -s "$SNAP/onnx/model_qint8_arm64.onnx" ] && { echo "already exported" >&2; exit 0; }
for f in config.json special_tokens_map.json tokenizer_config.json tokenizer.json pytorch_model.bin; do
  [ -s "$SNAP/$f" ] || curl -sSfL --retry 3 -o "$SNAP/$f" "https://huggingface.co/$REPO/resolve/$REV/$f"
done
# The box has no python3-venv: install into a plain directory (--target) and put it on PYTHONPATH.
LIBS=$HOME/Gits/.tmp/pylibs-onnx-export
[ -d "$LIBS/torch" ] || python3 -m pip install -q --target "$LIBS" --break-system-packages torch transformers onnx onnxruntime numpy >&2
PYTHONPATH=$LIBS python3 - "$SNAP" <<'PY'
import sys, torch, transformers, onnx, onnxruntime as ort, numpy as np
from onnxruntime.quantization import quantize_dynamic, QuantType
snap = sys.argv[1]
print("versions torch", torch.__version__, "transformers", transformers.__version__, "onnx", onnx.__version__, "onnxruntime", ort.__version__)
tok = transformers.AutoTokenizer.from_pretrained(snap)
model = transformers.AutoModelForSequenceClassification.from_pretrained(snap).eval()
enc = tok(["query one", "second query longer"], ["passage text", "another passage with more words in it"], padding=True, return_tensors="pt")
fp32 = f"{snap}/onnx/model.onnx"
torch.onnx.export(model, (enc["input_ids"], enc["attention_mask"]), fp32, input_names=["input_ids", "attention_mask"],
                  output_names=["logits"], dynamic_axes={"input_ids": {0: "b", 1: "s"}, "attention_mask": {0: "b", 1: "s"}, "logits": {0: "b"}},
                  opset_version=17, dynamo=False)
int8 = f"{snap}/onnx/model_qint8_arm64.onnx"
quantize_dynamic(fp32, int8, weight_type=QuantType.QInt8)
# Check both files against torch on real-length pairs.
qs = ["come funziona il recall dei ricordi", "why does the hook time out", "indice FTS5 porter unicode61"]
ps = ["The recall hook injects memory entries before the deadline.", "Il deadline del hook taglia la ricerca.", "CREATE VIRTUAL TABLE docs USING fts5(title, body, tokenize='porter unicode61')"]
e = tok(qs, ps, padding=True, truncation=True, max_length=512, return_tensors="pt")
ref = model(**e).logits.detach().numpy()
for name, path in (("fp32", fp32), ("int8", int8)):
    s = ort.InferenceSession(path, providers=["CPUExecutionProvider"])
    out = s.run(["logits"], {"input_ids": e["input_ids"].numpy(), "attention_mask": e["attention_mask"].numpy()})[0]
    print(name, "outputs", [o.name for o in s.get_outputs()], "max|logit - torch|", float(np.abs(out - ref).max()), "logits", out.ravel().tolist(), "torch", ref.ravel().tolist())
PY
ls -l "$SNAP/onnx" >&2
