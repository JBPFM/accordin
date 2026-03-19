/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __STATS_BPF_H
#define __STATS_BPF_H

/* Keep this header self-contained for standalone clangd parsing. */
#include <scx/common.bpf.h>

#include "intf.h"
#include "maps.bpf.h"

#define SSC_SCORE_SCALE 1024ULL
#define SSC_SHIFT_EWMA_ALPHA_NUM 1ULL
#define SSC_SHIFT_EWMA_ALPHA_DEN 4ULL
#define SSC_SHIFT_WAIT_ABS_THRESHOLD (SSC_SCORE_SCALE / 12ULL)
#define SSC_SHIFT_WAIT_REL_PCT 25ULL
#define SSC_SHIFT_CONFIRM_WINDOWS 3U
#define SSC_RESIZE_HOLDOFF_WINDOWS 3U

/*
 * Statistics layer: window-based EWMA wait ratio computation.
 *
 * Provides seqcount-protected user-space reads, per-task run/wait
 * accounting, and the elected-leader window rollover with EWMA
 * and hysteresis-driven target adjustment.
 */

/* ------------------------------------------------------------------ */
/*  User-space context read                                            */
/* ------------------------------------------------------------------ */

/*
 * Read a user-space lock_sched_thread_ctx.
 *
 * The seqcount write-side is currently disabled in userspace (mcs_tas.rs),
 * so we skip the seq validation and do a single bpf_probe_read_user.
 * Returns true on success.
 */
static __always_inline bool read_thread_ctx(__u64 user_ptr,
                                            struct lock_sched_thread_ctx *out) {
  if (!user_ptr)
    return false;

  return bpf_probe_read_user(out, sizeof(*out),
                             (void *)(unsigned long)user_ptr) == 0;
}

/* ------------------------------------------------------------------ */
/*  Stats map helper                                                   */
/* ------------------------------------------------------------------ */

static __always_inline void update_stat(__u32 key, __u64 val) {
  __u64 *sp = bpf_map_lookup_elem(&stats_map, &key);
  if (sp)
    *sp = val;
}

static __always_inline struct agg_percpu *lookup_cpu_agg(void) {
  __u32 key = 0;

  return bpf_map_lookup_elem(&agg_percpu_map, &key);
}

static __always_inline __u32 clamp_ssc_active_count(__u32 active_count) {
  if (active_count < 2)
    active_count = 2;

  if (ssc_cpu_count && active_count > ssc_cpu_count)
    active_count = ssc_cpu_count;

  if (active_count < 2)
    active_count = 2;

  return active_count;
}

static __always_inline __u64 compute_ssc_vote_score(__u32 active_count) {
  __u64 useful_run;

  if (!ssc_vote_sum_run)
    return 0;

  useful_run = ssc_vote_sum_run > ssc_vote_sum_wait
                   ? ssc_vote_sum_run - ssc_vote_sum_wait
                   : 0;

  return ((__u64)active_count * useful_run * SSC_SCORE_SCALE) /
         ssc_vote_sum_run;
}

static __always_inline __u64 abs_diff_u64(__u64 a, __u64 b) {
  return a > b ? a - b : b - a;
}

static __always_inline bool rel_change_exceeds(__u64 cur, __u64 base,
                                               __u64 pct) {
  if (!base)
    return cur > 0;

  return abs_diff_u64(cur, base) * 100ULL >= base * pct;
}

static __always_inline __u64 ewma_update_u64(__u64 prev, __u64 sample) {
  __u64 delta;

  if (!prev)
    return sample;

  if (sample > prev) {
    delta = sample - prev;
    return prev + (delta * SSC_SHIFT_EWMA_ALPHA_NUM) /
                      SSC_SHIFT_EWMA_ALPHA_DEN;
  }

  delta = prev - sample;
  return prev -
         (delta * SSC_SHIFT_EWMA_ALPHA_NUM) / SSC_SHIFT_EWMA_ALPHA_DEN;
}

static __always_inline __u64 compute_ssc_wait_ratio(__u64 run_ns,
                                                    __u64 wait_ns) {
  if (!run_ns)
    return 0;

  return (wait_ns * SSC_SCORE_SCALE) / run_ns;
}

static __always_inline void reset_ssc_refine_bounds(__u32 active_count) {
  active_count = clamp_ssc_active_count(active_count);
  ssc_refine_low = active_count;
  ssc_refine_high = active_count;
}

