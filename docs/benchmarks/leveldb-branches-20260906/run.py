#!/usr/bin/env python3
"""Compare fixed branch snapshots using one LevelDB binary and fresh databases."""
import argparse,datetime,fcntl,hashlib,json,os,re,shutil,signal,statistics,subprocess,tempfile,time
from pathlib import Path
ROOT=next(p for p in Path(__file__).resolve().parents if (p/'src/direct.c').is_file())
WORK=ROOT/'target/leveldb-branches-20260906'
EXE=ROOT/'target/flexguard-suite-20260905/leveldb/out-static/db_bench'
EXPECTED_EXE='5f4ee4e128e60af6ff13a434d82c39bfee52ff80dc10d815f5a6f60b93034171'
BRANCHES={'fullhook-admission':{'source':WORK/'fullhook-src','commit':'4e5998e21c458e9b162855ef3c3d5c7e42b42ebb'},'simplify':{'source':WORK/'simplify-src','commit':'8fc18994aa51f27b91bf6b490a575a309add8da3'}}
CONFIG={'LC_ALL':'C','MCS_ACCORDIN_DIRECT_DISABLE_BPF':'0','MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF':'0','MCS_ACCORDIN_DIRECT_STATS_ONLY':'0','MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY':'0','ACCORDIN_DISABLE_ADMISSION':'0','ACCORDIN_HOOK_STATS':'0','OMP_PROC_BIND':'false','OMP_WAIT_POLICY':'PASSIVE'}
ENV={k:v for k,v in os.environ.items() if not k.startswith(('ACCORDIN_','MCS_ACCORDIN_','MCS_TAS_ACCORDIN_','SCX_','LD_','COND_VAR','COND_VAE'))}|CONFIG
ARMS=[]
for backend,stem,adapter in [('mcs_accordin','mcs_accordin','mcsaccordin'),('mcs_tas_accordin','mcs_tas_accordin','mcstasaccordin')]:
 for branch,spec in BRANCHES.items():
  source=spec['source'];libdir=source/'target/release'
  library=libdir/f'lib{stem}_fullhook.so' if branch=='fullhook-admission' else source/f'third_party/litl/lib/lib{adapter}_original.so'
  expected=[library] if branch=='fullhook-admission' else [library,libdir/f'lib{stem}_direct.so']
  ARMS.append({'id':f'{branch}-{backend}','branch':branch,'backend':backend,'library':library,'libdir':libdir,'expected_maps':sorted(str(p.resolve()) for p in expected)})

def read(p):
 try:return Path(p).read_text().strip()
 except OSError:return ''
def state():return read('/sys/kernel/sched_ext/state')
def seq():return int(read('/sys/kernel/sched_ext/enable_seq'))
def sha(p):
 with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def inventory(p):
 return {str(f.relative_to(p)):{'bytes':f.stat().st_size,'sha256':sha(f)} for f in sorted(p.rglob('*')) if f.is_file()}
def command_output(cmd):return subprocess.check_output(cmd,text=True).strip()
def stop(p):
 if p.poll() is None:os.killpg(p.pid,signal.SIGKILL);p.wait()
 for _ in range(50):
  if state()=='disabled':return
  time.sleep(.1)
 raise RuntimeError('sched_ext did not detach')

