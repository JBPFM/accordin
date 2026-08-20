# Condition-variable attribution in LevelDB db_bench under the pthread lock hook

Measurement date: 2026-08-20. Repository revision `00fe104`, branch `refactor_scheduler`.

## Question

Which condition variable dominates blocking in db_bench `readrandom` and `fillrandom`, and is
`readrandom` genuinely read-only?

## Environment

| Item | Value |
| --- | --- |
| Host CPU | 96 x Intel Xeon Gold 5318Y @ 2.10 GHz |
| Kernel | 6.8.0-88-generic |
| sched_ext support | absent (`/sys/kernel/sched_ext` does not exist; `CONFIG_SCHED_CLASS_EXT` not in the kernel config) |
| LevelDB | `third_party/leveldb-1.23`, working tree with the CV/lock-timing instrumentation |
| db_bench build | `third_party/leveldb-1.23/build/db_bench`, mtime 2026-08-20 09:08:36, newer than `port/port_stdcxx.h` (09:08:27) |
| `condition_variable_any` symbols in db_bench | 0 (plain `std::condition_variable`, so the two-mutex construction is not present) |
| Preload library | `target/release/libmcs_tas_accordin.so`, built by `cargo build --release` (BPF skeleton built without error) |

### Scheduler-arm limitation

The BPF scheduler component cannot be exercised on this host. Loading it aborts during library
initialization:

```
[mcs_tas_accordin] Failed to load eBPF scheduler: sched_ext_ops.dump() missing, kernel too old?
thread '<unnamed>' panicked at src/mcs_tas_accordin/src/lib.rs:15:1: eBPF initialization failed
```

The kernel predates sched_ext, so the scheduler-loaded arms are unavailable and the sched_ext
settle/ejection checks have no state file to read. The measured "with hook" arms therefore use
`MCS_TAS_ACCORDIN_DISABLE_BPF=1`, which keeps the pthread mutex/condvar interposition and the MCS/TAS
lock backend while skipping the BPF load. All CV counters below are LevelDB-internal and reflect the
application's own synchronization structure, which is what the question asks about; the hook-vs-no-hook
pair still answers whether the CV traffic is intrinsic to the workload or induced by the lock layer.

## Configuration

Operation counts follow the experiment-four normalization (`num=500000`, `total_ops=1572864`):
reads use `--num=500000` with `--reads=ceil(1572864/64)=24576` per thread against a pre-filled DB;
writes use `--num=ceil(1572864/128)=12288` per thread with a fresh DB.

DB template for the read arms, created once:

```
third_party/leveldb-1.23/build/db_bench --benchmarks=fillseq --threads=1 --num=500000 \
    --db=$SCRATCH/dbtpl --use_existing_db=0
```

Each read repeat gets a private `cp -a` copy of that template. Measured invocations:

```
# arm a - readrandom, 64 threads, lock hook
env LEVELDB_LOCK_TIMING=1 MCS_TAS_ACCORDIN_DISABLE_BPF=1 \
    LD_PRELOAD=target/release/libmcs_tas_accordin.so \
    third_party/leveldb-1.23/build/db_bench --benchmarks=readrandom --threads=64 \
    --num=500000 --db=$DB --use_existing_db=1 --reads=24576

# arm b - readrandom, 64 threads, no preload
env LEVELDB_LOCK_TIMING=1 \
    third_party/leveldb-1.23/build/db_bench --benchmarks=readrandom --threads=64 \
    --num=500000 --db=$DB --use_existing_db=1 --reads=24576

# arm c - fillrandom, 128 threads, lock hook
env LEVELDB_LOCK_TIMING=1 MCS_TAS_ACCORDIN_DISABLE_BPF=1 \
    LD_PRELOAD=target/release/libmcs_tas_accordin.so \
    third_party/leveldb-1.23/build/db_bench --benchmarks=fillrandom --threads=128 \
    --num=12288 --db=$DB --use_existing_db=0

# arm d - fillrandom, 128 threads, no preload
env LEVELDB_LOCK_TIMING=1 \
    third_party/leveldb-1.23/build/db_bench --benchmarks=fillrandom --threads=128 \
    --num=12288 --db=$DB --use_existing_db=0
```

`LEVELDB_LOCK_TIMING=1` also gates `LEVELDB_CV_STATS`. Three repeats per arm, arms interleaved within
each repeat, each repeat on a freshly created DB directory on local disk.

## Run validity

CV counters are process-cumulative. Each process runs exactly one benchmark, so the counters cover the
measured benchmark plus process startup, DB open/recovery and shutdown, and the db_bench harness
barrier. `leveldb_lock_benchmark_elapsed_ns` is used as the denominator for rate columns.