static __always_inline void ssc_note_resize(__u64 effective_score) {
  ssc_resize_holdoff = SSC_RESIZE_HOLDOFF_WINDOWS;
  ssc_vote_last_score = 0;
  ssc_vote_last_effective_score = effective_score;
  ssc_vote_consec_grow = 0;
  ssc_vote_consec_shrink = 0;
}

static __always_inline void ssc_set_active_count(__u32 active_count,
                                                 __u64 effective_score) {
  active_count = clamp_ssc_active_count(active_count);

  if (active_count == ssc_active_count)
    return;

  ssc_active_count = active_count;
  ssc_note_resize(effective_score);
}

static __always_inline void ssc_enter_refine_mode(__u32 low, __u32 high,
                                                  __u64 score) {
  if (low > high) {
    __u32 tmp = low;
    low = high;
    high = tmp;
  }

  ssc_refine_low = clamp_ssc_active_count(low);
  ssc_refine_high = clamp_ssc_active_count(high);
  if (ssc_refine_low > ssc_refine_high) {
    __u32 tmp = ssc_refine_low;
    ssc_refine_low = ssc_refine_high;
    ssc_refine_high = tmp;
  }

  ssc_search_phase = SSC_SEARCH_REFINE;
  if (ssc_best_count < ssc_refine_low || ssc_best_count > ssc_refine_high)
    ssc_best_count = ssc_refine_low;
  if (!ssc_best_score)
    ssc_best_score = score;
}

static __always_inline __u32 ssc_next_refine_target(void) {
  if (ssc_refine_high <= ssc_refine_low + 1)
    return ssc_best_count ? ssc_best_count : ssc_refine_low;

  return clamp_ssc_active_count(ssc_refine_low +
                                ((ssc_refine_high - ssc_refine_low) >> 1));
}

static __always_inline bool detect_ssc_workload_shift(__u64 now,
                                                      __u64 *wait_ratio_out) {
  __u64 wait_ratio = compute_ssc_wait_ratio(ssc_vote_sum_run, ssc_vote_sum_wait);
  __u64 demand = ssc_vote_sum_run + ssc_vote_sum_wait;
  __u64 prev_wait_ratio = ssc_wait_ratio_ewma;
  bool wait_shift = false;

  (void)now;
  (void)demand;

  if (wait_ratio_out)
    *wait_ratio_out = wait_ratio;

  if (!ssc_shift_baseline_valid) {
    ssc_wait_ratio_ewma = wait_ratio;
    ssc_shift_baseline_valid = 1;
    return false;
  }

  if (ssc_resize_holdoff) {
    ssc_resize_holdoff--;
    ssc_shift_streak = 0;
    return false;
  }

  if (abs_diff_u64(wait_ratio, prev_wait_ratio) >= SSC_SHIFT_WAIT_ABS_THRESHOLD &&
      rel_change_exceeds(wait_ratio, prev_wait_ratio, SSC_SHIFT_WAIT_REL_PCT))
    wait_shift = true;

  if (wait_shift)
    ssc_shift_streak++;
  else
    ssc_shift_streak = 0;

  if (ssc_shift_streak >= SSC_SHIFT_CONFIRM_WINDOWS) {
    ssc_wait_ratio_ewma = wait_ratio;
    ssc_shift_streak = 0;

    return true;
  }

  ssc_wait_ratio_ewma = ewma_update_u64(prev_wait_ratio, wait_ratio);

  return false;
}

static __always_inline void rotate_ssc_vote_window(__u64 now) {
  if (!ssc_vote_epoch)
    ssc_vote_epoch = 1;
  else
    ssc_vote_epoch++;

  ssc_vote_start_ns = now;
  ssc_vote_sum_run = 0;
  ssc_vote_sum_wait = 0;
  ssc_vote_publish_count = 0;
}

static __always_inline void maybe_rotate_ssc_vote_window(__u64 now) {
  if (!ssc_vote_epoch) {
    rotate_ssc_vote_window(now);
    return;
  }

  if (now > ssc_vote_start_ns &&
      now - ssc_vote_start_ns >= ssc_vote_window_ns)
    rotate_ssc_vote_window(now);
}

/* ------------------------------------------------------------------ */
/*  Per-task activity accounting                                       */
/* ------------------------------------------------------------------ */

/*
 * Account per-task run and wait time.
 *
 * Always reads userspace lock context and accumulates both run_ns
 * and wait_ns to the per-CPU aggregators symmetrically.
 */
