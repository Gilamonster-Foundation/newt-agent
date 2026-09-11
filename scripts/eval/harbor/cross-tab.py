#!/usr/bin/env python3
"""newt self-report x Harbor verdict, per run. The number that matters is
'completed' rows Harbor marked False: newt claimed success on unfinished work."""
import json,sys,os,glob,collections,re
def flat(o,p="",out=None):
    out={} if out is None else out
    if isinstance(o,dict):
        for k,v in o.items(): flat(v,f"{p}.{k}",out)
    else: out[p]=o
    return out
def verdict(d,td):
    ff=flat(json.load(open(f"{d}/{td}/result.json")))
    for k,v in ff.items():
        if re.search(r'is_resolved|\.resolved$',k) and isinstance(v,bool): return v
    for k,v in ff.items():
        if re.search(r'reward',k,re.I) and isinstance(v,(int,float)): return v>0
    return None
def newt(d,td):
    """P4 (review 5175154929): `end_reason` lives on the solve_result record and
    `outcome` on the trailing contract record; merge both rather than keeping
    whichever record came last."""
    outcome=None; reason=None; seen=False
    for ev in glob.glob(f"{d}/{td}/**/newt-events.jsonl",recursive=True):
        for line in open(ev,errors="replace"):
            try: o=json.loads(line)
            except Exception: continue
            if "end_reason" in o: reason=o["end_reason"]; seen=True
            if "outcome" in o: outcome=o["outcome"]; seen=True
            elif "status" in o and outcome is None and o.get("kind")=="solve_result": outcome=o["status"]; seen=True
    if not seen: return ("no-record","")
    return (str(outcome), str(reason).replace("Some(","").rstrip(")"))
for d in sys.argv[1:]:
    tds=sorted(p for p in os.listdir(d) if os.path.isdir(f"{d}/{p}"))
    tab=collections.Counter((newt(d,td),verdict(d,td)) for td in tds)
    res=sum(c for (n,h),c in tab.items() if h is True)
    false=sum(c for ((o,_),h),c in tab.items() if o=="completed" and h is False)
    claimed=sum(c for ((o,_),h),c in tab.items() if o=="completed")
    print(f"\n{os.path.basename(d)}: Harbor resolved {res}/{len(tds)}; newt claimed completed {claimed}; FALSE COMPLETIONS {false}")
    for ((o,er),h),c in sorted(tab.items(),key=lambda x:-x[1]): print(f"  {c:3}  outcome={o:<12} end_reason={er:<24} harbor={h}")
