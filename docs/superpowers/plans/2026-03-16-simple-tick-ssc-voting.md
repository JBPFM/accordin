# Simple Tick SSC Voting Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add quorum-based SSC-core publishing in `simple_tick` so majority votes can adjust `ssc_active_count` by doubling or halving from per-window run/wait ratios.

**Architecture:** Keep the hot-path decision inside BPF by introducing epoch-scoped SSC voting state and per-core publication slots that deduplicate each SSC core within a window. `simple_tick` will publish only once per SSC core per epoch, trigger a decision as soon as votes exceed half of `ssc_active_count`, and use a timeout window only to rotate stale partial votes.

**Tech Stack:** sched_ext BPF C, BPF global data and maps, Rust source-level tests, cargo test

---

## Chunk 1: Test-First Coverage

### Task 1: Add failing source-level tests for SSC voting state

**Files:**
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add source-level tests asserting BPF state exists for:
- per-window SSC vote tracking
- per-SSC-core publish slots
- score baselines and consecutive up/down counters

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ssc_vote`
Expected: FAIL because the vote state and helpers do not exist yet

- [ ] **Step 3: Write minimal implementation**

Add the BPF declarations needed for SSC vote state and expose helper names that `simple_tick` can call.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ssc_vote`
Expected: PASS

### Task 2: Add failing source-level tests for majority-quorum tick logic

**Files:**
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add source-level tests asserting `lb_simple_tick`:
- publishes SSC-core data into vote state
- checks majority quorum with `publish_count * 2 > ssc_active_count`
- doubles and halves `ssc_active_count` through consecutive comparisons

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib tick_quorum`
Expected: FAIL because `simple_tick` does not contain the new voting logic yet

- [ ] **Step 3: Write minimal implementation**

Implement the missing BPF logic so the new source-level tests pass.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib tick_quorum`
Expected: PASS

## Chunk 2: BPF Voting And Verification

### Task 3: Implement epoch-scoped SSC voting helpers

**Files:**
- Modify: `src/bpf/intf.h`
- Modify: `src/bpf/maps.bpf.h`
- Modify: `src/bpf/stats.bpf.h`
- Modify: `src/bpf/main.bpf.c`
- Test: `src/lib.rs`

- [ ] **Step 1: Add BPF vote-state declarations**

Introduce:
- a per-SSC-core publication slot layout
- epoch/window globals
- score history and consecutive comparison counters

- [ ] **Step 2: Implement tick-side publish and timeout rotation**

Add helpers so `simple_tick` can:
- rotate stale epochs after `window_ns`
- publish one vote per SSC core per epoch
- aggregate `sum_run`, `sum_wait`, and `publish_count`

- [ ] **Step 3: Implement majority decision logic**

Update `simple_tick` so majority quorum triggers:
- score computation from published run/wait totals
- consecutive-up tracking for doubling
- consecutive-below-effective tracking for halving
- `ssc_active_count` clamping to `[2, ssc_cpu_count]`

- [ ] **Step 4: Run focused verification**

Run: `cargo test --lib ssc_vote tick_quorum`
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
