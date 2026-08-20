/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INACTIVE_SELECT_H
#define __INACTIVE_SELECT_H

#include "intf.h"

#define INACTIVE_PROBABILITY_SCALE 100U
#define MANAGED_LOCK_COUNT (MAX_LOCK_CLASSES - 1U)

/* Managed classes a single deficit probe examines. The probe cannot sweep the
 * whole class space per call: the loop is unrolled and the verifier explores
 * each iteration's paths separately, so the cost this adds to the dispatch
 * program grows steeply with the width. Measured against the verifier's
 * 1000000-instruction budget at MAX_LOCK_CLASSES 64, dispatch costs 44% of the
 * budget at width 4, 99% at width 7, and is rejected at width 8; width 4 also
 * keeps the program loadable, at 74% of the budget, if the class count doubles
 * again. Any width change has to be re-measured, not reasoned about, because
 * the growth is superlinear and the rest of dispatch shares the same budget.
 *
 * A rotating start rank spreads consecutive probes across the space, so a class
 * outside one window is reached by a following one. */
#define INACTIVE_DEFICIT_PROBE_WIDTH 4U

static __always_inline bool inactive_managed_lock_id(__u32 lock_id) {
  return lock_id != UNMANAGED_LOCK_ID && lock_id < MAX_LOCK_CLASSES;
}

/* Managed ranks the deficit probe rotates over, from the span userspace
 * published. A span of 0 has not been published yet and one past the managed
 * space is not trusted; both fall back to the whole ring. Never returns 0: the
 * rank walk divides and wraps by this. */
static __always_inline __u32 inactive_probe_ranks(__u32 published) {
  if (published == 0U || published > MANAGED_LOCK_COUNT)
    return MANAGED_LOCK_COUNT;

  return published;
}

/* Managed ids form a ring of ranks [0, span); rank 0 is lock 1. */
static __always_inline __u32 inactive_lock_at_rank(__u32 rank) {
  return rank + 1U;
}

/* Next rank in the ring. Increment-and-wrap rather than a modulo because this
 * runs inside the unrolled probe loop, where every extra path is explored once
 * per unrolled iteration. */
static __always_inline __u32 inactive_wrap_rank(__u32 rank, __u32 span) {
  rank++;
  if (rank >= span)
    rank = 0U;

  return rank;
}

/* Start rank of the probe after the one starting at `rank`. Advancing by the
 * full window rather than by one keeps the sweep to span /
 * INACTIVE_DEFICIT_PROBE_WIDTH calls. `span` is never 0. */
static __always_inline __u32 inactive_next_probe_rank(__u32 rank, __u32 span) {
  return (rank + INACTIVE_DEFICIT_PROBE_WIDTH) % span;
}

/* Managed classes one cvready selection examines. It shares the dispatch
 * program's instruction budget with the deficit probe and the fallback scan, so
 * it stays a bounded window with no full-space scan behind it: a class the
 * window misses is reached by a following call through the rotating cursor, and
 * by the periodic forced drain in the worst case. */
#define CVREADY_PROBE_WIDTH 4U

/* Start rank of the cvready probe after the one starting at `rank`. `span` is
 * never 0. */
static __always_inline __u32 cvready_next_probe_rank(__u32 rank, __u32 span) {
  return (rank + CVREADY_PROBE_WIDTH) % span;
}

static __always_inline bool inactive_prefer_previous_lock(__u32 random,
                                                          __u32 percent) {
  if (percent > INACTIVE_PROBABILITY_SCALE)
    percent = INACTIVE_PROBABILITY_SCALE;

  return (random % INACTIVE_PROBABILITY_SCALE) < percent;
}

static __always_inline __u32 inactive_other_lock_at(__u32 previous_lock_id,
                                                    __u32 start,
                                                    __u32 offset) {
  __u32 rank;
  __u32 lock_id;

  if (inactive_managed_lock_id(previous_lock_id)) {
    if (offset >= MANAGED_LOCK_COUNT - 1U)
      return UNMANAGED_LOCK_ID;

    rank = (start + offset) % (MANAGED_LOCK_COUNT - 1U);
    lock_id = rank + 1U;
    if (lock_id >= previous_lock_id)
      lock_id++;
    return lock_id;
  }

  if (offset >= MANAGED_LOCK_COUNT)
    return UNMANAGED_LOCK_ID;

  rank = (start + offset) % MANAGED_LOCK_COUNT;
  return rank + 1U;
}

#endif /* __INACTIVE_SELECT_H */
