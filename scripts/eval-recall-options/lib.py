import json, math, os, random
M=os.path.expanduser('~/Gits/.tmp/mdkb-ml/')
O=M+'opt/'
K=5
GRID=0.0
it=json.load(open(M+'it_set.json'))
IT_EXP=[p['expected'] for p in it['pairs']]
fix=json.load(open(os.path.expanduser('~/Gits/personal/mdkb__wt/eval-recall-options/assets/eval/memory-recall.json')))
EN_EXP=[r['expected_ids'] for r in fix['recall']]
EXP={'it_pos':IT_EXP,'tr_pos':IT_EXP,'en_pos':EN_EXP}
def load(name): return json.load(open(O+name+'.json'))
def wilson(k,n,z=1.96):
    if n==0: return (float('nan'),)*2
    p=k/n; d=1+z*z/n; c=(p+z*z/(2*n))/d; h=z*math.sqrt(p*(1-p)/n+z*z/(4*n*n))/d
    return (max(0,c-h),min(1,c+h))
def sign_test(a,b):
    """exact two-sided sign test on paired booleans"""
    x=sum(1 for i,j in zip(a,b) if i and not j); y=sum(1 for i,j in zip(a,b) if j and not i)
    n=x+y
    if n==0: return 1.0,x,y
    k=min(x,y); p=sum(math.comb(n,i) for i in range(0,k+1))/2**n*2
    return min(1.0,p),x,y
# A query record: ranked list [(id,score)] (best first), plus scalar features
def hit(ranked,exp): return any(d in exp for d,_ in ranked[:K])
def gate_eval(rec, scalar, sets=('it_pos','it_neg','en_pos','en_neg','tr_pos','tr_neg')):
    """rec[set] = list of ranked lists. scalar(ranked)->float or None(no candidates). Return dict of per-set scalars."""
    return {s:[scalar(r) for r in rec[s]] for s in sets}
def admitted(v,t): return v is not None and v>=t
def clean_threshold(neg_vals):
    m=[v for v in neg_vals if v is not None]
    if not m: return -1e18
    mx=max(m)
    if GRID: return math.ceil(round(mx/GRID,9)+1e-9)*GRID if False else (math.floor(round(mx/GRID,9))+1)*GRID   # lowest grid step admitting none (admit = v>=t)
    return mx+1e-9   # just above max
def recall_at(vals_pos,ranked_pos,exp,t):
    h=[admitted(v,t) and hit(r,e) for v,r,e in zip(vals_pos,ranked_pos,exp)]
    return h
def report(name,rec,scalar,out=None):
    V=gate_eval(rec,scalar)
    res={'name':name}
    for proto,negset in (('A_fitEN','en_neg'),('B_fitIT','it_neg')):
        t=clean_threshold(V[negset])
        itp=recall_at(V['it_pos'],rec['it_pos'],IT_EXP,t)
        enp=recall_at(V['en_pos'],rec['en_pos'],EN_EXP,t)
        trp=recall_at(V['tr_pos'],rec['tr_pos'],IT_EXP,t)
        fp_it=sum(admitted(v,t) for v in V['it_neg']); fp_en=sum(admitted(v,t) for v in V['en_neg']); fp_tr=sum(admitted(v,t) for v in V['tr_neg'])
        h=sum(itp)
        res[proto]={'t':t,'it_hits':itp,'en_hits':enp,'tr_hits':trp,'it_recall':h/len(itp),'it_ci':wilson(h,len(itp)),
           'it_negFP':fp_it,'it_prec':(h/(h+fp_it) if h+fp_it else None),'en_recall':sum(enp)/len(enp),'en_ci':wilson(sum(enp),len(enp)),'en_negFP':fp_en,
           'tr_recall':sum(trp)/len(trp),'tr_negFP':fp_tr}
    res['nogate']={s:sum(hit(r,e) for r,e in zip(rec[s],EXP[s]))/len(EXP[s]) for s in ('it_pos','en_pos','tr_pos')}
    return res
def dense_rec(m):
    d=load('embed-'+m)['sets']
    rec={s:[[(a,b) for a,b in q['top']] for q in d[s]] for s in d}
    stats={s:[(q['mean'],q['std']) for q in d[s]] for s in d}
    return rec,stats,d
def fmt(res):
    out=[res['name']]
    for p in ('A_fitEN','B_fitIT'):
        r=res[p]; out.append(f"  {p}: t={r['t']:.4g} IT {r['it_recall']:.3f} [{r['it_ci'][0]:.2f},{r['it_ci'][1]:.2f}] negFP {r['it_negFP']} prec {r['it_prec'] if r['it_prec'] is None else round(r['it_prec'],2)} | EN {r['en_recall']:.3f} ENnegFP {r['en_negFP']} | tr {r['tr_recall']:.3f} trFP {r['tr_negFP']}")
    return '\n'.join(out)
