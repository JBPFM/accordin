# AGENT.md

## Mission

Build a `sched-ext` based experimental scheduler named `scx_ulock` that implements the core idea of a lock-contention-aware scheduler **for user-space locks only**.

This project must:

- classify tasks based on **user-space lock waiting ratio** collected from an instrumented user-space lock library,
- concentrate lock-intensive tasks onto a dynamically sized **SSC (Special Set of Cores)**,
- search online for a near-optimal SSC width,
- keep SSC and non-SSC scheduling paths separated,
- avoid hot-path tracing of lock operations with `uprobe`, `kprobe`, or kernel lock instrumentation.

This project must **not**:

- track kernel lock contention,
- patch or modify the kernel,
- depend on `uprobe` on every `pthread_mutex_lock()` / `pthread_mutex_unlock()` call,
- attempt to support arbitrary third-party lock implementations in v1,
- optimize for heterogeneous mixed workloads in v1.

The implementation should preserve the paper's scheduling idea while adapting the measurement path to modern `sched-ext` and low-overhead user-space instrumentation.

---

## Architectural Positioning

The design is split into three planes:

1. **Scheduling plane**: `sched-ext` BPF scheduler.
2. **Control plane**: user-space controller that aggregates metrics, classifies tasks, and updates SSC configuration.
3. **Measurement plane**: instrumented user-space lock library that writes per-thread epoch aggregates into a shared mmapable BPF map.

There is **no kernel lock tracing plane** in this project.

---

## Non-Negotiable Design Rules

### Rule 1: User-space lock metrics are the source of truth
The scheduler must classify tasks only from user-space lock metrics produced by the lock library.

Accepted primary metrics:

- `wait_ns`
- `hold_ns`
- `park_ns`
- `contended_acq`
- `park_count`
- `lock_domain_id`

Derived primary classification signal:

- `wait_ratio = user_lock_wait_ns / epoch_runtime_ns`

### Rule 2: No hot-path `uprobe`
Do not build the measurement path around `uprobe` or `uretprobe` on lock hot paths. These may be useful only for occasional debugging or migration analysis, not for steady-state production sampling.

### Rule 3: No kernel lock tracking
Do not implement `lockstat`, `kprobe`, `fentry`, `fexit`, or tracepoint logic for kernel lock contention. Scheduler accounting such as runtime or migration counts is allowed, but lock-wait measurement must remain entirely user-space driven.

### Rule 4: Keep state machines in user space
Complex policy decisions belong in the user-space controller, not in BPF hot paths.

BPF should focus on:

- DSQ routing,
- CPU selection,
- lightweight per-task bookkeeping,
- reading already-computed task classes.

### Rule 5: v1 only supports instrumented workloads
The target application must link against or explicitly use `liblca_lock` / `libulock_sched`. Transparent interception of arbitrary lock libraries is out of scope for v1.

---

## Core Idea to Preserve

The implementation should preserve these ideas from the reference design:

- classify tasks by lock-wait fraction in a time window,
- migrate lock-intensive tasks into an SSC,
- search online for the SSC size,
- separate scheduling/balancing between SSC and non-SSC CPUs,
- prefer topology-aware SSC allocation.

Project defaults:

- enter threshold: `10%`
- exit threshold: `5%`
- hot epochs to enter SSC: `3`
- cool epochs to leave SSC: `5`
- default epoch: `20ms`
- default control period: `100ms`

All defaults must be CLI-configurable.

---

## Target Workloads

Prioritize:

- homogeneous worker pools,
- thread-pool style services,
- lock-intensive user-space critical sections,
- short to medium critical sections,
- workloads using the project lock library.

Examples:

- shared in-memory counters,
- lock-heavy allocators or metadata paths,
- sharded cache index with a few hot locks,
- high-concurrency queue/map structures using project locks.

---

## Explicit Non-Goals

Do not spend v1 effort on:

- transparent support for `pthread_mutex` in arbitrary binaries,
- fairness perfection across unrelated applications,
- energy-aware tuning,
- NUMA-wide multi-SSC orchestration,
- lock-free data structures,
- kernel lock bottleneck diagnosis,
- full observability UI,
- automatic lock-domain inference from arbitrary binaries.

---

## Repository Layout

