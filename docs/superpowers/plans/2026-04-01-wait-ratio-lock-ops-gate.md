# Wait-Ratio Lock-Ops Gate Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the interrupted SSC controller rewrite so migration is gated by the current SSC-core wait ratio while `ssc_active_count` search is driven by SSC-core lock-operation throughput.

**Architecture:** Keep the existing SSC vote window and seek/refine search skeleton, but replace the hybrid useful-run helpers with a simple wait-ratio gate and a lock-op-based score. When the SSC-core vote window wait ratio is above 10%, non-SSC tasks remain not admitted and get migrated into `SSC_DSQ`; when it drops to 10% or below, new migrations stop by restoring `admitted=1`.

**Tech Stack:** Rust source-shape tests, sched_ext BPF C headers/helpers, libbpf build/test flow via Cargo.

---

## Chunk 1: Lock Down the Intended Semantics

### Task 1: Confirm red tests describe the unfinished implementation

**Files:**
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Keep the existing source-shape tests focused on the intended end state**

The target assertions are:
- `simple_tick` uses `compute_ssc_wait_ratio()`
- `simple_tick` checks `ssc_wait_ratio > SSC_WAIT_RATIO_THRESHOLD`
- `simple_tick` restores `tc->admitted = 1` below threshold
- scoring uses `ssc_vote_sum_lock_count`
- no `estimate_*useful_run*`
- no `detect_ssc_workload_shift`

- [ ] **Step 2: Run test to verify it fails against the current half-switched code**

Run: `rtk cargo test --lib`
Expected: FAIL in the wait-ratio / lock-op source-shape tests

## Chunk 2: Replace Hybrid Useful-Run Helpers

### Task 2: Convert BPF stats helpers to wait-ratio gate plus lock-op score

**Files:**
- Modify: `src/bpf/stats.bpf.h`
- Modify: `src/bpf/maps.bpf.h`
- Modify: `src/bpf/intf.h`

- [ ] **Step 1: Write the minimal helper set**

Implement:
- `#define SSC_WAIT_RATIO_THRESHOLD 100ULL`
- `compute_ssc_wait_ratio(void)` using `ssc_vote_sum_wait * 1000 / ssc_vote_sum_run`
- `compute_ssc_vote_score(__u32 active_count)` using `ssc_vote_sum_lock_count * active_count * SSC_SCORE_SCALE / ssc_vote_publish_count`

Remove:
- `estimate_hold_total_ns`
- `estimate_useful_run_ns`
- `estimate_task_useful_run_ns`
- `estimate_ssc_useful_run_ns`
- `compute_ssc_useful_fraction`
- `detect_ssc_workload_shift`
- obsolete EWMA / shift-detection globals

- [ ] **Step 2: Preserve only the vote-window fields still needed**

Keep the SSC vote window accounting for:
- `run`
- `wait`
- `hold`
- `hold_sample_count`
- `lock_count`

But ensure only `wait` and `lock_count` feed control logic.

- [ ] **Step 3: Run test to verify helper/header tests pass**

Run: `rtk cargo test --lib hold_stats_score_path_uses_lock_ops wait_ratio_gate_headers_define_state`
Expected: PASS

## Chunk 3: Finish the Tick Control Path

### Task 3: Switch `simple_tick` from useful-run gating to wait-ratio gating

**Files:**
- Modify: `src/bpf/main.bpf.c`
- Test: `src/lib.rs`

- [ ] **Step 1: Keep the existing quorum/search skeleton**

Do not change:
- `publish_ssc_core_vote(tc, p, now);`
- quorum rule `ssc_vote_publish_count * 2 > ssc_active_count`
- seek/refine flow
- doubling after two consecutive grows
- refine after two consecutive shrinks

- [ ] **Step 2: Replace the non-SSC migration condition**

Implement:
- compute current SSC wait ratio once per tick
- if `ssc_wait_ratio > SSC_WAIT_RATIO_THRESHOLD`, set `tc->admitted = 0` and `p->scx.slice = 0`
- else set `tc->admitted = 1`

Remove:
- `estimate_task_useful_run_ns(tc)`
- workload-shift reset path

- [ ] **Step 3: Run targeted tests**

Run: `rtk cargo test --lib hold_stats_tick_path_uses_ssc_wait_ratio_gate quorum_lock_op_search_keeps_refine_without_shift_detection`
Expected: PASS

## Chunk 4: Final Verification

### Task 4: Verify the finished implementation against the full library suite

**Files:**
- Modify: `src/bpf/intf.h`
- Modify: `src/bpf/main.bpf.c`
- Modify: `src/bpf/maps.bpf.h`
- Modify: `src/bpf/stats.bpf.h`
- Modify: `src/lib.rs`

- [ ] **Step 1: Run the full library test suite**

Run: `rtk cargo test --lib`
Expected: PASS

- [ ] **Step 2: Inspect the final diff**

Run: `rtk git diff -- src/bpf/intf.h src/bpf/main.bpf.c src/bpf/maps.bpf.h src/bpf/stats.bpf.h src/lib.rs`
Expected: Only the intended wait-ratio gate / lock-op score completion remains

