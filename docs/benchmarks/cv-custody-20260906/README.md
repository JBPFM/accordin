# LevelDB db_bench comparison of fixed commits

`run.py` compares named commits of this repository on one prebuilt LevelDB
`db_bench`, using fresh databases for `fillrandom` and copies of a fixed
`fillseq` seed for `readrandom`. It follows the methodology of
[leveldb-branches-20260906](../leveldb-branches-20260906/README.md); the
differences are listed at the end.

## Results

Measured outcomes of the arms this harness was built for — tail insertion, the
sharded admission queue with its home-CPU idle kick, the shared notify slot and
the dispatch-side release — are written up in [RESULTS.md](RESULTS.md), one
table per session with the acceptance rule, the environment facts and the
caveats that limit what the numbers support.

## Arms

Every arm is `NAME=COMMIT`, optionally followed by `:` and a comma-separated
list of environment overrides applied on top of the shared configuration:

```sh
--arm baseline=934bd1c
--arm custody=<sha>
--arm custody-off=<sha>:ACCORDIN_CV_CUSTODY=0
```

Each distinct commit is checked out once with `git worktree add --detach` into
`target/cv-custody-20260906/<name>-src` and built there with `make -j all`
followed by `make litl`, producing `target/release/libmcs{,_tas}_accordin_direct.so`
and `third_party/litl/lib/libmcs{,tas}accordin_original.so`. Arms that share a
commit share the worktree. A worktree must be clean before any measurement, and
every measured file is re-hashed at the end of the session.

Both lock backends run through the standard LiTL adapter: `LD_PRELOAD` is the
worktree's `lib<adapter>_original.so`, `LD_LIBRARY_PATH` is the worktree's
`target/release`, and `/proc/<pid>/maps` must contain exactly the adapter plus
the matching direct library.

`ACCORDIN_CV_FLUSH_FLAGS` carries most of the per-arm overrides used so far.
It is the bit set of `src/bpf/intf.h` — `EXPIRE` 1, `REV` 2, `TAIL` 4,
`MOVE` 8, `SPREAD` 16 — and defaults to `MOVE|REV` = 10. The `custody-tail`
arm passes 12 (`MOVE|TAIL`, appending moved waits to the admission queue
instead of inserting them at its head) and the `shard-spread` arm passes 26
(`MOVE|REV|SPREAD`, sweeping every free admission slot after a release).

`ACCORDIN_CV_SCAN=0`, which the `scan-off` arm uses to switch off the release
passes running from `ops.dispatch` and the custody timer and leave the
notifier's flush syscall as the only release path, is a knob of the
`cv_slot_release` branch, not of the retained head. The same branch carries
`ACCORDIN_CV_SLOTS`, `ACCORDIN_CV_SCAN_WIDTH` and `ACCORDIN_CV_FLUSH_SYSCALL`;
arms using any of them have to name a commit on that branch.

## Configuration

- `db_bench` at `/mnt/data/home/jz/accordin-m0/target/flexguard-suite-20260905/leveldb/out-static/db_bench`,
  SHA-256 `f958f932acbffe73bba697e3e19898141d78c6486f06dc9830896b171b81a96d`;
  seed at `/tmp/accordin-flexguard-suite-20260905/seed` (`--seed` to override).
- 192 worker threads, `--time_ms=30000`, 1,000,000 keys, 100 B values,
  databases on `/tmp` tmpfs, so the numbers do not represent disk throughput.
- CPU affinity is the set listed in `/sys/devices/system/cpu/online` and is
  recorded in `metadata.json`; nothing assumes a fixed core count.
- BPF and admission on, stats-only and hook statistics off,
  `ACCORDIN_CV_COUNTERS=1`. Per-arm overrides are applied last.
- Sessions serialize on `/tmp/mutexbench-sweep-multi-lock.lock` and wait for
  `/sys/kernel/sched_ext/state` to read `disabled` (up to 120 s) before each run.
- `--repeats N` rotates the arm order once per repeat; `--benchmarks` and
  `--backends` restrict the matrix.

A run is valid when the process exits 0 within its budget, `BENCH_TOTAL` is
present with a plausible measurement window, the library maps match, BPF file
descriptors are open, at least 193 process threads were observed, sched_ext was
seen `enabled` and `enable_seq` advanced by exactly one, the log carries no
error or `[hook_stats]` marker, and the kernel log gained no `ERROR_STALL`,
`failed to run for`, or `runnable task stall` line during the run. Any such
kernel line is stored in the sample.

## Invalid runs

Every failed check is recorded as a `reason` on the sample, so an arm with a
known hang rate does not abort the comparison.

- An invalid attempt is written to `results.jsonl` with `valid: false`, its
  `attempt` number and its `reason`, and the same cell (benchmark, backend,
  arm, repeat) is retried until it produces a valid sample or reaches
  `--max-attempts` (default 3) attempts. Each attempt waits for
  `/sys/kernel/sched_ext/state` to read `disabled` and starts from a fresh
  database directory, meaning a new `fillrandom` directory or a new copy of
  the seed for `readrandom`.