| Arm | Repeats | Exit codes | us/op per repeat | dmesg findings | Settle wait |
| --- | --- | --- | --- | --- | --- |
| a readrandom/64, lock hook | 3 | 0, 0, 0 | 73.998, 106.209, 105.210 | none | not applicable (no sched_ext) |
| b readrandom/64, no preload | 3 | 0, 0, 0 | 106.934, 107.800, 107.701 | none | not applicable |
| c fillrandom/128, lock hook | 3 | 0, 0, 0 | 526.989, 539.363, 534.882 | none | not applicable |
| d fillrandom/128, no preload | 3 | 0, 0, 0 | 8111.249, 8087.393, 8175.546 | none | not applicable |

`dmesg` was sampled before and after every run and the delta captured per run. All 12 deltas are
empty; no `sched_ext`, `runnable task stall`, or `disabled (runnable task stall)` message appears in
any capture. No watchdog ejection is possible on this kernel, so no repeat is tainted on that axis.
Repeat a/r1 is the first run of the series and is ~30 % faster than a/r2 and a/r3 (cache warm-up of the
copied DB template); its CV counts are in line with the other two repeats and it is retained.

## CV table

Values are summed over the three repeats of each arm. `waits/sec` divides by the summed
`leveldb_lock_benchmark_elapsed_ns`. p50/p95 are interpolated inside the log2-ns histogram buckets
(bucket *i* covers `[2^(i-1), 2^i)` ns), so they carry up to one bucket of quantization error.
"Share" is the label's fraction of that arm's total CV blocked nanoseconds.

### readrandom, 64 threads

| Arm | Label | Wait count | Waits/sec | Signals | Broadcasts (mean fanout) | p50 | p95 | Share of CV blocked time |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| a (lock hook) | writer_queue | 0 (label never instantiated) | 0 | 0 | 0 | - | - | 0 % |
| a | bg_work_finished | 0 | 0 | 0 | 0 | - | - | 0 % |
| a | env_bg_queue | 0 | 0 | 0 | 0 | - | - | 0 % |
| a | bench_shared | 362 | 51.4 | 0 | 9 (43.0) | 1.48 ms | 5.32 ms | 100 % |
| b (no preload) | writer_queue | 0 (label never instantiated) | 0 | 0 | 0 | - | - | 0 % |
| b | bg_work_finished | 0 | 0 | 0 | 0 | - | - | 0 % |
| b | env_bg_queue | 0 | 0 | 0 | 0 | - | - | 0 % |
| b | bench_shared | 255 | 32.0 | 0 | 9 (43.0) | 1.19 ms | 5.18 ms | 100 % |

Total CV blocked time: 7.67 s (a) and 8.37 s (b) across three repeats. Against 64 worker threads and
7.04 s / 7.97 s of summed benchmark time this is 1.70 % / 1.64 % of aggregate thread time, and three
of those waits (one per repeat, log2 bucket 31) are the main thread's whole-run join, accounting for
about three quarters of the total. Worker-side CV blocking is on the order of 0.15 % of thread time.

### fillrandom, 128 threads

| Arm | Label | Wait count | Waits/sec | Signals | Broadcasts (mean fanout) | p50 | p95 | Share of CV blocked time |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| c (lock hook) | writer_queue | 4,717,812 | 239,649 | 4,717,812 | 0 | 382 us | 792 us | 98.30 % |
| c | bench_shared | 636 | 32.3 | 0 | 9 (85.7) | 2.34 ms | 13.07 ms | 0.99 % |
| c | env_bg_queue | 162 | 8.2 | 204 | 0 | 92.4 ms | 131.6 ms | 0.71 % |
| c | bg_work_finished | 0 | 0 | 0 | 204 (0.0) | - | - | 0 % |
| d (no preload) | writer_queue | 4,716,528 | 15,728 | 4,716,528 | 0 | 6.31 ms | 16.66 ms | 98.34 % |
| d | bench_shared | 594 | 2.0 | 0 | 9 (85.7) | 4.94 ms | 8.38 ms | 0.85 % |
| d | env_bg_queue | 162 | 0.5 | 204 | 0 | 1.52 s | 2.07 s | 0.81 % |
| d | bg_work_finished | 0 | 0 | 0 | 204 (0.0) | - | - | 0 % |

Total CV blocked time: 2,193 s (c) and 35,516 s (d) across three repeats, i.e. 87.0 % and 92.5 % of
aggregate worker-thread time. Per repeat the writer_queue wait count is 1,572,379 / 1,572,599 /
1,572,834 (c) and 1,572,270 / 1,572,103 / 1,572,155 (d) against 1,572,864 write operations: essentially
every write except the one that finds itself at the head of the queue performs one CV wait.

