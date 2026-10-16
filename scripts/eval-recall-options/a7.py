"""Eval gate of plan configurable-embedder Step 1 (story 005-a632): does an e5-small retriever under the jina
reranker beat the shipped jina-on-MiniLM path?

    EVAL_DATA=~/Gits/.tmp/mdkb-ml/ python3 a7.py

Reads the private fit data in EVAL_DATA/opt/ (embed-minilm-l6.json, embed-e5-small.json, rerank-jina-v2-int8.json)
and the labels in EVAL_DATA/it_set.json plus assets/eval/memory-recall.json. Same recall definition as a6.py: a positive
counts when an expected id is among the INJECTED entries (pool entries at or above the language floor, best 5 by
reranker score). Floors follow the store's fit rule (lowest 0.05 step above the best negative of that language), once
in-sample and once held out (300 random halves of the negatives: fit on one half, admit-rate on the other).
"""
import json, math, os, random

M = os.path.expanduser(os.environ.get('EVAL_DATA', '~/Gits/.tmp/mdkb-ml/'))
O = M + 'opt/'
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
INJECT = 5
SPLITS = 300

IT_EXP = [p['expected'] for p in json.load(open(M + 'it_set.json'))['pairs']]
EN_EXP = [r['expected_ids'] for r in json.load(open(REPO + '/assets/eval/memory-recall.json'))['recall']]
RR = json.load(open(O + 'rerank-jina-v2-int8.json'))['scores']
EMB = {m: json.load(open(O + f'embed-{m}.json'))['sets'] for m in ('minilm-l6', 'e5-small')}

CONFIGS = [('minilm top5 + jina (shipped)', 'minilm-l6', 5),
           ('e5-small top5 + jina', 'e5-small', 5),
           ('e5-small top10 + jina', 'e5-small', 10)]


def wilson(k, n, z=1.96):
    p = k / n; d = 1 + z * z / n; c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return max(0, c - h), min(1, c + h)


def sign_test(a, b):
    x = sum(1 for i, j in zip(a, b) if i and not j); y = sum(1 for i, j in zip(a, b) if j and not i)
    n = x + y
    if n == 0:
        return 1.0, x, y
    return min(1.0, 2 * sum(math.comb(n, i) for i in range(min(x, y) + 1)) / 2 ** n), x, y


def scored(model, k, s):
    """Per query of set s: [(jina score, id)] over the retriever's top k, best first. Raises on a missing score."""
    rows = []
    for i, q in enumerate(EMB[model][s]):
        cands = [c for c, _ in q['top'][:k]]
        sc = RR[f'{s}/{i}']
        missing = [c for c in cands if c not in sc]
        if missing:
            raise SystemExit(f'{model} top{k} {s}/{i}: no jina score for {missing}')
        rows.append(sorted(((sc[c], c) for c in cands), reverse=True))
    return rows


def floor_above(neg_tops):
    return math.floor(max(neg_tops) / 0.05) * 0.05 + 0.05


def hits(rows, exp, t):
    return [any(c in e for s, c in r[:INJECT] if s >= t) for r, e in zip(rows, exp)]


def heldout(neg_tops, rows, exp, rng):
    adm, rec = [], []
    for _ in range(SPLITS):
        idx = list(range(len(neg_tops))); rng.shuffle(idx); half = len(idx) // 2
        t = floor_above([neg_tops[i] for i in idx[:half]])
        test = [neg_tops[i] for i in idx[half:]]
        adm.append(sum(v >= t for v in test) / len(test))
        rec.append(sum(hits(rows, exp, t)) / len(exp))
    return sum(adm) / SPLITS, sum(rec) / SPLITS


def main():
    rng = random.Random(7)
    base_it = None
    for name, model, k in CONFIGS:
        print(f'== {name}')
        for lang, exp in (('it', IT_EXP), ('en', EN_EXP)):
            pos, neg = scored(model, k, f'{lang}_pos'), scored(model, k, f'{lang}_neg')
            neg_tops = [r[0][0] for r in neg]
            t = floor_above(neg_tops)
            h = hits(pos, exp, t); n = len(h); lo, hi = wilson(sum(h), n)
            adm, rec = heldout(neg_tops, pos, exp, rng)
            line = f'  {lang.upper()} floor {t:+.2f}: recall {sum(h)}/{n} = {sum(h) / n:.3f} [{lo:.2f}, {hi:.2f}]' \
                   f' | held-out: recall {rec:.3f}, negatives admitted {adm:.3f}'
            if lang == 'it':
                if base_it is None:
                    base_it = h
                else:
                    p, gained, lost = sign_test(h, base_it)
                    line += f' | vs shipped: +{gained} -{lost}, p = {p:.3f}'
            print(line)


if __name__ == '__main__':
    main()
