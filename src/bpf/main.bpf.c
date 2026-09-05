/* SPDX-License-Identifier: GPL-2.0-only */
#include <scx/common.bpf.h>
#include "maps.bpf.h"

char _license[] SEC("license") = "GPL";
UEI_DEFINE(uei);

static __always_inline volatile __u64 *owner_slot(__u32 cpu) {
  barrier_var(cpu);
  return cpu < MAX_CPUS ? &admission.owners[cpu] : 0;
}

static __always_inline bool allowed(struct task_struct *p, __u32 cpu) {
  return cpu < MAX_CPUS && bpf_cpumask_test_cpu(cpu, p->cpus_ptr);
}

static __always_inline struct task_scx_ctx *task_ctx(struct task_struct *p) {
  return bpf_task_storage_get(&task_ctx_map, p, 0, 0);
}

static __always_inline bool user_state(struct task_struct *p, __u32 *state) {
  __u32 tid = p->pid;
  __u64 *address = bpf_map_lookup_elem(&thread_ctx_addr_map, &tid);
  *state = 0;
  if (!address)
    return true;
  /* A failed read is not a release, and must not revoke a spinning waiter. */
  return !bpf_probe_read_user(state, sizeof(*state), (const void *)*address);
}

static __always_inline void release_slot(struct task_struct *p,
                                        struct task_scx_ctx *tctx) {
  __u32 assigned = tctx->admission_cpu;
  volatile __u64 *owner;

  if (!assigned)
    return;
  owner = owner_slot(assigned - 1);
  if (owner)
    __sync_val_compare_and_swap(owner, tctx->ticket, 0);
  tctx->admission_cpu = 0;
  scx_bpf_kick_cpu(assigned - 1, 0);
}

static __always_inline __u64 request_ticket(struct task_struct *p, __u32 state) {
  return ((__u64)(state & ~USER_FLAGS) << 32) | (__u32)p->pid;
}

/* Unless renewed at yield, a new request retires the old slot.
 * A changed affinity cannot park an existing MCS node behind its successor. */
static __always_inline void refresh_episode(struct task_struct *p,
                                            struct task_scx_ctx *tctx,
                                            __u32 state) {
  if (!(state & USER_FLAGS) || tctx->ticket != request_ticket(p, state)) {
    release_slot(p, tctx);
  } else if (tctx->admission_cpu && !allowed(p, tctx->admission_cpu - 1)) {
    release_slot(p, tctx);
  }
}

s32 BPF_STRUCT_OPS(accordin_select_cpu, struct task_struct *p, s32 prev_cpu,
                   u64 wake_flags) {
  struct task_scx_ctx *tctx = task_ctx(p);
  bool idle = false;

  if (!stats_only_mode && tctx && tctx->admission_cpu &&
      allowed(p, tctx->admission_cpu - 1))
    return tctx->admission_cpu - 1;
  /* All tasks pass enqueue, including wakeups on an idle CPU. */
  return scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &idle);
}

void BPF_STRUCT_OPS(accordin_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *tctx;
  __u64 dsq = NORMAL_DSQ;
  __u32 cpu = scx_bpf_task_cpu(p);
  __u32 state;

  if (!stats_only_mode) {
    bool known = user_state(p, &state);
    bool holder = known && (state & USER_FLAGS) == USER_HELD;
    tctx = bpf_task_storage_get(&task_ctx_map, p, 0,
                              BPF_LOCAL_STORAGE_GET_F_CREATE);
    if (tctx) {
      if (known)
        refresh_episode(p, tctx, state);
      /* Where the task runs next, decided once: an admitted task returns to
       * its reserved CPU, a holder preempts whatever occupies the CPU it is
       * bound to, because a holder left in the global queue stalls every
       * spinner that wants its lock, and a new waiter joins the admission
       * queue. */
      if (tctx->admission_cpu) {
        cpu = tctx->admission_cpu - 1;
        dsq = SCX_DSQ_LOCAL_ON | cpu;
        if (holder)
          enq_flags |= SCX_ENQ_PREEMPT;
      } else if (holder && allowed(p, cpu)) {
        dsq = SCX_DSQ_LOCAL_ON | cpu;
        enq_flags |= SCX_ENQ_PREEMPT;
      } else if (known && (state & USER_FLAGS) == USER_WAITING) {
        tctx->ticket = request_ticket(p, state);
        dsq = WAITING_DSQ;
      }
    }
  }
  scx_bpf_dsq_insert(p, dsq, SCX_SLICE_DFL, enq_flags);
  scx_bpf_kick_cpu(cpu, SCX_KICK_IDLE);
}

/* Reserve both the task and this CPU before moving a candidate. Different CPUs
 * may examine the same global queue concurrently; a failed move rolls back both
 * reservations. There is no path that admits a new waiter onto an occupied CPU. */
