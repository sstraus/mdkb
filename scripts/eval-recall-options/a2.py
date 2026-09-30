import sys
from lib import *
import lib
lib.GRID=0.0
def rerank_rec(rr,src,k,rec_src):
    sc=rr['scores']; out={}
    for s in ('it_pos','it_neg','en_pos','en_neg','tr_pos','tr_neg'):
        rows=[]
        for i,q in enumerate(rec_src[s]):
            cands=[d for d,_ in q[:k]]; row=sc[f'{s}/{i}']
            rows.append(sorted([(d,row[d]) for d in cands],key=lambda x:-x[1]))
        out[s]=rows
    return out
src={m:dense_rec(m)[0] for m in ('minilm-l6','e5-small')}
for name in sys.argv[1:]:
    rr=load('rerank-'+name)
    print('##',name,'lat5',rr['latency_top5'],'lat10',rr['latency_top10'],'rss',rr['rss_loaded_bytes']//10**6,'MB load',round(rr['load_ms']),'ms')
    for m in src:
        for k in (5,10):
            rec=rerank_rec(rr,name,k,src[m])
            print(fmt(report(f'{name} on {m} top{k}',rec,lambda r:r[0][1])))
