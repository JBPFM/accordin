# Upstream provenance

- Project: LiTL, Library for Transparent Lock Interposition
- Repository: https://github.com/multicore-locks/litl
- Commit: `916469ca797ee299a4ae674b41c4fac9ac4ae21b`
- Commit date: 2018-09-10
- Retrieved: 2026-09-05
- License: MIT, preserved in [LICENSE](LICENSE)

This directory vendors the official repository's root files, `include/`, and
`src/` at that commit. The upstream `paper/` directory (publications, datasets,
and benchmark patches) is omitted. No nested Git checkout or FlexGuard checkout
is required to build the Accordin adapters. The source is tracked with the
Accordin repository so that the integration is included in a fresh checkout.

The existing upstream lock algorithms and their headers are unchanged. Local
integration changes are:

- `include/accordin.h`, `include/accordin-internal.h`, `src/accordin.c`,
  `src/accordin-cond.c`: current MCS and
  MCS-TAS direct backends using the standard LiTL `NO_INDIRECTION` path and a
  shared direct futex condvar implementation. There is no shadow mutex. Condvar
  notifications transfer waiters into a mutex parking queue, and unlock hands
  wakeups to successive relock waiters. Wake publishes an admission request
  that reacquisition consumes with the same epoch. The mutex object stores its
  direct pointer and parking metadata; ordinary unlock checks the parking
  state. Condvar support is unconditional.
- `include/directalgo.h`, `include/directlock.h`, `src/directlock.c`,
  `src/directcond.c`: a shared front end for baseline lock algorithms that keep
  their state outside the intercepted pthread objects. It stores the algorithm
  instance pointer in the intercepted mutex, hands out per-thread storage that
  is private to one (thread, lock) pair, and implements condition variables as a
  futex sequence inside the intercepted `pthread_cond_t`. An algorithm on this
  front end supplies six entry points and needs neither CLHT, ssmem nor PAPI.
- `include/gcrmcs.h`, `src/gcrmcs.c`, `src/gcr.c`: GCR-MCS, generic concurrency
  restriction over an MCS queue, moved from the standalone interposer in
  `bench/otherlocks` with the algorithm unchanged.
- `src/cna.c`: compact NUMA-aware lock over the libvsync implementation, moved
  from the same interposer. `third_party/libvsync` is cloned on demand.
- `Makefile`, `Makefile.config`, `src/Makefile`: register the two algorithms,
  build/link the direct libraries, track configuration changes, and provide
  `check` / `check-bpf`. The condition-variable build switch and mutex-only
  target have been removed; original algorithms use a fixed enabled feature
  define. The Accordin targets do not require CLHT, ssmem, or PAPI; other
  algorithms retain their upstream dependencies.
- `src/interpose.c`, `src/interpose.h`, `src/interpose.map`,
  `src/interpose-aarch64.map`: adapter dispatch, mutex initialization, native
  spinlock/rwlock passthrough for Accordin, pthread-create allocation cleanup,
  and glibc symbol versions for AArch64 and recent glibc on x86-64, including
  GLIBC_2.30/2.34 `pthread_cond_clockwait` for Accordin.
- `src/utils.h`: AArch64 helpers needed by the interposer and removal of the
  unused local `gettid` definition that conflicts with modern glibc.
- `src/liblock.in`: launchers resolve their own location and `exec` the program.
- `tests/direct.c`, `tests/gcr-wakeup.c`, `tests/run-direct.sh`: interposition,
  mutex, condition-variable and GCR wakeup tests for the baseline algorithms.
  None of them attaches a BPF scheduler.
- `tests/`, `README.md`: integration tests and usage documentation.

Building all of the historical upstream algorithms still has their original
platform constraints; the AArch64 integration checks cover the two Accordin
adapters. See [README.md](README.md) for build commands, supported pthread
operations, and the direct futex condvar protocol.
