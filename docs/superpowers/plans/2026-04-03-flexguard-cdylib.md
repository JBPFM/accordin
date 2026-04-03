# FlexGuard cdylib Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a standalone `libflexguard.so` Rust `cdylib` that exports `pthread_mutex_*` / `pthread_cond_*`, reuses the existing `lb_simple` scheduler load path, and links `src/bpf/main.bpf.c` with `src/bpf/flexguard.bpf.c` into one BPF skeleton.

**Architecture:** Add a new Cargo workspace member under `crates/libflexguard` and reuse the existing root modules via `#[path = ...]` so the first implementation stays small. The new target gets its own `build.rs`, generated skeleton, target-local hook module, and dual-thread-registration path that writes both scheduler `thread_ctx_addr_map` entries and FlexGuard `nodes_map` entries.

**Tech Stack:** Rust `cdylib`, `scx_cargo::BpfBuilder`, `libbpf-rs`, sched_ext struct_ops, pthread `LD_PRELOAD` interposition, mutexbench smoke validation.

---

### Task 1: Add the new Cargo target and lock in the BPF build contract

**Files:**
- Modify: `Cargo.toml`
- Create: `crates/libflexguard/Cargo.toml`
- Create: `crates/libflexguard/build.rs`
- Create: `crates/libflexguard/src/bpf_skel.rs`
- Modify: `src/bpf/flexguard.bpf.c`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing source-shape test for the new target**

Add a test to `src/lib.rs` that checks the new target is wired the right way before implementing it:

```rust
#[test]
fn flexguard_cdylib_target_links_main_and_flexguard_bpf() {
    let cargo = include_str!("../Cargo.toml");
    let target_manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("crates/libflexguard/Cargo.toml"),
    )
    .unwrap_or_default();
    let build_rs = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("crates/libflexguard/build.rs"),
    )
    .unwrap_or_default();

    assert!(cargo.contains("crates/libflexguard"));
    assert!(target_manifest.contains("crate-type = [\"cdylib\"]"));
    assert!(build_rs.contains("enable_skel(\"../../src/bpf/main.bpf.c\", \"bpf\")"));
    assert!(build_rs.contains("add_source(\"../../src/bpf/flexguard.bpf.c\")"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib flexguard_cdylib_target_links_main_and_flexguard_bpf`
Expected: FAIL because `crates/libflexguard` and its `build.rs` do not exist yet.

- [ ] **Step 3: Add the new workspace member and target manifest**

Update the root workspace and create the new target manifest with the same runtime deps as the root crate:

```toml
# Cargo.toml
[workspace]
members = ["crates/libflexguard"]
exclude = ["bench/mutexbench_rust"]
```

```toml
# crates/libflexguard/Cargo.toml
[package]
name = "libflexguard"
version = "0.1.0"
edition = "2024"
license = "GPL-2.0-only"

[lib]
crate-type = ["cdylib"]

[dependencies]
anyhow = "1.0.65"
libbpf-rs = "=0.26.0-beta.1"
log = "0.4.17"
scx_utils = { version = "1.0.22", features = ["autopower"] }
simplelog = "0.12"
libc = "0.2.177"

[build-dependencies]
scx_cargo = { version = "1.0.22" }
```

- [ ] **Step 4: Add the target-local BPF build files**

Create the new build script and skeleton shim:

```rust
// crates/libflexguard/build.rs
fn main() {
    scx_cargo::BpfBuilder::new()
        .unwrap()
        .enable_intf("../../src/bpf/intf.h", "bpf_intf.rs")
        .enable_skel("../../src/bpf/main.bpf.c", "bpf")
        .add_source("../../src/bpf/flexguard.bpf.c")
        .build()
        .unwrap();
}
```

```rust
// crates/libflexguard/src/bpf_skel.rs
include!(concat!(env!("OUT_DIR"), "/bpf_skel.rs"));
```

Remove the duplicate license symbol from `src/bpf/flexguard.bpf.c` so the linked BPF object has only the `main.bpf.c` license section.

- [ ] **Step 5: Re-run the test to verify it passes**

Run: `cargo test --lib flexguard_cdylib_target_links_main_and_flexguard_bpf`
Expected: PASS.

### Task 2: Add the new target-local library skeleton and lock in dual registration behavior

**Files:**
- Create: `crates/libflexguard/src/lib.rs`
- Create: `crates/libflexguard/src/mutex_hook.rs`
- Modify: `src/lib.rs`
- Test: `crates/libflexguard/src/mutex_hook.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing dual-registration helper test**

Create unit tests in `crates/libflexguard/src/mutex_hook.rs` that prove the new hook layer updates both maps:

```rust
#[test]
fn register_thread_updates_scheduler_and_flexguard_maps() {
    let thread_ctx_map = RecordingMap::default();
    let nodes_map = RecordingMap::default();

    assert!(super::register_thread_with_maps(
        &thread_ctx_map,
        &nodes_map,
        7,
        0x1122_3344_5566_7788,
        13,
    ));

    assert_eq!(thread_ctx_map.calls().len(), 1);
    assert_eq!(nodes_map.calls().len(), 1);
}
```

Also add a source-shape test in `src/lib.rs` that checks the new target references both `thread_ctx_addr_map` and `nodes_map`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p libflexguard register_thread_updates_scheduler_and_flexguard_maps`
Expected: FAIL because the new hook module and helpers do not exist yet.

- [ ] **Step 3: Create the new target-local hook layer**

