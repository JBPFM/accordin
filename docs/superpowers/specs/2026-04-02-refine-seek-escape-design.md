# Refine Seek Escape Design

**Date:** 2026-04-02

**Status:** Proposed

## Context

The `accordin` controller still shows major stability outliers at the fixed `SSC_UNLOCK_GATE_THRESHOLD = 320000ULL` baseline even after fixing the helper-level stale-anchor bug in `ssc_set_active_count()`.

Fresh runtime evidence from `bench/mutexbench/results-tmp/debug_snapshots_envfix_20260402T0300Z/` shows that the dominant failure mode is not repeated threshold mis-tuning. Instead, the controller enters `SSC_SEARCH_REFINE` and then remains there while repeatedly observing:

- refinement intervals that have already collapsed to a single point
- refinement targets that do not change `ssc_active_count`
- continued bad benchmark rounds with degraded throughput and handoff latency

Representative evidence:

- benchmark outliers remain in `bench/mutexbench/results-tmp/debug_snapshots_envfix_20260402T0300Z/accordin/raw.csv`
- `dbg_refine_single_point` and `dbg_refine_noop_targets` climb rapidly in captured `.bss` snapshots
- `dbg_refine_entries` stays comparatively low, which suggests the controller is not repeatedly re-entering refine, but rather getting stuck there

## Problem Statement

When the controller is already in `SSC_SEARCH_REFINE`, the current refine path can compute a target equal to the current `ssc_active_count` after the refine interval has collapsed to a single point. In that state, the controller keeps rotating vote windows and reevaluating refine without making forward progress.

This creates a stable no-op loop:

1. controller is in `REFINE`
2. bounds collapse to a single point
3. `ssc_next_refine_target()` returns the current width
4. no width change happens
5. controller stays in `REFINE`
6. the same condition repeats for many windows

The intended search process has effectively stalled.

## Goal

Break the refine no-op loop deterministically.

When `REFINE` has collapsed to a single point and the computed refine target is the current width, the controller should stop refining and return to `SEEK` so it can resume useful search behavior.

## Non-Goals

This change does **not**:

- retune the 320k threshold
- redesign the overall seek/refine search strategy
- remove refine mode entirely
- change the unlock-based score definition
- remove the debug counters added for investigation

## Proposed Behavior

In the `SSC_SEARCH_REFINE` branch of `src/bpf/main.bpf.c`, treat the following condition as an explicit escape case:

- `ssc_refine_low == ssc_refine_high`
- `ssc_next_refine_target() == ssc_active_count`

When that condition is true, the controller will:

1. leave `SSC_SEARCH_REFINE`
2. switch `ssc_search_phase` back to `SSC_SEARCH_SEEK`
3. reset refine bounds to the current active width
4. refresh score anchors and hysteresis state so the next window is evaluated from a clean baseline rather than immediately re-triggering the same stale shrink path

The controller will **not** stay in refine and will **not** continue spinning on a no-op refine target.

## State Handling Requirements

The escape path must preserve these invariants:

- `ssc_active_count` stays unchanged during the escape itself
- `ssc_search_phase` becomes `SSC_SEARCH_SEEK`
- `ssc_refine_low` / `ssc_refine_high` are synchronized to the current active width
- `ssc_vote_last_score` / `ssc_vote_last_effective_score` are refreshed through the same resize-anchor machinery already used for width transitions
- grow/shrink streak counters are cleared before the next search decision

The intent is to resume search from a coherent baseline, not to keep any partially-collapsed refine state alive.

## Implementation Shape

### BPF controller

Modify the refine branch in `src/bpf/main.bpf.c` so that after updating refine bounds and computing `next_target`, it distinguishes between two cases:

- **normal refine step:** `next_target != ssc_active_count`
  - keep current behavior and call `ssc_set_active_count(next_target, ssc_best_score)`
- **refine escape:** single-point refine with `next_target == ssc_active_count`
  - switch back to `SSC_SEARCH_SEEK`
  - reset refine bounds around current width
  - refresh anchors/counters using the existing helper path

### Helper reuse

Do not introduce a broad new state-management abstraction. Reuse the existing helpers where possible:

- `reset_ssc_refine_bounds()` for bounds cleanup
- `ssc_note_resize()` or `ssc_set_active_count()`-based anchor refresh logic for score/counter reset semantics

If a tiny helper is needed for the seek escape, it should be specific to this controller state transition rather than a generic framework abstraction.

### Debug counters

Keep the new debug counters in place for post-fix verification.

After the fix, `dbg_refine_single_point` and `dbg_refine_noop_targets` should stop growing unboundedly within a run. They may still increment transiently before the controller escapes, but they should no longer indicate a long-lived stall.

## Testing Strategy

Use the existing source-shape test style in `src/lib.rs`.

Add a focused failing test first that locks in the new behavior:

- when refine is single-point and `next_target == ssc_active_count`, the controller exits to `SSC_SEARCH_SEEK`
- the escape path refreshes anchors instead of leaving stale refine state active

Keep the test narrow. Do not broaden the test suite into general search-policy assertions.

## Verification Plan

After implementation, verify with:

1. `cargo test --lib`
2. `rtk cargo build --release`
3. a fresh fixed-320k long benchmark run
4. a debug-counter benchmark run with `ACCORDIN_DEBUG_COUNTERS=1`

Success criteria:

- source-shape tests pass
- release build passes
- bad rounds become less frequent or less severe
- runtime snapshots no longer show the controller spending most of a run in a single-point refine/no-op loop

## Risks and Trade-Offs

### Risk: more frequent reseek

Returning to `SEEK` may cause the controller to search more often than before.

Why this is acceptable:
- the current behavior is a stalled loop, which is worse than a fresh search
- reseek is a deterministic recovery path, not an unbounded new heuristic

### Risk: premature escape from a legitimate refine endpoint

A single-point refine state could sometimes represent a valid local optimum.

Why this is acceptable:
- the captured evidence shows the controller is not merely converging, but looping without progress
- if the current width is still good, `SEEK` can rediscover or retain it through normal score evaluation

## Out of Scope Follow-Ups

If stability remains poor after this change, the next investigation should focus on whether refine should have an explicit dwell limit or whether seek/refine transitions need stronger score hysteresis. Those are separate changes and are not part of this design.