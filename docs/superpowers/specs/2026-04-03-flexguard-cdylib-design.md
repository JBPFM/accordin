# FlexGuard Rust cdylib Design

**Date:** 2026-04-03

**Status:** Proposed

## Context

We want to add a new lock backend implemented in `src/flexguard.rs` and ship it as a separate Rust `cdylib` named `libflexguard.so`.

This library must:

- export `pthread_mutex_*` and `pthread_cond_*` interposition symbols for `LD_PRELOAD`
- keep the existing `lb_simple` scheduler loading behavior
- build `src/bpf/main.bpf.c` and `src/bpf/flexguard.bpf.c` into one linked BPF object for the `libflexguard.so` target
- coexist with the existing `liblb_simple.so` build instead of replacing it

The current tree already contains most of the ingredients:

- scheduler initialization and BPF loading in `src/lib.rs`
- pthread interposition glue in `src/mutex_hook.rs`
- shared lock backend trait in `src/lock_backend.rs`
- FlexGuard userspace runtime in `src/flexguard.rs`
- FlexGuard BPF probe in `src/bpf/flexguard.bpf.c`

The missing piece is a target boundary that combines them into an independently built shared object without breaking the existing `lb_simple` artifact.

## Goals

1. Produce a standalone `libflexguard.so` Rust `cdylib`.
2. Export `pthread_mutex_*` and `pthread_cond_*` hooks suitable for `LD_PRELOAD`.
3. Reuse the existing scheduler lifecycle and `main.bpf.c` admission/stats logic.
4. Link `main.bpf.c` and `flexguard.bpf.c` into one generated BPF skeleton for the new target.
5. Keep `liblb_simple.so` buildable and behaviorally unchanged.

## Non-Goals

This work does not:

- replace the existing `lb_simple` backend
- add runtime backend switching inside a single shared object
- redesign the scheduler control logic in `main.bpf.c`
- refactor all shared code into a perfect common crate on the first pass
- change benchmark scripts unless they need a new `LD_PRELOAD` path

## Final Design

### 1. New build target

Add a new workspace member dedicated to FlexGuard, for example `crates/libflexguard`.

That crate will:

- declare `[lib] crate-type = ["cdylib"]`
- build to `target/release/libflexguard.so`
- own its own `build.rs`
- reuse existing source modules from the repository root via `#[path = "..."]` on the first pass to keep the diff small

This gives `libflexguard.so` an independent Cargo target while preserving the current root crate that produces `liblb_simple.so`.

### 2. BPF build model

The `libflexguard` target will generate its own BPF skeleton by compiling and linking:

- `src/bpf/main.bpf.c`
- `src/bpf/flexguard.bpf.c`

into one BPF object.

The target-specific `build.rs` will use `scx_cargo::BpfBuilder` with:

- `enable_intf(".../src/bpf/intf.h", "bpf_intf.rs")`
- `enable_skel(".../src/bpf/main.bpf.c", "bpf")`
- `add_source(".../src/bpf/flexguard.bpf.c")`

`main.bpf.c` remains the primary skeleton source because it owns the sched_ext struct_ops entrypoints and the scheduler-facing globals. `flexguard.bpf.c` is added as a second BPF source and linked into the same final object.

### 3. Single license section in the linked BPF object

The final linked BPF object must expose only one `license` section.

`src/bpf/main.bpf.c` already defines the GPL license string. `src/bpf/flexguard.bpf.c` must stop defining its own `license` object so the linked target does not contain duplicate license sections.

No other semantic change is required in `flexguard.bpf.c` for this step.

### 4. Userspace initialization flow

`libflexguard.so` keeps the same high-level library-load flow as the current `lb_simple` library:

1. library constructor runs on load
2. BPF skeleton is opened, loaded, and attached
3. scheduler topology and global maps are initialized
4. pthread interposition symbols become active for the process lifetime

