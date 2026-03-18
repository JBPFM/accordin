# Workload Shift Detection Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add workload-shift detection and warm-started SSC admission so the scheduler can react to sustained lock-pressure changes without spending most short runs ramping up from `ssc_active_count=2`.

**Architecture:** Extend the existing BPF voting state with a wait-ratio EWMA baseline, resize holdoff, and a simple search phase enum. Reuse the quorum point in `lb_simple_tick()` to compute shift signals from per-window run/wait aggregates, switch between fast seek and bounded refinement, and seed the initial SSC width from topology publication so short mutexbench runs reach a useful core count faster.

**Tech Stack:** sched_ext BPF C, BPF global data/maps, Rust source-level tests, cargo test

---

## Review Status

This plan no longer matches the exact initial implementation that landed. The current code uses:

- wait-ratio-only shift detection
- shift detection only while in `SSC_SEARCH_REFINE`
- fallback to the last best known SSC width on confirmed shift
- a warm-start initial `ssc_active_count` in userspace topology publication

External review after the initial implementation found four correctness bugs that still need follow-up before the search/refinement controller can be considered complete:

- seek-mode growth can overwrite the historical best point with a locally improving but globally worse point
- zero-width refine intervals can leave the controller stuck in `SSC_SEARCH_REFINE`
- the shift-detection EWMA baseline moves during streak accumulation, which can prevent sustained step changes from ever confirming
- saturated grow attempts at `ssc_cpu_count` do not record an effective resize, leaving stale hysteresis state behind

## Chunk 1: Test-First Coverage

### Task 1: Add failing source-level tests for shift-detection state

**Files:**
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add source-level tests asserting BPF headers expose:
- workload-shift EWMA state
- resize holdoff and shift streak counters
- search phase and refinement bounds

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib shift_detection_headers_define_state`
Expected: FAIL because the new state does not exist yet

- [ ] **Step 3: Write minimal implementation**

Add the required globals and enum definitions to the BPF headers.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib shift_detection_headers_define_state`
Expected: PASS

### Task 2: Add failing source-level tests for quorum-side shift handling

**Files:**
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add source-level tests asserting `lb_simple_tick()`:
- calls a workload-shift helper after quorum forms
- resets to fast search on confirmed shift
- applies refinement bounds instead of unconditional doubling/halving

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib quorum_shift_detection_resets_search_phase`
Expected: FAIL because `simple_tick` does not contain the new shift-detection path yet

- [ ] **Step 3: Write minimal implementation**

Implement the missing helper calls and phase transitions in the BPF vote controller.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib quorum_shift_detection_resets_search_phase`
Expected: PASS

## Chunk 2: BPF Shift Detection And Verification

### Task 3: Implement workload-shift detection helpers

**Files:**
- Modify: `src/bpf/intf.h`
- Modify: `src/bpf/maps.bpf.h`
- Modify: `src/bpf/stats.bpf.h`
- Modify: `src/bpf/main.bpf.c`
- Test: `src/lib.rs`

- [ ] **Step 1: Add BPF shift-detection declarations**

Introduce:
- search phase enum/constants
- an EWMA baseline for wait ratio
- resize holdoff, shift streak, and refinement bounds

- [ ] **Step 2: Implement quorum-side helpers**

Add helpers so `lb_simple_tick()` can:
- normalize per-window wait ratio
- detect confirmed workload shifts
- enter seek/refine tracking modes

- [ ] **Step 3: Implement phase-aware active-count changes**

Update `lb_simple_tick()` so quorum decisions:
- keep multiplicative search in seek mode
- switch to bounded refinement after the first clear regression
- reset back to seek mode when shift detection fires

- [ ] **Step 4: Run focused verification**

Run: `cargo test --lib shift_detection_headers_define_state quorum_shift_detection_resets_search_phase`
Expected: PASS

### Task 4: Run broader verification

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/bpf/intf.h`
- Modify: `src/bpf/maps.bpf.h`
- Modify: `src/bpf/stats.bpf.h`
- Modify: `src/bpf/main.bpf.c`

- [ ] **Step 1: Run library tests**

Run: `cargo test --lib`
Expected: PASS

- [ ] **Step 2: Run a build**

Run: `cargo build`
Expected: PASS

## Chunk 3: Review-Driven Correctness Fixes

### Task 5: Preserve the true best search point

**Files:**
- Modify: `src/bpf/main.bpf.c`
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add a source-level test asserting seek-mode growth only updates `ssc_best_count` / `ssc_best_score` when the current score exceeds the historical best, not merely when a local grow streak fires.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib`
Expected: FAIL because seek-mode growth can still overwrite the historical best point

- [ ] **Step 3: Write minimal implementation**

Update the quorum-side growth path so:
- historical best state only changes on true score improvement
- grow streak bookkeeping remains separate from the best-point anchor used by refine/shift fallback

- [ ] **Step 4: Run focused verification**

Run: `cargo test --lib`
Expected: PASS

### Task 6: Prevent zero-width refine stalls

**Files:**
- Modify: `src/bpf/stats.bpf.h`
- Modify: `src/bpf/main.bpf.c`
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add a source-level test asserting that when refine bounds collapse to the current SSC width, the controller still falls back to a smaller count instead of remaining in `SSC_SEARCH_REFINE` with a no-op target.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib`
Expected: FAIL because zero-width refine intervals can currently keep the controller pinned at the same width

- [ ] **Step 3: Write minimal implementation**

Update refine-target selection so:
- zero-width intervals force a smaller fallback candidate
- `SSC_SEARCH_REFINE` cannot persist without an actual `ssc_active_count` change

- [ ] **Step 4: Run focused verification**

Run: `cargo test --lib`
Expected: PASS

### Task 7: Hold the shift baseline fixed during confirmation

**Files:**
- Modify: `src/bpf/stats.bpf.h`
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add a source-level test asserting that `detect_ssc_workload_shift()` keeps a stable reference baseline while `ssc_shift_streak` is accumulating, rather than updating the EWMA on each non-confirmed window.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib`
Expected: FAIL because the current EWMA chases step changes during confirmation

- [ ] **Step 3: Write minimal implementation**

Split the shift baseline from the adaptive EWMA so:
- confirmation compares against a fixed baseline snapshot
- the adaptive baseline is only refreshed after confirmation succeeds or the streak is abandoned

- [ ] **Step 4: Run focused verification**

Run: `cargo test --lib`
Expected: PASS

### Task 8: Treat saturated grows as effective resize events

**Files:**
- Modify: `src/bpf/stats.bpf.h`
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add a source-level test asserting that growth attempts clamped at `ssc_cpu_count` still refresh `ssc_vote_last_effective_score` and resize hysteresis state.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib`
Expected: FAIL because clamped grows can currently leave stale score/counter state behind

- [ ] **Step 3: Write minimal implementation**

Update the resize helper so saturated grows:
- record the effective resize attempt
- refresh the score anchor and grow/shrink counters even when the clamped width equals the current one

- [ ] **Step 4: Run focused verification**

Run: `cargo test --lib`
Expected: PASS
