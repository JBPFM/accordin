# Best Score Decay Confirmation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `ssc_best_score` forget early outlier peaks over time and require two mature windows before a new width can rewrite the best-score anchor.

**Architecture:** Keep the existing seek/refine controller shape, but change best-anchor maintenance from a permanent historical peak into a slowly decaying anchor with candidate confirmation. Add two small BPF globals for pending candidate state, add helper functions in `stats.bpf.h` for decay/reset/promotion, and update `lb_simple_tick()` so mature-window decisions decay `ssc_best_score` before comparing scores and stop overwriting the best anchor immediately on single-window spikes.

**Tech Stack:** sched_ext BPF C, BPF global state in `.bss`, Rust source-shape tests in `src/lib.rs`, `cargo test`, `cargo build`

---

## File Structure

- `src/bpf/maps.bpf.h` — BPF global controller state. Add pending best-candidate tracking fields next to `ssc_best_count` / `ssc_best_score`.
- `src/bpf/stats.bpf.h` — controller constants and inline helpers. Add the decay rate, confirmation window count, candidate reset helper, score decay helper, promotion helper, and clear pending candidate state from resize bookkeeping.
- `src/bpf/main.bpf.c` — quorum-side controller loop. Seed bootstrap as today, then decay the best score once per mature window, run candidate confirmation, remove inline best-score overwrites, and keep existing grow/shrink/refine transitions.
- `src/lib.rs` — source-shape tests that lock in the new state, helper API, and quorum-side control-flow strings.

## Task 1: Lock in the new best-score decay state and helper API

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/bpf/maps.bpf.h`
- Modify: `src/bpf/stats.bpf.h`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add this test under `#[cfg(test)] mod tests` in `src/lib.rs`:

```rust
#[test]
fn best_score_decay_headers_define_candidate_tracking() {
    let maps = include_str!("bpf/maps.bpf.h");
    let stats = compact(include_str!("bpf/stats.bpf.h"));

    assert!(
        maps.contains("ssc_best_candidate_count"),
        "BPF globals should track which SSC width is waiting for best-score promotion",
    );
    assert!(
        maps.contains("ssc_best_candidate_streak"),
        "BPF globals should track how many mature windows have confirmed the candidate width",
    );
    assert!(
        stats.contains("SSC_BEST_SCORE_DECAY_SHIFT"),
        "stats helpers should define the per-window best-score decay rate",
    );
    assert!(
        stats.contains("SSC_BEST_CONFIRM_WINDOWS"),
        "stats helpers should define how many mature windows confirm a new best anchor",
    );
    assert!(
        stats.contains("static__always_inlinevoidssc_reset_best_candidate(void){ssc_best_candidate_count=0;ssc_best_candidate_streak=0;}"),
        "stats helpers should expose a helper that clears pending best-score promotion state",
    );
    assert!(
        stats.contains("static__always_inline__u64ssc_decay_best_score(void){"),
        "stats helpers should expose a helper that decays the historical best score once per mature window",
    );
    assert!(
        stats.contains("static__always_inlinevoidssc_maybe_promote_best_candidate(__u32active_count,__u64score,__u64compare_best_score){"),
        "stats helpers should expose a helper that requires repeated mature windows before rewriting the best anchor",
    );
    assert!(
        stats.contains("ssc_reset_best_candidate();ssc_vote_last_score=0;ssc_vote_last_effective_score=effective_score;"),
        "resize bookkeeping should clear any pending best-score candidate before refreshing the effective-score anchor",
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
cargo test --lib best_score_decay_headers_define_candidate_tracking
```

Expected: FAIL because the new BPF globals, constants, and helpers do not exist yet.

- [ ] **Step 3: Write the minimal implementation**

In `src/bpf/maps.bpf.h`, add the two pending-candidate globals immediately after `ssc_best_score`:

```c
volatile __u32 ssc_best_count = 2;
volatile __u64 ssc_best_score = 0;
volatile __u32 ssc_best_candidate_count = 0;
volatile __u32 ssc_best_candidate_streak = 0;
volatile __u32 ssc_refine_low = 2;
volatile __u32 ssc_refine_high = 2;
```

In `src/bpf/stats.bpf.h`, add the new constants near the existing bootstrap/refine constants:

```c
#define SSC_BEST_SCORE_DECAY_SHIFT 4U
#define SSC_BEST_CONFIRM_WINDOWS 2U
```

