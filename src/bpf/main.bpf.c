/* SPDX-License-Identifier: GPL-2.0-only */
#include <scx/common.bpf.h>

#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

#include "maps.bpf.h"

static __always_inline bool valid_cpu(s32 cpu) {
  return cpu >= 0 && cpu < MAX_CPUS;
}

static __always_inline bool task_cpu_allowed(struct task_struct *p, __u32 cpu) {
  if (cpu >= MAX_CPUS)
    return false;

  return bpf_cpumask_test_cpu(cpu, p->cpus_ptr);
}

static __always_inline __u64 inactive_dsq_id(__u32 cpu) {
  return INACTIVE_DSQ_BASE + cpu;
}

static __always_inline void init_task_ctx_if_needed(struct task_scx_ctx *task_ctx) {
  if (task_ctx->initialized)
    return;

  task_ctx->initialized = 1;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
}

static __always_inline struct task_scx_ctx *lookup_task_ctx(struct task_struct *p) {
  struct task_scx_ctx *task_ctx;

  task_ctx = bpf_task_storage_get(&task_ctx_map, p, 0, 0);
  if (!task_ctx)
    return 0;

  init_task_ctx_if_needed(task_ctx);
  return task_ctx;
}

static __always_inline struct task_scx_ctx *get_task_ctx(struct task_struct *p) {
  struct task_scx_ctx *task_ctx;

  task_ctx =
      bpf_task_storage_get(&task_ctx_map, p, 0, BPF_LOCAL_STORAGE_GET_F_CREATE);
  if (!task_ctx)
    return 0;

  init_task_ctx_if_needed(task_ctx);
  return task_ctx;
}

static __always_inline __u32 *lookup_cpu_owner(__u32 cpu) {
  if (cpu >= MAX_CPUS)
    return 0;

  return bpf_map_lookup_elem(&cpu_admission_owner_map, &cpu);
}

static __always_inline bool refresh_user_ctx_ptr(struct task_struct *p,
                                                 struct task_scx_ctx *task_ctx) {
  __u32 pid = p->pid;
  __u64 *user_ctx_ptr;

  if (task_ctx->user_ctx_ptr)
    return true;

  user_ctx_ptr = bpf_map_lookup_elem(&thread_ctx_addr_map, &pid);
  if (!user_ctx_ptr || !*user_ctx_ptr)
    return false;

  task_ctx->user_ctx_ptr = *user_ctx_ptr;
  return true;
}

static __always_inline bool read_user_ctx(struct task_struct *p,
                                          struct task_scx_ctx *task_ctx,
                                          struct lock_sched_thread_ctx *user_ctx) {
  if (!refresh_user_ctx_ptr(p, task_ctx))
    return false;

  if (bpf_probe_read_user(user_ctx, sizeof(*user_ctx),
                          (const void *)(unsigned long)task_ctx->user_ctx_ptr))
    return false;

  task_ctx->slow_path_pending = user_ctx->slow_path_pending;
  task_ctx->in_critical_section = user_ctx->in_critical_section;
  return true;
}

static __always_inline bool user_explicit_release(
    const struct lock_sched_thread_ctx *user_ctx) {
  return !user_ctx->admission_owned && !user_ctx->slow_path_pending &&
         !user_ctx->in_critical_section;
}

static __always_inline void clear_admission_state(struct task_scx_ctx *task_ctx) {
  task_ctx->admitted = 0;
  task_ctx->holds_admission = 0;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->inactive_wait = 0;
  task_ctx->slow_path_pending = 0;
  task_ctx->in_critical_section = 0;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
}

static __always_inline void release_admission(struct task_struct *p,
                                              struct task_scx_ctx *task_ctx) {
  __u32 cpu = task_ctx->admission_cpu;
  __u32 pid = p->pid;
  __u32 *owner;

  owner = lookup_cpu_owner(cpu);
  if (owner && *owner == pid)
    *owner = 0;

  clear_admission_state(task_ctx);

  if (cpu < MAX_CPUS)
    scx_bpf_kick_cpu(cpu, 0);
}

