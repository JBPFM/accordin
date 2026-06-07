/* SPDX-License-Identifier: GPL-2.0-only */
#include <scx/common.bpf.h>

#include "intf.h"
#include "inactive_select.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

#include "maps.bpf.h"

static __always_inline bool valid_cpu(s32 cpu) {
  return cpu >= 0 && cpu < MAX_CPUS;
}

static __always_inline bool stats_only_enabled(void) {
  return stats_only_mode != 0;
}

static __always_inline bool single_lock_enabled(void) {
  return single_lock_mode != 0;
}

static __always_inline bool task_cpumask_allows(struct task_struct *p,
                                                __u32 cpu) {
  if (cpu >= MAX_CPUS)
    return false;

  return bpf_cpumask_test_cpu(cpu, p->cpus_ptr);
}

static __always_inline bool task_cpu_allowed(struct task_struct *p, __u32 cpu) {
  if (cpu >= MAX_CPUS)
    return false;

  return task_cpumask_allows(p, cpu);
}

static __always_inline bool valid_lock_id(__u32 lock_id) {
  return lock_id != UNMANAGED_LOCK_ID && lock_id < MAX_LOCK_CLASSES;
}

static __always_inline __u32 user_lock_id(__u32 user_ctx_word) {
  return (user_ctx_word & ~USER_ADMISSION_FLAG_MASK) >>
         USER_ADMISSION_LOCK_ID_SHIFT;
}

static __always_inline __u32 effective_lock_id(__u32 user_ctx_word) {
  if (single_lock_enabled())
    return 1;

  return user_lock_id(user_ctx_word);
}

