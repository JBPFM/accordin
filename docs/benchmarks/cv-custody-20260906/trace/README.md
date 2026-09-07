# Regime tracing for the condvar-custody fillrandom runs

Custody fillrandom is bimodal on this host: most 30 s runs land near 29 Kops/s,
a few near 62, while the `[accordin_cv]` counters per operation are the same in
both. The counters therefore cannot separate the two; something about the
scheduling dynamics does. These scripts measure the candidates.

Everything here reads the build in
`target/cv-custody-20260906/custody-src` and the `db_bench` at
`/mnt/data/home/jz/accordin-m0/target/flexguard-suite-20260905/leveldb/out-static/db_bench`
(sha256 `f958f932…1a96d`). Both arms — `custody` and `custody-off`
(`ACCORDIN_CV_CUSTODY=0`) — use the same libraries, so the probe paths are
fixed and no substitution is needed. `../run.py` gives `custody-off` its own
worktree because it identifies arms by commit; here the two arms differ only by
the environment variable, which is the same measured code either way.

## What is measured

| Question | Where it comes from |
|---|---|
| followers signalled per leader batch | `cv_regime.bt` `@batch_signals`, `@batch_signals_stats` — `accordin_wait_notify` calls between `DBImpl::Write` entry and return |
| leader critical-section length | `cv_regime.bt` `@leader_cs_us` — leader `Write` duration minus its `log::Writer::AddRecord`, the only stretch of `Write` that runs with the DB mutex released; `@leader_signal_loop_us` and `@leader_lastsignal_to_ret_us` split its tail |
| hop latency, signal to relock return | `cv_regime.bt` `@notify_to_relock_return_us` (and `@notify_to_relock_entry_us`, `@relock_call_us`, `@relock_to_relock_us`) |
| how wide each batched release is | `cv_regime.bt` `@flush_moved`, `@flush_moved_stats`, `@flush_empty`, `@flush_gap_us`, `@flush_us` |
| custody queue depth (WAITFORSIGNAL) | `cv_regime.bt` `@custody_depth_at_park` / `@custody_depth_at_flush`; exactly, over time, from `admission-slots.csv` column `cv_parked_now` |
| whether the leader itself is delayed | `cv_regime.bt` `@leader_parks` vs `@follower_parks`, `@leader_write_us` vs `@follower_write_us` |
| how many followers run concurrently | `cv_sched.bt` `@db_samples[bucket]` / `@cpu_samples[bucket]` × online CPUs, per 10 ms |
| wake-to-run latency | `cv_sched.bt` `@wake_to_run_us`; `@db_prev_state` separates "still runnable" (scheduler custody, preemption, yield) from a real sleep |
| fraction of time all admission slots are occupied | `admission-slots.csv`, columns `owners_busy` / `owners_total`; the wrapper prints the percentage |
| idle-depth regime | `cpuidle-delta.csv` and the `cpuidle` lines in `run.log` |

## Files

- `cv_regime.bt` — user-space probes only, attached with `-p <pid>` so nothing
  outside the traced process is touched. This is the primary script.
- `cv_sched.bt` — kernel scheduler view. `sched:sched_switch` fires for every
  context switch on the machine, so this is the heavier of the two. Run it in a
  separate window; never together with `cv_regime.bt`.
- `sample_bss.py` — 10 Hz sampler of the loaded scheduler's `.bss` map through
  `bpftool`, giving the admission slots and the custody counters over time. It
  takes the byte offsets from the `accordin.bpf.o` of the build under
  measurement and refuses to run when the loaded map is a different size, which
  is what happens if a scheduler from another worktree is up: the `934bd1c`
  baseline has a 2064 byte `.bss` and no `cv_*` symbols at all, the custody
  build has 2144, and reading one at the other's offsets returns numbers that
  look plausible and mean nothing. Each `bpftool` dump costs about 20 ms, so
  the samples land near 120 ms apart rather than exactly 100.
- `run_traced.sh` — the harness.

## Symbols

Resolved and confirmed present in the measured build:

