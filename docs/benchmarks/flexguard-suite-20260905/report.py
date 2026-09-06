#!/usr/bin/env python3
import json,csv,statistics,math,sys,shutil,os
from pathlib import Path
D=Path(__file__).resolve().parent
R=D.parents[2]/'target'/os.environ.get('FLEXGUARD_SUITE_NAME','flexguard-suite-20260905')
source=R/'results.jsonl'
if not source.exists():source=D/'results.jsonl'
rows=[json.loads(x) for x in source.read_text().splitlines()]
rows=[x for x in rows if x['phase']=='performance']
fields=['benchmark','threads','backend','repeat','valid','value','unit','wall_seconds','returncode','timeout','roi_seconds','operations','seq_before','seq_after','max_process_threads','log']
with (D/'raw.csv').open('w') as f:
 w=csv.DictWriter(f,fields,extrasaction='ignore');w.writeheader();w.writerows(rows)
order=['scheduling','buckets','leveldb-readrandom','leveldb-fillrandom','leveldb-fillseq','leveldb-readseq','leveldb-overwrite','kyotocabinet','raytrace','dedup','volrend','streamcluster','index','index-4k']
backends=['mcs_accordin','mcs_tas_accordin','flexguard'];summaries=[]
for name in order:
 for t in [96,192]:
  ss=[]
  for b in backends:
   group=[x for x in rows if x['benchmark']==name and x['threads']==t and x['backend']==b]
   valid=[x for x in group if x['valid'] and 'value' in x]
   s={'benchmark':name,'threads':t,'backend':b,'n':len(valid),'failures':len(group)-len(valid)}
   if valid:
    a=[x['value'] for x in valid];s.update(unit=valid[0]['unit'],mean=statistics.mean(a),stdev=statistics.stdev(a) if len(a)>1 else 0,min=min(a),max=max(a));s['cv_percent']=100*s['stdev']/s['mean']
   if not valid and group:s['status']='timeout' if any(x.get('timeout') for x in group) else 'failed'
   elif not group:s['status']='pending'
   else:s['status']='complete' if len(valid)==3 else 'partial'
   ss.append(s)
  for s in ss:
   if 'mean' in s and 'mean' in ss[-1]:s['relative_to_flexguard']=s['mean']/ss[-1]['mean'] if s['unit']=='ops/s' else ss[-1]['mean']/s['mean']
  summaries.extend(ss)
fields=['benchmark','threads','backend','status','n','failures','unit','mean','stdev','min','max','cv_percent','relative_to_flexguard']
with (D/'summary.csv').open('w') as f:
 w=csv.DictWriter(f,fields);w.writeheader();w.writerows(summaries)
lines=['# Performance results','', 'Values are arithmetic means; higher ops/s and lower seconds are better. Ratios >1 mean faster than FlexGuard.','', '| Workload | Threads | MCS-Accordin | MCS-TAS-Accordin | FlexGuard | MCS / FG | TAS / FG |','|---|---:|---:|---:|---:|---:|---:|']
def fmt(s):
 if 'mean' not in s:return s['status']
 v=f"{s['mean']/1e6:.3f} Mops/s" if s['unit']=='ops/s' else f"{s['mean']:.3f} s"
 return v+f" (n={s['n']}, CV {s['cv_percent']:.1f}%)"
for i in range(0,len(summaries),3):
 a=summaries[i:i+3]
 lines.append('| '+' | '.join([a[0]['benchmark'],str(a[0]['threads'])]+[fmt(s) for s in a]+[f"{s['relative_to_flexguard']:.3f}x" if 'relative_to_flexguard' in s else '-' for s in a[:2]])+' |')
(D/'results.md').write_text('\n'.join(lines)+'\n')
print(f"{len(rows)} completed attempts, {sum(x['valid'] for x in rows)} valid samples")
if '--plot' in sys.argv:
 import matplotlib;matplotlib.use('Agg')
 import matplotlib.pyplot as plt
 import numpy as np
 fig,axes=plt.subplots(1,2,figsize=(13,8),sharey=True,sharex=True)
 for ax,t in zip(axes,[96,192]):
  ax.axvline(1,color='#777',lw=1)
  for b,off,color in [('mcs_accordin',-.14,'#2374ab'),('mcs_tas_accordin',.14,'#d35b32')]:
   for j,name in enumerate(order):
    s=next(x for x in summaries if x['benchmark']==name and x['threads']==t and x['backend']==b)
    if 'relative_to_flexguard' in s:
     v=s['relative_to_flexguard'];ax.plot(v,j+off,'o',color=color,label=b if j==0 else None)
  ax.set_xscale('log');ax.set_xticks([.03,.1,.3,1,3],['0.03','0.1','0.3','1','3']);ax.set_xlabel('Speed relative to FlexGuard (1x)');ax.set_title(f'{t} requested workers');ax.grid(axis='x',alpha=.2);ax.set_yticks(range(len(order)),order)
 axes[0].invert_yaxis()
 handles,labels=axes[0].get_legend_handles_labels();fig.legend(handles,labels,loc='lower center',bbox_to_anchor=(.5,.025),ncol=2)
 fig.text(.5,.012,'Missing points: KyotoCabinet timed out for both Accordin locks; MCS-Accordin aborted with 256 B Index pages.',ha='center',fontsize=9)
 fig.suptitle('FlexGuard benchmark suite: ARM64, BPF enabled, three repetitions')
 fig.tight_layout(rect=(0,.08,1,.96));fig.savefig(D/'comparison.svg');fig.savefig(D/'comparison.png',dpi=180);plt.close(fig)
