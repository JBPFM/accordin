# MCS Preempt Yield Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `src/mcs_tas.rs` mirror FlexGuard's factored MCS exit path and let the head waiter abandon the MCS slow path, clear timeslice extension state, and yield when preemption is detected.

**Architecture:** Keep the existing TAS fast path and current wait-time accounting. Add a small runtime timeslice-extension helper module, factor queue-release into a dedicated `mcs_exit()` helper, and make the slow path retry after a preemption-triggered MCS exit/yield event.

**Tech Stack:** Rust, `libc`, pthread interposition, existing TSC-based wait accounting.

---

### Task 1: Lock-Path Tests

**Files:**
- Modify: `src/mcs_tas.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing tests**

Add unit tests that cover:
- `mcs_exit()` clearing `tail` when there is no successor.
- `mcs_exit()` waking the successor when it exists.
- Preemption-gap detection tripping only on large observed spin gaps.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test mcs_exit --lib`
Expected: FAIL because `mcs_exit` and the new preemption helpers do not exist yet.

### Task 2: Timeslice Extension Helpers

**Files:**
- Create: `src/timeslice_extension.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add a runtime-detected timeslice-extension helper**

Expose helpers to request extension during the contended wait, clear the request, and yield in a way that is testable.

- [ ] **Step 2: Add test hooks**

Expose `#[cfg(test)]` hooks to force request/grant behavior so the lock tests can assert the exit/yield path deterministically.

### Task 3: Slow-Path Implementation

**Files:**
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Factor queue-release into `mcs_exit()`**

Move the existing “find successor or repair tail, then wake successor” logic into one helper used by both normal acquisition handoff and the new abort path.

- [ ] **Step 2: Add preemption-aware retry loop**

Request timeslice extension before the TAS spin at the MCS head. If the spin observes a preemption-sized gap, call `mcs_exit()`, clear extension state, yield, and restart the slow path.

- [ ] **Step 3: Keep wait accounting correct**

Preserve `wait_start_ns`, `wait_end_ns`, and `wait_ns_total` semantics across retries and successful acquisition.

### Task 4: Verification

**Files:**
- Modify: `src/lib.rs` (only if source-layout assertions need updates)

- [ ] **Step 1: Run targeted unit tests**

Run: `cargo test mcs_tas --lib`
Expected: PASS

- [ ] **Step 2: Run broader library tests**

Run: `cargo test --lib`
Expected: PASS