static __always_inline void account_task_activity(struct task_scx_ctx *tc,
                                                  __u32 pid, __u64 now) {
  __u64 run_delta = 0;
  __u64 wait_delta = 0;
  bool have_uctx = false;
  struct lock_sched_thread_ctx uctx = {};
  struct agg_percpu *agg = lookup_cpu_agg();

  (void)pid;
  dbg_acct_calls++;

  if (tc->window_epoch != ssc_vote_epoch) {
    tc->window_epoch = ssc_vote_epoch;
    tc->run_ns_window = 0;
    tc->wait_ns_window = 0;
    tc->pending_wait_ns = 0;
  }

  if (agg && agg->epoch != ssc_vote_epoch) {
    agg->epoch = ssc_vote_epoch;
    agg->run_ns = 0;
    agg->wait_ns = 0;
  }

  if (tc->user_ctx_ptr && read_thread_ctx(tc->user_ctx_ptr, &uctx)) {
    have_uctx = true;
    tc->lock_state = uctx.lock_state;
  } else {
    tc->lock_state = LOCK_SCHED_STATE_NONE;
  }

  if (!tc->run_start_ns) {
    tc->run_start_ns = now;
    return;
  }

  if (now > tc->run_start_ns)
    run_delta = now - tc->run_start_ns;

  tc->run_start_ns = now;

  if (have_uctx) {
    if (uctx.wait_ns_total >= tc->last_wait_ns) {
      __u64 completed_wait = uctx.wait_ns_total - tc->last_wait_ns;

      tc->last_wait_ns = uctx.wait_ns_total;

      if (completed_wait > tc->pending_wait_ns)
        wait_delta += completed_wait - tc->pending_wait_ns;

      tc->pending_wait_ns = 0;
    }

    if (uctx.wait_end_ns < uctx.wait_start_ns && now > uctx.wait_start_ns) {
      __u64 pending_wait = now - uctx.wait_start_ns;

      if (pending_wait > tc->pending_wait_ns)
        wait_delta += pending_wait - tc->pending_wait_ns;

      tc->pending_wait_ns = pending_wait;
    } else {
      tc->pending_wait_ns = 0;
    }
  } else {
    tc->pending_wait_ns = 0;
  }

  tc->run_ns_window += run_delta;
  tc->wait_ns_window += wait_delta;

  if (agg) {
    agg->run_ns += run_delta;
    agg->wait_ns += wait_delta;
  }
}

static __always_inline void publish_ssc_core_vote(struct task_scx_ctx *tc,
                                                  struct task_struct *p,
                                                  __u64 now) {
  __s32 cpu;
  __u16 rank;
  __u32 key;
  struct agg_percpu *agg;
  struct ssc_vote_slot *slot;

  if (!tc)
    return;

  if (!ssc_vote_epoch)
    rotate_ssc_vote_window(now);

  agg = lookup_cpu_agg();
  if (!agg)
    return;

  if (agg->epoch != ssc_vote_epoch) {
    agg->epoch = ssc_vote_epoch;
    agg->run_ns = 0;
    agg->wait_ns = 0;
  }

  cpu = scx_bpf_task_cpu(p);
  if (cpu < 0 || cpu >= MAX_CPUS)
    return;

  rank = ssc_cpu_rank[cpu];
  if (rank >= ssc_active_count || rank >= ssc_cpu_count || rank >= MAX_CPUS)
    return;

  key = (__u32)rank;
  slot = bpf_map_lookup_elem(&ssc_vote_slot_map, &key);
  if (!slot)
    return;

  if (slot->epoch != ssc_vote_epoch) {
    slot->epoch = ssc_vote_epoch;
    slot->last_run_ns = agg->run_ns;
    slot->last_wait_ns = agg->wait_ns;
    ssc_vote_sum_run += agg->run_ns;
    ssc_vote_sum_wait += agg->wait_ns;
    ssc_vote_publish_count++;
    return;
  }

  if (agg->run_ns > slot->last_run_ns) {
    ssc_vote_sum_run += agg->run_ns - slot->last_run_ns;
    slot->last_run_ns = agg->run_ns;
  }

  if (agg->wait_ns > slot->last_wait_ns) {
    ssc_vote_sum_wait += agg->wait_ns - slot->last_wait_ns;
    slot->last_wait_ns = agg->wait_ns;
  }
}

static __always_inline bool try_advance_window(__u64 now) { return false; }

#endif /* __STATS_BPF_H */
