import io,contextlib,random,sys
with contextlib.redirect_stdout(io.StringIO()):
    from a2 import rerank_rec,src
from lib import *
def prep(name,m,k):
    rr=load('rerank-'+name); rec=rerank_rec(rr,name,k,src[m]); return rec,lambda r:r[0][1]
def dense_prep(m): return src[m],lambda r:r[0][1]
def split_half(rec,sc,trials=300,seed=1):
    neg=[sc(r) for r in rec['it_neg']]; pos=[(sc(r),hit(r,e)) for r,e in zip(rec['it_pos'],IT_EXP)]
    rnd=random.Random(seed); idx=list(range(len(neg))); rs=[];fps=[]
    for _ in range(trials):
        rnd.shuffle(idx); a=idx[:len(idx)//2]; b=idx[len(idx)//2:]
        t=max(neg[i] for i in a)+1e-9
        rs.append(sum(1 for v,h in pos if v>=t and h)/len(pos)); fps.append(sum(1 for i in b if neg[i]>=t)/len(b))
    rs.sort(); return sum(rs)/trials,sum(fps)/trials,sum(1 for f in fps if f>0)/trials
def hits_at_clean(rec,sc):
    t=max(sc(r) for r in rec['it_neg'])+1e-9
    return [sc(r)>=t and hit(r,e) for r,e in zip(rec['it_pos'],IT_EXP)]
rows=[('e5-small abs',)+dense_prep('e5-small'),('minilm abs',)+dense_prep('minilm-l6')]
for n in sys.argv[1:]:
    for m in ('minilm-l6','e5-small'):
        for k in (5,10):
            rows.append((f'{n} on {m} top{k}',)+prep(n,m,k))
base=hits_at_clean(rows[0][1],rows[0][2])
for nm,rec,sc in rows:
    h=hits_at_clean(rec,sc); p,x,y=sign_test(h,base)
    sr,sf,pf=split_half(rec,sc)
    print(f'{nm:38s} IT@clean {sum(h)/40:.3f} | vs e5-small abs: +{x}/-{y} sign p={p:.3f} | split-half: recall {sr:.3f}, held-out negFP rate {sf:.3f}, P(any FP) {pf:.2f}')
