# Simple FlexGuard SCX Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a minimal sched_ext scheduler to `src/bpf/flexguard_userspace_state.bpf.c` while keeping the existing FlexGuard `sched_switch` tracking program and single-skeleton Rust loader flow.

**Architecture:** Keep one BPF source file and one generated skeleton. Extend the existing tracepoint-focused BPF source with a minimal `sched_ext` `struct_ops` implementation that uses default CPU selection and the builtin global/local DSQs, without restoring the older SSC/admission logic or adding new per-task state.

**Tech Stack:** Rust, libbpf-rs, scx_cargo, sched_ext BPF C, source-level tests via `cargo test`

---

## Chunk 1: Source Contract

### Task 1: Lock in the minimal combined BPF contract

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add a source-level test that asserts `src/bpf/flexguard_userspace_state.bpf.c` contains:
- `#include <scx/common.bpf.h>`
- the existing `SEC("tp_btf/sched_switch")` program
- `BPF_STRUCT_OPS(lb_simple_select_cpu`
- `SCX_OPS_DEFINE(lb_simple_ops`
- `.name = "lb_simple"`

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test simple_scx_scheduler_contract --lib -- --nocapture`
Expected: FAIL because the current BPF source only contains the tracepoint program.

- [ ] **Step 3: Write minimal implementation**

Extend `src/bpf/flexguard_userspace_state.bpf.c` with the minimum `sched_ext` callbacks:
- `lb_simple_select_cpu`
- `lb_simple_enqueue`
- `lb_simple_dispatch`
- `lb_simple_init`
- `lb_simple_exit`
- `SCX_OPS_DEFINE(lb_simple_ops, ...)`

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test simple_scx_scheduler_contract --lib -- --nocapture`
Expected: PASS

## Chunk 2: Build Verification

### Task 2: Verify the combined skeleton still builds

**Files:**
- Modify: `src/bpf/flexguard_userspace_state.bpf.c`
- Modify: `src/lib.rs`

- [ ] **Step 1: Run formatting**

Run: `cargo fmt`
Expected: success

- [ ] **Step 2: Run focused library tests**

Run: `cargo test --lib -- --nocapture`
Expected: PASS

- [ ] **Step 3: Run compile-only verification**

Run: `cargo test --no-run`
Expected: PASS
