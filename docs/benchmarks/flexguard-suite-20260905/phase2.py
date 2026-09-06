from run import *
lock=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(lock,fcntl.LOCK_EX)
os.sched_setaffinity(0,range(96))
tmp=Path('/tmp/accordin-flexguard-suite-20260905')
failed=set()
for rep in [1,2,3]:
 for t in [96,192]:
  order=list(LIBS);order=order[rep-1:]+order[:rep-1]
  for name in ['raytrace','dedup','volrend','streamcluster','index']:
   for b in order:
    if (name,t,b) in failed:continue
    d=tmp/f'{name}-{t}-{b}-r{rep}';d.mkdir()
    if name=='raytrace':
     for f in (R/'inputs/raytrace').glob('car.*'):shutil.copy2(f,d/f.name)
     cmd=[R/'raytrace/raytrace',f'-p{t}','-a8',d/'car.env'];timeout=180
    elif name=='dedup':
     n=(t-2)//3
     cmd=[R/'dedup/dedup','-c','-p','-wgzip',f'-t{n}','-i',R/'inputs/dedup/FC-6-x86_64-disc1.iso','-o',d/'output.ddp'];timeout=300
    elif name=='volrend':
     for f in (R/'inputs/volrend').glob('head.*'):shutil.copy2(f,d/f.name)
     cmd=[R/'volrend/volrend',str(t),'head','1000'];timeout=900
    elif name=='streamcluster':
     cmd=[R/'streamcluster/streamcluster','10','30','512','32768','32768','2000','none',d/'clusters.txt',str(t)];timeout=600
    else:
     cmd=[R/'pibench-build/src/PiBench',R/'btreelc_mutex.so',f'--threads={t}','--mode=time','--read_ratio=0','--update_ratio=1','--seconds=10','--records=100000000','--distribution=SELFSIMILAR','--skew=0.2','--bulk_load','--pcm=false','--skip_verify=true','--apply_hash=false'];timeout=600
    result=run(b,name,t,rep,cmd,timeout,cwd=d)
    if not result['valid']:
     failed.add((name,t,b))
    artifacts=[]
    for f in d.iterdir():
     if f.is_file() and not f.name.startswith(('head.','car.env','car.geo')):
      artifacts.append({'name':f.name,'bytes':f.stat().st_size,'sha256':hashlib.file_digest(f.open('rb'),'sha256').hexdigest()})
    with (R/'artifacts.jsonl').open('a') as out:out.write(json.dumps({'id':result['id'],'artifacts':artifacts})+'\n')
    if result['valid'] and name=='dedup' and rep==1:
     # Verify one full round trip per backend/thread setting with the same native input.
     q=subprocess.run([str(R/'dedup/dedup'),'-u','-i',str(d/'output.ddp'),'-o',str(d/'decoded.iso')],env=BASE,capture_output=True,timeout=90)
     good=q.returncode==0 and hashlib.file_digest((d/'decoded.iso').open('rb'),'sha256').hexdigest()==hashlib.file_digest((R/'inputs/dedup/FC-6-x86_64-disc1.iso').open('rb'),'sha256').hexdigest()
     with (R/'verification.jsonl').open('a') as out:out.write(json.dumps({'id':result['id'],'dedup_round_trip':good,'returncode':q.returncode})+'\n')
     if not good:raise RuntimeError('Dedup output verification failed')
    shutil.rmtree(d)