static __always_inline bool grant_admission(struct task_struct *p,
                                            struct task_scx_ctx *task_ctx,
                                            __u32 cpu) {
  __u32 pid = p->pid;
  __u32 *owner;

  owner = lookup_cpu_owner(cpu);
  if (!owner)
    return false;

  if (*owner && *owner != pid)
    return false;

  *owner = pid;
  task_ctx->admitted = 1;
  task_ctx->holds_admission = 1;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->inactive_wait = 0;
  task_ctx->admission_cpu = cpu;
  return true;
}

static __always_inline __u32 pick_allowed_cpu(struct task_struct *p,
                                              s32 fallback_cpu) {
  s32 cpu;

  if (valid_cpu(fallback_cpu) && task_cpu_allowed(p, (__u32)fallback_cpu))
    return fallback_cpu;

  cpu = scx_bpf_task_cpu(p);
  if (valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
    return cpu;

  cpu = scx_bpf_pick_idle_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu))
    return cpu;

  cpu = scx_bpf_pick_any_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu))
    return cpu;

  return ADMISSION_CPU_NONE;
}

static __always_inline __u32 requested_cpu(struct task_struct *p,
                                           struct task_scx_ctx *task_ctx,
                                           s32 fallback_cpu) {
  if (task_ctx->admission_cpu < MAX_CPUS &&
      task_cpu_allowed(p, task_ctx->admission_cpu))
    return task_ctx->admission_cpu;

  return pick_allowed_cpu(p, fallback_cpu);
}

static __always_inline void clear_invalid_admission_cpu(
    struct task_struct *p, struct task_scx_ctx *task_ctx) {
  if (task_ctx->admission_cpu < MAX_CPUS &&
      !task_cpu_allowed(p, task_ctx->admission_cpu))
    release_admission(p, task_ctx);
}

static __always_inline bool slow_path_requested(
    const struct task_scx_ctx *task_ctx, bool have_user,
    const struct lock_sched_thread_ctx *user_ctx) {
  if (task_ctx->holds_admission)
    return false;

  if (have_user)
    return user_ctx->slow_path_pending;

  return task_ctx->slow_path_pending;
}

static __always_inline bool should_release_from_user(
    const struct task_scx_ctx *task_ctx, bool have_user,
    const struct lock_sched_thread_ctx *user_ctx) {
  if (!task_ctx->holds_admission || !have_user)
    return false;

  return user_explicit_release(user_ctx);
}

static __always_inline void refresh_running_state(struct task_struct *p) {
  struct task_scx_ctx *task_ctx;
  struct lock_sched_thread_ctx user_ctx = {};
  bool have_user = false;
  __u32 cpu = bpf_get_smp_processor_id();

  task_ctx = get_task_ctx(p);
  if (!task_ctx)
    return;

  have_user = read_user_ctx(p, task_ctx, &user_ctx);
  if (should_release_from_user(task_ctx, have_user, &user_ctx)) {
    release_admission(p, task_ctx);
    return;
  }

  if (task_ctx->must_run_on_admission_cpu && task_ctx->admission_cpu == cpu)
    task_ctx->must_run_on_admission_cpu = 0;
}

s32 BPF_STRUCT_OPS(accordin_select_cpu, struct task_struct *p, s32 prev_cpu,
                   u64 wake_flags) {
  struct task_scx_ctx *task_ctx;
  struct lock_sched_thread_ctx user_ctx = {};
  bool is_idle = false;
  bool have_user = false;
  s32 cpu;

  task_ctx = get_task_ctx(p);
  if (task_ctx)
    have_user = read_user_ctx(p, task_ctx, &user_ctx);

  if (task_ctx)
    clear_invalid_admission_cpu(p, task_ctx);

  if (task_ctx && task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu))
    return (s32)task_ctx->admission_cpu;

  if (task_ctx && slow_path_requested(task_ctx, have_user, &user_ctx) &&
      valid_cpu(prev_cpu) && task_cpu_allowed(p, (__u32)prev_cpu)) {
    task_ctx->admission_cpu = (__u32)prev_cpu;
    return prev_cpu;
  }

  cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

  if (task_ctx && slow_path_requested(task_ctx, have_user, &user_ctx) &&
      valid_cpu(cpu))
    task_ctx->admission_cpu = (__u32)cpu;

  if (is_idle && !(task_ctx && slow_path_requested(task_ctx, have_user, &user_ctx)))
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

  return cpu;
}

