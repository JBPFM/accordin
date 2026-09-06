import os,sys,json,time,subprocess,signal,fcntl,re
from pathlib import Path
ROOT=Path('/mnt/sde/jz/accordin');R=ROOT/'target/streamcluster-analysis-20260905';S=ROOT/'target/flexguard-suite-20260905'
sys.path.insert(0,str(S))
from run import BASE,LIBS,state,seq,read
lock=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(lock,fcntl.LOCK_EX)
os.sched_setaffinity(0,range(96))
def events(path):
    return ['perf','stat','-x,','-o',str(path),'-e','task-clock,context-switches,cpu-migrations','-e','syscalls:sys_enter_futex','--filter','op == 129','-e','syscalls:sys_enter_sched_yield']
def save(s):
    with (R/'measurements.jsonl').open('a') as f:f.write(json.dumps(s)+'\n')
    print(json.dumps(s),flush=True)
def finish(p):
    if p.poll() is None:os.killpg(p.pid,signal.SIGKILL);p.wait()
    for _ in range(50):
        if state()=='disabled':return
        time.sleep(.1)
    raise RuntimeError('Scheduler still active')
for t in [96,192]:
    for b in LIBS:
        ident=f'barrier-{t}-{b}';print('START '+ident,flush=True);assert state()=='disabled'
        cmd=events(R/(ident+'.stat.csv'))+['--','env','LD_PRELOAD='+str(LIBS[b]),str(R/'barrier'),str(t),'100']
        before=seq();start=time.monotonic();enabled=False;maps=[]
        with (R/(ident+'.log')).open('w') as f:
            p=subprocess.Popen(cmd,env=BASE,stdout=f,stderr=subprocess.STDOUT,start_new_session=True)
            while p.poll() is None and time.monotonic()-start<90:
                enabled |= state()=='enabled'
                if not maps:
                    children=read(f'/proc/{p.pid}/task/{p.pid}/children').split()
                    if children:maps=[l.split()[-1] for l in read(f'/proc/{children[0]}/maps').splitlines() if '.so' in l and any(s in l for s in ['accordin','fg/interpose'])]
                time.sleep(.02)
            timeout=p.poll() is None
            if timeout:finish(p)
        finish(p)
        save(dict(id=ident,backend=b,threads=t,rounds=100,returncode=p.returncode,timeout=timeout,wall=time.monotonic()-start,seq_before=before,seq_after=seq(),scx_observed=enabled,maps=sorted(set(maps))))
for b in LIBS:
    ident='prefix-192-'+b;print('START '+ident,flush=True);assert state()=='disabled'
    before=seq();start=time.monotonic()
    cmd=[str(S/'streamcluster/streamcluster'),'10','30','512','32768','32768','2000','none',str(R/(ident+'.clusters')),'192']
    with (R/(ident+'.log')).open('w') as f:
        p=subprocess.Popen(cmd,env=BASE|{'LD_PRELOAD':str(LIBS[b])},stdout=f,stderr=subprocess.STDOUT,start_new_session=True)
        time.sleep(3)
        maps=read(f'/proc/{p.pid}/maps');threads=read(f'/proc/{p.pid}/status');enabled=state()=='enabled'
        with (R/(ident+'.perf.log')).open('w') as pf:
            record=subprocess.Popen(['perf','record','-q','-e','cpu-clock','-F','99','-o',str(R/(ident+'.data')),'-p',str(p.pid),'--','sleep','30'],stdout=pf,stderr=subprocess.STDOUT,env=BASE)
            stat=subprocess.Popen(events(R/(ident+'.stat.csv'))+['-p',str(p.pid),'--','sleep','30'],stdout=pf,stderr=subprocess.STDOUT,env=BASE)
            record.wait(timeout=45);stat.wait(timeout=45)
        was_running=p.poll() is None
        finish(p)
    with (R/(ident+'.report.txt')).open('w') as f:
        subprocess.run(['perf','report','--stdio','--no-children','--percent-limit','0.3','-i',str(R/(ident+'.data'))],stdout=f,stderr=subprocess.STDOUT,check=True)
    save(dict(id=ident,backend=b,threads=192,profile_seconds=30,profile_offset_seconds=3,diagnostic_prefix_only=True,intentionally_killed=was_running,wall=time.monotonic()-start,seq_before=before,seq_after=seq(),scx_observed=enabled,record_rc=record.returncode,stat_rc=stat.returncode,maps=sorted(set(l.split()[-1] for l in maps.splitlines() if '.so' in l and any(s in l for s in ['accordin','fg/interpose']))),process_status=threads))