- The database of an invalid attempt is deleted, since it lives on tmpfs.
  `--keep-invalid` retains it and prints its path.
- A cell whose attempts are all invalid is reported and the session moves on to
  the next cell. The process exits non-zero at the end if any cell finished
  without a valid sample; the summary is still written.
- Everything else, including the seed audit and the input-hash checks, still
  raises immediately, because it points at the inputs rather than at a run.

## Usage

```sh
sudo -n python3 docs/benchmarks/cv-custody-20260906/run.py --arm baseline=934bd1c --preflight
sudo -n python3 docs/benchmarks/cv-custody-20260906/run.py \
    --arm baseline=934bd1c --arm custody=<sha> --arm custody-off=<sha>:ACCORDIN_CV_CUSTODY=0 \
    --repeats 3 --max-attempts 3
```

Root is required to load the scheduler. `sudo` clears the environment, so pass
anything the build or the run needs on the command line. Files created under
`sudo` are handed back to `SUDO_UID`/`SUDO_GID`.

`--preflight` builds the worktrees, checks the binary and the seed, and takes a
single 5 s `fillrandom` sample of the first arm on `mcs_tas_accordin`, printing
the parsed sample.

A session is resumable: point `--out` at a directory that already holds a
`results.jsonl` and every cell with a valid sample there is skipped, while a
cell that only has invalid attempts keeps its remaining budget out of
`--max-attempts`. Repeat the original command with the same arms and the same
`--out` to continue an interrupted session.

```sh
sudo -n python3 docs/benchmarks/cv-custody-20260906/run.py \
    --arm baseline=934bd1c --arm custody=<sha> \
    --repeats 3 --out target/cv-custody-20260906/<timestamp>
```

## Output

Results land in `target/cv-custody-20260906/<timestamp>/` unless `--out` names a
directory, which is resumed when it already holds a `results.jsonl`.

- `metadata.json` — arms, commits, hashes, online CPUs, governors, toolchain,
  attempt budget, and the cells that finished without a valid sample.
- `results.jsonl` — one object per attempt, carrying its `attempt` number, a
  `reason` when invalid, and the parsed
  `[accordin_cv] parked=… flushed=… expired=… drained=… parked_now=…
  flush_calls=… flush_misses=…` counters when the build emits them. Builds
  from the `cv_slot_release` branch append further fields to that line —
  `rel_dispatch`, `rel_timer`, `rel_syscall`, `slotless`, `flush_skipped`,
  `flush_contended` — which the harness leaves in the log rather than parsing
  into a sample.
- `summary.json` / `summary.md` — computed from valid samples only, per
  benchmark × backend × arm: number of valid samples, number of invalid
  attempts, mean, stdev, CV%, ratio against the first arm, per-run throughput,
  and mean counters, followed by an invalid-run list of every failed attempt id
  and its reason.
- `<run>.log` — stdout and stderr of each attempt, named after its cell and
  attempt; `build-<arm>.log` per worktree.

## mutexbench comparison

`mutexbench.py` compares the same arms on the `bench/mutexbench` submodule's
`mutex_bench`, a pure lock microbenchmark with no condition variables. It
imports `run.py` as a module, so arm parsing, worktree preparation and build,
environment scrubbing, the shared lock, the `disabled` wait, the kernel-log
check and the resume logic are the same code.

Library selection is the one real difference. `mutex_bench` takes no LiTL
adapter and no `LD_PRELOAD`: its `mcs_accordin_direct` and
`mcs_tas_accordin_direct` lock kinds `dlopen` the path in
`MCS_ACCORDIN_DIRECT_LIB` and `MCS_TAS_ACCORDIN_DIRECT_LIB`, so an arm is
selected by pointing those two variables at its own `target/release`, and
`/proc/<pid>/maps` must contain exactly that arm's direct library. One
`mutex_bench` binary, built once from this worktree with
`make -C bench/mutexbench mutex_bench` and recorded by SHA-256 in
`metadata.json`, serves every arm; initialise the submodule with
`git submodule update --init bench/mutexbench` if it is empty.

Two workload shapes share the harness, selected by `--workload`, and both run at
`--duration-ms 5000 --warmup-ms 1000 --timeslice-extension off` by default. A
run is valid under the same rules, with `throughput_ops_per_sec` above zero and
the `eBPF scheduler loaded successfully` marker in place of the `BENCH_TOTAL`
check, and `EXIT:` added to the log error pattern.

