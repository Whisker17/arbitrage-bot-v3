import csv, collections, sys
WMNT='0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8'
KIND={'agni-v2':'v2','agni-v3':'v3','moe':'moe'}
rows=list(csv.DictReader(open(sys.argv[1])))
adj=collections.defaultdict(list)
for r in rows:
    t0,t1=r['token0'].lower(),r['token1'].lower()
    adj[t0].append((t1,r['pool'].lower(),KIND[r['protocol']]))
    adj[t1].append((t0,r['pool'].lower(),KIND[r['protocol']]))
cyc=collections.Counter()
def dfs(tok,path,seen):
    if len(path)>=3: return
    for nxt,pool,k in adj[tok]:
        if path and path[-1][0]==pool: continue
        if nxt==WMNT:
            if len(path)+1>=2: cyc[tuple(p[1] for p in path)+(k,)]+=1
            continue
        if nxt in seen: continue
        dfs(nxt,path+[(pool,k)],seen|{nxt})
dfs(WMNT,[],{WMNT})
tot=sum(cyc.values())
print('total_cycles',tot)
for k,v in sorted(cyc.items(),key=lambda x:-x[1]): print(v,len(k),'+'.join(k))