static __always_inline __u64 inactive_dsq_id(__u32 lock_id) {
  return INACTIVE_DSQ_BASE + lock_id;
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

static __always_inline volatile __u32 *lookup_cpu_owner(__u32 cpu) {
  __u32 idx;

  if (cpu >= MAX_CPUS)
    return 0;

  idx = cpu & (MAX_CPUS - 1);
  barrier_var(idx);
  return &cpu_admission_owner[idx];
}

static __always_inline volatile __u32 *lookup_cpu_last_inactive_lock(__u32 cpu) {
  __u32 idx;

  if (cpu >= MAX_CPUS)
    return 0;

  idx = cpu & (MAX_CPUS - 1);
  barrier_var(idx);
  return &cpu_last_inactive_lock[idx];
}

static __always_inline volatile __u32 *
lookup_cpu_inactive_dispatch_count(__u32 cpu) {
  __u32 idx;

  if (cpu >= MAX_CPUS)
    return 0;

  idx = cpu & (MAX_CPUS - 1);
  barrier_var(idx);
  return &cpu_inactive_dispatch_count[idx];
}

static __always_inline bool inactive_dispatch_budget_available(__u32 cpu) {
  volatile __u32 *count = lookup_cpu_inactive_dispatch_count(cpu);

  return !count || *count < INACTIVE_DISPATCH_BURST;
}

static __always_inline void record_inactive_dispatch(__u32 cpu) {
  volatile __u32 *count = lookup_cpu_inactive_dispatch_count(cpu);

  if (count && *count < INACTIVE_DISPATCH_BURST)
    *count += 1;
}

static __always_inline void reset_inactive_dispatch_budget(__u32 cpu) {
  volatile __u32 *count = lookup_cpu_inactive_dispatch_count(cpu);

  if (count)
    *count = 0;
}

static __always_inline void record_inactive_dispatch_lock(__u32 cpu,
                                                          __u32 lock_id) {
  volatile __u32 *last_lock;

  if (!valid_lock_id(lock_id))
    return;

  last_lock = lookup_cpu_last_inactive_lock(cpu);
  if (last_lock)
    *last_lock = lock_id;
}

static __always_inline bool move_inactive_to_local(__u32 dispatch_cpu,
                                                   __u32 lock_id) {
  if (!valid_lock_id(lock_id))
    return false;

  if (scx_bpf_dsq_move_to_local(inactive_dsq_id(lock_id))) {
    record_inactive_dispatch_lock(dispatch_cpu, lock_id);
    return true;
  }

  return false;
}

static __always_inline bool move_selected_inactive_to_local(__u32 dispatch_cpu) {
  volatile __u32 *last_lock_ptr;
  __u32 previous_lock_id = UNMANAGED_LOCK_ID;
  __u32 random = bpf_get_prandom_u32();
  __u32 random_start = random / INACTIVE_PROBABILITY_SCALE;
  bool prefer_previous =
      inactive_prefer_previous_lock(random, inactive_previous_lock_percent);
  __u32 offset;

  last_lock_ptr = lookup_cpu_last_inactive_lock(dispatch_cpu);
  if (last_lock_ptr)
    previous_lock_id = *last_lock_ptr;

  if (valid_lock_id(previous_lock_id) && prefer_previous &&
      move_inactive_to_local(dispatch_cpu, previous_lock_id))
    return true;

#pragma unroll
  for (offset = 0; offset < MAX_LOCK_CLASSES; offset++) {
    __u32 lock_id =
        inactive_other_lock_at(previous_lock_id, random_start, offset);

    if (!valid_lock_id(lock_id))
      break;

    if (move_inactive_to_local(dispatch_cpu, lock_id))
      return true;
  }

  if (valid_lock_id(previous_lock_id) && !prefer_previous)
    return move_inactive_to_local(dispatch_cpu, previous_lock_id);

  return false;
}

static __always_inline bool drain_inactive(__u32 cpu) {
  volatile __u32 *owner;

  owner = lookup_cpu_owner(cpu);
  if (!owner || *owner)
    return false;

  return move_selected_inactive_to_local(cpu);
}

static __always_inline bool drain_inactive_counted(__u32 cpu) {
  if (drain_inactive(cpu)) {
    record_inactive_dispatch(cpu);
    return true;
  }

  return false;
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
                                          __u32 *user_ctx_word) {
  if (!refresh_user_ctx_ptr(p, task_ctx))
    return false;

  if (bpf_probe_read_user(user_ctx_word, sizeof(*user_ctx_word),
                          (const void *)(unsigned long)task_ctx->user_ctx_ptr))
    return false;

  return true;
}

static __always_inline bool user_slow_path_pending(__u32 user_ctx_word) {
  return user_ctx_word & USER_ADMISSION_SLOW_PATH_PENDING;
}

static __always_inline bool user_in_critical_section(__u32 user_ctx_word) {
  return user_ctx_word & USER_ADMISSION_IN_CRITICAL_SECTION;
}

static __always_inline bool user_token_consumed(__u32 user_ctx_word) {
  return user_ctx_word & USER_ADMISSION_TOKEN_CONSUMED;
}

static __always_inline bool user_explicit_release(__u32 user_ctx_word) {
  return !user_slow_path_pending(user_ctx_word) &&
         !user_in_critical_section(user_ctx_word);
}

static __always_inline void clear_admission_state(struct task_scx_ctx *task_ctx) {
  task_ctx->holds_admission = 0;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->force_inactive_wait = 0;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
}

static __always_inline void release_admission(struct task_struct *p,
                                              struct task_scx_ctx *task_ctx) {
  __u32 cpu = task_ctx->admission_cpu;
  __u32 pid = p->pid;
  volatile __u32 *owner;

  owner = lookup_cpu_owner(cpu);
  if (owner && *owner == pid)
    *owner = 0;

  clear_admission_state(task_ctx);

  if (cpu < MAX_CPUS)
    scx_bpf_kick_cpu(cpu, 0);
}

static __always_inline bool grant_admission(struct task_struct *p,
                                            struct task_scx_ctx *task_ctx,
                                            __u32 lock_id, __u32 cpu) {
  __u32 pid = p->pid;
  volatile __u32 *owner;

  if (!valid_lock_id(lock_id))
    return false;

  owner = lookup_cpu_owner(cpu);
  if (!owner)
    return false;

  if (*owner && *owner != pid)
    return false;

  *owner = pid;
  task_ctx->holds_admission = 1;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->force_inactive_wait = 0;
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
  if (valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
    return cpu;

  cpu = scx_bpf_pick_any_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
    return cpu;

  return ADMISSION_CPU_NONE;
}

static __always_inline __u32 pick_task_cpu(struct task_struct *p,
                                           s32 fallback_cpu) {
  s32 cpu;

  if (valid_cpu(fallback_cpu) && task_cpumask_allows(p, (__u32)fallback_cpu))
    return fallback_cpu;

  cpu = scx_bpf_task_cpu(p);
  if (valid_cpu(cpu) && task_cpumask_allows(p, (__u32)cpu))
    return cpu;

  cpu = scx_bpf_pick_idle_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu))
    return cpu;

  cpu = scx_bpf_pick_any_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu))
    return cpu;

  return 0;
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
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (task_ctx->holds_admission)
    return false;

  if (have_user && user_slow_path_pending(user_ctx_word)) {
    if (single_lock_enabled())
      return true;

    return valid_lock_id(user_lock_id(user_ctx_word));
  }

  return false;
}

static __always_inline bool consumed_token_reuse_requested(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  return task_ctx->holds_admission && have_user &&
         user_slow_path_pending(user_ctx_word) && user_token_consumed(user_ctx_word);
}

