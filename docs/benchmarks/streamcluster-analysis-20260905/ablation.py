import os,sys,json,time,subprocess,signal,fcntl
from pathlib import Path
ROOT=Path('/mnt/sde/jz/accordin');R=ROOT/'target/streamcluster-analysis-20260905';S=ROOT/'target/flexguard-suite-20260905'
sys.path.insert(0,str(S))
from run import BASE,LIBS,state,seq,read
lock=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(lock,fcntl.LOCK_EX)
os.sched_setaffinity(0,range(96))
for variant,bulk,disable in [('bulk',True,False),('no-admission',False,True),('bulk-no-admission',True,True)]:
 for b in ['mcs_accordin','mcs_tas_accordin']:
  ident=f'ablation-{variant}-{b}';print('START '+ident,flush=True);assert state()=='disabled'
  preload=(str(R/'bulk_cond.so')+':' if bulk else '')+str(LIBS[b]);before=seq();start=time.monotonic();enabled=False
  cmd=['perf','stat','-x,','-o',str(R/(ident+'.stat.csv')),'-e','task-clock,context-switches,cpu-migrations','-e','syscalls:sys_enter_futex','--filter','op == 129','-e','syscalls:sys_enter_sched_yield','--','env','LD_PRELOAD='+preload,str(R/'barrier'),'192','100']
  with (R/(ident+'.log')).open('w') as f:
   p=subprocess.Popen(cmd,env=BASE|{'ACCORDIN_DISABLE_ADMISSION':str(int(disable))},stdout=f,stderr=subprocess.STDOUT,start_new_session=True)
   while p.poll() is None and time.monotonic()-start<20:
    enabled |= state()=='enabled';time.sleep(.02)
   timeout=p.poll() is None
   if timeout:os.killpg(p.pid,signal.SIGKILL);p.wait()
  for _ in range(50):
   if state()=='disabled':break
   time.sleep(.1)
  assert state()=='disabled'
  s=dict(id=ident,backend=b,threads=192,rounds=100,bulk_cond=bulk,admission_disabled=disable,returncode=p.returncode,timeout=timeout,wall=time.monotonic()-start,seq_before=before,seq_after=seq(),scx_observed=enabled)
  with (R/'ablation.jsonl').open('a') as f:f.write(json.dumps(s)+'\n')
  print(json.dumps(s),flush=True)
