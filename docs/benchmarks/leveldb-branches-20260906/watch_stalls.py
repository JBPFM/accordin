#!/usr/bin/env python3
"""Observe only our runs after 65s, beyond the 30s requested measurement."""
import collections,json,os,subprocess,time
from pathlib import Path
R=Path(__file__).resolve().parent
seen=set();hz=os.sysconf('SC_CLK_TCK')
def read(p):
 try:return Path(p).read_text()
 except OSError:return ''
while not (R/'summary.json').exists():
 uptime=float(read('/proc/uptime').split()[0])
 for p in Path('/proc').glob('[0-9]*'):
  if p.name in seen or read(p/'comm').strip()!='db_bench':continue
  args=read(p/'cmdline').split('\0');db=next((v[5:] for v in args if v.startswith('--db=/tmp/accordin-leveldb-branches-resume-')) ,None)
  if not db:continue
  stat=read(p/'stat');fields=stat[stat.rfind(')')+2:].split()
  if len(fields)<20:continue
  age=uptime-float(fields[19])/hz
  if age<65:continue
  seen.add(p.name);name=Path(db).name
  tasks=[{'tid':t.name,'wchan':read(t/'wchan').strip(),'status':read(t/'status'),'kernel_stack':read(t/'stack')} for t in (p/'task').glob('[0-9]*')]
  data={'pid':p.name,'db':db,'age_seconds':age,'tasks':tasks,'wchan_counts':dict(collections.Counter(t['wchan'] for t in tasks))}
  (R/(name+'.stall.json')).write_text(json.dumps(data,indent=2)+'\n')
  print(name,'age',age,'wchan',data['wchan_counts'],flush=True)
  with (R/(name+'.perf.log')).open('w') as log:
   result=subprocess.run(['perf','record','-q','-e','cpu-clock','-F','99','-g','-o',str(R/(name+'.perf.data')),'-p',p.name,'--','sleep','2'],stdout=log,stderr=subprocess.STDOUT,timeout=10)
  if result.returncode==0:
   with (R/(name+'.perf.txt')).open('w') as report:subprocess.run(['perf','report','--stdio','--no-children','--percent-limit','1','-i',str(R/(name+'.perf.data'))],stdout=report,stderr=subprocess.STDOUT,timeout=15)
 time.sleep(2)
