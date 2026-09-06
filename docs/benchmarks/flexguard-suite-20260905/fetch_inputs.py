from pathlib import Path
import urllib.request,concurrent.futures,tarfile,shutil
r=Path(__file__).resolve().parent; d=r/'downloads';d.mkdir(exist_ok=True)
base='https://github.com/cirosantilli/parsec-benchmark/releases/download/3.0/'
names=['parsec-3.0-input-sim.tar.gz']+[f'parsec-3.0-input-native.tar.gz.{i}' for i in range(5)]
def fetch(name):
 p=d/name
 if not p.exists():
  urllib.request.urlretrieve(base+name,str(p)+'.part');Path(str(p)+'.part').rename(p)
 print('downloaded',name,p.stat().st_size,flush=True)
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:list(pool.map(fetch,names))
p=d/'parsec-3.0-input-native.tar.gz'
if not p.exists():
 with p.open('wb') as out:
  for name in names[1:]:
   with (d/name).open('rb') as f:shutil.copyfileobj(f,out)
for name in ['parsec-3.0-input-sim.tar.gz','parsec-3.0-input-native.tar.gz']:
 with tarfile.open(d/name,'r:gz') as t:
  for m in t:
   if m.isfile() and any(x in m.name for x in ['raytrace/inputs/input_simsmall','volrend/inputs/input_native','dedup/inputs/input_native']):
    dest=d/Path(m.name).name
    # Same input_native basename; prefix application.
    app=next(x for x in ['raytrace','volrend','dedup'] if x+'/' in m.name)
    dest=d/(app+'-'+Path(m.name).name)
    with t.extractfile(m) as f,dest.open('wb') as out:shutil.copyfileobj(f,out)
    outdir=r/'inputs'/app;outdir.mkdir(parents=True,exist_ok=True)
    with tarfile.open(dest) as inner:inner.extractall(outdir,filter='data')
    print('extracted',app,m.name,flush=True)
