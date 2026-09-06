from run import *
lock=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(lock,fcntl.LOCK_EX)
os.sched_setaffinity(0,range(96))
tmp=Path('/tmp/accordin-flexguard-suite-20260905')
previous=[json.loads(x) for x in (R/'results.jsonl').read_text().splitlines()]
done={x['id'] for x in previous if x['phase']=='performance'}
failed={(x['benchmark'],x['threads'],x['backend']) for x in previous if x['phase']=='performance' and not x['valid']}
# Stop at case boundaries if further compilation or input extraction is needed.
for rep in [1,2,3]:
 for t in [96,192]:
  order=list(LIBS);order=order[rep-1:]+order[:rep-1]
  for name in ['scheduling','buckets','leveldb-readrandom','leveldb-fillrandom','leveldb-fillseq','leveldb-readseq','leveldb-overwrite','kyotocabinet']:
   for b in order:
    if f'performance-{name}-{t}-{b}-r{rep}' in done or (name,t,b) in failed:continue
    if name=='scheduling':
     cmd=[R/'pthread/scheduling','-b',str(t),'-n',str(t),'-s',str(t),'-i','1','-d','5000','-t','2','-c','100','-l','0']; timeout=40
    elif name=='buckets':
     cmd=[R/'pthread/buckets','-n',str(t),'-d','10000','-b','100','-m','100000','-o','40','-c','0','-p','0']; timeout=60
    elif name.startswith('leveldb-'):
     mode=name.split('-',1)[1];d=tmp/f'{name}-{t}-{b}-r{rep}';existing=mode in ['readrandom','readseq','overwrite']
     if existing:shutil.copytree(tmp/'seed',d)
     cmd=[R/'leveldb/out-static/db_bench',f'--threads={t}','--time_ms=30000',f'--benchmarks={mode}',f'--use_existing_db={int(existing)}',f'--db={d}'];timeout=120
    else:
     d=tmp/f'{name}-{t}-{b}-r{rep}.kct';d.unlink(missing_ok=True);cmd=[R/'kyoto-driver-build/db_bench_tree_db',f'--threads={t}','--num=50000','--benchmarks=fillrandom,readrandom',f'--db={d}'];timeout=180
    # Avoid overlapping benchmark measurements with input extraction.
    while (R/'extracting').exists():time.sleep(1)
    result=run(b,name,t,rep,cmd,timeout,extra={'LD_LIBRARY_PATH':str(R/'kyotocabinet')} if name=='kyotocabinet' else None)
    if not result['valid']:failed.add((name,t,b))
    if name.startswith('leveldb-') and d.exists():shutil.rmtree(d)
    if name=='kyotocabinet' and d.exists():d.unlink()