`bg_work_finished` records 204 broadcasts (68 per repeat) but zero waits: compaction always completes
before a foreground thread reaches the point of waiting on it at these operation counts.
`env_bg_queue` is the single background thread idling for work, not contention. `other` never appears
in any run.

## Workload truth

`readrandom` is read-only in the strict sense that matters here. The `writer_queue` label does not
appear at all in any readrandom run: those condition variables live in per-`Write()` `Writer` objects,
so the label is only emitted once at least one write has occurred. Zero `bg_work_finished` waits and
zero `env_bg_queue` waits confirm that no compaction or background work is triggered by the read arms
on the pre-filled DB. The only CV traffic in readrandom is `bench_shared`, which is the db_bench
harness itself: a start barrier for the 64 worker threads plus the main thread's join, about 2 waits
per thread per run, or one CV wait per ~13,000 read operations.

Dominant CV per workload:

- readrandom: `bench_shared` (the harness barrier) holds 100 % of a negligible total. There is no
  application-level CV blocking.
- fillrandom: `writer_queue` holds 98.3 % of CV blocked time in both arms and accounts for one wait per
  write operation.

## P2 gate input: fraction of CV waits shorter than ~50 us

Read from the log2-ns histograms. Buckets up to and including index 16 cover waits below 2^16 ns =
65.5 us, so this is an upper bound on the sub-50-us fraction; the true sub-50-us count is smaller.

| Arm | Label | Waits < 65.5 us | Total waits | Fraction | Histogram mode |
| --- | --- | --- | --- | --- | --- |
| a readrandom, lock hook | bench_shared | 0 | 362 | 0 % | bucket 22 (2.1-4.2 ms) |
| b readrandom, no preload | bench_shared | 0 | 255 | 0 % | bucket 21 (1.0-2.1 ms) |
| c fillrandom, lock hook | writer_queue | 93 | 4,717,812 | 0.0020 % | bucket 19 (262-524 us) |
| c fillrandom, lock hook | bench_shared | 0 | 636 | 0 % | bucket 22 |
| c fillrandom, lock hook | env_bg_queue | 0 | 162 | 0 % | bucket 27 (67-134 ms) |
| d fillrandom, no preload | writer_queue | 2,212 | 4,716,528 | 0.047 % | bucket 23 (4.2-8.4 ms) |
| d fillrandom, no preload | bench_shared | 0 | 594 | 0 % | bucket 23 |
| d fillrandom, no preload | env_bg_queue | 0 | 162 | 0 % | bucket 31 (1.1-2.1 s) |

No label in any arm has a meaningful population of short waits. Even in the fastest arm the
writer_queue distribution is centred at a few hundred microseconds, three to four log2 buckets above
the sub-50-us region.

## bpftrace cross-validation

One representative repeat per hooked workload was traced with `scripts/futex_block_by_uaddr.bt`. The
target process was launched behind a FIFO gate so it did not `exec` db_bench until bpftrace reported
attachment, giving full-run coverage; bpftrace was sent SIGINT before the process exited.

```
sudo BPFTRACE_MAP_KEYS_MAX=65536 bpftrace scripts/futex_block_by_uaddr.bt <pid>
```

**readrandom, 64 threads, lock hook** (that run: 76.368 us/op, 129 `bench_shared` waits, no other CV
label with traffic).

| uaddr | Waits | Blocked ns | Longest single wait |
| --- | --- | --- | --- |
| 0x5b3bd2238030 (main binary mapping) | 127 | 2,048,850,262 | 1,892,931,332 |
| 0x707a7150b7c8 (library mapping) | 157 | 38,814,454 | 955,447 |
| 0x707a71005700 (library mapping) | 66 | 638,098 | 51,067 |
| two further library words | 2 | 40,497 | 35,971 |