`--threads` takes a list, `192` (`run.py`'s count) alone by default, and every
count multiplies the workload points: each point is one shape at one thread
count, labelled `<shape>-t<count>`, and that label is stored as the sample's
`benchmark`, so resuming a session and the per-cell attempt budget stay per
point and per thread count for both shapes.

### Single lock

`--workload single` (the default) measures one contended lock. Each shape is a
`--pairs CRITICAL_NS:OUTSIDE_NS` timing.

Every run also prints how many operations each thread completed, and the
harness turns that line into the sample's fairness factor: the share of all
operations taken by the busier half of the threads. A perfectly even split
gives 0.5, and 1.0 means half the threads never acquired the lock, so the
factor separates a scheduler that spreads the lock across all waiters from one
that keeps handing it back to a subset at the same throughput. The per-thread
minimum, maximum and mean operation counts are recorded alongside it, and the
raw counters stay in the sample. These fields are recorded whenever the line is
present; `--per-thread` additionally makes a run invalid unless the line covers
exactly the requested thread count.

`summary.md` carries one throughput table per point × backend in Mops/s, a mean
`avg_lock_hold_ns` / `avg_wait_ns_estimated` table, a fairness table with the
mean, minimum and maximum factor over the runs, the mean per-thread minimum and
maximum operation counts and the per-run factors, the mean `[accordin_cv]`
counters, which stay at zero here, and the invalid-run list.

```sh
sudo -n python3 docs/benchmarks/cv-custody-20260906/mutexbench.py \
    --arm baseline=934bd1c --arm shard=<sha> --arm tidy=<sha> \
    --pairs 100:3000 300:3000 --repeats 5 \
    --out target/cv-custody-20260906/mutexbench-<name>
```

The fairness sweep of the multi-lock experiment's seventh experiment is the
same shape at four critical sections and three thread counts:

```sh
sudo -n python3 docs/benchmarks/cv-custody-20260906/mutexbench.py \
    --arm baseline=934bd1c --arm custody=<sha> --arm shard=<sha> --arm tidy=<sha> \
    --pairs 100:3000 300:3000 1000:3000 30000:3000 --threads 48 96 192 \
    --warmup-ms 2000 --duration-ms 8000 --repeats 3 --per-thread \
    --out target/cv-custody-20260906/exp7-<name>
```

### Two locks, two groups

`--workload two-lock --cases ...` measures fairness between two thread groups
that never share a lock. The threads are split in half — 96 and 96 at 192 — and
each half loops on its own mutex with its own critical and outside timing, so
the only thing the groups compete for is CPU. A group that the scheduler starves
loses throughput without any lock handoff explaining it, which is what makes
this shape a fairness probe rather than a contention probe.

Each `--cases` name selects one timing combination, matching the two-lock cases
of the multi-lock experiment:

| case | group A critical / outside ns | group B critical / outside ns |
|---|---|---|
| `homogeneous` | 300 / 3000 | 300 / 3000 |
| `heterogeneous_mild` | 3000 / 3000 | 300 / 3000 |
| `heterogeneous_extreme` | 3000 / 300 | 100 / 3000 |

`homogeneous` gives both groups the same shape, so any split away from an even
one comes from the scheduler. The heterogeneous cases give group A the longer
critical section, and `heterogeneous_extreme` also makes group A far more lock
hungry by shortening its outside work, which is where a policy that favours
long holders or high arrival rates shows up as a group-level shift.

The metrics parsed from each run are the total `throughput_ops_per_sec`,
`fairness_jain`, and per group `throughput_ops_per_sec`,
`normalized_efficiency`, `normalized_slowdown` and `avg_wait_ns_estimated`.
A group's normalized efficiency is its measured throughput over the throughput
its timing would allow with no contention, and its normalized slowdown is the
reciprocal, so a slowdown of 10 means the group ran an order of magnitude below
its own ideal. `fairness_jain` is the Jain index of the two normalized
efficiencies: 1.0 when both groups are held back to the same degree, and
approaching 0.5 when one group keeps its share while the other is squeezed out.
The index compares the two groups against their own ideals rather than against
each other, so a case whose groups have different ideal rates can still reach
1.0. A run is additionally invalid when either group finished with no
operations or when the fairness line is missing.

`summary.md` carries one table per case × backend with the arm, its commit, the
valid and invalid counts, mean total Mops/s with CV% and the ratio against the
first arm, the mean, minimum and maximum `fairness_jain` with the per-run
values, the mean per-group throughput, and the mean per-group normalized
slowdown. `summary.json` keeps every parsed metric, including the per-group wait
estimates, as a mean and as the list of per-run values.

```sh
sudo -n python3 docs/benchmarks/cv-custody-20260906/mutexbench.py \
    --arm baseline=934bd1c --arm custody=<sha> --arm shard=<sha> --arm tidy=<sha> \
    --workload two-lock \
    --cases homogeneous heterogeneous_mild heterogeneous_extreme \
    --repeats 5 --out target/cv-custody-20260906/fairness-<name>
```

## Differences from leveldb-branches-20260906

- Arms come from the command line instead of a hard-coded branch table, and
  each arm may carry environment overrides.
- Only the standard LiTL adapter path is used; there is no fullhook variant and
  no `ACCORDIN_CV_SPIN_US` default.
- Affinity follows the online CPU mask rather than a fixed range.
- `ACCORDIN_CV_COUNTERS=1` is part of the shared configuration and the counter
  line is parsed into each sample.
- Kernel-log stall lines observed during a run invalidate that run.
- An invalid run is retried on a fresh database instead of ending the session,
  and a session can be resumed into an existing output directory.
- The measurement window accepted for validity is derived from the requested
  duration, so the 5 s preflight uses the same rule as a 30 s run.
- `summary.md` is written alongside `summary.json`, and output goes to a
  timestamped subdirectory by default.
