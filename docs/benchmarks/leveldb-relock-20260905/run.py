#!/usr/bin/env python3
"""Run the two requested LevelDB workloads with fresh DBs and serial BPF use."""
import argparse,csv,datetime,fcntl,hashlib,json,os,re,shutil,signal,statistics,subprocess,tempfile,time
from pathlib import Path
ROOT=next(p for p in Path(__file__).resolve().parents if (p/'src/direct.c').is_file())
SUITE=ROOT/'target/flexguard-suite-20260905'
LIBS={'mcs_accordin':ROOT/'third_party/litl/lib/libmcsaccordin_original.so', 'mcs_tas_accordin':ROOT/'third_party/litl/lib/libmcstasaccordin_original.so', 'flexguard':SUITE/'fg/interpose.so'}
EXE=SUITE/'leveldb/out-static/db_bench'
CONFIG={'LC_ALL':'C','MCS_ACCORDIN_DIRECT_DISABLE_BPF':'0','MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF':'0','MCS_ACCORDIN_DIRECT_STATS_ONLY':'0','MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY':'0','ACCORDIN_DISABLE_ADMISSION':'0','OMP_PROC_BIND':'false','OMP_WAIT_POLICY':'PASSIVE'}
ENV={k:v for k,v in os.environ.items() if not k.startswith(('ACCORDIN_','MCS_ACCORDIN_','MCS_TAS_ACCORDIN_','SCX_','LD_','COND_VAR','COND_VAE'))}|CONFIG

def read(p):
 try:return Path(p).read_text().strip()
 except OSError:return ''
def state():return read('/sys/kernel/sched_ext/state')
def seq():return int(read('/sys/kernel/sched_ext/enable_seq'))
def sha(p):
 with Path(p).open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def inventory(p):
 return {str(f.relative_to(p)):{'bytes':f.stat().st_size,'sha256':sha(f)} for f in sorted(p.rglob('*')) if f.is_file()}
def stop(p):
 if p.poll() is None:os.killpg(p.pid,signal.SIGKILL);p.wait()
 for _ in range(50):
  if state()=='disabled':return
  time.sleep(.1)
 raise RuntimeError('sched_ext did not detach')

