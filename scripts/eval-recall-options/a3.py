import sys
from lib import *
import lib
lib.GRID=float(sys.argv[1]) if len(sys.argv)>1 else 0.0
NAMES=('it_pos','it_neg','en_pos','en_neg')
def remap(rec,pfx):
    r=dict(rec); r['it_pos']=rec[pfx+'_pos']; r['it_neg']=rec[pfx+'_neg']; r['tr_pos']=rec['tr_pos']; r['tr_neg']=rec['tr_neg']; return r
def lexrec(f):
    d=load(f)['sets']; return {s:[[(a,b) for a,b in q['top']] for q in d[s]] for s in d}
def rrf(a,b,k=60,n=50):
    out=[]
    for x,y in zip(a,b):
        sc={}
        for lst in (x,y):
            for r,(d,_) in enumerate(lst[:n]): sc[d]=sc.get(d,0)+1/(k+r+1)
        out.append(sorted(sc.items(),key=lambda t:-t[1]))
    return out
def G(g,rows):
    lib.GRID=g; return rows
def top(r): return r[0][1] if r else None
def both(a,b): # rrf over two legs, per-set
    return {s:rrf(a[s],b[s]) for s in a if s in b}
def run(rows):
    for x in rows: print(fmt(x))
dense={m:dense_rec(m)[0] for m in ('minilm-l6','e5-small','e5-large')}
lex=lexrec('lex'); lexg=lexrec('lex-g')
dg={m:dense_rec('g-'+m)[0] for m in ('minilm-l6','e5-small')}
print('### GRID',lib.GRID)
print('## baseline abs cosine'); 
for m in dense: run([report(m+' abs',dense[m],top)])
lib.GRID=0.1
print('## 5 lexical only (BM25 top score) [IT, EN sets have lex]')
L=dict(lex); L['tr_pos']=lex['tr_pos']; L['tr_neg']=lex['tr_neg']; L['en_pos']=lex['en_pos']; L['en_neg']=lex['en_neg']
run([report('BM25 abs top1',L,top)])
lib.GRID=0.0
print('## 3 hybrid RRF dense+lex (gate on fused score)')
for m in ('minilm-l6','e5-small','e5-large'):
    H={s:rrf(dense[m][s],lex[s]) for s in NAMES+('tr_pos','tr_neg')}
    run([report(f'RRF {m}+bm25 fused top1',H,top)])
print('## 4 local translation gemma4:e4b -> EN')
for m in dg:
    lib.GRID=0.01; run([report(f'gemma->{m} abs cosine',remap(dg[m],'gemma'),top)]); lib.GRID=0.0
    H={s:rrf(dg[m][s],lexg[s]) for s in ('gemma_pos','gemma_neg','en_pos','en_neg','tr_pos','tr_neg','it_pos','it_neg')}
    run([report(f'gemma->RRF({m}+bm25)',remap(H,'gemma'),top)])
lib.GRID=0.1; run([report('gemma->BM25',remap(lexg,'gemma'),top)])
print('## 3b hybrid: RRF order, gate on dense cosine of the fused top-1 (absent from dense top-50 = -inf)')
def hyb(dn,lx,mode):
    out=[]
    for x,y in zip(dn,lx):
        dm=dict(x); sc={}
        for lst in (x,y):
            for r,(d,_) in enumerate(lst[:50]): sc[d]=sc.get(d,0)+1/(61+r)
        f=sorted(sc.items(),key=lambda t:-t[1])
        out.append([(d,dm.get(d,-9.0) if mode=='dense' else dict(y).get(d,0.0)) for d,_ in f])
    return out
lib.GRID=0.01
for m in ('minilm-l6','e5-small'):
    run([report(f'RRF order {m}+bm25, gate=dense cosine of fused top1',{s:hyb(dense[m][s],lex[s],'dense') for s in NAMES+('tr_pos','tr_neg')},top)])