```text
scx-ulock/
  README.md
  Makefile
  LICENSE

  bpf/
    scx_ulock.bpf.c
    common.bpf.h
    vmlinux.h

  user/
    scx_ulock_ctl.c
    scx_ulock_ctl.h
    topo.c
    topo.h
    classify.c
    classify.h
    search.c
    search.h
    metrics.c
    metrics.h
    debug.c
    debug.h
    bpf_api.h

  lib/
    lca_mutex.h
    lca_mutex.c
    lca_rwlock.h
    lca_rwlock.c
    slot_registry.h
    slot_registry.c
    epoch_sync.h
    epoch_sync.c
    time_util.h
    time_util.c

  bench/
    bench_counter.c
    bench_queue.c
    bench_hotset_map.c
    run_bench.sh
    collect_metrics.py

  docs/
    REQUIREMENTS.md
    DESIGN.md
    CALLBACKS.md
    MAPS.md
    TESTPLAN.md

  scripts/
    run_partial.sh
    show_scx_state.sh
    collect_trace.sh
```

---

## Scheduling Plane Requirements

### BPF Scheduler Name
`scx_ulock`

### Required behavior

#### CPU partitioning
Maintain two CPU sets:

- `ssc_mask`
- `normal_mask`

SSC CPUs run lock-intensive tasks.
Non-SSC CPUs run normal tasks.

#### DSQs
Use at least:

- `DSQ_SSC`
- `DSQ_NORMAL`

#### Dispatch separation
- SSC CPUs dispatch only from `DSQ_SSC`.
- normal CPUs dispatch only from `DSQ_NORMAL`.
- v1 must not cross-steal between sets.

#### Partial mode
Must support `SCX_OPS_SWITCH_PARTIAL` so only opted-in tasks are managed by this scheduler.

#### Task lifecycle hooks
Implement at least:

- `select_cpu`
- `enqueue`
- `dispatch`
- `running`
- `stopping`
- `init_task`
- `exit_task`

Keep `tick` logic minimal.

#### Lazy migration
Use generation-based lazy migration:

- global `ssc_gen`
- per-task `last_ssc_gen`

Do not force eager migration of all tasks whenever SSC width changes.

---

## Measurement Plane Requirements

### Lock Library
Implement a project-owned user-space lock library.

Primary lock type for v1:

- `lca_mutex`

Optional later:

- `lca_rwlock`

### Lock algorithm requirements for `lca_mutex`

Fast path:

- CAS-based acquisition.

Slow path:

- MCS-style queueing,
- bounded spin,
- `futex_wait`,
- direct handoff,
- `futex_wake` on successor.

### Hot-path instrumentation rules
Only measure on state transitions:

- first contention detected,
- acquisition completed,
- unlock completed,
- park entered,
- wake/acquire completion.

Do **not**:

- read clocks in every spin iteration,
- write global shared counters in the spin loop,
- call BPF syscalls from the lock fast path,
- emit per-event records.

### Export model
The lock library writes **epoch aggregates** into a per-thread shared slot.

Use:

- `BPF_MAP_TYPE_ARRAY`
- `BPF_F_MMAPABLE`

Each thread owns one slot and writes only to its own slot.

### Required slot fields
Each slot must include at least:

- `tid`
- `tgid`
- `slot_id`
- `epoch_id`
- `lock_domain_id`
- `wait_ns`
- `hold_ns`
- `park_ns`
- `contended_acq`
- `park_count`
- `seq`
- `flags`

Use a seqlock-like even/odd version field so the controller can read consistent snapshots.

---

## Control Plane Requirements

The control plane owns all policy decisions.

### Responsibilities

- load and attach the BPF scheduler,
- create and mmap shared maps,
- register workload threads and slot ownership,
- collect per-thread epoch aggregates,
- merge user-space lock metrics with scheduler runtime metrics,
- classify tasks,
- run SSC width search,
- update global config and masks,
- expose metrics and debug output.

### Classification policy
For each task and epoch:

- `epoch_runtime_ns` is the denominator,
- `user_lock_wait_ns` is the numerator,
- `wait_ratio = user_lock_wait_ns / epoch_runtime_ns`.

Suggested v1 policy:

- enter SSC if `wait_ratio >= enter_threshold_pct`
- and `contended_acq >= min_contended_acq`
- for `hot_epochs_needed` consecutive epochs.

Exit SSC if:

- `wait_ratio <= exit_threshold_pct`
- for `cool_epochs_needed` consecutive epochs.

Tasks without enough signal remain `CANDIDATE` or `NORMAL`.

### SSC width search policy
Implement online search using a normalized-throughput proxy.

Suggested proxy:

- `p = voting_lock_ns / voting_slice_ns`
- `work_cores = min(ssc_width, nr_lock_intensive_tasks)`
- `throughput_proxy = work_cores * (1 - p)`

