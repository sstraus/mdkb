import json,time,urllib.request
from lib import *
prompts=[p['q'] for p in it['pairs']]+it['negs']
SYS="Translate the user's message from Italian to English. Keep technical terms, identifiers and code as they are. Output only the translation."
out=[];lat=[]
for q in prompts:
    t=time.time()
    body={"model":"gemma4:e4b-mlx","stream":False,"messages":[{"role":"system","content":SYS},{"role":"user","content":q}],"options":{"temperature":0}}
    req=urllib.request.Request("http://127.0.0.1:11434/api/chat",json.dumps(body).encode(),{"Content-Type":"application/json"})
    r=json.load(urllib.request.urlopen(req,timeout=120))
    out.append(r['message']['content'].strip()); lat.append((time.time()-t)*1000)
n=len(it['pairs'])
json.dump({"pos":out[:n],"neg":out[n:]},open('tr-gemma.json','w'),ensure_ascii=False)
l=sorted(lat)
json.dump({"n":len(l),"p50_ms":l[len(l)//2],"p95_ms":l[int(len(l)*.95)]},open('tr-gemma.stats.json','w'))
print(open('tr-gemma.stats.json').read())
