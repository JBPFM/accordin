#!/usr/bin/env python3
"""Export immutable inputs and build one-factor attribution candidates."""
import argparse,difflib,hashlib,io,json,subprocess,tarfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[2]
OUT=Path(__file__).resolve().parent
BASE='8fc18994aa51f27b91bf6b490a575a309add8da3'
FULL='4e5998e21c458e9b162855ef3c3d5c7e42b42ebb'
PATHS=['Makefile','src','include','scripts','third_party/scx','third_party/litl']
def original(path,ref=BASE):return subprocess.check_output(['git','-C',str(ROOT),'show',f'{ref}:{path}'],text=True)
def section(s,start,end):return s[s.index(start):s.index(end,s.index(start))]
def change(s,old,new):
 assert old in s,old
 return s.replace(old,new,1)
def transform(files,kind):
 if kind=='baseline':return
 if kind=='padding':
  p='src/raw_lock.h';files[p]=change(files[p],'#define RAW_LOCK_ALIGN 8','#define RAW_LOCK_ALIGN 64');return
 if kind=='fullhook_heap':
  p='src/fullhook.c';s=files[p]
  s=change(s,'struct raw_lock raw;','struct raw_lock *raw;')
  s=change(s,'_Static_assert(sizeof(struct raw_lock) <= 16, "raw lock exceeds the overlay slot");\n','')
  helper='''/* Attribution control: isolate raw state from application cache lines. */
static struct raw_lock *get_raw(struct hook_mutex *state)
{
    struct raw_lock *raw = __atomic_load_n(&state->raw, __ATOMIC_ACQUIRE);
    if (!raw) {
        size_t bytes = (sizeof(*raw) + 63) & ~(size_t)63;
        struct raw_lock *candidate = aligned_alloc(64, bytes);
        if (!candidate)
            abort();
        memset(candidate, 0, bytes);
        if (__atomic_compare_exchange_n(&state->raw, &raw, candidate, false,
                                         __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE))
            raw = candidate;
        else
            free(candidate);
    }
    return raw;
}

'''
  s=change(s,'static TRACED void traced_acquire',helper+'static TRACED void traced_acquire')
  s=s.replace('lock_ops_lock(&state->raw,','lock_ops_lock(get_raw(state),').replace('lock_ops_trylock(&state->raw)','lock_ops_trylock(get_raw(state))').replace('lock_ops_unlock(&state->raw)','lock_ops_unlock(get_raw(state))')
  s=change(s,'EXPORT int pthread_mutex_destroy(pthread_mutex_t *mutex)\n{\n    memset','EXPORT int pthread_mutex_destroy(pthread_mutex_t *mutex)\n{\n    free(__atomic_exchange_n(&mutex_overlay(mutex)->raw, NULL, __ATOMIC_ACQ_REL));\n    memset')
  files[p]=s;return
 if kind=='fullhook_tas':
  p='src/mcs_tas.h';files[p]=change(files[p],'while (atomic_load_explicit(&lock->locked, memory_order_relaxed) ||\n           atomic_exchange_explicit(&lock->locked, true, memory_order_acquire))','while (atomic_exchange_explicit(&lock->locked, true, memory_order_acquire))');return
 if kind=='compact':files['src/mcs_tas.h']=files['src/mcs_tas.h'].replace('_Alignas(64)','_Alignas(8)');return
 if kind=='ttas':
  p='src/mcs_tas.h';files[p]=change(files[p],'while (atomic_exchange_explicit(&lock->locked, true, memory_order_acquire))','while (atomic_load_explicit(&lock->locked, memory_order_relaxed) ||\n           atomic_exchange_explicit(&lock->locked, true, memory_order_acquire))');return
 if kind=='tls':files['Makefile']=change(files['Makefile'],'HOST_FLAGS :=','HOST_FLAGS := -ftls-model=initial-exec');return
 if kind=='signal':
  p='third_party/litl/include/accordin-internal.h'
  files[p]=files[p].replace('unsigned int armed, queued;','unsigned int armed, queued, independent;').replace('void accordin_wait_notify(struct accordin_park_waiter *waiter);','void accordin_wait_notify(struct accordin_park_waiter *waiter, int independent);')
  p='third_party/litl/src/accordin-cond.c';files[p]=change(files[p],'accordin_wait_notify(&waiter->park);','accordin_wait_notify(&waiter->park, signaled);')
  p='third_party/litl/src/accordin.c';s=files[p]
  s=change(s,'waiter->queued && waiter->request.nested','waiter->queued && (waiter->request.nested || waiter->independent)')
  s=change(s,'void accordin_wait_notify(struct accordin_park_waiter *waiter) {','void accordin_wait_notify(struct accordin_park_waiter *waiter, int independent) {')
  s=change(s,'if (waiter->armed && waiter->request.nested) {','waiter->independent = independent;\n    if (waiter->armed && (waiter->request.nested || independent)) {')
  files[p]=s;return
 p='src/bpf/main.bpf.c';s=files[p];f=original(p,FULL).replace('~USER_META','~USER_FLAGS').replace(' &&\n                 !(state & USER_CV)','')
 if kind=='bpf_all':files[p]=f;return
 if kind=='enqlast':files[p]=change(s,'SCX_OPS_DEFINE(accordin_ops,','SCX_OPS_DEFINE(accordin_ops,\n               .flags = SCX_OPS_ENQ_LAST,');return
 bounds={
  'idle':('s32 BPF_STRUCT_OPS(accordin_select_cpu','void BPF_STRUCT_OPS(accordin_enqueue'),
  'holder':('void BPF_STRUCT_OPS(accordin_enqueue','/* Reserve both the task'),
  'self':('bool BPF_STRUCT_OPS(accordin_yield','void BPF_STRUCT_OPS(accordin_tick'),
  'tick':('void BPF_STRUCT_OPS(accordin_tick','void BPF_STRUCT_OPS(accordin_stopping')}
 if kind not in bounds:raise ValueError(kind)
 start,end=bounds[kind];files[p]=s.replace(section(s,start,end),section(f,start,end),1)