static __always_inline bool should_release_from_user(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (!task_ctx->holds_admission || !have_user)
    return false;

  return user_explicit_release(user_ctx_word);
}

static __always_inline bool task_in_critical_section(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (have_user)
    return user_in_critical_section(user_ctx_word);

  return false;
}

static __always_inline bool enqueue_force_inactive(struct task_struct *p,
                                                   struct task_scx_ctx *task_ctx,
                                                   __u32 lock_id, __u32 cpu,
                                                   __u64 enq_flags) {
  if (!valid_lock_id(lock_id))
    return false;

  if (cpu >= MAX_CPUS || !task_cpu_allowed(p, cpu)) {
    scx_bpf_dsq_insert(p, NORMAL_DSQ_ID, SCX_SLICE_DFL, enq_flags);
    return true;
  }

  task_ctx->admission_cpu = cpu;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->force_inactive_wait = 0;
  scx_bpf_dsq_insert(p, inactive_dsq_id(lock_id), SCX_SLICE_DFL, enq_flags);
  return true;
}

static __always_inline void protect_critical_section(struct task_struct *p,
                                                     struct task_scx_ctx *task_ctx,
                                                     __u32 lock_id) {
  __u32 cpu = task_ctx->admission_cpu;

  if (!valid_lock_id(lock_id))
    return;

  if (cpu >= MAX_CPUS || !task_cpu_allowed(p, cpu))
    cpu = pick_allowed_cpu(p, bpf_get_smp_processor_id());

  if (cpu >= MAX_CPUS)
    return;

  task_ctx->admission_cpu = cpu;
  if (!task_ctx->holds_admission)
    grant_admission(p, task_ctx, lock_id, cpu);

  task_ctx->must_run_on_admission_cpu = 1;
}

static __always_inline void refresh_running_state(struct task_struct *p) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  __u32 lock_id = UNMANAGED_LOCK_ID;
  bool have_user = false;
  __u32 cpu = bpf_get_smp_processor_id();

  if (stats_only_enabled())
    return;

  task_ctx = get_task_ctx(p);
  if (!task_ctx)
    return;

  have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
  if (have_user)
    lock_id = effective_lock_id(user_ctx_word);
  if (should_release_from_user(task_ctx, have_user, user_ctx_word)) {
    release_admission(p, task_ctx);
    return;
  }

  if (!task_ctx->holds_admission && have_user &&
      user_slow_path_pending(user_ctx_word) && valid_lock_id(lock_id) &&
      cpu < MAX_CPUS && task_cpu_allowed(p, cpu))
    grant_admission(p, task_ctx, lock_id, cpu);

  if (task_ctx->must_run_on_admission_cpu && task_ctx->admission_cpu == cpu)
    task_ctx->must_run_on_admission_cpu = 0;
}

s32 BPF_STRUCT_OPS(accordin_select_cpu, struct task_struct *p, s32 prev_cpu,
                   u64 wake_flags) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  bool is_idle = false;
  bool have_user = false;
  bool wants_slow_path = false;
  s32 cpu;

  if (stats_only_enabled()) {
    cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);
    if (!valid_cpu(cpu) || !task_cpu_allowed(p, (__u32)cpu)) {
      is_idle = false;
      cpu = pick_allowed_cpu(p, prev_cpu);
      if (cpu >= MAX_CPUS)
        cpu = pick_task_cpu(p, prev_cpu);
    }

    if (is_idle && valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

    return cpu;
  }

  task_ctx = get_task_ctx(p);
  if (task_ctx) {
    have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
    clear_invalid_admission_cpu(p, task_ctx);
  }

  if (task_ctx && task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu))
    return (s32)task_ctx->admission_cpu;

  wants_slow_path =
      task_ctx && slow_path_requested(task_ctx, have_user, user_ctx_word);

  if (wants_slow_path && valid_cpu(prev_cpu) &&
      task_cpu_allowed(p, (__u32)prev_cpu)) {
    task_ctx->admission_cpu = (__u32)prev_cpu;
    return prev_cpu;
  }

  cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

  if (wants_slow_path && (!valid_cpu(cpu) || !task_cpu_allowed(p, (__u32)cpu))) {
    is_idle = false;
    cpu = pick_allowed_cpu(p, prev_cpu);
    if (cpu >= MAX_CPUS)
      cpu = pick_task_cpu(p, prev_cpu);
  } else if (!wants_slow_path &&
             (!valid_cpu(cpu) || !task_cpumask_allows(p, (__u32)cpu))) {
    is_idle = false;
    cpu = pick_task_cpu(p, prev_cpu);
  }

  if (wants_slow_path && valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
    task_ctx->admission_cpu = (__u32)cpu;

  if (is_idle && valid_cpu(cpu) && task_cpumask_allows(p, (__u32)cpu) &&
      !wants_slow_path)
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

  return cpu;
}

