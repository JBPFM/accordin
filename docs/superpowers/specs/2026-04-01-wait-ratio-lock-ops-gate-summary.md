# Wait-Ratio Lock-Ops Gate Implementation Summary

**Date:** 2026-04-01

**Status:** Implemented locally, not yet committed

## Context

This change finishes the interrupted controller rewrite from the previous `handofftime` session.

The earlier draft had already moved the data path toward sampled outermost-lock statistics, but the control logic was left in a half-switched state:

- source-shape tests had been updated to expect wait-ratio gating and lock-op scoring
- BPF control code still depended on hybrid `useful_run` helpers and workload-shift detection

This implementation completes that switch.

## Final Behavior

### SSC Migration Gate

Migration into `SSC_DSQ` is now gated by the current SSC-core vote-window wait ratio.

- `simple_tick()` computes `ssc_wait_ratio` from the active SSC-core vote window
- the threshold is fixed at `10%`, represented as `SSC_WAIT_RATIO_THRESHOLD = 100ULL`
- when `ssc_wait_ratio > 10%`, non-SSC tracked tasks keep `tc->admitted = 0` and force a reschedule through `p->scx.slice = 0`
- when `ssc_wait_ratio <= 10%`, new migrations stop by restoring `tc->admitted = 1`

This gate is based only on the current active SSC cores. It does not introduce any global control-enable bit.

### Active-Count Search Signal

`ssc_active_count` search now optimizes toward lock-operation throughput instead of time-derived `useful_run`.

- `compute_ssc_vote_score(__u32 active_count)` now uses `ssc_vote_sum_lock_count`
- the score formula is:
  - `(ssc_vote_sum_lock_count * active_count * SSC_SCORE_SCALE) / ssc_vote_publish_count`
- quorum remains unchanged:
  - `ssc_vote_publish_count * 2 > ssc_active_count`
- the existing seek/refine skeleton remains unchanged:
  - two consecutive grows can double `ssc_active_count`
  - two consecutive regressions can enter bounded refine mode

### What No Longer Drives Control

The controller no longer uses hybrid time-based demand estimation in the main control path.

Removed from control logic:

- `estimate_hold_total_ns()`
- `estimate_useful_run_ns()`
- `estimate_task_useful_run_ns()`
- `estimate_ssc_useful_run_ns()`
- `compute_ssc_useful_fraction()`
- `detect_ssc_workload_shift()`

Also removed from BPF globals:

- `ssc_useful_fraction_ewma`
- `ssc_shift_streak`
- `ssc_resize_holdoff`
- `ssc_shift_baseline_valid`

## Data Path Kept Intact

The SSC vote window still aggregates the same raw counters from the per-CPU publishing path:

- `run`
- `wait`
- `hold`
- `hold_sample_count`
- `lock_count`

Only `wait` and `lock_count` now feed controller decisions.

The hold-related counters are still collected and published because they remain useful as observability data and may be reused by later controller experiments.

## Files Changed

### [src/bpf/main.bpf.c](/home/jz/Projects/lb_simple/.codex/worktree/handofftime/src/bpf/main.bpf.c)

- updated the file-level description to match the new controller behavior
- computes `ssc_wait_ratio` in `simple_tick()`
- gates SSC migration on `ssc_wait_ratio > SSC_WAIT_RATIO_THRESHOLD`
- restores `tc->admitted = 1` below threshold
- removes the workload-shift reset branch

### [src/bpf/stats.bpf.h](/home/jz/Projects/lb_simple/.codex/worktree/handofftime/src/bpf/stats.bpf.h)

- adds `SSC_WAIT_RATIO_THRESHOLD`
- adds `compute_ssc_wait_ratio(void)`
- rewrites `compute_ssc_vote_score(__u32)` to use `ssc_vote_sum_lock_count`
- removes the hybrid `useful_run` helper family
- removes workload-shift detection helpers
- keeps vote-window accumulation for run, wait, hold, hold samples, and lock count

### [src/bpf/maps.bpf.h](/home/jz/Projects/lb_simple/.codex/worktree/handofftime/src/bpf/maps.bpf.h)

- retains vote-window sums for:
  - `ssc_vote_sum_run`
  - `ssc_vote_sum_wait`
  - `ssc_vote_sum_hold_ns`
  - `ssc_vote_sum_hold_samples`
  - `ssc_vote_sum_lock_count`
- removes obsolete useful-fraction and shift-detection globals

### [src/bpf/intf.h](/home/jz/Projects/lb_simple/.codex/worktree/handofftime/src/bpf/intf.h)

- keeps the outermost-hold bookkeeping fields added earlier
- includes the `last_hold_ns`, `last_hold_sample_count`, and `last_lock_count` snapshots used by BPF window accounting

### [src/lib.rs](/home/jz/Projects/lb_simple/.codex/worktree/handofftime/src/lib.rs)

- updates source-shape tests to lock in the finished semantics:
  - wait-ratio gate exists
  - `admitted` is restored below threshold
  - score uses `ssc_vote_sum_lock_count`
  - `useful_run` helpers are gone
  - workload-shift detection is gone
  - refine mode still exists

## Validation Run

Fresh verification was run after finishing the implementation:

- `rtk cargo test --lib`
  - result: `26 passed`
- `rtk cargo build --release`
  - result: success, `1 crates compiled`

## Non-Goals of This Change

This change does not:

- alter the userspace outermost-hold bookkeeping design
- remove hold-related observability counters from the vote window
- introduce a global SSC enable/disable state
- add a new workload-shift detector
- change the quorum rule or the seek/refine search skeleton

## Working-Tree Notes

At the time this summary was written:

- the implementation changes were present in the working tree and uncommitted
- [results-tmp/](/home/jz/Projects/lb_simple/.codex/worktree/handofftime/results-tmp) remained untouched by this documentation change
- the matching implementation plan is [2026-04-01-wait-ratio-lock-ops-gate.md](/home/jz/Projects/lb_simple/.codex/worktree/handofftime/docs/superpowers/plans/2026-04-01-wait-ratio-lock-ops-gate.md)
