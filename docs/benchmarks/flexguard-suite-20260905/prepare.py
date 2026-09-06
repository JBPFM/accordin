#!/usr/bin/env python3
"""Rebuild the tested benchmark variants in an empty target directory."""
import os,subprocess,shutil,sys
from pathlib import Path
HERE=Path(__file__).resolve().parent
ROOT=HERE.parents[2]
R=ROOT/'target'/os.environ.get('FLEXGUARD_SUITE_NAME','flexguard-suite-reproduction')
FG=ROOT/'bench/flexguard'
if R.exists():raise SystemExit(f'Refusing to overwrite existing build: {R}')
R.mkdir(parents=True)
def command(args,cwd):
 with (R/'prepare.log').open('a') as log:
  log.write('\n'+repr(list(map(str,args)))+'\n');log.flush()
  subprocess.run(list(map(str,args)),cwd=cwd,stdout=log,stderr=subprocess.STDOUT,check=True)
def copy(src,name,patch=None):
 d=R/name
 shutil.copytree(src,d,ignore=shutil.ignore_patterns('.git','build','*.o','out-static','out-shared'))
 if patch:
  with (HERE/'patches'/f'{patch}.patch').open() as f:subprocess.run(['patch','-p1'],cwd=d,stdin=f,check=True,stdout=subprocess.DEVNULL)
 return d
# Dependencies: GCC/G++, Clang/LLVM, make, cmake, m4, git, libelf-dev,
# zlib1g-dev, libnuma-dev, libgoogle-glog-dev, libgmock-dev, libssl-dev.
fg=R/'fg';fg.mkdir()
for name in ['include','src','bmarks','vmlinux']:shutil.copytree(FG/name,fg/name)
for name in ['Makefile','interpose.in']:shutil.copy2(FG/name,fg/name)
with (HERE/'patches/flexguard-arm.patch').open() as f:subprocess.run(['patch','-p1'],cwd=fg,stdin=f,check=True,stdout=subprocess.DEVNULL)
for name in ['libbpf','bpftool']:(fg/name).symlink_to(FG/name,target_is_directory=True)
command(['make','-j16','LOCK_VERSION=FLEXGUARD','HYBRID_VERSION=MCS','CONDVARSWAIT=BLOCK','ADD_PADDING=1','DEBUG=0','interpose.so','test_flexguard'],fg)
pth=R/'pthread';pth.mkdir()
for name in ['include','src','bmarks']:shutil.copytree(fg/name,pth/name)
shutil.copy2(fg/'Makefile',pth/'Makefile')
# The FlexGuard patch already carries scheduling's counter conversion; apply
# the remaining micro fixes by using the exact patched source snapshot.
for name in ['scheduling.c','test_init.c']:shutil.copy2(HERE/'sources'/name,pth/'bmarks'/name)
command(['make','-j8','LOCK_VERSION=MUTEX','USE_REAL_PTHREAD=0','CONDVARSWAIT=BLOCK','ADD_PADDING=1','scheduling','buckets','test_correctness','test_init'],pth)
for name in ['run.py','phase1.py','phase2.py','phase3.py','fetch_inputs.py','bench_timer.h']:shutil.copy2(HERE/name,R/name)
ldb=copy(FG/'ext/leveldb-1.20','leveldb','leveldb')
command(['./build_detect_platform','build_config.mk','.'],ldb);command(['make','-j16','out-static/db_bench'],ldb)
for app in ['dedup','streamcluster']:
 d=copy(FG/f'ext/parsec-benchmark/pkgs/kernels/{app}/src',app,app)
 if app=='dedup':flags='-O3 -I. -I.. -D_XOPEN_SOURCE=600 -DENABLE_PTHREADS -DENABLE_GZIP_COMPRESSION -pthread -fcommon';key='CFLAGS'
 else:flags='-O3 -DNDEBUG -I.. -DENABLE_THREADS -pthread';key='CXXFLAGS'
 command(['make','-j8','version=pthreads',f'{key}={flags}'],d)
macro=FG/'ext/parsec-benchmark/pkgs/libs/parmacs/src/m4/parmacs.pthreads.c.m4'
for app in ['raytrace','volrend']:
 d=copy(FG/f'ext/parsec-benchmark/ext/splash2x/apps/{app}/src',app)
 flags='-O3 -DNDEBUG -DENABLE_THREADS -pthread -fcommon -Wno-implicit-function-declaration -Wno-implicit-int -Wno-int-conversion -Wno-incompatible-pointer-types -I./libtiff -I..'
 if app=='volrend':flags+=' -DUSE_PROTOTYPES=1 -DUSE_VARARGS=0 -DHAVE_IEEEFP=1 -DUSE_CONST=1'
 command(['make','-j8','version=pthreads','HOSTCC=gcc',f'MACROS={macro}','M4=m4',f'CFLAGS={flags}'],d)
command(['git','clone','--branch','optiql','https://github.com/sfu-dis/pibench.git',R/'pibench'],R)
import json
revision=json.loads((HERE/'metadata.json').read_text())['pibench_commit']
command(['git','checkout',revision],R/'pibench')
with (HERE/'patches/pibench.patch').open() as f:subprocess.run(['patch','-p1'],cwd=R/'pibench',stdin=f,check=True,stdout=subprocess.DEVNULL)
command(['cmake','-S',R/'pibench','-B',R/'pibench-build','-DCMAKE_BUILD_TYPE=Release','-DBUILD_TESTING=OFF','-DPIBENCH_USE_EPOCH_BASED_RECLAMATION=OFF'],R)
command(['cmake','--build',R/'pibench-build','--target','pibench-bin','-j12'],R)
copy(FG/'ext/index-benchmarks','index','index')
command(['g++','-shared','-fPIC','-O3','-Ofast','-DNDEBUG','-std=c++17','-march=native','-DRWLOCK','-DMUTEX_LOCK','-DBTREE_RWLOCK','-DBTREE_PAGE_SIZE=256',f'-I{R}/index',f'-I{R}/pibench/include',R/'index/wrappers/btreeolc_wrapper.cpp','-o',R/'btreelc_mutex.so','-lglog','-lnuma','-pthread'],R)
k=copy(FG/'ext/kyotocabinet','kyotocabinet');command(['./configure',f'--prefix={R}/kyoto-install'],k);command(['make','-j16','libkyotocabinet.so'],k)
(k/'libkyotocabinet.so.16').symlink_to('libkyotocabinet.so.16.14.0')
copy(FG/'ext/leveldb','kyoto-driver','kyoto-driver')
command(['cmake','-S',R/'kyoto-driver','-B',R/'kyoto-driver-build','-DCMAKE_BUILD_TYPE=Release','-DCMAKE_CXX_STANDARD=17','-DLEVELDB_BUILD_TESTS=OFF','-DLEVELDB_BUILD_BENCHMARKS=ON',f'-DCMAKE_CXX_FLAGS=-I{R}/kyotocabinet',f'-DCMAKE_EXE_LINKER_FLAGS=-L{R}/kyotocabinet -Wl,-rpath,{R}/kyotocabinet'],R)
command(['cmake','--build',R/'kyoto-driver-build','--target','db_bench_tree_db','-j12'],R)
shutil.copy2(HERE/'sources/timer.c',R/'timer.c');command(['gcc','timer.c','-o','timer'],R)
(R/'counter-hz.txt').write_bytes(subprocess.check_output([R/'timer']))
command(['python3',R/'fetch_inputs.py'],R)
print(f'Built {R}. See README.md for seed preparation and serial execution.')