void BPF_STRUCT_OPS(accordin_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  __u32 lock_id = UNMANAGED_LOCK_ID;
  bool have_user = false;
  __u32 cpu;

  if (stats_only_enabled()) {
    if (p->nr_cpus_allowed == 1) {
      cpu = pick_allowed_cpu(p, -1);
      if (valid_cpu(cpu) && task_cpu_allowed(p, cpu)) {
        scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL,
                           enq_flags);
        return;
      }
    }

    scx_bpf_dsq_insert(p, SCX_DSQ_GLOBAL, SCX_SLICE_DFL, enq_flags);
    return;
  }

  task_ctx = get_task_ctx(p);
  if (!task_ctx) {
    scx_bpf_dsq_insert(p, NORMAL_DSQ_ID, SCX_SLICE_DFL, enq_flags);
    return;
  }

  have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
  if (have_user)
    lock_id = effective_lock_id(user_ctx_word);

  clear_invalid_admission_cpu(p, task_ctx);

  if (consumed_token_reuse_requested(task_ctx, have_user, user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    release_admission(p, task_ctx);
    enqueue_force_inactive(p, task_ctx, lock_id, cpu, enq_flags);
    return;
  }

  if (should_release_from_user(task_ctx, have_user, user_ctx_word))
    release_admission(p, task_ctx);

  if (task_ctx->force_inactive_wait && have_user &&
      user_slow_path_pending(user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    enqueue_force_inactive(p, task_ctx, lock_id, cpu, enq_flags);
    return;
  }

  if (task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu)) {
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | task_ctx->admission_cpu,
                       SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (slow_path_requested(task_ctx, have_user, user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    if (cpu >= MAX_CPUS) {
      scx_bpf_dsq_insert(p, NORMAL_DSQ_ID, SCX_SLICE_DFL, enq_flags);
      return;
    }
    task_ctx->admission_cpu = cpu;

    if (grant_admission(p, task_ctx, lock_id, cpu)) {
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
      return;
    }

    scx_bpf_dsq_insert(p, inactive_dsq_id(lock_id), SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (p->nr_cpus_allowed == 1) {
    cpu = pick_task_cpu(p, -1);
    if (valid_cpu(cpu) && task_cpumask_allows(p, cpu)) {
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL,
                         enq_flags);
      return;
    }
  }

  scx_bpf_dsq_insert(p, NORMAL_DSQ_ID, SCX_SLICE_DFL, enq_flags);
}

void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
  (void)prev;

  if (!valid_cpu(cpu))
    return;

  if (stats_only_enabled()) {
    scx_bpf_dsq_move_to_local(SCX_DSQ_GLOBAL);
    return;
  }

  if (inactive_dispatch_budget_available((__u32)cpu) &&
      drain_inactive_counted((__u32)cpu))
    return;

  if (scx_bpf_dsq_move_to_local(NORMAL_DSQ_ID)) {
    reset_inactive_dispatch_budget((__u32)cpu);
    return;
  }

  drain_inactive_counted((__u32)cpu);
}

void BPF_STRUCT_OPS(accordin_running, struct task_struct *p) {
  refresh_running_state(p);
}

void BPF_STRUCT_OPS(accordin_tick, struct task_struct *p) {
  refresh_running_state(p);
}

void BPF_STRUCT_OPS(accordin_stopping, struct task_struct *p, bool runnable) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  __u32 lock_id = UNMANAGED_LOCK_ID;
  bool have_user = false;

  task_ctx = lookup_task_ctx(p);
  if (!task_ctx)
    return;

  if (stats_only_enabled())
    return;

  have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
  if (have_user)
    lock_id = effective_lock_id(user_ctx_word);

  if (should_release_from_user(task_ctx, have_user, user_ctx_word)) {
    release_admission(p, task_ctx);
    return;
  }

  if (consumed_token_reuse_requested(task_ctx, have_user, user_ctx_word)) {
    release_admission(p, task_ctx);
    task_ctx->force_inactive_wait = 1;
    return;
  }

  if (runnable && task_in_critical_section(task_ctx, have_user, user_ctx_word)) {
    protect_critical_section(p, task_ctx, lock_id);
    return;
  }

  if (!task_ctx->holds_admission)
    return;

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
  __u32 lock_id;
  s32 ret;

  ret = scx_bpf_create_dsq(NORMAL_DSQ_ID, -1);
  if (ret)
    return ret;

#pragma unroll
  for (lock_id = 0; lock_id < MAX_LOCK_CLASSES; lock_id++) {
    ret = scx_bpf_create_dsq(inactive_dsq_id(lock_id), -1);
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