Then add the helper functions directly after `reset_ssc_refine_bounds()` and update `ssc_note_resize()` so any width change or no-op resize clears pending candidate state:

```c
static __always_inline void ssc_reset_best_candidate(void) {
  ssc_best_candidate_count = 0;
  ssc_best_candidate_streak = 0;
}

static __always_inline __u64 ssc_decay_best_score(void) {
  __u64 decay;

  if (!ssc_best_score)
    return 0;

  decay = ssc_best_score >> SSC_BEST_SCORE_DECAY_SHIFT;
  if (!decay)
    return ssc_best_score;

  ssc_best_score -= decay;
  return ssc_best_score;
}

static __always_inline void ssc_maybe_promote_best_candidate(__u32 active_count,
                                                             __u64 score,
                                                             __u64 compare_best_score) {
  if (!compare_best_score || score < compare_best_score) {
    ssc_reset_best_candidate();
    return;
  }

  if (ssc_best_candidate_count != active_count) {
    ssc_best_candidate_count = active_count;
    ssc_best_candidate_streak = 1;
    return;
  }

  if (ssc_best_candidate_streak < SSC_BEST_CONFIRM_WINDOWS)
    ssc_best_candidate_streak++;

  if (ssc_best_candidate_streak < SSC_BEST_CONFIRM_WINDOWS)
    return;

  ssc_best_score = score;
  ssc_best_count = active_count;
  ssc_reset_best_candidate();
}

static __always_inline void ssc_note_resize(__u64 effective_score) {
  ssc_reset_best_candidate();
  ssc_vote_last_score = 0;
  ssc_vote_last_effective_score = effective_score;
  ssc_vote_consec_grow = 0;
  ssc_vote_consec_shrink = 0;
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run:

```bash
cargo test --lib best_score_decay_headers_define_candidate_tracking
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/bpf/maps.bpf.h src/bpf/stats.bpf.h
git commit -m "test: lock in best-score decay helper state"
```

## Task 2: Lock in mature-window decay and two-window best promotion in the controller loop

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/bpf/main.bpf.c`
- Modify: `src/bpf/stats.bpf.h`
- Test: `src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add this test in `src/lib.rs` near the other quorum/refine controller tests:

```rust
#[test]
fn best_score_decay_quorum_logic_requires_decay_and_two_window_confirmation() {
    let main = compact(include_str!("bpf/main.bpf.c"));

    assert!(
        main.contains("compare_best_score=seeded_best?ssc_best_score:ssc_decay_best_score();ssc_maybe_promote_best_candidate(ssc_active_count,score,compare_best_score);"),
        "simple_tick should decay the best anchor before routing mature-window scores through the two-window promotion helper",
    );
    assert!(
        main.contains("if(score>=compare_best_score){if(ssc_active_count>ssc_refine_low)ssc_refine_low=ssc_active_count;}elseif(ssc_active_count>ssc_refine_low){ssc_refine_high=ssc_active_count;}"),
        "refine-mode bounds should compare against the decayed best anchor instead of a permanent historical peak",
    );
    assert!(
        !main.contains("if(score>ssc_best_score){ssc_best_score=score;ssc_best_count=ssc_active_count;}"),
        "a single mature window should no longer overwrite the best anchor immediately",
    );
    assert!(
        !main.contains("if(score>=ssc_best_score){ssc_best_score=score;ssc_best_count=ssc_active_count;"),
        "refine-mode should no longer rewrite the best anchor inline on the first qualifying window",
    );
    assert!(
        !main.contains("ssc_best_count=ssc_active_count;ssc_best_score=score;ssc_set_active_count(grow_target,score);"),
        "seek-mode growth should stop overwriting the best anchor just because the controller is about to grow",
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run:

```bash
cargo test --lib best_score_decay_quorum_logic_requires_decay_and_two_window_confirmation
```

Expected: FAIL because `lb_simple_tick()` still rewrites `ssc_best_score` inline and does not decay the anchor before promotion.

- [ ] **Step 3: Write the minimal implementation**

In `src/bpf/main.bpf.c`, keep the bootstrap gate and bootstrap seeding exactly as they are, but replace the immediate best-score rewrite path with a seeded/decayed comparison anchor and helper-driven promotion:

```c
__u64 score = compute_ssc_vote_score(ssc_active_count);
bool seeded_best = false;
__u64 compare_best_score;

if (!ssc_vote_last_effective_score)
  ssc_vote_last_effective_score = score;
if (!ssc_best_score) {
  ssc_best_score = ssc_vote_sum_unlock_count * SSC_SCORE_SCALE;
  ssc_best_count = ssc_active_count;
  reset_ssc_refine_bounds(ssc_active_count);
  seeded_best = true;
}

compare_best_score = seeded_best ? ssc_best_score : ssc_decay_best_score();
ssc_maybe_promote_best_candidate(ssc_active_count, score, compare_best_score);

if (ssc_vote_last_score) {
  if (score > ssc_vote_last_score)
    ssc_vote_consec_grow++;
  else
    ssc_vote_consec_grow = 0;

  if (score < ssc_vote_last_effective_score)
    ssc_vote_consec_shrink++;
  else
    ssc_vote_consec_shrink = 0;
}

ssc_vote_last_score = score;
```

Still in `src/bpf/main.bpf.c`, update the refine and grow paths so they compare against `compare_best_score` and stop overwriting `ssc_best_score` / `ssc_best_count` inline:

```c
if (ssc_search_phase == SSC_SEARCH_REFINE) {
  __u32 next_target;

  if (score >= compare_best_score) {
    if (ssc_active_count > ssc_refine_low)
      ssc_refine_low = ssc_active_count;
  } else if (ssc_active_count > ssc_refine_low) {
    ssc_refine_high = ssc_active_count;
  }

  if (dbg_counters_enabled && ssc_refine_low == ssc_refine_high)
    dbg_refine_single_point++;

  next_target = ssc_next_refine_target();
  if (ssc_refine_low == ssc_refine_high && next_target == ssc_active_count &&
      ssc_best_score &&
      score * SSC_REFINE_BAD_STEADY_RATIO_DEN <
          ssc_best_score * SSC_REFINE_BAD_STEADY_RATIO_NUM &&
      ssc_vote_consec_shrink >= SSC_REFINE_BAD_STEADY_WINDOWS) {
    if (dbg_counters_enabled) {
      dbg_refine_noop_targets++;
      dbg_bad_steady_rebases++;
    }
    reset_ssc_refine_bounds(ssc_active_count);
    ssc_note_resize(score);
  } else if (next_target != ssc_active_count) {
    ssc_set_active_count(next_target, ssc_best_score);
  } else if (dbg_counters_enabled) {
    dbg_refine_noop_targets++;
  }
} else if (ssc_vote_consec_grow >= 2) {
  __u32 grow_target = ssc_active_count << 1;

  if (ssc_bootstrap_mature_windows == SSC_MIN_BOOTSTRAP_WINDOWS &&
      ssc_active_count >= 8)
    grow_target = ssc_active_count + (ssc_active_count >> 1);

  if (dbg_counters_enabled) {
    dbg_last_grow_target = grow_target;
    if (grow_target != (ssc_active_count << 1))
      dbg_grow_uses_capped_step++;
  }

  ssc_set_active_count(grow_target, score);
  reset_ssc_refine_bounds(ssc_active_count);
}
```

Do not reintroduce any direct `ssc_best_score = score; ssc_best_count = ssc_active_count;` assignments outside bootstrap and `ssc_maybe_promote_best_candidate()`.

- [ ] **Step 4: Run the focused tests to verify they pass**

Run:

```bash
cargo test --lib best_score_decay
```

Expected: PASS for both `best_score_decay_headers_define_candidate_tracking` and `best_score_decay_quorum_logic_requires_decay_and_two_window_confirmation`.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/bpf/main.bpf.c src/bpf/stats.bpf.h
git commit -m "feat: decay and confirm ssc best score"
```

## Task 3: Run the existing library suite and build with the new controller semantics

**Files:**
- Modify: none expected
- Verify: `src/lib.rs`
- Verify: `src/bpf/maps.bpf.h`
- Verify: `src/bpf/stats.bpf.h`
- Verify: `src/bpf/main.bpf.c`

- [ ] **Step 1: Run the full library test suite**

Run:

```bash
cargo test --lib
```

Expected: PASS.

- [ ] **Step 2: Run a build**

Run:

```bash
cargo build
```

Expected: PASS.

- [ ] **Step 3: Verify the diff stays scoped**

Run:

```bash
git diff -- src/lib.rs src/bpf/maps.bpf.h src/bpf/stats.bpf.h src/bpf/main.bpf.c
```

Expected: the diff only shows the new best-score decay state, helper functions, and mature-window controller-flow changes described above.