static __always_inline void admit_waiter(__u32 cpu) {
  volatile __u64 *owner = owner_slot(cpu);
  struct task_struct *p;

  if (!owner || *owner)
    return;
  bpf_for_each(scx_dsq, p, WAITING_DSQ, 0) {
    struct task_scx_ctx *tctx;

    if (!allowed(p, cpu))
      continue;
    tctx = task_ctx(p);
    if (!tctx || __sync_val_compare_and_swap(&tctx->admission_cpu, 0, cpu + 1))
      continue;
    if (__sync_val_compare_and_swap(owner, 0, tctx->ticket)) {
      tctx->admission_cpu = 0;
      return;
    }
    if (__COMPAT_scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, SCX_DSQ_LOCAL, 0))
      return;
    release_slot(p, tctx);
  }
}

void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
  (void)prev;
  /* Serve ordinary work alongside the reserved waiter. This also lets an
   * unadmitted fast-path holder run and unlock while the waiter is spinning. */
  scx_bpf_dsq_move_to_local(NORMAL_DSQ);
  if (stats_only_mode || cpu < 0 || cpu >= MAX_CPUS)
    return;
  admit_waiter((__u32)cpu);
}

bool BPF_STRUCT_OPS(accordin_yield, struct task_struct *from, struct task_struct *to) {
  struct task_scx_ctx *tctx = task_ctx(from);
  __u32 cpu = bpf_get_smp_processor_id(), state;
  volatile __u64 *owner = owner_slot(cpu);

  (void)to;
  /* Renew only our existing slot; userspace still confirms the new ticket. */
  if (!stats_only_mode && tctx && owner && user_state(from, &state) &&
      (state & USER_FLAGS) == USER_WAITING && tctx->admission_cpu == cpu + 1) {
    __u64 next = request_ticket(from, state);
    if (__sync_val_compare_and_swap(owner, tctx->ticket, next) == tctx->ticket)
      tctx->ticket = next;
  }
  from->scx.slice = 0;
  return false;
}

void BPF_STRUCT_OPS(accordin_tick, struct task_struct *p) {
  struct task_scx_ctx *tctx = task_ctx(p);
  __u32 state;

  if (stats_only_mode || !tctx || !user_state(p, &state))
    return;
  refresh_episode(p, tctx, state);
  /* An admitted thread that spins never empties its CPU, so ops.dispatch is
   * never called there and the global queue is never consumed. Ending the
   * slice hands the CPU to one queued ordinary task; the spinner keeps its
   * slot and is re-enqueued behind it. */
  if (tctx->admission_cpu &&
      ((state & USER_FLAGS) == USER_WAITING ||
       (state & USER_FLAGS) == USER_SPINNING) &&
      scx_bpf_dsq_nr_queued(NORMAL_DSQ))
    p->scx.slice = 0;
}

void BPF_STRUCT_OPS(accordin_stopping, struct task_struct *p, bool runnable) {
  struct task_scx_ctx *tctx = task_ctx(p);
  __u32 state;

  if (stats_only_mode || !tctx || !user_state(p, &state))
    return;
  refresh_episode(p, tctx, state);
  /* A holder or an existing raw-lock node must be allowed to resume. */
  if (!runnable && (state & USER_FLAGS) != USER_HELD &&
      (state & USER_FLAGS) != USER_SPINNING)
    release_slot(p, tctx);
}

void BPF_STRUCT_OPS(accordin_exit_task, struct task_struct *p,
                    struct scx_exit_task_args *args) {
  struct task_scx_ctx *tctx = task_ctx(p);
  __u32 tid = p->pid;

  (void)args;
  if (tctx)
    release_slot(p, tctx);
  bpf_map_delete_elem(&thread_ctx_addr_map, &tid);
  bpf_task_storage_delete(&task_ctx_map, p);
}

void BPF_STRUCT_OPS(accordin_dump, struct scx_dump_ctx *dump_ctx) {
  __u32 cpu;

  (void)dump_ctx;
  scx_bpf_dump("accordin normal=%d waiting=%d\n",
               scx_bpf_dsq_nr_queued(NORMAL_DSQ),
               scx_bpf_dsq_nr_queued(WAITING_DSQ));
  bpf_for(cpu, 0, MAX_CPUS) {
    volatile __u64 *owner = owner_slot(cpu);
    if (owner && *owner)
      scx_bpf_dump("accordin cpu=%u owner=%u\n", cpu, (__u32)*owner);
  }
}

s32 BPF_STRUCT_OPS_SLEEPABLE(accordin_init) {
  s32 ret;

  ret = scx_bpf_create_dsq(NORMAL_DSQ, -1);
  if (ret)
    return ret;
  ret = scx_bpf_create_dsq(WAITING_DSQ, -1);
  if (ret)
    return ret;
  admission.enabled = !stats_only_mode;
  return 0;
}

void BPF_STRUCT_OPS(accordin_exit, struct scx_exit_info *ei) {
  admission.enabled = 0;
  UEI_RECORD(uei, ei);
}

SCX_OPS_DEFINE(accordin_ops,
               .select_cpu = (void *)accordin_select_cpu,
               .enqueue = (void *)accordin_enqueue,
               .dispatch = (void *)accordin_dispatch,
               .yield = (void *)accordin_yield,
               .tick = (void *)accordin_tick,
               .stopping = (void *)accordin_stopping,
               .exit_task = (void *)accordin_exit_task,
               .dump = (void *)accordin_dump,
               .init = (void *)accordin_init,
               .exit = (void *)accordin_exit,
               .name = "accordin");
