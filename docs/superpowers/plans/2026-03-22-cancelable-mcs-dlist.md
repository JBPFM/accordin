# Cancelable MCS Doubly-Linked Queue Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rework `src/mcs_tas.rs` so the MCS slow path supports kernel-style cancellation through a doubly-linked queue and can trigger scheduling checks while waiting for either the predecessor handoff or the lock itself.

**Architecture:** Replace the current forward-only MCS wait node with a cancelable doubly-linked queue shape modeled after Linux optimistic spinning (`osq_lock`). Keep the TAS fast path and existing wait accounting, but split the slow path into explicit phases: enqueue, predecessor wait, head spin, cancel/unqueue, and successful acquisition. Timeslice helpers remain small state/action primitives; the lock code owns when to request extension, when to poll for reschedule conditions, and when to abort and retry.

**Tech Stack:** Rust, `std::sync::atomic`, thread-local per-thread MCS nodes, Linux/glibc rseq slice extension helpers.

---

### Task 1: Lock-Path Tests For Cancelable Queue State

**Files:**
- Modify: `src/mcs_tas.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write the failing tests**

Add unit tests in `src/mcs_tas.rs` for:
- the new queue node layout exposing backward links needed for cancellation,
- cancellation check stride behavior,
- unlinking a queued node from the middle or tail of the queue without corrupting neighbor links,
- preserving existing `mcs_exit()` handoff behavior for the success path.

Add or update source-level assertions in `src/lib.rs` so they expect:
- a cancelable queue helper instead of the old one-way wait loop shape,
- schedule checks in both predecessor-wait and head-spin paths,
- no `gap_ns`-style heuristic.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test mcs_tas --lib -- --nocapture`
Expected: FAIL because the doubly-linked cancellation helpers and new slow-path shape do not exist yet.

### Task 2: Queue Node And Cancellation Helpers

**Files:**
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Add backward-link queue state**

Extend `Node` to carry the minimum state needed for cancellation:
- `next`,
- `prev`,
- a wait/locked flag appropriate for predecessor handoff.

Keep the node thread-local and cache-aligned as today.

- [ ] **Step 2: Add focused queue helpers**

Introduce helpers with one responsibility each:
- initialize/reset the thread node before enqueue,
- link after the predecessor,
- wait for predecessor handoff,
- wait as head on `locked`,
- unlink/cancel from the queue using a Linux `osq_lock`-style stabilize-prev / stabilize-next / relink sequence,
- release the successor on successful acquisition or unlock.

Do not mix timeslice policy into these queue-manipulation helpers.

- [ ] **Step 3: Run targeted tests**

Run: `cargo test mcs_tas::tests::mcs_exit --lib -- --nocapture`
Expected: existing exit tests still PASS, while any new cancellation tests may still fail until the slow path uses the helpers.

### Task 3: Slow-Path Rewrite Around Explicit Phases

**Files:**
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Rewrite `lock()` around explicit phases**

Restructure the slow path as:
1. initialize node,
2. enqueue,
3. if not head, wait for predecessor handoff,
4. once head, spin on `locked`,
5. on success, unlink/handoff and finish wait accounting,
6. on cancellation, unqueue safely and retry from the slow-path top.

- [ ] **Step 2: Request timeslice extension at the correct boundary**

Move `timeslice_extension::on_mcs_spin_start()` to the point where the thread enters the cancellable optimistic-spin regime. Make sure the request lifetime is coherent across both waiting phases and retries.

- [ ] **Step 3: Check scheduling conditions in both waiting phases**

Use a shared polling helper in `src/mcs_tas.rs` that:
- rate-limits checks with the existing stride counter,
- consults `timeslice_extension` state to see whether the current grant/request state says scheduling should happen,
- is called from both predecessor-wait and head-spin loops.

On trigger:
- cancel/unqueue the current node,
- perform the appropriate timeslice clear/yield path,
- retry the slow path.

- [ ] **Step 4: Keep wait accounting and unlock semantics correct**

Preserve:
- `wait_start_ns`,
- `wait_end_ns`,
- `wait_ns_total`,
- `TIMESLICE_REQUESTED` ownership semantics for the eventual lock holder.

Do not regress the uncontended TAS fast path.

- [ ] **Step 5: Run lock-path tests**

Run: `cargo test mcs_tas --lib -- --nocapture`
Expected: PASS

### Task 4: Timeslice Helper Cleanup For Lock-Owned Policy

**Files:**
- Modify: `src/timeslice_extension.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Keep helper boundaries minimal**

Ensure `src/timeslice_extension.rs` only exposes primitives the lock code needs:
- request extension,
- inspect current request/grant state,
- clear request,
- yield an active granted slice.

Avoid embedding queue-stage policy into this module.

- [ ] **Step 2: Add/update source-level assertions**

Update the `src/lib.rs` string-based tests to reflect the new helper names and the split between:
- state inspection,
- clear,
- yield.

- [ ] **Step 3: Run focused tests**

Run: `cargo test timeslice_extension --lib -- --nocapture`
Expected: PASS

### Task 5: Final Verification

**Files:**
- Modify: `src/mcs_tas.rs`
- Modify: `src/timeslice_extension.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Run full library tests**

Run: `cargo test --lib`
Expected: PASS

- [ ] **Step 2: Run formatting checks on touched files**

Run: `rustfmt --check src/mcs_tas.rs src/timeslice_extension.rs src/lib.rs`
Expected: PASS

- [ ] **Step 3: Inspect final diff**

Run: `git diff -- src/mcs_tas.rs src/timeslice_extension.rs src/lib.rs`
Expected: diff only contains the cancelable queue refactor, helper boundary updates, and tests needed for this work.
