#!/usr/bin/env python3
"""Finish the original matrix, retaining timeouts instead of aborting it."""
import datetime,fcntl,importlib.util,json,os,shutil,statistics,tempfile
from pathlib import Path
ROOT=next(p for p in Path(__file__).resolve().parents if (p/'src/direct.c').is_file())
R=ROOT/'target/leveldb-branches-20260906'
spec=importlib.util.spec_from_file_location('branch_bench',R/'run.py');b=importlib.util.module_from_spec(spec);spec.loader.exec_module(b)
guard=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(guard,fcntl.LOCK_EX)
assert b.state()=='disabled';os.sched_setaffinity(0,range(96))
meta=json.loads((R/'metadata.json').read_text());seed=Path(meta['seed'])
assert b.inventory(seed)==meta['seed_manifest'];assert all(b.sha(p)==h for p,h in meta['hashes'].items())
meta['continuation_script']={'path':str(Path(__file__).resolve()),'sha256':b.sha(__file__)}
(R/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
rows=[json.loads(l) for l in (R/'results.jsonl').read_text().splitlines()];done={s['id'] for s in rows}
tmp=Path(tempfile.mkdtemp(prefix='accordin-leveldb-branches-resume-',dir='/tmp'))
for rep in [1,2,3]:
 order=b.ARMS[rep-1:]+b.ARMS[:rep-1]
 for mode in ['readrandom','fillrandom']:
  for arm in order:
   ident=f'{mode}-192-{arm["id"]}-r{rep}'
   if ident in done:continue
   db=tmp/f'{mode}-{arm["id"]}-r{rep}'
   if mode=='readrandom':shutil.copytree(seed,db);assert b.inventory(db)==meta['seed_manifest']
   s=b.run(R,db,mode,arm,rep);rows.append(s)
   if s['valid']:shutil.rmtree(db)
   else:print(f'INVALID retained {ident}: {db}',flush=True)
assert len(rows)==24 and len({s['id'] for s in rows})==24
assert b.inventory(seed)==meta['seed_manifest'];assert all(b.sha(p)==h for p,h in meta['hashes'].items())
assert b.sha(__file__)==meta['continuation_script']['sha256']
for spec in b.BRANCHES.values():assert not b.command_output(['git','-C',str(spec['source']),'status','--porcelain'])
summary=[]
for mode in ['readrandom','fillrandom']:
 for backend in ['mcs_accordin','mcs_tas_accordin']:
  baseline=[s['ops_per_second'] for s in rows if s['benchmark']==mode and s['backend']==backend and s['branch']=='simplify' and s['valid']]
  for branch in b.BRANCHES:
   group=[s for s in rows if s['benchmark']==mode and s['backend']==backend and s['branch']==branch]
   good=[s for s in group if s['valid']];vals=[s['ops_per_second'] for s in good]
   mean=statistics.mean(vals) if vals else None;sd=statistics.stdev(vals) if len(vals)>1 else None
   summary.append({'benchmark':mode,'backend':backend,'branch':branch,'commit':b.BRANCHES[branch]['commit'],'attempts':len(group),'n':len(vals),'timeouts':sum(s['timeout'] for s in group),'complete':len(vals)==3,'mean_ops_per_second':mean,'stdev':sd,'cv_percent':100*sd/mean if sd is not None else None,'relative_to_simplify':mean/statistics.mean(baseline) if mean is not None and len(vals)==3 and len(baseline)==3 else None,'runs_ops_per_second':[s.get('ops_per_second') if s['valid'] else None for s in sorted(group,key=lambda s:s['repeat'])],'failed_ids':[s['id'] for s in group if not s['valid']]})
(R/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),attempted_runs=len(rows),valid_runs=sum(s['valid'] for s in rows),timeouts=sum(s['timeout'] for s in rows),hashes_unchanged=True,seed_unchanged=True,state_after=b.state())
(R/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
if not any(tmp.iterdir()):tmp.rmdir()
print('SUMMARY '+json.dumps(summary),flush=True)
