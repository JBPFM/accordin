/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * lb_simple - sched_ext lock-aware SSC admission control scheduler.
 *
 * Continuously observes per-thread lock wait statistics exported from
 * userspace, computes workload-level wait ratios, and uses a single
 * SSC (Scheduling Suppression Chamber) DSQ to throttle concurrency
 * via admission control.
 */
#include <scx/common.bpf.h>

#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

#include "admission.bpf.h"
#include "maps.bpf.h"
#include "stats.bpf.h"

/* ------------------------------------------------------------------ */
/*  Callbacks                                                          */
/* ------------------------------------------------------------------ */

s32 BPF_STRUCT_OPS(lb_simple_select_cpu, struct task_struct *p, s32 prev_cpu,
                   u64 wake_flags) {
  bool is_idle = false;
  s32 cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);
  if (is_idle) {
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);
  }

  return cpu;
}

void BPF_STRUCT_OPS(lb_simple_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *tc = lookup_task_ctx(p);
  if (!tc) {
    /* Untracked task (not yet seen in running()) — let it run. */
    scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (is_task_lock_protected(tc)) {
    tc->admitted = 1;
    scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (tc->admitted) {
    scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
    return;
  }

  scx_bpf_dsq_insert(p, SSC_DSQ_ID, SCX_SLICE_DFL, enq_flags);
}

void BPF_STRUCT_OPS(lb_simple_dispatch, s32 cpu, struct task_struct *prev) {
  if (stats_only_mode) {
    scx_bpf_dsq_move_to_local(READY_DSQ_ID);
    return;
  }

  if (is_cpu_ssc_core(cpu) && scx_bpf_dsq_nr_queued(SSC_DSQ_ID) > 0) {
    scx_bpf_dsq_move_to_local(SSC_DSQ_ID);
  } else {
    scx_bpf_dsq_move_to_local(READY_DSQ_ID);
  }

  /* Regular dispatch from READY_DSQ */
  scx_bpf_dsq_move_to_local(READY_DSQ_ID);
}

void BPF_STRUCT_OPS(lb_simple_running, struct task_struct *p) {
  struct task_scx_ctx *tc = get_or_create_task_ctx(p);
  if (!tc)
    return;

  tc->run_start_ns = scx_bpf_now();
}

// void BPF_STRUCT_OPS(lb_simple_stopping, struct task_struct *p, bool runnable)
// {
//   __u32 pid = p->pid;
//   struct task_scx_ctx *tc = lookup_task_ctx(p);
//   if (!tc)
//     return;
//
//   /* Window advance moved to tick() — only CPU 0 advances */
//
//   if (stats_only_mode)
//     return;
//
//   // we should determine if the task should be parked (i.e. move to SSC)
//   based
//   // on its context and current state
// }

void BPF_STRUCT_OPS(lb_simple_tick, struct task_struct *p) {
  __u32 pid = p->pid;
  struct task_scx_ctx *tc = lookup_task_ctx(p);
  bool lock_protected = false;
  // scx_bpf_now is efficient than bpf_task_storage_delete
  __u64 now = scx_bpf_now();

  maybe_rotate_ssc_vote_window(now);

  if (tc) {
    account_task_activity(tc, pid, now);
    lock_protected = is_task_lock_protected(tc);
  }

  if (stats_only_mode)
    return;

  if (is_task_on_ssc_core(p)) {
    publish_ssc_core_vote(tc, p, now);

    if (ssc_vote_publish_count * 2 > ssc_active_count) {
      __u64 score = compute_ssc_vote_score(ssc_active_count);
      __u64 wait_ratio = 0;
      bool shifted = ssc_search_phase == SSC_SEARCH_REFINE &&
                     detect_ssc_workload_shift(now, &wait_ratio);

      (void)wait_ratio;

      if (!ssc_vote_last_effective_score)
        ssc_vote_last_effective_score = score;
      if (!ssc_best_score) {
        ssc_best_score = score;
        ssc_best_count = ssc_active_count;
        reset_ssc_refine_bounds(ssc_active_count);
      }

      if (shifted) {
        ssc_search_phase = SSC_SEARCH_SEEK;

        if (ssc_best_count != ssc_active_count)
          ssc_set_active_count(ssc_best_count, ssc_best_score);
        else
          ssc_note_resize(ssc_best_score);

        ssc_best_count = ssc_active_count;
        reset_ssc_refine_bounds(ssc_active_count);

        rotate_ssc_vote_window(now);
        return;
      }

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
      if (score > ssc_best_score) {
        ssc_best_score = score;
        ssc_best_count = ssc_active_count;
      }

      if (ssc_search_phase == SSC_SEARCH_REFINE) {
        __u32 next_target;

        if (score >= ssc_best_score) {
          ssc_best_score = score;
          ssc_best_count = ssc_active_count;
          if (ssc_active_count > ssc_refine_low)
            ssc_refine_low = ssc_active_count;
        } else if (ssc_active_count > ssc_refine_low) {
          ssc_refine_high = ssc_active_count;
        }

        next_target = ssc_next_refine_target();
        if (next_target != ssc_active_count)
          ssc_set_active_count(next_target, ssc_best_score);
      } else if (ssc_vote_consec_grow >= 2) {
        ssc_best_count = ssc_active_count;
        ssc_best_score = score;
        ssc_set_active_count(ssc_active_count << 1, score);
        reset_ssc_refine_bounds(ssc_active_count);
      } else if (ssc_vote_consec_shrink >= 2) {
        ssc_enter_refine_mode(ssc_best_count, ssc_active_count, score);
        ssc_set_active_count(ssc_next_refine_target(), ssc_best_score);
      }

      rotate_ssc_vote_window(now);
    }

    if (lock_protected)
      keep_task_lock_protected(p, tc);

  } else {
    if (lock_protected) {
      keep_task_lock_protected(p, tc);
      return;
    }

    /*
     * If active count is above target, force a reschedule so the
     * current task enters stopping() -> self-parking sooner.
     */
    if (tc && tc->run_ns_window / 10 < tc->wait_ns_window) {
      tc->admitted = 0;
      p->scx.slice = 0;
    }
  }
}

void BPF_STRUCT_OPS(lb_simple_exit_task, struct task_struct *p,
                    struct scx_exit_task_args *args) {
  // do some cleanup if needed
  __u32 pid = p->pid;

  bpf_task_storage_delete(&task_ctx_map, p);
  bpf_map_delete_elem(&thread_ctx_addr_map, &pid);
}

s32 BPF_STRUCT_OPS_SLEEPABLE(lb_simple_init) {
  s32 ret;

  ret = scx_bpf_create_dsq(READY_DSQ_ID, -1);
  if (ret)
    return ret;

  ret = scx_bpf_create_dsq(SSC_DSQ_ID, -1);
  if (ret)
    return ret;

  return 0;
}

void BPF_STRUCT_OPS(lb_simple_exit, struct scx_exit_info *ei) {
  UEI_RECORD(uei, ei);
}

SCX_OPS_DEFINE(lb_simple_ops, .select_cpu = (void *)lb_simple_select_cpu,
               .enqueue = (void *)lb_simple_enqueue,
               .dispatch = (void *)lb_simple_dispatch,
               .running = (void *)lb_simple_running,
               // .stopping = (void *)lb_simple_stopping,
               .tick = (void *)lb_simple_tick,
               .exit_task = (void *)lb_simple_exit_task,
               .init = (void *)lb_simple_init, .exit = (void *)lb_simple_exit,
               .name = "lb_simple");
