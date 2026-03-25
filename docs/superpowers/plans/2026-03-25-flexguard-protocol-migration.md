# FlexGuard Protocol Migration Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the old `lock_state` sched_ext protocol with the full FlexGuard userspace-state protocol across the Rust lock, mutex hooks, BPF program, and loader path.

**Architecture:** Keep the existing LD_PRELOAD mutex interposition surface, but switch `McsTasLockRaw` to a FlexGuard-style `lock_value + queue + futex` state machine backed by BPF-shared `qnodes` and `num_preempted_cs`. Replace the old sched_ext skeleton loading path with the new `sched_switch` BPF program and register tids into `nodes_map` by thread index so BPF can read `qnodes[*].cs_counter`.

**Tech Stack:** Rust, libbpf-rs, scx_cargo build helpers, eBPF tracepoint program, pthread interposition, futex syscalls

---

## Chunk 1: Test And Build Contract

### Task 1: Lock in the new source-level contract

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add tests that assert:
- `build.rs` targets `src/bpf/flexguard_userspace_state.bpf.c`
- `src/mutex_hook.rs` no longer references `thread_ctx_addr_map`
- `src/mcs_tas.rs` exposes `FLEXGUARD_CRITICAL_STATE_HELD/HANDOFF` usage

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test protocol_sources --lib -- --nocapture`
Expected: FAIL because the current sources still reference the old protocol or missing files.

- [ ] **Step 3: Write minimal implementation**

Update source tests to match the FlexGuard protocol surface and keep them focused on file contents rather than runtime BPF behavior.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test protocol_sources --lib -- --nocapture`
Expected: PASS

## Chunk 2: Shared BPF Assets

### Task 2: Restore the headers required by the new BPF program

**Files:**
- Create: `src/bpf/intf.h`
- Create: `src/bpf/platform_defs.h`
- Create: `src/bpf/flexguard_bpf.h`
- Create: `src/bpf/bpf_fixes.bpf.h`
- Modify: `build.rs`

- [ ] **Step 1: Write the failing test**

Add source tests that assert the new headers exist and `build.rs` points the skeleton builder at `src/bpf/flexguard_userspace_state.bpf.c`.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test bpf_source_files_follow_flexguard_protocol --lib -- --nocapture`
Expected: FAIL because the headers are missing and `build.rs` still points at `src/bpf/main.bpf.c`.

- [ ] **Step 3: Write minimal implementation**

Create the header files with the constants and helpers needed by both bindgen and the BPF program. Update `build.rs` to generate the same skeleton module name from the new BPF source.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test bpf_source_files_follow_flexguard_protocol --lib -- --nocapture`
Expected: PASS

## Chunk 3: Runtime And Lock State Machine

### Task 3: Port the FlexGuard userspace-state lock into Rust

**Files:**
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Write the failing test**

Add unit tests for the new runtime helpers and source tests that assert:
- shared qnodes runtime initialization exists
- `mark_lock_holder`, `mark_handoff_thread`, and `clear_critical_state` are used
- unlock clears the critical state after releasing the lock

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test flexguard_runtime --lib -- --nocapture`
Expected: FAIL because `src/mcs_tas.rs` is still empty / missing the FlexGuard state machine.

- [ ] **Step 3: Write minimal implementation**

Implement:
- global runtime with shared `qnodes`, `num_preempted_cs`, and thread index allocation
- futex-backed `lock_value` slow path with MCS handoff
- FlexGuard userspace-state markers for `NONE`, `HELD`, and `HANDOFF`

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test flexguard_runtime --lib -- --nocapture`
Expected: PASS

## Chunk 4: Thread Registration And BPF Loader

### Task 4: Switch hook registration and loader logic to the new protocol

**Files:**
- Modify: `src/mutex_hook.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add tests that assert:
- thread registration updates `nodes_map` with thread indices, not user-space context pointers
- `lib.rs` extracts `nodes_map`, `num_preempted_cs`, and `qnodes` from the new skeleton
- the old sched_ext helpers/macros are no longer used

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test loader_uses_flexguard_bpf_runtime --lib -- --nocapture`
Expected: FAIL because the loader and hook path still target the old scheduler protocol.

- [ ] **Step 3: Write minimal implementation**

Change:
- `mutex_hook.rs` to register tids via thread index helper from `mcs_tas.rs`
- `lib.rs` to use generic libbpf skeleton open/load/attach flow for the `sched_switch` program
- BPF-disabled mode to fall back to local runtime allocation without a map

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test loader_uses_flexguard_bpf_runtime --lib -- --nocapture`
Expected: PASS

## Chunk 5: Full Verification

### Task 5: Compile and run the focused test suite

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/mcs_tas.rs`
- Modify: `src/mutex_hook.rs`
- Modify: `build.rs`
- Create: `src/bpf/intf.h`
- Create: `src/bpf/platform_defs.h`
- Create: `src/bpf/flexguard_bpf.h`
- Create: `src/bpf/bpf_fixes.bpf.h`

- [ ] **Step 1: Run formatting**

Run: `cargo fmt`
Expected: success

- [ ] **Step 2: Run focused library tests**

Run: `cargo test --lib -- --nocapture`
Expected: PASS

- [ ] **Step 3: Run compile-only verification for the crate**

Run: `cargo test --no-run`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add build.rs docs/superpowers/plans/2026-03-25-flexguard-protocol-migration.md src/lib.rs src/mcs_tas.rs src/mutex_hook.rs src/bpf/intf.h src/bpf/platform_defs.h src/bpf/flexguard_bpf.h src/bpf/bpf_fixes.bpf.h src/bpf/flexguard_userspace_state.bpf.c
git commit -m "feat: migrate lb_simple to flexguard userspace protocol"
```
