from run import *
lock=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(lock,fcntl.LOCK_EX)
os.sched_setaffinity(0,range(96))
# Supplemental existing 4 KiB page configuration: the larger branching factor
# can keep the BTree below MCS's four concurrently-held-node limit.
with (R/'build-index-4k.log').open('w') as log:
 subprocess.run(['g++','-shared','-fPIC','-O3','-Ofast','-DNDEBUG','-std=c++17','-march=native','-DRWLOCK','-DMUTEX_LOCK','-DBTREE_RWLOCK','-DBTREE_PAGE_SIZE=4096',f'-I{R}/index',f'-I{R}/pibench/include',str(R/'index/wrappers/btreeolc_wrapper.cpp'),'-o',str(R/'btreelc_mutex_4K.so'),'-lglog','-lnuma','-pthread'],stdout=log,stderr=subprocess.STDOUT,check=True)
# Diagnose the existing 256-byte MCS failure with BPF disabled, outside all
# performance measurements. Debugger stops must not pause an active scheduler.
env=BASE|{'MCS_ACCORDIN_DIRECT_DISABLE_BPF':'1'}
gdb=['gdb','-batch','-ex',f'set environment LD_PRELOAD {LIBS["mcs_accordin"]}','-ex','run','-ex','bt','--args',str(R/'pibench-build/src/PiBench'),str(R/'btreelc_mutex.so'),'--threads=1','--mode=time','--read_ratio=0','--update_ratio=1','--seconds=1','--records=1000000','--distribution=SELFSIMILAR','--skew=0.2','--bulk_load','--pcm=false','--skip_verify=true','--apply_hash=false']
with (R/'index-mcs-backtrace.log').open('w') as log:subprocess.run(gdb,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=90)
failed=set()
for rep in [1,2,3]:
 for t in [96,192]:
  order=list(LIBS);order=order[rep-1:]+order[:rep-1]
  for b in order:
   if (t,b) in failed:continue
   cmd=[R/'pibench-build/src/PiBench',R/'btreelc_mutex_4K.so',f'--threads={t}','--mode=time','--read_ratio=0','--update_ratio=1','--seconds=10','--records=100000000','--distribution=SELFSIMILAR','--skew=0.2','--bulk_load','--pcm=false','--skip_verify=true','--apply_hash=false']
   result=run(b,'index-4k',t,rep,cmd,600)
   if not result['valid']:failed.add((t,b))
