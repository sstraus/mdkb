"""Option 4b: translate the IT prompts (40 positives + 38 negatives) to English through OpenRouter.

Approved by Boss 2026-09-30 (no filter, spend cap $1). The key is read from the
jevclassifier .env into this process only; it is never printed or written.
Usage: translate_openrouter.py <data-dir> <model> <out.json>
"""
import json, os, re, sys, time, urllib.request

data, model, out_path = sys.argv[1:4]
CAP_USD = 1.0
SYS = "Translate the user's message from Italian to English. Keep technical terms, identifiers and code as they are. Output only the translation."
key = re.search(r'^OPENROUTER_API_KEY=(.+)$', open(os.path.expanduser('~/Gits/LS/jevclassifier/.env')).read(), re.M).group(1).strip().strip('"\'')
it = json.load(open(f'{data}/it_set.json'))
prompts = [p['q'] for p in it['pairs']] + it['negs']
out, lat, cost = [], [], 0.0
for q in prompts:
    body = {"model": model, "temperature": 0, "usage": {"include": True},
            "messages": [{"role": "system", "content": SYS}, {"role": "user", "content": q}]}
    req = urllib.request.Request("https://openrouter.ai/api/v1/chat/completions", json.dumps(body).encode(),
                                 {"Content-Type": "application/json", "Authorization": f"Bearer {key}"})
    t = time.time()
    r = json.load(urllib.request.urlopen(req, timeout=60))
    lat.append((time.time() - t) * 1000)
    out.append(r['choices'][0]['message']['content'].strip())
    cost += r.get('usage', {}).get('cost', 0.0)
    assert cost < CAP_USD, 'spend cap reached'
n = len(it['pairs'])
lat.sort()
json.dump({"pos": out[:n], "neg": out[n:]}, open(out_path, 'w'), ensure_ascii=False, indent=1)
print(json.dumps({"model": model, "n": len(lat), "p50_ms": lat[len(lat) // 2], "p95_ms": lat[int(len(lat) * .95)],
                  "cost_usd": round(cost, 6)}))