Reuse the structure of `src/mutex_hook.rs`, but bind it to `crate::flexguard::FlexguardLockRaw` and dual map registration:

```rust
pub fn set_thread_ctx_map(map: MapHandle) { ... }
pub fn set_nodes_map(map: MapHandle) { ... }

fn register_thread_with_maps<T, U>(
    thread_ctx_map: &T,
    nodes_map: &U,
    tid: u32,
    ctx_ptr: u64,
    thread_index: u32,
) -> bool
where
    T: ThreadCtxMapOps + ?Sized,
    U: ThreadCtxMapOps + ?Sized,
{
    thread_ctx_map.update_entry(&tid.to_ne_bytes(), &ctx_ptr.to_ne_bytes(), MapFlags::ANY)
        && nodes_map.update_entry(&tid.to_ne_bytes(), &thread_index.to_ne_bytes(), MapFlags::ANY)
}
```

Use `lock_stats::thread_ctx()` for the scheduler map and `flexguard::current_thread_index()` for the FlexGuard map.

- [ ] **Step 4: Add the new library entry point**

Create `crates/libflexguard/src/lib.rs` by reusing the current `src/lib.rs` scheduler init pattern, but import root modules by path:

```rust
mod bpf_skel;
pub use bpf_skel::*;
#[path = "../../../src/arch.rs"] mod arch;
pub mod bpf_intf { include!(concat!(env!("OUT_DIR"), "/bpf_intf.rs")); }
#[path = "../../../src/flexguard.rs"] mod flexguard;
#[path = "../../../src/lock_backend.rs"] mod lock_backend;
#[path = "../../../src/lock_stats.rs"] mod lock_stats;
mod mutex_hook;
```

Keep the constructor and scheduler state pattern from `src/lib.rs` so the new shared library loads on `LD_PRELOAD` exactly like `liblb_simple.so`.

- [ ] **Step 5: Re-run the tests to verify they pass**

Run:
- `cargo test -p libflexguard register_thread_updates_scheduler_and_flexguard_maps`
- `cargo test --lib flexguard_target_registers_scheduler_and_flexguard_maps`

Expected: PASS.

### Task 3: Wire runtime installation and attach the extra FlexGuard BPF program

**Files:**
- Modify: `crates/libflexguard/src/lib.rs`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing source-shape test for runtime install and extra attach**

Add a root source-shape test that checks the new target:

```rust
#[test]
fn flexguard_target_installs_runtime_and_attaches_probe() {
    let lib = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("crates/libflexguard/src/lib.rs"),
    )
    .unwrap_or_default();

    assert!(lib.contains("flexguard::install_bpf_runtime"));
    assert!(lib.contains("thread_ctx_addr_map"));
    assert!(lib.contains("nodes_map"));
    assert!(lib.contains("sched_switch_btf"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib flexguard_target_installs_runtime_and_attaches_probe`
Expected: FAIL because the new library does not yet wire the runtime or probe attachment.

- [ ] **Step 3: Implement the target-specific scheduler init**

In `crates/libflexguard/src/lib.rs`:

- duplicate the `src/lib.rs` scheduler state structure
- grab both `thread_ctx_addr_map` and `nodes_map` handles after `scx_ops_load!`
- pass those handles to the new hook module
- install the FlexGuard runtime from the linked skeleton's mutable BSS view
- attach the extra probe from the generated `sched_switch_btf` program and keep its `Link` alive in `SchedulerState`

The target state should look like:

```rust
struct SchedulerState {
    _scheduler_link: Option<Link>,
    _flexguard_link: Option<Link>,
    _skel: Option<BpfSkel<'static>>,
}
```

- [ ] **Step 4: Re-run the test to verify it passes**

Run: `cargo test --lib flexguard_target_installs_runtime_and_attaches_probe`
Expected: PASS.

### Task 4: Build the new shared library and run mutexbench smoke validation

**Files:**
- Modify: `crates/libflexguard/src/lib.rs`
- Modify: `crates/libflexguard/src/mutex_hook.rs`
- Modify: `src/flexguard.rs` (only if backend naming cleanup is needed)
- Test: `crates/libflexguard/src/mutex_hook.rs`

- [ ] **Step 1: Build the new shared library**

Run: `cargo build -p libflexguard --release`
Expected: PASS and `target/release/libflexguard.so` exists.

- [ ] **Step 2: Verify exported symbols**

Run: `nm -D target/release/libflexguard.so | grep 'pthread_mutex_lock\|pthread_mutex_unlock\|pthread_cond_wait'`
Expected: output includes the interposed pthread symbols.

- [ ] **Step 3: Run focused tests for the new target**

Run: `cargo test -p libflexguard`
Expected: PASS.

- [ ] **Step 4: Run a mutexbench smoke test with the new preload library**

Run:

```bash
bench/mutexbench/scripts/sweep_mutex_throughput.sh \
  --threads 8 \
  --critical-ns 350 \
  --outside-ns 350 \
  --duration-ms 1000 \
  --warmup-duration-ms 300 \
  --repeats 1 \
  --timeslice-extension off \
  --bench-ld-preload target/release/libflexguard.so \
  --lock-kind mutex \
  --output-raw /tmp/flexguard_mutexbench_raw.csv \
  --output-summary /tmp/flexguard_mutexbench_summary.csv
```

Expected: benchmark completes successfully and produces both CSV files.

- [ ] **Step 5: Run the existing library test suite as a regression guard**

Run: `cargo test --lib`
Expected: PASS so `liblb_simple.so` behavior remains intact.
