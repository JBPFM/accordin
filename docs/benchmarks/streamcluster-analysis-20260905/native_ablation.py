import os,sys,json,time,subprocess,signal,fcntl,hashlib,re
from pathlib import Path
ROOT=Path('/mnt/sde/jz/accordin');R=ROOT/'target/streamcluster-analysis-20260905';S=ROOT/'target/flexguard-suite-20260905'
sys.path.insert(0,str(S))
from run import BASE,LIBS,state,seq,read
lock=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(lock,fcntl.LOCK_EX)
os.sched_setaffinity(0,range(96))
for b in ['mcs_accordin','mcs_tas_accordin']:
 ident='native-no-admission-'+b;print('START '+ident,flush=True);assert state()=='disabled'
 before=seq();start=time.monotonic();enabled=False;maps=[];bpf=[]
 output=R/(ident+'.clusters')
 cmd=[str(S/'streamcluster/streamcluster'),'10','30','512','32768','32768','2000','none',str(output),'192']
 with (R/(ident+'.log')).open('w') as f:
  p=subprocess.Popen(cmd,env=BASE|{'LD_PRELOAD':str(LIBS[b]),'ACCORDIN_DISABLE_ADMISSION':'1'},stdout=f,stderr=subprocess.STDOUT,start_new_session=True)
  while p.poll() is None and time.monotonic()-start<600:
   enabled |= state()=='enabled'
   if not maps or not bpf:
    maps=sorted(set(l.split()[-1] for l in read(f'/proc/{p.pid}/maps').splitlines() if any(x in l for x in ['accordin_direct','accordin_original'])))
    for fd in Path(f'/proc/{p.pid}/fdinfo').glob('*'):
     info=read(fd)
     if re.search(r'^(map_id|prog_id|link_id):',info,re.M):bpf.append(info)
   time.sleep(.1)
  timeout=p.poll() is None
  if timeout:os.killpg(p.pid,signal.SIGKILL);p.wait()
 for _ in range(50):
  if state()=='disabled':break
  time.sleep(.1)
 assert state()=='disabled'
 text=(R/(ident+'.log')).read_text();m=re.search(r'Benchmark time:\s*(\d+)',text)
 s=dict(id=ident,backend=b,threads=192,admission_disabled=True,bpf_enabled=True,returncode=p.returncode,timeout=timeout,wall=time.monotonic()-start,roi_seconds=int(m[1])/100000000 if m else None,seq_before=before,seq_after=seq(),scx_observed=enabled,maps=maps,bpf_fds=bpf,command=cmd)
 s['valid']=p.returncode==0 and not timeout and m is not None and enabled and bool(bpf) and bool(maps) and int(seq())==int(before)+1
 if output.exists():s['output_sha256']=hashlib.file_digest(output.open('rb'),'sha256').hexdigest();s['output_bytes']=output.stat().st_size
 with (R/'native-ablation.jsonl').open('a') as f:f.write(json.dumps(s)+'\n')
 print(json.dumps({k:v for k,v in s.items() if k not in ['bpf_fds','command']}),flush=True)
