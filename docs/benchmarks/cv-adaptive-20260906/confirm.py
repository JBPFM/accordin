#!/usr/bin/env python3
"""Serial, resumable attribution matrix using the audited 30-second runner."""
import argparse,datetime,fcntl,importlib.util,json,os,re,shutil,statistics,subprocess,tempfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
WORK=Path(__file__).resolve().parent
HELPER=ROOT/'docs/benchmarks/leveldb-branches-20260906/run.py'
spec=importlib.util.spec_from_file_location('fixed_runner',HELPER);b=importlib.util.module_from_spec(spec);spec.loader.exec_module(b)
BASE='934bd1c'
FULL='4e5998e21c458e9b162855ef3c3d5c7e42b42ebb'
def arm(name,backend):
 if name.startswith('fullhook'):
  source=ROOT/'target/leveldb-branches-20260906/fullhook-src';libdir=source/'target/release';library=libdir/f'lib{backend}_fullhook.so';expected=[library];commit=FULL
 else:
  source=WORK/'variants'/name;libdir=source/'target/release'
  stem='mcsaccordin' if backend=='mcs_accordin' else 'mcstasaccordin'
  library=source/f'third_party/litl/lib/lib{stem}_original.so';expected=[library,libdir/f'lib{backend}_direct.so'];commit=BASE
 b.BRANCHES[name]={'commit':commit,'source':source}
 return {'id':f'{name}-{backend}','branch':name,'backend':backend,'library':library,'libdir':libdir,'expected_maps':sorted(str(p.resolve()) for p in expected)}
def main():
 ap=argparse.ArgumentParser();ap.add_argument('--stage',required=True);ap.add_argument('--spin-us',default='50');ap.add_argument('--arms',nargs='+',required=True);ap.add_argument('--repeats',type=int,default=1);ap.add_argument('--backends',nargs='+',default=['mcs_tas_accordin']);ap.add_argument('--modes',nargs='+',default=['readrandom','fillrandom']);ap.add_argument('--bracket',action='store_true');a=ap.parse_args()
 out=WORK/a.stage;out.mkdir(parents=True,exist_ok=True)
 guard=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(guard,fcntl.LOCK_EX)
 assert b.state()=='disabled';os.sched_setaffinity(0,range(96));assert b.sha(b.EXE)==b.EXPECTED_EXE
 seed=Path('/tmp/accordin-flexguard-suite-20260905/seed');seed_manifest=b.inventory(seed)
 assert seed_manifest
 arms=[arm(n,backend) for n in a.arms for backend in a.backends]
 hashes={str(p):b.sha(p) for p in [Path(__file__).resolve(),HELPER,b.EXE,*[Path(p) for x in arms for p in x['expected_maps']]]}
 for name in a.arms:
  if not name.startswith('fullhook'):
   m=json.loads((WORK/f'{name}.json').read_text());source=Path(m['source'])
   assert all(b.sha(source/p)==h for p,h in m['hashes'].items())
   hashes[str(WORK/f'{name}.json')]=b.sha(WORK/f'{name}.json')
 meta={'started_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'arguments':vars(a),'hashes':hashes,'seed':str(seed),'seed_manifest':seed_manifest,'uname':list(os.uname()),'cpus':list(range(96)),'threads':192,'requested_time_ms':30000,'governors':sorted(set(b.read(p) for p in Path('/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor')))}
 if (out/'metadata.json').exists():
  prev=json.loads((out/'metadata.json').read_text());assert prev['hashes']==hashes and prev['arguments']==vars(a);meta=prev
 else:(out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 rows=[json.loads(l) for l in (out/'results.jsonl').read_text().splitlines()] if (out/'results.jsonl').exists() else []
 done={s['id'] for s in rows};counters={}
 tmp=Path(tempfile.mkdtemp(prefix='accordin-attribution-',dir='/tmp'))
 if not (out/'seed-audit.log').exists():
  db=tmp/'audit';shutil.copytree(seed,db)
  with (out/'seed-audit.log').open('w') as log:r=subprocess.run([str(b.EXE),'--benchmarks=readseq','--threads=1','--use_existing_db=1',f'--db={db}'],env=b.ENV,stdout=log,stderr=subprocess.STDOUT,timeout=30)
  assert r.returncode==0 and re.search(r'BENCH_TOTAL ops=1000000 ',(out/'seed-audit.log').read_text());shutil.rmtree(db)
 for rep in range(a.repeats):
  order=arms[rep%len(arms):]+arms[:rep%len(arms)]
  if a.bracket:
   assert len(a.backends)==1 and 'baseline' in a.arms
   extra=arm('baseline',a.backends[0]);order=[x for x in order if x['branch']!='baseline'];order=[extra]+sum([order[i:i+4]+[extra] for i in range(0,len(order),4)],[])
  for mode in a.modes:
   for x in order:
    key=(mode,x['id']);counters[key]=counters.get(key,0)+1;n=counters[key];ident=f'{mode}-192-{x["id"]}-r{n}'
    if ident in done:continue
    db=tmp/ident
    if mode=='readrandom':shutil.copytree(seed,db);assert b.inventory(db)==seed_manifest
    b.CONFIG['ACCORDIN_CV_SPIN_US']=a.spin_us
    if x['branch'].startswith('fullhook'):b.CONFIG['ACCORDIN_CV_SPIN_US']='0' if x['branch']=='fullhook0' else '1000'
    s=b.run(out,db,mode,x,n);rows.append(s)
    if s['valid']:shutil.rmtree(db)
    else:print('INVALID database retained '+str(db),flush=True)
 assert b.inventory(seed)==seed_manifest and all(b.sha(p)==h for p,h in hashes.items())
 summary=[]
 for mode in a.modes:
  for backend in a.backends:
   base=[s['ops_per_second'] for s in rows if s['benchmark']==mode and s['backend']==backend and s['branch']=='baseline' and s['valid']]
   for name in a.arms:
    group=[s for s in rows if s['benchmark']==mode and s['backend']==backend and s['branch']==name];vals=[s['ops_per_second'] for s in group if s['valid']]
    mean=statistics.mean(vals) if vals else None
    summary.append({'benchmark':mode,'backend':backend,'variant':name,'n':len(vals),'attempts':len(group),'values':vals,'mean':mean,'cv_percent':100*statistics.stdev(vals)/mean if len(vals)>1 else None,'relative_to_baseline':mean/statistics.mean(base) if base and mean else None})
 (out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
 meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),attempts=len(rows),valid=sum(s['valid'] for s in rows),hashes_unchanged=True,seed_unchanged=True,state_after=b.state());(out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 if not any(tmp.iterdir()):tmp.rmdir()
 print('SUMMARY '+json.dumps(summary),flush=True)
if __name__=='__main__':main()