def run(out,db,mode,arm,rep):
 assert state()=='disabled'
 ident=f'{mode}-192-{arm["id"]}-r{rep}'
 cmd=[str(EXE),'--threads=192','--time_ms=30000',f'--benchmarks={mode}',f'--use_existing_db={int(mode=="readrandom")}',f'--db={db}']
 overrides=CONFIG|{'LD_PRELOAD':str(arm['library']),'LD_LIBRARY_PATH':str(arm['libdir'])}
 if arm['branch']=='fullhook-admission':overrides['ACCORDIN_CV_SPIN_US']='1000'
 s={'id':ident,'benchmark':mode,'branch':arm['branch'],'backend':arm['backend'],'commit':BRANCHES[arm['branch']]['commit'],'repeat':rep,'command':cmd,'environment':overrides,'threads':192,'cpus':list(range(96)),'seq_before':seq(),'scx_enabled':False,'maps':[],'bpf_fds':[],'max_process_threads':0,'timeout':False,'expected_maps':arm['expected_maps']}
 print('START '+ident,flush=True);start=time.monotonic()
 with (out/(ident+'.log')).open('w') as log:
  p=subprocess.Popen(cmd,env=ENV|overrides,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
  try:
   while p.poll() is None and time.monotonic()-start<120:
    s['scx_enabled'] |= state()=='enabled'
    status=read(f'/proc/{p.pid}/status');m=re.search(r'^Threads:\s+(\d+)',status,re.M)
    if m:s['max_process_threads']=max(s['max_process_threads'],int(m[1]))
    if not s['maps'] or not s['bpf_fds']:
     s['maps']=sorted(set(str(Path(l.split()[-1]).resolve()) for l in read(f'/proc/{p.pid}/maps').splitlines() if any(x in l for x in ['accordin_direct','accordin_original','accordin_fullhook'])))
     for fd in Path(f'/proc/{p.pid}/fdinfo').glob('*'):
      info=read(fd)
      if re.search(r'^(map_id|prog_id|link_id):',info,re.M):s['bpf_fds'].append(info)
    time.sleep(.1 if s['maps'] and s['bpf_fds'] else .005)
   s['timeout']=p.poll() is None
  finally:stop(p)
 s.update(returncode=p.returncode,wall_seconds=time.monotonic()-start,seq_after=seq(),state_after=state())
 text=(out/(ident+'.log')).read_text();m=re.search(r'BENCH_TOTAL ops=(\d+) seconds=([\d.]+) ops_per_second=([\d.]+)',text)
 if m:s.update(operations=int(m[1]),roi_seconds=float(m[2]),ops_per_second=float(m[3]))
 s['valid']=not s['timeout'] and p.returncode==0 and bool(m) and s['maps']==s['expected_maps'] and bool(s['bpf_fds']) and s['max_process_threads']>=193 and s['scx_enabled'] and s['seq_after']==s['seq_before']+1
 if m:s['valid'] &= s['operations']>0 and 29<=s['roi_seconds']<60
 if re.search(r'put error|open error|Corruption:|Failed to (?:load|attach|register)|DEBUG DUMP|Too many threads|Assertion.*failed|\[hook_stats\]',text):s['valid']=False
 if db.exists():s['db_after']={'files':len(list(db.iterdir())),'bytes':sum(f.stat().st_size for f in db.iterdir() if f.is_file())}
 with (out/'results.jsonl').open('a') as f:f.write(json.dumps(s)+'\n')
 print('DONE '+json.dumps({k:s[k] for k in ['id','valid','operations','roi_seconds','ops_per_second','max_process_threads'] if k in s}),flush=True)
 return s

def main():
 parser=argparse.ArgumentParser();parser.add_argument('--out',type=Path,default=WORK);parser.add_argument('--seed',type=Path,default=Path('/tmp/accordin-flexguard-suite-20260905/seed'));args=parser.parse_args()
 out=args.out.resolve();out.mkdir(parents=True,exist_ok=True)
 if (out/'results.jsonl').exists():raise RuntimeError('Results exist: choose a fresh --out directory')
 guard=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(guard,fcntl.LOCK_EX)
 assert state()=='disabled';os.sched_setaffinity(0,range(96));assert sha(EXE)==EXPECTED_EXE
 files={EXE,Path(__file__).resolve()};versions={}
 for branch,spec in BRANCHES.items():
  source=spec['source'];commit=command_output(['git','-C',str(source),'rev-parse','HEAD']);assert commit==spec['commit']
  assert not command_output(['git','-C',str(source),'status','--porcelain'])
  versions[branch]={'commit':commit,'source':str(source),'interposer':'fullhook' if branch=='fullhook-admission' else 'standard LiTL','cv_spin_us':1000 if branch=='fullhook-admission' else None}
  for pattern in ['src/*.c','src/*.h','src/bpf/*.c','src/bpf/*.h','include/*.h']:
   files.update(source.glob(pattern))
  files.add(source/'Makefile')
  if branch=='simplify':files.update((source/'third_party/litl/src').glob('accordin*.c'))
 for arm in ARMS:files.update(Path(p) for p in arm['expected_maps'])
 hashes={str(p):sha(p) for p in sorted(files)};seed_manifest=inventory(args.seed)
 meta={'started_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'branches':versions,'uname':list(os.uname()),'compiler':command_output(['clang','--version']),'litl_compiler':command_output(['cc','--version']),'hashes':hashes,'seed':str(args.seed),'seed_manifest':seed_manifest,'configuration':CONFIG,'cpu_governors':sorted(set(read(p) for p in Path('/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor'))),'cpu0_khz':read('/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq'),'threads':192,'cpus':list(range(96)),'num_keys':1000000,'value_bytes':100,'requested_time_ms':30000,'db_filesystem':'/tmp tmpfs','repeats':3}
 (out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 tmp=Path(tempfile.mkdtemp(prefix='accordin-leveldb-branches-',dir='/tmp'))
 try:
  audit=tmp/'seed-audit';shutil.copytree(args.seed,audit)
  with (out/'seed-audit.log').open('w') as f:
   result=subprocess.run([str(EXE),'--benchmarks=readseq','--threads=1','--use_existing_db=1',f'--db={audit}'],env=ENV,stdout=f,stderr=subprocess.STDOUT,timeout=30)
  assert result.returncode==0 and re.search(r'BENCH_TOTAL ops=1000000 ',(out/'seed-audit.log').read_text())
  shutil.rmtree(audit);print('Seed audit: 1000000 entries',flush=True)
  samples=[]
  for rep in [1,2,3]:
   order=ARMS[rep-1:]+ARMS[:rep-1]
   for mode in ['readrandom','fillrandom']:
    for arm in order:
     db=tmp/f'{mode}-{arm["id"]}-r{rep}'
     if mode=='readrandom':shutil.copytree(args.seed,db);assert inventory(db)==seed_manifest
     s=run(out,db,mode,arm,rep);samples.append(s)
     if not s['valid']:raise RuntimeError(f'Invalid run {s["id"]}; DB retained at {db}')
     shutil.rmtree(db)
  assert inventory(args.seed)==seed_manifest;assert all(sha(p)==h for p,h in hashes.items())
  for spec in BRANCHES.values():assert not command_output(['git','-C',str(spec['source']),'status','--porcelain'])
  summary=[]
  for mode in ['readrandom','fillrandom']:
   for backend in ['mcs_accordin','mcs_tas_accordin']:
    baseline=statistics.mean(s['ops_per_second'] for s in samples if s['benchmark']==mode and s['backend']==backend and s['branch']=='simplify')
    for branch in BRANCHES:
     vals=[s['ops_per_second'] for s in samples if s['benchmark']==mode and s['backend']==backend and s['branch']==branch]
     mean=statistics.mean(vals);sd=statistics.stdev(vals)
     summary.append({'benchmark':mode,'backend':backend,'branch':branch,'commit':BRANCHES[branch]['commit'],'n':len(vals),'mean_ops_per_second':mean,'stdev':sd,'cv_percent':100*sd/mean,'relative_to_simplify':mean/baseline,'runs_ops_per_second':vals})
  (out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
  meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),valid_runs=len(samples),hashes_unchanged=True,seed_unchanged=True,state_after=state());(out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
  tmp.rmdir();print('SUMMARY '+json.dumps(summary),flush=True)
 except BaseException:print(f'Remaining owned DBs retained in {tmp}',flush=True);raise
if __name__=='__main__':main()