Suggested v1 search:

- initialize `ssc_width = 1`,
- if proxy improves in two consecutive control rounds, try doubling,
- if proxy degrades in two consecutive rounds, step back,
- clear or decay voting after updates,
- re-enter search when workload behavior changes materially.

All search logic must live in user space.

---

## Topology Rules

SSC allocation must be topology-aware.

Selection preference:

1. same NUMA node,
2. same LLC domain,
3. contiguous CPU IDs when possible,
4. compact expansion from current SSC boundary,
5. compact shrink from the edge.

Topology discovery should happen in user space by reading sysfs.

---

## Required Data Structures

### Per-task scheduler context
Maintain a BPF-visible task context with fields equivalent to:

- `pid`
- `tgid`
- `cls`
- `epoch_id`
- `run_ns`
- `runnable_ns`
- `mig_count`
- `last_ssc_gen`
- `hot_epochs`
- `cool_epochs`
- `lock_domain_id`
- `hotness_score`

Do not store heavyweight policy-only state in BPF if it can stay in user space.

### Global config
Need at least:

- `epoch_ns`
- `control_period_ns`
- `enter_threshold_pct`
- `exit_threshold_pct`
- `min_contended_acq`
- `ssc_width`
- `ssc_gen`
- `max_ssc_width`
- `partial_mode`
- `hot_epochs_needed`
- `cool_epochs_needed`

---

## CLI and Runtime Configuration

The controller must expose CLI parameters for at least:

```text
--partial
--epoch-ms
--control-ms
--enter-pct
--exit-pct
--min-contended
--hot-epochs
--cool-epochs
--max-ssc
--target-cgroup
--cpu-list
--metrics-out
--enable-rseq-slice-ext
```

No threshold or timing constant may be hard-coded without also being configurable.

---

## Coding Standards

- Use `libbpf` style for BPF and user-space loader code.
- Keep comments technical and concrete.
- Prefer explicit names over abbreviations.
- Every exported function must have a short contract comment.
- Every shared struct must document ownership and update semantics.
- Avoid clever lock-free tricks unless they clearly reduce hot-path cost.
- Optimize only after building a measurable baseline.

---

## Phased Delivery Plan

### Phase 0
- repository skeleton,
- build system,
- minimal sched-ext scheduler,
- partial mode,
- one fixed SSC width,
- manual task classification.

### Phase 1
- per-thread mmapable slots,
- `lca_mutex` implementation,
- controller-side epoch aggregation,
- static classification from user lock data.

### Phase 2
- SSC width online search,
- topology-aware mask management,
- lazy migration generations,
- benchmark automation.

### Phase 3
- optional `lca_rwlock`,
- optional rseq slice extension,
- better lock-domain management,
- improved observability.

---

## Acceptance Criteria

The project is acceptable when all of the following are true:

1. a workload using `lca_mutex` can opt into `SCHED_EXT`,
2. per-thread user lock waiting data is visible in mmapable slots,
3. the controller can classify tasks without using kernel lock instrumentation,
4. SSC and normal CPU sets are separated in dispatch,
5. SSC width can change online,
6. tasks migrate lazily according to generation updates,
7. the system falls back safely if sched-ext exits,
8. benchmarks show the expected trend on lock-heavy user-space workloads.

---

## Benchmark Expectations

Every benchmark report should include:

- throughput,
- p50 / p95 / p99 latency,
- wait ratio,
- hold ratio,
- park ratio,
- SSC width over time,
- migration rate,
- SSC CPU utilization,
- normal CPU utilization.

At minimum compare:

- baseline scheduler,
- fixed SSC width,
- dynamic SSC search,
- dynamic SSC search + user lock instrumentation enabled.

---

## What the Agent Should Produce First

When starting implementation, produce in this order:

1. repository tree,
2. shared struct definitions,
3. BPF map declarations,
4. minimal sched-ext scheduler with fixed SSC,
5. mmapable user slot plumbing,
6. `lca_mutex` slow-path accounting,
7. controller epoch loop,
8. classification state machine,
9. SSC width search,
10. benchmarks and docs.

Do not jump straight to advanced optimizations before the fixed-SSC path works.

---

## Final Reminder

This project is a **user-space-lock-driven scheduler experiment**.

The scheduler should behave as if it were informed by lock contention, but the contention signal must come from **cooperative user-space instrumentation**, not from kernel lock tracing and not from hot-path uprobes.

