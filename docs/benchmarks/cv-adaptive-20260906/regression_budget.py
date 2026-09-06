#!/usr/bin/env python3
"""Compare the complete streamcluster and its barrier with fixed variants."""
import argparse,datetime,fcntl,json,os,re,signal,statistics,subprocess,time
from pathlib import Path
from run import ROOT,WORK,arm,b
STREAM=ROOT/'target/flexguard-suite-20260905/streamcluster/streamcluster'
BARRIER=ROOT/'target/streamcluster-analysis-20260905/barrier'
EXPECTED='dfeea2357203cceeb8bcdac4984ffc9da9c953f1f1d19c06990626a4575ef01f'
def main():
 ap=argparse.ArgumentParser();ap.add_argument('--stage',required=True);ap.add_argument('--spin-us',default='1000');ap.add_argument('--arms',nargs='+',required=True);ap.add_argument('--backends',nargs='+',default=['mcs_accordin','mcs_tas_accordin']);ap.add_argument('--modes',nargs='+',default=['barrier','stream']);ap.add_argument('--repeats',type=int,default=3);a=ap.parse_args();b.CONFIG['ACCORDIN_CV_SPIN_US']=a.spin_us
 out=WORK/a.stage;out.mkdir(parents=True,exist_ok=True)
 guard=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(guard,fcntl.LOCK_EX)
 assert b.state()=='disabled';os.sched_setaffinity(0,range(96))
 arms=[arm(name,backend) for name in a.arms for backend in a.backends]
 files=[Path(__file__).resolve(),Path(__file__).with_name('run.py'),STREAM,BARRIER,*[Path(p) for x in arms for p in x['expected_maps']]]
 hashes={str(p):b.sha(p) for p in files};meta={'started_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'arguments':vars(a),'hashes':hashes,'stream_output_sha256':EXPECTED,'cpus':list(range(96))}
 if (out/'metadata.json').exists():
  prior=json.loads((out/'metadata.json').read_text());assert prior['hashes']==hashes and prior['arguments']==vars(a);meta=prior
 else:(out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 rows=[json.loads(l) for l in (out/'results.jsonl').read_text().splitlines()] if (out/'results.jsonl').exists() else [];done={s['id'] for s in rows}
 for rep in range(1,a.repeats+1):
  order=arms[(rep-1)%len(arms):]+arms[:(rep-1)%len(arms)]
  for mode in a.modes:
   for x in order:
    ident=f'{mode}-192-{x["id"]}-r{rep}'
    if ident in done:continue
    assert b.state()=='disabled';output=out/(ident+'.clusters')
    cmd=[str(BARRIER),'192','100'] if mode=='barrier' else [str(STREAM),'10','30','512','32768','32768','2000','none',str(output),'192']
    overrides=b.CONFIG|{'LD_PRELOAD':str(x['library']),'LD_LIBRARY_PATH':str(x['libdir'])}
    s={'id':ident,'benchmark':mode,'variant':x['branch'],'backend':x['backend'],'repeat':rep,'command':cmd,'environment':overrides,'seq_before':b.seq(),'scx_enabled':False,'maps':[],'bpf_fds':[],'max_threads':0,'expected_maps':x['expected_maps']}
    print('START '+ident,flush=True);start=time.monotonic()
    with (out/(ident+'.log')).open('w') as log:
     p=subprocess.Popen(cmd,env=b.ENV|overrides,stdout=log,stderr=subprocess.STDOUT,start_new_session=True)
     try:
      while p.poll() is None and time.monotonic()-start<(60 if mode=='barrier' else 600):
       s['scx_enabled'] |= b.state()=='enabled'
       m=re.search(r'^Threads:\s+(\d+)',b.read(f'/proc/{p.pid}/status'),re.M)
       if m:s['max_threads']=max(s['max_threads'],int(m[1]))
       if not s['maps'] or not s['bpf_fds']:
        s['maps']=sorted(set(str(Path(l.split()[-1]).resolve()) for l in b.read(f'/proc/{p.pid}/maps').splitlines() if any(n in l for n in ['accordin_direct','accordin_original'])))
        for fd in Path(f'/proc/{p.pid}/fdinfo').glob('*'):
         info=b.read(fd)
         if re.search(r'^(map_id|prog_id|link_id):',info,re.M):s['bpf_fds'].append(info)
       time.sleep(.01 if mode=='barrier' else .1)
      s['timeout']=p.poll() is None
     finally:b.stop(p)
    text=(out/(ident+'.log')).read_text();m=re.search(r'seconds=([\d.]+)' if mode=='barrier' else r'Benchmark time:\s*(\d+)',text)
    seconds=float(m[1])/(1 if mode=='barrier' else 100000000) if m else None
    s.update(returncode=p.returncode,wall_seconds=time.monotonic()-start,seconds=seconds,seq_after=b.seq(),state_after=b.state())
    s['valid']=not s['timeout'] and p.returncode==0 and seconds is not None and seconds>0 and s['maps']==s['expected_maps'] and bool(s['bpf_fds']) and s['scx_enabled'] and s['seq_after']==s['seq_before']+1
    if mode=='stream':
     s['output_sha256']=b.sha(output) if output.exists() else None;s['valid'] &= s['output_sha256']==EXPECTED
    if re.search(r'Failed to (?:load|attach|register)|DEBUG DUMP|Assertion.*failed',text):s['valid']=False
    with (out/'results.jsonl').open('a') as f:f.write(json.dumps(s)+'\n')
    rows.append(s);print('DONE '+json.dumps({k:s[k] for k in ['id','valid','seconds','timeout']}),flush=True)
 assert all(b.sha(p)==h for p,h in hashes.items())
 summary=[]
 for mode in a.modes:
  for backend in a.backends:
   for name in a.arms:
    group=[s for s in rows if s['benchmark']==mode and s['backend']==backend and s['variant']==name];vals=[s['seconds'] for s in group if s['valid']]
    summary.append({'benchmark':mode,'backend':backend,'variant':name,'attempts':len(group),'n':len(vals),'values':vals,'mean':statistics.mean(vals) if vals else None})
 (out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n');meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),hashes_unchanged=True,state_after=b.state());(out/'metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 print('SUMMARY '+json.dumps(summary),flush=True)
if __name__=='__main__':main()
