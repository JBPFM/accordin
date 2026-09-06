#!/usr/bin/env python3
"""Build and check the actual simplify workspace under the benchmark lock."""
import datetime,fcntl,hashlib,json,os,subprocess
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2];WORK=Path(__file__).resolve().parent
def sha(p):return hashlib.sha256(Path(p).read_bytes()).hexdigest()
guard=open('/tmp/mutexbench-sweep-multi-lock.lock','r');fcntl.flock(guard,fcntl.LOCK_EX)
assert Path('/sys/kernel/sched_ext/state').read_text().strip()=='disabled'
assert subprocess.check_output(['git','-C',str(ROOT),'branch','--show-current'],text=True).strip()=='simplify'
variant=WORK/'variants/tick';matched={}
for top in ['src','include','third_party/scx','third_party/litl/src','third_party/litl/include']:
 for p in (variant/top).rglob('*'):
  if p.is_file() and p.suffix in ('.c','.h') and p.name!='topology.h':
   rel=p.relative_to(variant);assert sha(ROOT/rel)==sha(p),rel;matched[str(rel)]=sha(p)
assert sha(ROOT/'Makefile')==sha(variant/'Makefile');matched['Makefile']=sha(ROOT/'Makefile')
env={k:v for k,v in os.environ.items() if not k.startswith(('ACCORDIN_','MCS_ACCORDIN_','MCS_TAS_ACCORDIN_','SCX_','LD_','COND_VAR','COND_VAE'))}
commands=[['make','-j8','check','check-litl'],['make','check-bpf','check-litl-bpf'],['taskset','-c','0,1','env','LITL_TEST_THREADS=24','LITL_TEST_ITERATIONS=1000','make','check-bpf','check-litl-bpf']]
meta={'started_utc':datetime.datetime.now(datetime.timezone.utc).isoformat(),'branch':'simplify','variant':'tick','source_hashes_matching_tested_variant':matched,'commands':[]}
for i,cmd in enumerate(commands):
 print('CHECK '+str(cmd),flush=True)
 with (WORK/f'integration-check-{i}.log').open('w') as log:
  result=subprocess.run(cmd,cwd=ROOT,env=env,stdout=log,stderr=subprocess.STDOUT,timeout=360)
 meta['commands'].append({'command':cmd,'returncode':result.returncode})
 (WORK/'integration-metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
 assert result.returncode==0
 assert Path('/sys/kernel/sched_ext/state').read_text().strip()=='disabled'
 print('PASS '+str(i),flush=True)
assert all(sha(ROOT/p)==h for p,h in matched.items())
paths=[ROOT/'target/release'/f'lib{name}_direct.so' for name in ['mcs_accordin','mcs_tas_accordin']]+[ROOT/'third_party/litl/lib'/f'lib{name}_original.so' for name in ['mcsaccordin','mcstasaccordin']]
meta.update(completed_utc=datetime.datetime.now(datetime.timezone.utc).isoformat(),source_unchanged=True,library_hashes={str(p):sha(p) for p in paths},state_after='disabled')
(WORK/'integration-metadata.json').write_text(json.dumps(meta,indent=2)+'\n')
(WORK/'production.patch').write_bytes(subprocess.check_output(['git','-C',str(ROOT),'diff','--binary','--','src','include','Makefile','third_party/litl','scripts']))