def run(out,db,mode,backend,rep):
 assert state()=='disabled'
 ident=f'{mode}-192-{backend}-r{rep}'
 cmd=[str(EXE),'--threads=192','--time_ms=30000',f'--benchmarks={mode}',f'--use_existing_db={int(mode=="readrandom")}',f'--db={db}']
 overrides=CONFIG|{'LD_PRELOAD':str(LIBS[backend]),'LD_LIBRARY_PATH':str(ROOT/'target/release')}
 sample={'id':ident,'benchmark':mode,'backend':backend,'repeat':rep,'command':cmd,'environment':overrides,'threads':192,'cpus':list(range(96)),'seq_before':seq(),'scx_enabled':False,'maps':[],'bpf_fds':[],'max_process_threads':0,'timeout':False}
 print('START '+ident,flush=True);start=time.monotonic()
 with (out/(ident+'.log')).open('w') as log:
  p=subprocess.Popen(cmd,env=ENV|overrides,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
  try:
   while p.poll() is None and time.monotonic()-start<120:
    sample['scx_enabled'] |= state()=='enabled'
    status=read(f'/proc/{p.pid}/status');m=re.search(r'^Threads:\s+(\d+)',status,re.M)
    if m:sample['max_process_threads']=max(sample['max_process_threads'],int(m[1]))
    if not sample['maps'] or not sample['bpf_fds']:
     maps=read(f'/proc/{p.pid}/maps')
     sample['maps']=sorted(set(l.split()[-1] for l in maps.splitlines() if any(x in l for x in ['accordin_direct','accordin_original','fg/interpose.so'])))
     for fd in Path(f'/proc/{p.pid}/fdinfo').glob('*'):
      info=read(fd)
      if re.search(r'^(map_id|prog_id|link_id):',info,re.M):sample['bpf_fds'].append(info)
    time.sleep(.1 if sample['maps'] and sample['bpf_fds'] else .005)
   sample['timeout']=p.poll() is None
  finally:stop(p)
 sample.update(returncode=p.returncode,wall_seconds=time.monotonic()-start,seq_after=seq(),state_after=state())
 text=(out/(ident+'.log')).read_text();m=re.search(r'BENCH_TOTAL ops=(\d+) seconds=([\d.]+) ops_per_second=([\d.]+)',text)
 if m:sample.update(operations=int(m[1]),roi_seconds=float(m[2]),ops_per_second=float(m[3]))
 sample['valid']=not sample['timeout'] and p.returncode==0 and bool(m) and bool(sample['maps']) and bool(sample['bpf_fds']) and sample['max_process_threads']>=193
 if backend=='flexguard':sample['valid'] &= sample['seq_after']==sample['seq_before']
 else:sample['valid'] &= sample['scx_enabled'] and sample['seq_after']==sample['seq_before']+1
 if m:sample['valid'] &= sample['operations']>0 and 29<=sample['roi_seconds']<60
 if re.search(r'put error|open error|Corruption:|Failed to (?:load|attach|register)|DEBUG DUMP|Too many threads|Assertion.*failed',text):sample['valid']=False
 sample['db_after']={'files':len(list(db.iterdir())),'bytes':sum(f.stat().st_size for f in db.iterdir() if f.is_file())}
 with (out/'results.jsonl').open('a') as f:f.write(json.dumps(sample)+'\n')
 print('DONE '+json.dumps({k:sample[k] for k in ['id','valid','operations','roi_seconds','ops_per_second','max_process_threads'] if k in sample}),flush=True)
 return sample

def main():
 parser=argparse.ArgumentParser();parser.add_argument('--out',type=Path,default=ROOT/'target/leveldb-relock-20260905');parser.add_argument('--seed',type=Path,default=Path('/tmp/accordin-flexguard-suite-20260905/seed'));args=parser.parse_args()
 out=args.out.resolve();out.mkdir(parents=True,exist_ok=True)
 if (out/'results.jsonl').exists():raise RuntimeError('Results exist: choose a fresh --out directory')
 guard=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(guard,fcntl.LOCK_EX)
 assert state()=='disabled';os.sched_setaffinity(0,range(96))
 files=[EXE,*LIBS.values(),ROOT/'target/release/libmcs_accordin_direct.so',ROOT/'target/release/libmcs_tas_accordin_direct.so',SUITE/'leveldb/db/db_bench.cc',ROOT/'src/direct.c',ROOT/'src/runtime.h',ROOT/'src/bpf/main.bpf.c',ROOT/'third_party/litl/src/accordin.c',ROOT/'third_party/litl/src/accordin-cond.c',Path(__file__).resolve()]
 hashes={str(p):sha(p) for p in files};seed_manifest=inventory(args.seed)
 meta={'started_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'uname':list(os.uname()),'hashes':hashes,'seed':str(args.seed),'seed_manifest':seed_manifest,'configuration':CONFIG,'cpu_governors':sorted(set(read(p) for p in Path('/sys/devices/system/cpu').glob('cpu[0-9]*/cpufreq/scaling_governor'))),'cpu0_khz':read('/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq'),'threads':192,'cpus':list(range(96)),'num_keys':1000000,'value_bytes':100,'requested_time_ms':30000,'db_filesystem':'/tmp tmpfs','repeats':3}
 (out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 tmp=Path(tempfile.mkdtemp(prefix='accordin-leveldb-relock-',dir='/tmp'))
 try:
  audit=tmp/'seed-audit';shutil.copytree(args.seed,audit)
  audit_cmd=[str(EXE),'--benchmarks=readseq','--threads=1','--use_existing_db=1',f'--db={audit}']
  with (out/'seed-audit.log').open('w') as f:
   result=subprocess.run(audit_cmd,env=ENV,stdout=f,stderr=subprocess.STDOUT,timeout=30)
  text=(out/'seed-audit.log').read_text();assert result.returncode==0 and re.search(r'BENCH_TOTAL ops=1000000 ',text)
  shutil.rmtree(audit);print('Seed audit: 1000000 entries',flush=True)
  samples=[]
  for rep in [1,2,3]:
   order=list(LIBS);order=order[rep-1:]+order[:rep-1]
   for mode in ['readrandom','fillrandom']:
    for backend in order:
     db=tmp/f'{mode}-{backend}-r{rep}'
     if mode=='readrandom':
      shutil.copytree(args.seed,db);assert inventory(db)==seed_manifest
     sample=run(out,db,mode,backend,rep);samples.append(sample)
     if not sample['valid']:raise RuntimeError(f'Invalid run {sample["id"]}: retaining DB {db}')
     shutil.rmtree(db)
  assert inventory(args.seed)==seed_manifest
  assert all(sha(p)==h for p,h in hashes.items())
  summary=[]
  for mode in ['readrandom','fillrandom']:
   fg=statistics.mean(s['ops_per_second'] for s in samples if s['benchmark']==mode and s['backend']=='flexguard')
   for backend in LIBS:
    values=[s['ops_per_second'] for s in samples if s['benchmark']==mode and s['backend']==backend]
    mean=statistics.mean(values);sd=statistics.stdev(values)
    summary.append({'benchmark':mode,'backend':backend,'n':len(values),'mean_ops_per_second':mean,'stdev':sd,'cv_percent':100*sd/mean,'relative_to_flexguard':mean/fg,'runs_ops_per_second':values})
  (out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
  meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),valid_runs=len(samples),hashes_unchanged=True,seed_unchanged=True,state_after=state())
  (out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
  tmp.rmdir()
  print('SUMMARY '+json.dumps(summary),flush=True)
 except BaseException:
  print(f'Run interrupted; remaining owned DBs retained in {tmp}',flush=True);raise
if __name__=='__main__':main()