void BPF_STRUCT_OPS(accordin_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *task_ctx;
  struct lock_sched_thread_ctx user_ctx = {};
  bool have_user = false;
  __u32 cpu;

  task_ctx = get_task_ctx(p);
  if (!task_ctx) {
    scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
    return;
  }

  have_user = read_user_ctx(p, task_ctx, &user_ctx);

  clear_invalid_admission_cpu(p, task_ctx);

  if (should_release_from_user(task_ctx, have_user, &user_ctx))
    release_admission(p, task_ctx);

  if (task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu)) {
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | task_ctx->admission_cpu,
                       SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (slow_path_requested(task_ctx, have_user, &user_ctx)) {
    cpu = requested_cpu(p, task_ctx, -1);
    if (cpu >= MAX_CPUS) {
      scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
      return;
    }
    task_ctx->admission_cpu = cpu;

    if (grant_admission(p, task_ctx, cpu)) {
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
      return;
    }

    task_ctx->inactive_wait = 1;
    scx_bpf_dsq_insert(p, inactive_dsq_id(cpu), SCX_SLICE_DFL, enq_flags);
    return;
  }

  task_ctx->inactive_wait = 0;
  scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
}

void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
  __u32 *owner;

  (void)prev;

  if (valid_cpu(cpu)) {
    owner = lookup_cpu_owner((__u32)cpu);
    if (owner && !*owner && scx_bpf_dsq_move_to_local(inactive_dsq_id((__u32)cpu)))
      return;
  }

  scx_bpf_dsq_move_to_local(READY_DSQ_ID);
}

void BPF_STRUCT_OPS(accordin_running, struct task_struct *p) {
  refresh_running_state(p);
}

void BPF_STRUCT_OPS(accordin_tick, struct task_struct *p) {
  refresh_running_state(p);
}

void BPF_STRUCT_OPS(accordin_stopping, struct task_struct *p, bool runnable) {
  struct task_scx_ctx *task_ctx;
  struct lock_sched_thread_ctx user_ctx = {};
  bool have_user = false;

  (void)runnable;

  task_ctx = lookup_task_ctx(p);
  if (!task_ctx || !task_ctx->holds_admission)
    return;

  have_user = read_user_ctx(p, task_ctx, &user_ctx);

  if (should_release_from_user(task_ctx, have_user, &user_ctx)) {
    release_admission(p, task_ctx);
    return;
  }

  if ((have_user && user_ctx.in_critical_section) ||
      (!have_user && task_ctx->in_critical_section)) {
    release_admission(p, task_ctx);
    return;
  }

  task_ctx->must_run_on_admission_cpu = 1;
}

void BPF_STRUCT_OPS(accordin_exit_task, struct task_struct *p,
                    struct scx_exit_task_args *args) {
  struct task_scx_ctx *task_ctx;
  __u32 pid = p->pid;

  (void)args;

  task_ctx = lookup_task_ctx(p);
  if (task_ctx && task_ctx->holds_admission)
    release_admission(p, task_ctx);

  bpf_map_delete_elem(&thread_ctx_addr_map, &pid);
  bpf_task_storage_delete(&task_ctx_map, p);
}

s32 BPF_STRUCT_OPS_SLEEPABLE(accordin_init) {
  __u32 cpu;
  __u32 nr_cpus = scx_bpf_nr_cpu_ids();
  s32 ret;

  if (nr_cpus > MAX_CPUS)
    nr_cpus = MAX_CPUS;

  ret = scx_bpf_create_dsq(READY_DSQ_ID, -1);
  if (ret)
    return ret;

  for (cpu = 0; cpu < nr_cpus; cpu++) {
    ret = scx_bpf_create_dsq(inactive_dsq_id(cpu), -1);
    if (ret)
      return ret;
  }

  return 0;
}

void BPF_STRUCT_OPS(accordin_exit, struct scx_exit_info *ei) {
  UEI_RECORD(uei, ei);
}

SCX_OPS_DEFINE(accordin_ops, .select_cpu = (void *)accordin_select_cpu,
               .enqueue = (void *)accordin_enqueue,
               .dispatch = (void *)accordin_dispatch,
               .running = (void *)accordin_running,
               .tick = (void *)accordin_tick,
               .stopping = (void *)accordin_stopping,
               .exit_task = (void *)accordin_exit_task,
               .init = (void *)accordin_init, .exit = (void *)accordin_exit,
               .name = "accordin");
