"""Held-out check of the recall reranker gate (story 202-4c67).

    python3 a6.py pool   <heldout-dir>   # writes <heldout-dir>/pool.json: MiniLM top-5 of every held-out query
    python3 a6.py report <heldout-dir>   # recall / admitted negatives at the shipped thresholds, Wilson 95%

<heldout-dir> is private (never committed) and holds: translations.json with the sets `it_held` and `en_held`
(`pos`, `neg` query lists, fed to the `recall_options_eval` harness as `it_held_pos` ...), labels.json (expected ids per
positive), embed-minilm.json (`recall_options_eval embed minilm-l6 <dir> assets/eval/memory-recall.json`) and
rerank-jina-v2-int8.json (`recall_options_eval rerank jina-v2-int8 <dir> assets/eval/memory-recall.json <dir>/pool.json`).
The fit data (EVAL_DATA, default ~/Gits/.tmp/mdkb-ml/, `opt/` inside) gives the thresholds the held-out set is judged at.

A positive counts as recalled when an expected id is among the INJECTED entries: the top-5 entries whose reranker
score is at or above the language's floor (the hook injects those, in score order). `top5-hit` is the eval's older
definition (expected id anywhere in the top 5 once the top score passes), kept so the two can be compared. A negative
is admitted when its top score reaches the floor.
"""
import json, math, os, sys

FIT = os.path.expanduser(os.environ.get('EVAL_DATA', '~/Gits/.tmp/mdkb-ml/')) + 'opt/'
T_IT, T_EN = -1.05, -1.95  # config.rs RECALL_RERANK_MIN_SCORE_{IT,EN}_DEFAULT
K = 5
SETS = ('it_held_pos', 'it_held_neg', 'en_held_pos', 'en_held_neg')


def wilson(k, n, z=1.96):
    if n == 0:
        return (float('nan'),) * 2
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return max(0, c - h), min(1, c + h)


def load(path):
    return json.load(open(path))


def pool(d):
    emb = load(d + 'embed-minilm.json')['sets']
    items = [
        {'set': s, 'i': i, 'cands': [c for c, _ in q['top'][:K]]}
        for s in SETS
        for i, q in enumerate(emb[s])
    ]
    json.dump(items, open(d + 'pool.json', 'w'))
    print(len(items), 'pool items')


def fit_thresholds():
    """The fit rule on the 2026-09-30 data: lowest 0.05 step above the best negative, per language."""
    emb = load(FIT + 'embed-minilm-l6.json')['sets']
    rr = load(FIT + 'rerank-jina-v2-int8.json')['scores']
    out = {}
    for lang, neg in (('it', 'it_neg'), ('en', 'en_neg')):
        tops = [max(rr[f'{neg}/{i}'][c] for c, _ in q['top'][:K]) for i, q in enumerate(emb[neg])]
        out[lang] = (max(tops), math.floor(max(tops) / 0.05) * 0.05 + 0.05)
    return out


def scored_rows(d, name):
    """Per query of set `name`: (embed record, [(reranker score, id)] best first over the MiniLM top 5)."""
    emb = load(d + 'embed-minilm.json')['sets']
    rr = load(d + 'rerank-jina-v2-int8.json')['scores']
    rows = []
    for i, q in enumerate(emb[name]):
        cands = [c for c, _ in q['top'][:K]]
        rows.append((q, sorted(((rr[f'{name}/{i}'][c], c) for c in cands), reverse=True)))
    return rows


def report(d):
    labels = load(d + 'labels.json')
    fit = fit_thresholds()
    print('fit thresholds (best negative, stepped):', {k: (round(v[0], 4), round(v[1], 2)) for k, v in fit.items()})
    print('shipped thresholds: IT', T_IT, 'EN', T_EN)
    for lang, t in (('it', T_IT), ('en', T_EN)):
        pos, neg = f'{lang}_held_pos', f'{lang}_held_neg'
        exp = labels[pos]
        pos_rows, neg_rows = scored_rows(d, pos), scored_rows(d, neg)
        hits = top5 = injected = 0
        for (q, scored), e in zip(pos_rows, exp):
            inj = [c for s, c in scored if s >= t]
            injected += len(inj)
            hits += any(c in e for c in inj)
            top5 += bool(inj) and any(c in e for _, c in scored)
        n, m = len(exp), len(neg_rows)
        admitted = [(scored[0][0], i) for i, (q, scored) in enumerate(neg_rows) if scored[0][0] >= t]
        adm_inj = sum(len([1 for s, _ in scored if s >= t]) for q, scored in neg_rows)
        best_neg = max(scored[0][0] for q, scored in neg_rows)
        # MiniLM alone at the automatic gate (recall_auto_min_cosine 0.50): top-5 cosine >= 0.50
        base = sum(any(c in e and s >= 0.5 for c, s in q['top'][:K]) for (q, _), e in zip(pos_rows, exp))
        base_neg = sum(any(s >= 0.5 for _, s in q['top'][:K]) for q, _ in neg_rows)
        print(f'{lang.upper()} held-out: {n} positives, {m} negatives, floor {t}')
        print(f'  recall (injected)   {hits}/{n} = {hits / n:.3f} [{wilson(hits, n)[0]:.2f}, {wilson(hits, n)[1]:.2f}]   entries injected {injected}')
        print(f'  recall (top5-hit)   {top5}/{n} = {top5 / n:.3f} [{wilson(top5, n)[0]:.2f}, {wilson(top5, n)[1]:.2f}]')
        print(f'  negatives admitted  {len(admitted)}/{m} = {len(admitted) / m:.3f} [{wilson(len(admitted), m)[0]:.2f}, {wilson(len(admitted), m)[1]:.2f}]   entries injected {adm_inj}; best negative {best_neg:.3f}')
        print(f'  MiniLM alone @0.50  recall {base}/{n} = {base / n:.3f} [{wilson(base, n)[0]:.2f}, {wilson(base, n)[1]:.2f}], negatives admitted {base_neg}/{m}')
        if admitted:
            print('  admitted negatives (score, index):', [(round(s, 3), i) for s, i in sorted(admitted, reverse=True)])


if __name__ == '__main__':
    cmd, d = sys.argv[1], os.path.expanduser(sys.argv[2]).rstrip('/') + '/'
    {'pool': pool, 'report': report}[cmd](d)
