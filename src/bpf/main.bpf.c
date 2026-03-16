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

#include "maps.bpf.h"
#include "stats.bpf.h"
#include "admission.bpf.h"

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

  // if the task is lock contention, it should enter SSC DSQ
  /* Owner is always admitted */
  if (tc) {
    scx_bpf_dsq_insert(p, SSC_DSQ_ID, SCX_SLICE_DFL, enq_flags);
    return;
  }
}

void BPF_STRUCT_OPS(lb_simple_dispatch, s32 cpu, struct task_struct *prev) {
  if (stats_only_mode) {
    scx_bpf_dsq_move_to_local(READY_DSQ_ID);
    return;
  }

  if (scx_bpf_dsq_nr_queued(SSC_DSQ_ID) > 0) {
    // if the core is SSC cores, it should prioritize dispatching SSC waiters
    //
    // but if there is some task with the cpu mask under timeout in global queue
    // it should also prioritize dispatching those tasks to avoid starvation
    //
  } else {
    // 如果有SSC DSQ中快要超时的任务，也进行调度
    // 否则即使CPU空闲也不调度SSC_DSQ中的任务，避免过早调度导致的频繁切换
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

void BPF_STRUCT_OPS(lb_simple_stopping, struct task_struct *p, bool runnable) {
  __u32 pid = p->pid;
  struct task_scx_ctx *tc = lookup_task_ctx(p);
  if (!tc)
    return;

  /* Window advance moved to tick() — only CPU 0 advances */

  if (stats_only_mode)
    return;

  // we should determine if the task should be parked (i.e. move to SSC) based
  // on its context and current state
}

void BPF_STRUCT_OPS(lb_simple_tick, struct task_struct *p) {
  __u32 pid = p->pid;
  struct task_scx_ctx *tc = lookup_task_ctx(p);
  // scx_bpf_now is efficient than bpf_task_storage_delete
  __u64 now = scx_bpf_now();
  __s32 cpu = scx_bpf_task_cpu(p);

  if (tc) {
    account_task_activity(tc, pid, now);
  }

  if (stats_only_mode)
    return;

  // try_advance_window(now);
  /*
   * If active count is above target, force a reschedule so the
   * current task enters stopping() -> self-parking sooner.
   */
  // p->scx.slice = 0;
}

void BPF_STRUCT_OPS(lb_simple_exit_task, struct task_struct *p,
                    struct scx_exit_task_args *args) {
  // do some cleanup if needed
  __u32 pid = p->pid;
  struct task_scx_ctx *tc = lookup_task_ctx(p);

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
               .stopping = (void *)lb_simple_stopping,
               .tick = (void *)lb_simple_tick,
               .exit_task = (void *)lb_simple_exit_task,
               .init = (void *)lb_simple_init, .exit = (void *)lb_simple_exit,
               .name = "lb_simple");
