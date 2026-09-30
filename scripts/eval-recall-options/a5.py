"""Light-reranker table: python3 a5.py <name>... (rerank-<name>.json and lat8-<name>.json in EVAL_DATA/opt; the control is jina-v2-ml).
Per row: IT/EN recall at the IT-fit gate (Wilson), negatives admitted, paired sign tests vs the retriever alone and vs the control,
latency at all cores and pinned to 8, RSS."""
import io, contextlib, sys
names = sys.argv[1:]
with contextlib.redirect_stdout(io.StringIO()):
    sys.argv = sys.argv[:1] + ['jina-v2-ml']   # a2 prints its table for its argv on import
    from a2 import rerank_rec, src
from lib import *
ref = 'jina-v2-ml'
def clean_hits(rec, exp_set='it_pos', neg='it_neg'):
    t = max(r[0][1] for r in rec[neg]) + 1e-9
    return t, [r[0][1] >= t and hit(r, e) for r, e in zip(rec[exp_set], EXP[exp_set])]
def row(name, m, k):
    rr = load('rerank-' + name)
    rec = rerank_rec(rr, name, k, src[m])
    t, it = clean_hits(rec)
    en = [r[0][1] >= t and hit(r, e) for r, e in zip(rec['en_pos'], EN_EXP)]
    fp_it = sum(r[0][1] >= t for r in rec['it_neg']); fp_en = sum(r[0][1] >= t for r in rec['en_neg'])
    return rr, rec, it, en, fp_it, fp_en
def lat(name):
    try: l8 = load('lat8-' + name)
    except FileNotFoundError: return None
    return l8
for name in names:
    rr = load('rerank-' + name); l8 = lat(name)
    print(f"## {name}: load {rr['load_ms']:.0f} ms, RSS loaded {rr['rss_loaded_bytes']/1e9:.2f} GB peak {rr['rss_peak_bytes']/1e9:.2f} GB, "
          f"lat32 top5 {rr['latency_top5']['p50_ms']:.0f}/{rr['latency_top5']['p95_ms']:.0f} top10 {rr['latency_top10']['p50_ms']:.0f}/{rr['latency_top10']['p95_ms']:.0f} ms"
          + (f", lat8 top5 {l8['latency_top5']['p50_ms']:.0f}/{l8['latency_top5']['p95_ms']:.0f} top10 {l8['latency_top10']['p50_ms']:.0f}/{l8['latency_top10']['p95_ms']:.0f} ms, lat8 RSS {l8['rss_loaded_bytes']/1e9:.2f}/{l8['rss_peak_bytes']/1e9:.2f} GB" if l8 else ''))
    for m in ('minilm-l6', 'e5-small'):
        base_t, base = clean_hits(src[m])
        for k in (5, 10):
            _, rec, it, en, fp_it, fp_en = row(name, m, k)
            _, _, itc, _, _, _ = row(ref, m, k)
            lo, hi = wilson(sum(it), 40); elo, ehi = wilson(sum(en), 36)
            pb, xb, yb = sign_test(it, base); pc, xc, yc = sign_test(it, itc)
            # EN recall vs the retriever alone, at each one's own IT-fit gate
            print(f"{name:20s} {m:9s} top{k:<2d} IT {sum(it)/40:.3f} [{lo:.2f},{hi:.2f}] | EN {sum(en)/36:.3f} [{elo:.2f},{ehi:.2f}] | admitted IT/EN neg {fp_it}/{fp_en} | vs retriever +{xb}/-{yb} p={pb:.3f} | vs jina fp32 +{xc}/-{yc} p={pc:.3f}")
