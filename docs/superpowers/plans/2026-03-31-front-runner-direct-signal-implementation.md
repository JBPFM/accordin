# Front-Runner Direct Signal Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the delayed `queue_bypass` relay with a direct lock-local front-runner signal so threads can observe a preempted front waiter without relying on its immediate successor.

**Architecture:** Keep the existing BPF-exported per-thread `preempted_flags[]` and holder-only global counter, but replace the per-lock `queue_bypass` relay token with a per-lock `front_runner` token. Publish that token whenever a waiter becomes `FRONT`, let both new arrivals and already-enqueued waiters consult `front_runner_blocked()`, and reuse the existing blocking-aware `mcs_exit_blocking()` protocol to retire queued nodes safely.

**Tech Stack:** Rust atomics, existing `mcs_tas` unit tests, `cargo test`, `cargo build --release`, `bench/mutexbench/mutex_bench`, `perf`.

---

## Chunk 1: Tests And Contracts

### Task 1: Lock In The Front-Runner Signal Contract

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/mcs_tas.rs`
- Test: `src/lib.rs`
- Test: `src/mcs_tas.rs`

- [ ] **Step 1: Write the failing source-contract assertions**

Add or adjust the source-level contract tests in `src/lib.rs` so they require:

- `front_runner`
- `front_runner_blocked`
- the removal of `queue_bypass`
- the queued wait loop breaking on the new direct signal

- [ ] **Step 2: Run the focused source-contract test and verify it fails**

Run: `cargo test mcs_tas_uses_flexguard_critical_state_markers --lib -- --nocapture`

Expected: FAIL because the current source still contains `queue_bypass` and does not expose the new direct-signal helpers.

- [ ] **Step 3: Write the failing unit tests in `src/mcs_tas.rs`**

Add focused tests for:

- stale `front_runner` tokens being ignored after qnode reuse
- `front_runner_blocked()` observing a preempted `FRONT`
- `should_enqueue_mcs()` skipping enqueue when `front_runner_blocked()` is true

- [ ] **Step 4: Run the focused unit tests and verify they fail**

Run: `cargo test front_runner --lib -- --nocapture`

Expected: FAIL because the implementation still uses `queue_bypass`.

## Chunk 2: Rust Lock Implementation

### Task 2: Replace Queue Bypass With Front-Runner Token

**Files:**
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Replace the per-lock field**

Change `McsTasLockRaw` from:

- `queue_bypass: CacheAligned<AtomicU64>`

to:

- `front_runner: CacheAligned<AtomicU64>`

and rename any constructor or zero-state initialization accordingly.

- [ ] **Step 2: Add direct-signal helpers**

Implement minimal helpers in `src/mcs_tas.rs` for:

- encoding and decoding the front-runner token
- publishing the current front-runner
- validating whether the published token still names a live `FRONT`
- `front_runner_blocked()`

Use the existing qnode generation slots to reject stale tokens.

- [ ] **Step 3: Remove the old bypass helpers**

Delete:

- `request_queue_bypass`
- `queue_bypass_present`
- `queue_bypass_active`
- `predecessor_blocks_queue`

and update callers.

- [ ] **Step 4: Run the focused tests and verify partial progress**

Run: `cargo test front_runner --lib -- --nocapture`

Expected: Some tests still fail until the slow path is updated, but the helpers compile and the intended tests are now exercising the new code.

### Task 3: Rewire The Slow Path To The Direct Signal

**Files:**
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Update admission and phase2 predicates**

Change:

- `should_enqueue_mcs()`
- `phase2_blocking()`

to consult:

- `holder_preempted()`
- `front_runner_blocked()`

instead of the old `queue_bypass` protocol.

- [ ] **Step 2: Publish the front-runner token at the right transitions**

Publish `front_runner` when:

- the thread enqueues with `pred.is_null()`
- a queued waiter observes `waiting == 0` and becomes the new `FRONT`

- [ ] **Step 3: Simplify the local MCS wait loop**

Remove the `pred`-based bypass publication from the hot loop and make queued waiters break based on `front_runner_blocked()` only.

- [ ] **Step 4: Preserve blocking-aware retirement**

Keep `mcs_exit_blocking()` and the parked-sentinel handoff protocol unchanged except for any minimal renames needed to compile.

- [ ] **Step 5: Run the focused tests and verify they pass**

Run: `cargo test front_runner --lib -- --nocapture`

Expected: PASS

## Chunk 3: Full Verification

### Task 4: Verify Correctness, Build, And Performance

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/mcs_tas.rs`

- [ ] **Step 1: Run the relevant library tests**

Run: `cargo test --lib -- --nocapture`

Expected: PASS

- [ ] **Step 2: Run the release build**

Run: `cargo build --release`

Expected: PASS

- [ ] **Step 3: Benchmark the modified worktree**

Run:

```bash
sudo -n env LD_PRELOAD=/home/jz/Projects/lb_simple/target/release/liblb_simple.so \
  /home/jz/Projects/lb_simple/bench/mutexbench/mutex_bench \
  --lock-kind mutex \
  --threads 64 \
  --critical-ns 350 \
  --outside-ns 3500 \
  --duration-ms 3000 \
  --warmup-duration-ms 1000 \
  --timeslice-extension require
```

Expected: Throughput improves and `avg_lock_handoff_ns_estimated` drops relative to the current dirty baseline.

- [ ] **Step 4: Compare against clean `da91cd5`**

Run the same command with:

- `LD_PRELOAD=/home/jz/Projects/lb_simple/.worktrees/clean-da91cd5/target/release/liblb_simple.so`

Expected: The modified worktree remains above the previous broken state, even if it may still be slower than clean `da91cd5`.

- [ ] **Step 5: Use `perf` to inspect user-space MCS residency**

Run:

```bash
sudo -n perf stat -e task-clock,context-switches,cpu-migrations,cycles,instructions \
  env LD_PRELOAD=/home/jz/Projects/lb_simple/target/release/liblb_simple.so \
  /home/jz/Projects/lb_simple/bench/mutexbench/mutex_bench \
  --lock-kind mutex \
  --threads 64 \
  --critical-ns 350 \
  --outside-ns 3500 \
  --duration-ms 2000 \
  --warmup-duration-ms 1000 \
  --timeslice-extension require
```

and:

```bash
sudo -n perf record -g --call-graph=dwarf -o /tmp/perf-front-runner-fixed.data \
  env LD_PRELOAD=/home/jz/Projects/lb_simple/target/release/liblb_simple.so \
  /home/jz/Projects/lb_simple/bench/mutexbench/mutex_bench \
  --lock-kind mutex \
  --threads 64 \
  --critical-ns 350 \
  --outside-ns 3500 \
  --duration-ms 2000 \
  --warmup-duration-ms 1000 \
  --timeslice-extension require
```

Expected:

- `pthread_mutex_lock` still dominates dirty-vs-clean comparisons, but less than before
- task-clock or instructions attributable to the lock path drop
- the hot wait loop in `src/mcs_tas.rs` accounts for fewer samples than in the broken build