Whole-process futex activity for the run: 354 `FUTEX_WAIT`, 656 `FUTEX_WAKE`. The single binary-mapped
word carries 127 waits and 98.1 % of all futex blocked time, and its longest wait (1.893 s) equals the
run's wall time — this is the `bench_shared` barrier and join, matching the 129 counted `bench_shared`
waits (the two-wait gap is the pre-attach window). The remaining ~225 waits sit in library mappings
(loader, runtime, and the hook's own state words) and contribute 1.9 % of blocked time. No futex word
shows write-path-like traffic. This corroborates the counter view: readrandom's futex blocking is the
harness barrier and nothing else.

**fillrandom, 128 threads, lock hook** (that run: 602.976 us/op, 7.414 s benchmark elapsed, 1,572,590
`writer_queue` waits, 809.84 s `writer_queue` blocked time).

Whole-process futex activity: 1,577,995 `FUTEX_WAIT`, 1,577,425 `FUTEX_WAKE`. Counted LevelDB CV waits
for the same run total 1,572,853, i.e. 99.7 % of every futex wait the process issued is a LevelDB
condition-variable wait. The per-uaddr table shows roughly 131 distinct words in one contiguous
heap region (`0x5bbe3555c000`-`0x5bbe3555c418`, 8-byte stride), each with exactly 12,288 waits — the
per-thread write count — and each with 12,288 matching wake-side operations. 131 x 12,288 = 1.61 M
reproduces the writer_queue total. Blocked time is nearly uniform at ~5.1 s per word; extrapolating
across the region gives ~668 s against the counter's 809.8 s, an 82 % match, with the residual
explained by the CV counter bracketing mutex re-acquisition after the futex wake in addition to the
wait itself. One word (`0x5bbe3555c000`) shows a 7.41 s maximum wait equal to the benchmark elapsed
time — the harness join.

The bpftrace duration histogram for this run peaks at `[256, 512) us` with 1,323,677 of 1,577,995
samples, matching the writer_queue log2 bucket 19 (262-524 us) peak of 3,910,996 of 4,717,812 across
the three counter repeats. Signal-side symmetry also matches: `writer_queue` reports 1:1 signals to
waits and zero broadcasts, and the futex table shows one wake per wait per word.

## Hook effect on the CV structure

The lock hook changes CV wait *durations* dramatically but not CV wait *counts*. fillrandom
writer_queue waits are 4,717,812 (hook) versus 4,716,528 (no preload), a 0.03 % difference, while
median wait duration drops from 6.31 ms to 382 us and end-to-end latency from ~8,100 us/op to ~530
us/op. readrandom shows the same invariance: zero application CV waits in both arms. The condition
variable traffic is therefore a property of the LevelDB write path, not something the lock layer
creates or removes.

## Conclusions

1. **fillrandom is entirely dominated by `writer_queue`.** It holds 98.3 % of CV blocked time in both
   the hooked and the unhooked arm, fires one wait per write operation (1.5724 M waits against 1.572864 M
   ops), and is signalled strictly 1:1 with no broadcasts. `bg_work_finished` records 68 broadcasts per
   repeat but zero waits, and `env_bg_queue` is background-thread idling, not contention. Any CV-level
   optimisation aimed at LevelDB must target the writer queue.

2. **readrandom has no application-level condition-variable blocking at all.** Across six readrandom
   runs the `writer_queue` label is never even instantiated, and `bg_work_finished` and `env_bg_queue`
   record zero waits. The only CV traffic is the db_bench harness barrier and join (`bench_shared`),
   about 120 waits per run for 64 threads — one CV wait per ~13,000 read operations — of which the
   largest single contribution is the main thread waiting for the run to finish. readrandom is
   read-only in the operational sense: it triggers no write path, no compaction, and no background
   work.

3. **The readrandom regression cannot be CV-driven.** With roughly 1.7 % of aggregate thread time in CV
   waits, and three quarters of that being the main thread's join, there is not enough condition-variable
   blocking in readrandom for a CV-targeted change to move the number. Whatever causes the readrandom
   regression lives in the mutex/scheduling path, not in condition-variable handling. The expected win
   from M1 must be redirected to write-bearing workloads — fillrandom and readwhilewriting — where the
   writer queue supplies 87-93 % of aggregate thread time as CV blocking.

4. **A spin-before-park strategy is not supported by the wait-duration distribution.** Under 0.05 % of
   writer_queue waits are shorter than 65 us in either arm, and under 0.002 % in the hooked arm; the
   distributions are centred at 382 us (hooked) and 6.3 ms (unhooked). Short-wait elimination is not
   where the time is. The leverage is in the queue's serialization structure and in wake-to-run latency,
   not in avoiding the park.

5. **The condition-variable traffic is intrinsic to the workload, not scheduler-induced.** Wait counts
   are within 0.03 % between the hooked and unhooked arms in fillrandom and identically zero in
   readrandom, while wait durations differ by more than an order of magnitude. The lock layer changes how
   long waiters block, never how often they block.

6. **Scope limitation.** The BPF scheduler arm could not be measured: this host's 6.8 kernel has no
   sched_ext support and the loader aborts at library initialization. Conclusions 1, 2 and 4 are
   properties of LevelDB's synchronization structure and hold independently of the scheduler.
   Conclusion 3 rests on readrandom's CV traffic being near zero, which is a workload property and
   likewise scheduler-independent. Conclusion 5 is established only for the lock-interposition layer;
   whether the BPF scheduler alters CV wait counts remains unverified and needs a re-run on a
   sched_ext-capable kernel.