For the new target, this flow is extended with FlexGuard runtime plumbing:

5. fetch pointers or handles for the FlexGuard BPF state exported by the shared skeleton
6. call `flexguard::install_bpf_runtime(...)` so userspace lock code and the FlexGuard BPF probe share the same runtime arrays/counters
7. attach the FlexGuard tracepoint program from the same skeleton in addition to the existing scheduler attachment

The scheduler behavior remains driven by `main.bpf.c`. The extra FlexGuard BPF program only contributes preemption tracking and shared lock runtime state.

### 5. Hook implementation strategy

`libflexguard.so` must export the same interposed API surface as the current library:

- `pthread_mutex_init`
- `pthread_mutex_destroy`
- `pthread_mutex_lock`
- `pthread_mutex_trylock`
- `pthread_mutex_unlock`
- `pthread_cond_init`
- `pthread_cond_destroy`
- `pthread_cond_signal`
- `pthread_cond_broadcast`
- `pthread_cond_wait`
- `pthread_cond_timedwait`

The implementation strategy is to reuse the existing interposition structure from `src/mutex_hook.rs` but bind it to the FlexGuard backend instead of `McsTasLockRaw`.

To keep the first implementation focused, the new target may carry a target-local hook module rather than immediately forcing both libraries through one generic hook implementation. The important requirement is behavioral reuse, not premature deduplication.

### 6. Backend binding

The new target uses `src/flexguard.rs` as the lock backend.

`src/lock_backend.rs` remains the common trait boundary:

- `lock()`
- `try_lock()`
- `unlock()`

`flexguard.rs` already contains the userspace state machine and runtime install hook needed for the FlexGuard path. The only cleanup required is to make the type naming match the target semantics so the new library does not present `McsTas`-named internals as the exported FlexGuard backend.

That can be done either by renaming the raw lock type in `src/flexguard.rs` or by creating a local alias in the new target. A real rename is preferred if it does not create unnecessary churn.

### 7. Thread registration model

This is the main functional difference from `liblb_simple.so`.

The current `lb_simple` path registers each thread into `thread_ctx_addr_map` so BPF can consume scheduler statistics.

The `libflexguard.so` path must register each thread into **both**:

- `thread_ctx_addr_map`, so `main.bpf.c` can keep creating and reading scheduler task context exactly as it does today
- FlexGuard BPF `nodes_map`, where the value is the FlexGuard userspace thread index used to address:
  - `qnodes`
  - `preempted_flags`
  - `num_preempted_holders`

The runtime contract is:

- the first time an interposed thread touches the library, userspace obtains or creates its FlexGuard thread slot via `flexguard::current_thread_index()`
- the hook layer records `tid -> thread_ctx()` in `thread_ctx_addr_map`
- the hook layer records `tid -> thread_index` in `nodes_map`
- the thread unregisters from both maps on exit, mirroring the current thread-registration lifecycle

This lets `main.bpf.c` keep its existing scheduler-accounting path while `flexguard.bpf.c` translates kernel task IDs to the same userspace qnode slots that the lock algorithm uses.

### 8. Shared-code strategy for the first pass

The first pass should optimize for correctness and small blast radius.

Instead of moving many files around immediately, `crates/libflexguard` will reuse existing source files through path-based module inclusion where practical:

- `src/arch.rs`
- `src/lock_backend.rs`
- `src/flexguard.rs`
- `src/lock_stats.rs` if condvar/accounting paths still need it
- shared BPF headers under `src/bpf/`

This keeps the initial change set focused on target creation, BPF composition, and FlexGuard-specific hook wiring.

If the resulting duplication between the two shared libraries becomes awkward, a follow-up refactor can extract a proper common crate.

## File-Level Plan

### New files

- `crates/libflexguard/Cargo.toml`
- `crates/libflexguard/build.rs`
- `crates/libflexguard/src/lib.rs`
- `crates/libflexguard/src/bpf_skel.rs`
- `crates/libflexguard/src/mutex_hook.rs` or `mutex_hook_flexguard.rs`