| Symbol | Object | Binding |
|---|---|---|
| `_ZN7leveldb6DBImpl5WriteERKNS_12WriteOptionsEPNS_10WriteBatchE` | `db_bench` | global |
| `_ZN7leveldb3log6Writer9AddRecordERKNS_5SliceE` | `db_bench` | global |
| `accordin_wait_notify` | `libmcstasaccordin_original.so` | local, `.symtab` |
| `accordin_wait_relock` | `libmcstasaccordin_original.so` | local, `.symtab` |
| `mcs_tas_accordin_direct_mutex_relock_park` | `libmcs_tas_accordin_direct.so` | exported |
| `mcs_tas_accordin_direct_mutex_cv_flush` | `libmcs_tas_accordin_direct.so` | exported |

`accordin_wait_notify` and `accordin_wait_relock` take the same
`struct accordin_park_waiter *`, which is what lets the notifier's timestamp be
handed to the woken thread and makes the hop latency a single measurement
rather than two halves joined by an assumption.

Not probed, and why:

- `flush_notified`, `park_start`, `park_wake`, `wake_first` in the adapter and
  `accordin_cv_flush_now` in the direct runtime are static and either absent
  from the symbol table or reachable only through the exported wrapper. The
  exported `…_mutex_cv_flush` catches every flush regardless of which trigger
  site called it, so nothing is lost.
- `admission_begin` / `admission_wait` / `admission_enter` are `static inline`
  in `src/runtime.h`. There is no symbol to attach to, and building with
  `PERF_SYMBOLS=1` would not help — the header is included into every caller.
- `accordin_mutex_lock` / `accordin_mutex_unlock` are probe-able but were left
  out on purpose: they fire on every `pthread_mutex` operation in the process,
  and the earlier chain measurement lost 57% of throughput to a script that
  included them. The leader critical section is reconstructed from `Write`
  minus `AddRecord` instead.
- The number of tasks queued in `WAITING_DSQ` is not reachable from bpftrace:
  `scx_bpf_dsq_nr_queued` is a kfunc for BPF scheduler programs, and bpftrace
  cannot read another program's maps. The `WAITFORSIGNAL_DSQ` depth is
  available exactly, as `cv_parked_now` in `admission-slots.csv`; the
  `WAITING_DSQ` side is covered indirectly by `owners_busy` (how many admission
  slots hold a ticket) and by the concurrency timeline in `cv_sched.bt`.

## Validating a change to these scripts

**There is no non-attaching validation mode for bpftrace v0.25.0 on this host.**
`--dry-run` attaches by design, and `-d ast` / `-d codegen` were both observed
to attach and run the script to completion; only a script whose symbols fail to
resolve exits early. Never run bpftrace here while another benchmark session
holds `/tmp/mutexbench-sweep-multi-lock.lock`.

Both scripts in this directory were compiled by bpftrace v0.25.0 through
`TypeResolver` and `ResourceAnalyser` and attached successfully, so the syntax,
the types and every probed symbol are confirmed against the measured build.
Two observations from that pass are already folded in:

- `delete(@map[key])` emits `WARNING: Return value discarded`. The form works;
  the warning is cosmetic and goes to `<script>.stderr.txt`.
- `cv_sched.bt` opens one map key per 10 ms bucket. A 5 s window needs 500, well
  under the 4096 default, but the wrapper sets `BPFTRACE_MAX_MAP_KEYS=16384` so
  a longer window cannot silently drop buckets.

## Overhead

`cv_regime.bt` adds about 150k uprobe hits per second at 30 Kops/s. The earlier
chain measurement on this host cost 31–57% of throughput with comparable probe
sets, so absolute microseconds are inflated; ratios, counts and the comparison
between a slow and a fast run are what these numbers are for. Take one
untraced run per condition (`--no-trace`) alongside each traced one and compare
`BENCH_TOTAL`.

## The harness

`run_traced.sh` reproduces the host contract of `../run.py`: exclusive `flock`
on `/tmp/mutexbench-sweep-multi-lock.lock`, `/sys/kernel/sched_ext/state` read
back as `disabled` before each run, affinity over
`/sys/devices/system/cpu/online`, the same environment (BPF and admission on,
stats-only and hook statistics off, `ACCORDIN_CV_COUNTERS=1`,
`OMP_PROC_BIND=false`, `OMP_WAIT_POLICY=PASSIVE`, `LC_ALL=C`) with every
`ACCORDIN_`/`MCS_`/`SCX_`/`LD_`/`COND_VAR` variable scrubbed first, the
worktree adapter as `LD_PRELOAD` and the worktree `target/release` as
`LD_LIBRARY_PATH`, 192 threads, `--time_ms=30000`, database on `/tmp` tmpfs.

