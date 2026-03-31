# Blocking-Aware MCS Exit Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let already-enqueued MCS waiters leave queue-spin safely when blocking conditions become active, following FlexGuard's sentinel-based handoff protocol.

**Architecture:** Keep the current Rust lock layout, but reinterpret `qnode.next` through atomic helper accessors so it can carry either a successor pointer or a parked sentinel. When blocking is active, waiters stop local MCS spinning, retire their queue node through a blocking-aware `mcs_exit`, then join the existing phase2/TAS path. Successor linking must wake parked predecessors to avoid orphaning queue nodes.

**Tech Stack:** Rust, atomics, futex syscalls, existing `cargo test` unit tests and source-contract tests.

---

## Chunk 1: Tests And Contracts

### Task 1: Lock in the sentinel protocol in tests

**Files:**
- Modify: `src/mcs_tas.rs`
- Test: `src/mcs_tas.rs`

- [ ] **Step 1: Write failing tests for successor linking and blocking-aware exit**
- [ ] **Step 2: Run `cargo test --lib mcs_tas::tests -- --nocapture` and confirm the new tests fail for the missing protocol**

## Chunk 2: Rust Lock Implementation

### Task 2: Add sentinel-aware qnode helpers

**Files:**
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Add atomic helpers for `qnode.next` as `AtomicUsize` plus sentinel encode/decode**
- [ ] **Step 2: Keep the BPF-visible qnode layout unchanged**

### Task 3: Implement FlexGuard-style blocking exit

**Files:**
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Replace plain predecessor linking with an exchange that detects parked predecessors**
- [ ] **Step 2: Split `mcs_exit` into normal and blocking-aware paths**
- [ ] **Step 3: Update the MCS wait loop so blocking can break queue-spin safely**
- [ ] **Step 4: Re-enter slow path only after the old queue node has retired**

## Chunk 3: Verification

### Task 4: Verify correctness and contracts

**Files:**
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Add/adjust source-contract tests for the sentinel-based protocol if the surface contract changes**
- [ ] **Step 2: Run `cargo test --lib -- --nocapture`**
- [ ] **Step 3: Run `cargo build --release`**
