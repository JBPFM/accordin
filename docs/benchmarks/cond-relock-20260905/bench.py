import os,sys,json,time,subprocess,signal,fcntl,hashlib,re
from pathlib import Path
ROOT=next(p for p in Path(__file__).resolve().parents if (p/'src/direct.c').is_file()); R=ROOT/'target/cond-relock-20260905'; S=ROOT/'target/flexguard-suite-20260905'; A=ROOT/'target/streamcluster-analysis-20260905'
sys.path.insert(0,str(S))
from run import BASE,LIBS,state,seq,read
lock=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(lock,fcntl.LOCK_EX)
os.sched_setaffinity(0,range(96))
mode=sys.argv[1]
variants=[(v,b) for v in ['baseline','relock'] for b in ['mcs_accordin','mcs_tas_accordin']]+[('baseline','flexguard')]
if mode=='stream':variants=[('relock',b) for b in ['mcs_accordin','mcs_tas_accordin']]
for rep in range(1,4):
 for variant,b in variants:
  ident=f'{mode}-{variant}-{b}-r{rep}';print('START '+ident,flush=True);assert state()=='disabled'
  lib=R/'baseline'/LIBS[b].name if variant=='baseline' and b!='flexguard' else LIBS[b]
  env=BASE|{'LD_PRELOAD':str(lib),'LD_LIBRARY_PATH':str(R/'baseline' if variant=='baseline' and b!='flexguard' else ROOT/'target/release')}
  before=seq();start=time.monotonic();enabled=False;maps=[];bpf=[];output=R/(ident+'.clusters')
  if mode=='barrier':
   cmd=['perf','stat','-x,','-o',str(R/(ident+'.stat.csv')),'-e','task-clock,context-switches,cpu-migrations','-e','syscalls:sys_enter_futex','--filter','op == 129','-e','syscalls:sys_enter_sched_yield','--','env','LD_PRELOAD='+env.pop('LD_PRELOAD'),str(A/'barrier'),'192','100']
  else:cmd=[str(S/'streamcluster/streamcluster'),'10','30','512','32768','32768','2000','none',str(output),'192']
  with (R/(ident+'.log')).open('w') as f:
   p=subprocess.Popen(cmd,env=env,stdout=f,stderr=subprocess.STDOUT,start_new_session=True)
   while p.poll() is None and time.monotonic()-start<(60 if mode=='barrier' else 600):
    enabled |= state()=='enabled'
    if not maps or not bpf:
     children=read(f'/proc/{p.pid}/task/{p.pid}/children').split()
     target=children[0] if mode=='barrier' and children else p.pid
     maps=sorted(set(l.split()[-1] for l in read(f'/proc/{target}/maps').splitlines() if any(x in l for x in ['accordin_direct','accordin_original','fg/interpose.so'])))
     for fd in Path(f'/proc/{target}/fdinfo').glob('*'):
      info=read(fd)
      if re.search(r'^(map_id|prog_id|link_id):',info,re.M):bpf.append(info)
    time.sleep(.01 if mode=='barrier' else .1)
   timeout=p.poll() is None
   if timeout:os.killpg(p.pid,signal.SIGKILL);p.wait()
  for _ in range(50):
   if state()=='disabled':break
   time.sleep(.1)
  assert state()=='disabled'
  log=(R/(ident+'.log')).read_text();m=re.search(r'seconds=([\d.]+)' if mode=='barrier' else r'Benchmark time:\s*(\d+)',log)
  seconds=float(m[1])/(1 if mode=='barrier' else 100000000) if m else None
  s=dict(id=ident,variant=variant,backend=b,threads=192,cpus=list(range(96)),returncode=p.returncode,timeout=timeout,wall=time.monotonic()-start,seconds=seconds,seq_before=before,seq_after=seq(),scx_observed=enabled,maps=maps,bpf_fds=bpf,command=cmd,env={k:v for k,v in env.items() if k.startswith(('MCS_ACCORDIN_DIRECT_', 'MCS_TAS_ACCORDIN_DIRECT_', 'ACCORDIN_', 'LD_', 'OMP_')) or k=='LC_ALL'})
  s['valid']=p.returncode==0 and not timeout and m is not None and bool(bpf) and bool(maps)
  s['valid'] &= seq()==before if b=='flexguard' else enabled and int(seq())==int(before)+1
  if output.exists():s['output_sha256']=hashlib.file_digest(output.open('rb'),'sha256').hexdigest();s['output_bytes']=output.stat().st_size
  with (R/(mode+'.jsonl')).open('a') as f:f.write(json.dumps(s)+'\n')
  print(json.dumps({k:v for k,v in s.items() if k not in ['bpf_fds','command','env','cpus']}),flush=True)
  if not s['valid']:sys.exit(1)