### Updated files

- root `Cargo.toml` to add the new workspace member
- `src/flexguard.rs` to expose a clean backend type name if needed
- `src/bpf/flexguard.bpf.c` to remove the duplicate BPF `license` definition

### Expected untouched behavior

- existing `src/lib.rs` initialization semantics for `liblb_simple.so`
- existing `main.bpf.c` scheduler logic
- existing `liblb_simple.so` release build

## Data Flow

### Library load

- dynamic loader loads `libflexguard.so`
- Rust constructor initializes logging and BPF state
- target-local skeleton loads linked scheduler + FlexGuard BPF programs
- scheduler struct_ops attach as before
- FlexGuard tracepoint program attaches from the same skeleton
- userspace FlexGuard runtime receives BPF state pointers

### First mutex use on a thread

- interposed `pthread_mutex_lock` resolves per-mutex state
- thread ensures it has a FlexGuard thread slot
- hook layer registers `tid -> thread_index` in `nodes_map`
- backend lock path uses `flexguard.rs`
- FlexGuard BPF probe can now observe sched_switch events for that thread and mark matching qnode state as preempted

### Unlock / condvar paths

- interposed unlock and condvar operations stay exported from the Rust cdylib
- unlock path continues through the FlexGuard backend
- condvar wait releases and reacquires the same backend lock implementation
- thread unregisters from `nodes_map` on thread exit

## Error Handling

- if BPF loading fails, the new library should fail the same way the current library fails during constructor initialization
- if thread registration into `nodes_map` fails, the failure should not silently corrupt the runtime contract; it should either fail fast or clearly preserve a safe degraded behavior
- if FlexGuard runtime installation has already happened, repeated installation attempts should remain harmless via existing one-time initialization guards

## Testing and Verification

Minimum verification for this design:

1. `cargo build -p libflexguard --release`
2. confirm `target/release/libflexguard.so` exists
3. confirm the library exports `pthread_mutex_*` and `pthread_cond_*` symbols
4. confirm the root crate still builds its existing shared object
5. run at least one `LD_PRELOAD=target/release/libflexguard.so` mutex benchmark path to verify constructor load and hook wiring
6. verify the generated skeleton includes both scheduler state and FlexGuard probe programs

Recommended additional checks:

- unit tests for the FlexGuard thread registration helpers
- a source-shape test ensuring the new target build includes both `.bpf.c` files
- a smoke test that `nodes_map` receives thread registrations when a benchmark thread first locks a mutex

## Risks and Mitigations

### Risk: duplicate or conflicting shared logic

If the new target tries to generalize the existing hook layer too early, the diff may become much larger than necessary.

**Mitigation:** keep the first pass target-local where needed and only share already-stable modules.

### Risk: duplicate BPF sections in the linked output

Because two BPF translation units are being linked, duplicate global sections such as `license` can break generation or loading.

**Mitigation:** keep `main.bpf.c` as the sole owner of the license section and remove the duplicate from `flexguard.bpf.c`.

### Risk: thread registration mismatch

If `tid -> thread_index` registration is wrong, the BPF probe will observe the wrong qnode slot and preemption detection will be invalid.

**Mitigation:** centralize thread-index acquisition in the hook layer and test the registration helper explicitly.

## Acceptance Criteria

The work is complete when all of the following are true:

- `libflexguard.so` is built as an independent Rust `cdylib`
- it exports pthread mutex and condvar interposition symbols
- its BPF skeleton is generated from linked `main.bpf.c` and `flexguard.bpf.c`
- its initialization flow matches the current scheduler-loading behavior
- FlexGuard userspace runtime is wired to the BPF state exposed by the shared skeleton
- thread registration for the FlexGuard target uses `nodes_map`
- the existing `liblb_simple.so` target still builds and behaves as before