def main():
 ap=argparse.ArgumentParser();ap.add_argument('variants',nargs='+');args=ap.parse_args()
 for name in args.variants:
  full=name.startswith('fullhook_');ref=FULL if full else BASE
  archive=subprocess.check_output(['git','-C',str(ROOT),'archive',ref,*[p for p in PATHS if not (full and p=='third_party/litl')]])
  dest=OUT/'variants'/name
  assert not dest.exists(),dest
  dest.mkdir(parents=True)
  with tarfile.open(fileobj=io.BytesIO(archive)) as tf:tf.extractall(dest,filter='data')
  editable=['Makefile','src/mcs_tas.h','src/bpf/main.bpf.c']+(['src/fullhook.c','src/raw_lock.h'] if full else ['third_party/litl/src/accordin.c','third_party/litl/src/accordin-cond.c','third_party/litl/include/accordin-internal.h'])
  before={p:(dest/p).read_text() for p in editable};after=dict(before)
  for kind in name.split('+'):transform(after,kind)
  patch=''
  for path,s in after.items():
   if s!=before[path]:
    (dest/path).write_text(s)
    patch+=''.join(difflib.unified_diff(before[path].splitlines(True),s.splitlines(True),fromfile='a/'+path,tofile='b/'+path))
  (OUT/f'{name}.patch').write_text(patch)
  print('BUILD '+name,flush=True)
  with (OUT/f'build-{name}.log').open('w') as log:
   subprocess.run(['make','-C',str(dest),'-j8','all' if full else 'litl'],stdout=log,stderr=subprocess.STDOUT,check=True)
  hashes={str(p.relative_to(dest)):hashlib.sha256(p.read_bytes()).hexdigest() for p in dest.rglob('*') if p.is_file() and (p.suffix in ['.h','.c','.so'] or p.name=='Makefile')}
  (OUT/f'{name}.json').write_text(json.dumps({'name':name,'base':ref,'fullhook':FULL,'source':str(dest),'hashes':hashes},indent=2)+'\n')
  print('BUILT '+name,flush=True)
if __name__=='__main__':main()
