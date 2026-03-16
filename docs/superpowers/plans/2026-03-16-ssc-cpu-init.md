# SSC CPU Init Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Initialize the first NUMA socket CPU list in userspace and expose the full list to BPF so later scheduler logic can fetch CPU IDs by index.

**Architecture:** Keep topology discovery in `src/lib.rs`, extending the existing NUMA parsing to retain the first socket's CPU IDs. Publish the full first-socket SSC CPU list through BPF global data so BPF code can read CPU IDs by index without implementing the activation logic yet.

**Tech Stack:** Rust, libbpf-rs generated skeleton data maps, sched_ext BPF C headers, cargo test

---

## Chunk 1: Topology Helpers And Tests

### Task 1: Add failing tests for NUMA CPU list derivation

**Files:**
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add unit tests for:
- selecting the first socket CPU list by NUMA node id
- preserving the first socket CPU list even when a different node is dominant

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ssc_`
Expected: FAIL because helper functions / fields do not exist yet

- [ ] **Step 3: Write minimal implementation**

Extend `NumaTopology` and add helper functions to:
- retain first socket CPUs during topology parsing
- provide the full first socket CPU list to BPF publication code

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ssc_`
Expected: PASS

## Chunk 2: BPF Data Exposure

### Task 2: Add failing tests for BPF-visible SSC CPU globals

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/bpf/maps.bpf.h`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add source-level tests asserting the BPF globals for `ssc_cpu_count` and `ssc_cpu_list` are present.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib bpf_headers_define_ssc_cpu_globals`
Expected: FAIL because the globals are not declared yet

- [ ] **Step 3: Write minimal implementation**

Declare fixed-size SSC CPU globals in `src/bpf/maps.bpf.h`, add a BPF helper for indexed lookup, and populate the globals from `lib.rs` during skeleton setup.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib bpf_headers_define_ssc_cpu_globals`
Expected: PASS

## Chunk 3: End-To-End Verification

### Task 3: Add SSC-core membership helpers

**Files:**
- Modify: `src/bpf/admission.bpf.h`
- Modify: `src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add source-level tests asserting:
- BPF globals expose `ssc_active_count` and `ssc_cpu_rank`
- `is_cpu_ssc_core` uses `ssc_cpu_rank[cpu]` and compares against `ssc_active_count`
- `is_task_on_ssc_core` is present

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib ssc_helper`
Expected: FAIL because the helpers are not declared yet

- [ ] **Step 3: Write minimal implementation**

In `src/bpf/maps.bpf.h` and `src/lib.rs`:
- add `ssc_active_count = 2`
- add `ssc_cpu_rank[MAX_CPUS]`
- populate the rank table during topology publication alongside `ssc_cpu_list`

In `src/bpf/admission.bpf.h`:
- change `is_cpu_ssc_core(__s32 cpu)` to indexed rank lookup plus `rank < ssc_active_count`
- keep `is_task_on_ssc_core(struct task_struct *p)` as a thin wrapper over `scx_bpf_task_cpu(p)`

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib ssc_helper`
Expected: PASS

## Chunk 4: End-To-End Verification

### Task 4: Verify scoped behavior

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/bpf/maps.bpf.h`

- [ ] **Step 1: Run focused library tests**

Run: `cargo test --lib`
Expected: PASS

- [ ] **Step 2: Run a build to refresh the generated skeleton**

Run: `cargo build`
Expected: PASS
