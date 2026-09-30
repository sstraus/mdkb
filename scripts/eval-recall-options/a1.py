from lib import *
import statistics as st
R={}
for m in ('minilm-l6','e5-small','e5-large'):
    rec,stats,_=dense_rec(m)
    def s_abs(r): return r[0][1] if r else None
    def s_m2(r): return r[0][1]-r[1][1] if len(r)>1 else None
    def s_m5(r): return r[0][1]-st.mean(c for _,c in r[1:5]) if len(r)>5 else None
    def s_m10(r): return r[0][1]-st.mean(c for _,c in r[1:10]) if len(r)>10 else None
    def s_zt(r): # z over top-50 candidate distribution
        c=[x for _,x in r]; sd=st.pstdev(c); return (c[0]-st.mean(c))/sd if sd>0 else None
    # z over whole pool: index by position
    def mk_zpool(setname):
        pass
    res=[report(m+' abs cosine',rec,s_abs),report(m+' margin top1-top2',rec,s_m2),report(m+' margin top1-mean(2..5)',rec,s_m5),
         report(m+' margin top1-mean(2..10)',rec,s_m10),report(m+' z over top-50',rec,s_zt)]
    # pool z needs per-query stats: attach via index trick
    for nm in ('it_pos','it_neg','en_pos','en_neg','tr_pos','tr_neg'):
        for r,(mu,sd) in zip(rec[nm],stats[nm]): r.append(('_stat',(mu,sd)))
    # (use wrapper below)
    def s_zp(r):
        mu,sd=r[-1][1]; top=r[0][1]; return (top-mu)/sd
    def strip(f): return lambda r: f([x for x in r if x[0]!='_stat'] ) if False else f(r)
    # rebuild rec without stat for other scalars: zpool only
    recz={k:[q for q in v] for k,v in rec.items()}
    def s_zp2(r): mu,sd=r[-1][1]; return (r[0][1]-mu)/sd
    # hit() uses r[:5] so sentinel at end is harmless
    res.append(report(m+' z over whole pool',recz,s_zp2))
    for x in res: print(fmt(x))
    R[m]=res
