#!/usr/bin/env python3
import os,sys,json,time,subprocess,re,signal,fcntl,shutil,hashlib
from pathlib import Path
R=Path(__file__).resolve().parent
ROOT=R.parent.parent
LIBS={'mcs_accordin':ROOT/'third_party/litl/lib/libmcsaccordin_original.so','mcs_tas_accordin':ROOT/'third_party/litl/lib/libmcstasaccordin_original.so','flexguard':R/'fg/interpose.so'}
BASE={k:v for k,v in os.environ.items() if not k.startswith(('ACCORDIN_','MCS_ACCORDIN_','MCS_TAS_ACCORDIN_','SCX_','LD_','COND_VAR','COND_VAE'))}
BASE.update({'LC_ALL':'C','MCS_ACCORDIN_DIRECT_DISABLE_BPF':'0','MCS_TAS_ACCORDIN_DIRECT_DISABLE_BPF':'0','MCS_ACCORDIN_DIRECT_STATS_ONLY':'0','MCS_TAS_ACCORDIN_DIRECT_STATS_ONLY':'0','ACCORDIN_DISABLE_ADMISSION':'0','OMP_PROC_BIND':'false','OMP_WAIT_POLICY':'PASSIVE'})
(R/'logs').mkdir(exist_ok=True)
def read(p):
 try:return Path(p).read_text().strip()
 except OSError:return ''
def state():return read('/sys/kernel/sched_ext/state')
def seq():return read('/sys/kernel/sched_ext/enable_seq')
def run(backend,name,threads,repeat,cmd,timeout=90,cwd=None,extra=None,phase='performance'):
 assert state()=='disabled', 'An existing sched_ext scheduler is active; refusing overlap'
 ident=f'{phase}-{name}-{threads}-{backend}-r{repeat}';log=R/'logs'/f'{ident}.log'
 env=BASE|{'LD_PRELOAD':str(LIBS[backend])}|(extra or {})
 s={'id':ident,'backend':backend,'benchmark':name,'threads':threads,'repeat':repeat,'phase':phase,'command':list(map(str,cmd)),'cwd':str(cwd or R),'log':str(log.relative_to(R)),'seq_before':seq(),'maps':[],'bpf_fds':[],'scx_enabled':False,'max_process_threads':0}
 print('START',ident,flush=True);start=time.monotonic()
 with log.open('w') as f:
  p=subprocess.Popen(s['command'],cwd=cwd or R,env=env,stdout=f,stderr=subprocess.STDOUT,start_new_session=True)
  while p.poll() is None:
   if time.monotonic()-start>timeout:
    s['timeout']=True;os.killpg(p.pid,signal.SIGKILL);p.wait();break
   s['scx_enabled'] |= state()=='enabled'
   status=read(f'/proc/{p.pid}/status');m=re.search(r'^Threads:\s+(\d+)',status,re.M)
   if m:s['max_process_threads']=max(s['max_process_threads'],int(m[1]))
   if not s['maps'] or not s['bpf_fds']:
    maps=read(f'/proc/{p.pid}/maps')
    s['maps']=sorted(set(line.split()[-1] for line in maps.splitlines() if any(x in line for x in ['libmcsaccordin','libmcstasaccordin','libmcs_accordin_direct','libmcs_tas_accordin_direct','fg/interpose.so'])))
    for fd in Path(f'/proc/{p.pid}/fdinfo').glob('*'):
     info=read(fd)
     if re.search(r'^(map_id|prog_id|link_id):',info,re.M):s['bpf_fds'].append(info)
   time.sleep(.2 if s["bpf_fds"] and s["maps"] else .005)
  s['returncode']=p.returncode
 s['wall_seconds']=time.monotonic()-start
 for _ in range(50):
  if state()=='disabled':break
  time.sleep(.1)
 s['seq_after']=seq();s['state_after']=state();out=log.read_text(errors='replace')
 s['valid']=p.returncode==0 and not s.get('timeout') and bool(s['maps']) and bool(s['bpf_fds']) and state()=='disabled'
 if backend!='flexguard':s['valid'] &= s['scx_enabled'] and int(s['seq_after'])==int(s['seq_before'])+1
 else:s['valid'] &= s['seq_after']==s['seq_before']
 if re.search(r'Incorrect lock behavior|Failed to (?:load|attach|register)|Too many threads|DEBUG DUMP|ENOTSUP|Assertion.*failed',out):s['valid']=False
 if name=='scheduling':
  vals=re.findall(r'^\d+,\s*(\d+),\s*([\d.]+)$',out,re.M)
  s['measurements']=vals
  if vals:s['value']=float(vals[-1][1])*1000;s['unit']='ops/s'
 elif name=='buckets':
  m=re.search(r'#Throughput:\s+([\d.]+)',out)
  if m:s['value']=float(m[1]);s['unit']='ops/s'
 elif name.startswith('leveldb-'):
  m=re.search(r'BENCH_TOTAL ops=(\d+) seconds=([\d.]+) ops_per_second=([\d.]+)',out)
  if m:s['operations']=int(m[1]);s['roi_seconds']=float(m[2]);s['value']=float(m[3]);s['unit']='ops/s'
 elif name in ['streamcluster','dedup','volrend']:
  m=re.search(r'Benchmark time:\s*(\d+)',out)
  if m:s['value']=int(m[1])/(int(read(R/'counter-hz.txt')) if name!='volrend' else 1000000);s['unit']='s'
 elif name=='kyotocabinet':
  vals=re.findall(r'BENCH_TOTAL name=(\w+) ops=(\d+) seconds=([\d.]+) ops_per_second=([\d.]+)',out)
  s['submetrics']={v[0]:{'ops':int(v[1]),'seconds':float(v[2]),'ops_per_second':float(v[3])} for v in vals}
  if len(vals)==2:s['value']=sum(int(v[1]) for v in vals)/sum(float(v[2]) for v in vals);s['unit']='ops/s'
 elif name=='raytrace':
  m=re.search(r'Total time without initialization\s+(\d+)',out)
  if m:s['value']=int(m[1])/1e6;s['unit']='s'
 elif name in ['index','index-4k']:
  m=re.search(r'Run time:\s*([\d.]+) milliseconds',out);ops=re.findall(r'Operations:\s+(\d+)',out)
  if m and ops:s['value']=int(ops[0])/(float(m[1])/1e3);s['unit']='ops/s'
 if phase=='performance' and 'value' not in s:s['valid']=False
 with (R/'results.jsonl').open('a') as f:f.write(json.dumps(s)+'\n')
 print('DONE',ident,'valid='+str(s['valid']),'rc='+str(p.returncode),'value='+str(s.get('value')),f"wall={s['wall_seconds']:.2f}",flush=True)
 return s
if __name__=='__main__':
 lock=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(lock,fcntl.LOCK_EX)
 os.sched_setaffinity(0,range(96))
 phase=sys.argv[1] if len(sys.argv)>1 else 'preflight'
 if phase=='preflight':
  for b in LIBS:
   for t in [96,192]:
    run(b,'correctness',t,0,[R/'pthread/test_correctness','-d','1500','-n',str(t)],30,phase=phase)
   run(b,'init',1,0,[R/'pthread/test_init','-d','1000'],20,extra={'LD_DEBUG':'bindings'},phase=phase)
