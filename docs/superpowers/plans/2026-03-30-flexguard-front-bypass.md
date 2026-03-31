# FlexGuard Front-Bypass Narrowing Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Narrow FlexGuard's bypass trigger so only preempted lock holders or preempted front-runners force fallback behavior, instead of any preempted critical-state thread globally degrading MCS.

**Architecture:** Keep userspace qnode state shared with the BPF program, but split the protocol into per-thread preemption flags plus holder-only global blocking and per-lock bypass signaling. Replace the broad `HANDOFF` marker with a narrower `FRONT` marker and let Rust decide when a preempted predecessor should collapse the local MCS queue.

**Tech Stack:** Rust, libbpf-rs, eBPF tracepoint program, futex syscalls, pthread interposition

---

### Task 1: Lock In The Narrower Protocol Contract

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add source assertions that the protocol now exposes `FLEXGUARD_CRITICAL_STATE_FRONT`, `preempted_flags`, and a per-lock bypass field instead of the broad handoff/global-blocking contract.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test narrow_front_bypass_contract --lib -- --nocapture`
Expected: FAIL because the current source still uses `HANDOFF` and `num_preempted_cs`.

- [ ] **Step 3: Write minimal implementation**

Update the source-level tests to describe the FRONT-based protocol, per-thread preemption flags, and per-lock bypass state.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test narrow_front_bypass_contract --lib -- --nocapture`
Expected: PASS

### Task 2: Implement The FRONT-Based Protocol

**Files:**
- Modify: `src/mcs_tas.rs`
- Modify: `src/bpf/flexguard_bpf.h`
- Modify: `src/bpf/flexguard_userspace_state.bpf.c`

- [ ] **Step 1: Write the failing test**

Add unit and source tests that require:
- `FRONT` replaces the broad handoff marker
- BPF exports per-thread `preempted_flags` and holder-only global counts
- the Rust lock uses a per-lock bypass flag and checks predecessor preemption after linking `pred->next`

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test flexguard_runtime --lib -- --nocapture`
Expected: FAIL because the runtime still uses `HANDOFF` and global `blocking_condition()`.

- [ ] **Step 3: Write minimal implementation**

Change the Rust slow path, BPF state tracking, and shared header definitions to implement the narrower bypass protocol.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test flexguard_runtime --lib -- --nocapture`
Expected: PASS

### Task 3: Verify The Updated Contract

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/mcs_tas.rs`
- Modify: `src/bpf/flexguard_bpf.h`
- Modify: `src/bpf/flexguard_userspace_state.bpf.c`

- [ ] **Step 1: Run formatting**

Run: `cargo fmt`
Expected: success

- [ ] **Step 2: Run focused tests**

Run: `cargo test --lib -- --nocapture`
Expected: PASS

- [ ] **Step 3: Run compile-only verification**

Run: `cargo test --no-run`
Expected: PASS
