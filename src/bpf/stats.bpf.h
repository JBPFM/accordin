/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __STATS_BPF_H
#define __STATS_BPF_H

/* Keep this header self-contained for standalone clangd parsing. */
#include <scx/common.bpf.h>

#include "intf.h"
#include "maps.bpf.h"

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
  struct lock_sched_thread_ctx uctx = {};

  (void)pid;
  dbg_acct_calls++;

  if (!tc->run_start_ns) {
    tc->run_start_ns = now;
    return;
  }

  if (now > tc->run_start_ns)
    run_delta = now - tc->run_start_ns;

  tc->run_start_ns = now;

  if (tc->user_ctx_ptr) {
    if (read_thread_ctx(tc->user_ctx_ptr, &uctx)) {
      if (uctx.wait_ns_total >= tc->last_wait_ns) {
        wait_delta = uctx.wait_ns_total - tc->last_wait_ns;
        tc->last_wait_ns = uctx.wait_ns_total;
      }

      if (uctx.wait_end_ns < uctx.wait_start_ns && now > uctx.wait_start_ns) {
        __u64 pending_wait = now - uctx.wait_start_ns;
        __u64 *wait_start_ptr =
            (__u64 *)(unsigned long)(tc->user_ctx_ptr +
                                     __builtin_offsetof(
                                         struct lock_sched_thread_ctx,
                                         wait_start_ns));

        if (bpf_probe_write_user(wait_start_ptr, &now, sizeof(now)) == 0)
          wait_delta += pending_wait;
      }
    }
  }

  tc->run_ns_window += run_delta;
  tc->wait_ns_window += wait_delta;
}

static __always_inline bool try_advance_window(__u64 now) { return false; }

#endif /* __STATS_BPF_H */