It runs one `fillrandom`, waits out a warm-up, attaches bpftrace for a fixed
window, and records `BENCH_TOTAL`, the `[accordin_cv]` counters, the admission
slot timeline, and the cpuidle accounting into one output directory. Root is
required.

### Idle accounting

Per-state `time` and `usage` are summed over the online CPUs immediately before
`db_bench` starts and immediately after it exits; `cpuidle-delta.csv` carries
the difference and the share of `wall × online CPUs` each state took, which is
what distinguishes a machine that stayed in C1/C1E from one that sank into C6
between hops. The host runs `intel_idle` with the `menu` governor and states
POLL / C1 (1 µs) / C1E (4 µs) / C6 (exit latency 170 µs, target residency
600 µs); C6 is `state3`.

`--no-c6` writes `1` to `state3/disable` on every online CPU before the run and
restores the previous value afterwards, including on error or interrupt through
an `EXIT`/`INT`/`TERM` trap. `config.json` records whether it was active.

`cpufreq-before.txt` / `cpufreq-after.txt` hold
`/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq`. This host exposes no
`cpufreq` sysfs, so both normally read `absent`; the files exist so a host that
does expose it is covered without a change.

### Output

```
run.log                 every step, BENCH_TOTAL, [accordin_cv], idle shares
config.json             arm, options, online CPUs, wall time, return code
fillrandom.log          db_bench stdout and stderr
pre-readrandom.log      only with --pre
cv_regime.trace.txt     bpftrace maps, printed at END
cv_regime.stderr.txt    attach messages and warnings
admission-slots.csv     t_ms, owners_busy, owners_total, cv_* counters
cpuidle-before.txt cpuidle-after.txt cpuidle-delta.csv
cpufreq-before.txt cpufreq-after.txt
```

## Running it

One traced slow run (custody, nothing before it — the condition under which the
slow regime has always appeared):

```sh
sudo /mnt/data/home/jz/accordin-simplify/docs/benchmarks/cv-custody-20260906/trace/run_traced.sh \
  --arm custody --script cv_regime \
  --out /mnt/data/home/jz/accordin-simplify/target/cv-custody-20260906/trace-slow
```

One traced fast-candidate run (a full `readrandom` of the same arm immediately
before the measured `fillrandom`, which is the ordering the fast regime has
followed):

```sh
sudo /mnt/data/home/jz/accordin-simplify/docs/benchmarks/cv-custody-20260906/trace/run_traced.sh \
  --arm custody --pre --script cv_regime \
  --out /mnt/data/home/jz/accordin-simplify/target/cv-custody-20260906/trace-fast
```

Slow condition with idle accounting only, no probes — the clean throughput
reference for the pair above:

```sh
sudo /mnt/data/home/jz/accordin-simplify/docs/benchmarks/cv-custody-20260906/trace/run_traced.sh \
  --arm custody --no-trace \
  --out /mnt/data/home/jz/accordin-simplify/target/cv-custody-20260906/idle-slow
```

Fast-candidate condition, no probes:

```sh
sudo /mnt/data/home/jz/accordin-simplify/docs/benchmarks/cv-custody-20260906/trace/run_traced.sh \
  --arm custody --pre --no-trace \
  --out /mnt/data/home/jz/accordin-simplify/target/cv-custody-20260906/idle-fast
```

Slow condition with C6 held out, which is the test of the idle-depth
hypothesis: if the slow regime is C6 exit latency on every hop, this run should
reach the fast regime without a preceding `readrandom`.

```sh
sudo /mnt/data/home/jz/accordin-simplify/docs/benchmarks/cv-custody-20260906/trace/run_traced.sh \
  --arm custody --no-c6 --no-trace \
  --out /mnt/data/home/jz/accordin-simplify/target/cv-custody-20260906/idle-slow-noc6
```

The kernel-side window, taken separately because it is the heavier script:

```sh
sudo /mnt/data/home/jz/accordin-simplify/docs/benchmarks/cv-custody-20260906/trace/run_traced.sh \
  --arm custody --script cv_sched \
  --out /mnt/data/home/jz/accordin-simplify/target/cv-custody-20260906/sched-slow
```

Add `--arm custody-off` for the control; every option above is independent of
the arm.
