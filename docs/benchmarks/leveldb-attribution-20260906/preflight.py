#!/usr/bin/env python3
import fcntl,subprocess,sys
from pathlib import Path
WORK=Path(__file__).resolve().parent
guard=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(guard,fcntl.LOCK_EX)
for name in sys.argv[1:]:
 source=WORK/'variants'/name
 targets=['check-bpf']
 if name=='tls':targets=['check-litl-bpf']
 if 'signal' in name.split('+'):targets=['check','check-litl','check-bpf','check-litl-bpf']
 print('CHECK '+name,flush=True)
 with (WORK/f'check-{name}-preload.log' if name=='tls' else WORK/f'check-{name}.log').open('w') as log:
  subprocess.run(['make','-C',str(source),*targets],stdout=log,stderr=subprocess.STDOUT,timeout=300,check=True)
 print('PASS '+name,flush=True)
