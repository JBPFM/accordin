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

static __always_inline bool debug_counters_enabled(void) {
  return debug_counters_mode != 0;
}

static __always_inline bool registered_threads_active(void) {
  return registered_thread_count != 0;
}

static __always_inline void bump_counter(volatile __u64 *counter) {
  __sync_fetch_and_add(counter, 1);
}

/* For sites that are not already inside a hoisted debug gate. */
static __always_inline void bump_debug_counter(volatile __u64 *counter) {
  if (debug_counters_enabled())
    bump_counter(counter);
}

static __always_inline bool inactive_dispatch_needed(void) {
  return inactive_empty_seq != inactive_enqueue_seq;
}

static __always_inline bool inactive_dispatch_needed_for(__u32 inactive_seq) {
  return inactive_empty_seq != inactive_seq;
}

static __always_inline void record_inactive_enqueue(void) {
  inactive_enqueue_seq += 1;
}

static __always_inline void record_inactive_scan_empty(__u32 inactive_seq) {
  inactive_empty_seq = inactive_seq;
}

static __always_inline bool normal_dispatch_needed_for(__u32 normal_seq) {
  return normal_empty_seq != normal_seq;
}

static __always_inline bool normal_dispatch_needed(void) {
  return normal_dispatch_needed_for(normal_enqueue_seq);
}

static __always_inline void record_normal_enqueue(void) {
  normal_enqueue_seq += 1;
}

static __always_inline void record_normal_scan_empty(__u32 normal_seq) {
  normal_empty_seq = normal_seq;
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

static __always_inline bool inactive_dispatch_cpu_available(__u32 cpu) {
  volatile __u32 *owner = lookup_cpu_owner(cpu);

  return owner && !*owner;
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

/* Single sink for enqueue evidence: every routing decision lands here exactly
 * once, so a hinted wakeup contributes exactly one wake_consumed_* class. */
static __always_inline void
record_enqueue_state(struct task_scx_ctx *task_ctx, __u64 dsq_id,
                     __u32 lock_id, __u32 path, __u32 cpu,
                     __u32 user_ctx_word, bool hinted) {
  if (!debug_counters_enabled())
    return;

  if (hinted) {
    switch (path) {
    case ENQ_PATH_ADMISSION_LOCAL:
    case ENQ_PATH_SLOW_GRANTED_LOCAL:
      bump_counter(&wake_consumed_granted);
      break;
    case ENQ_PATH_SLOW_INACTIVE:
    case ENQ_PATH_FORCE_INACTIVE:
      bump_counter(&wake_consumed_inactive);
      break;
    case ENQ_PATH_NORMAL_DSQ:
    case ENQ_PATH_NORMAL_LOCAL_FAST:
      bump_counter(&wake_consumed_normal);
      break;
    default:
      break;
    }
  }

  if (!task_ctx)
    return;

  task_ctx->last_enqueue_dsq = dsq_id;
  task_ctx->last_enqueue_lock_id = lock_id;
  task_ctx->last_enqueue_path = path;
  task_ctx->last_enqueue_cpu = cpu;
  task_ctx->last_user_ctx_word = user_ctx_word;
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

/* Wakeup carrying the cond-reacquire signature. A failed user-word read leaves
 * the word at zero, so an unregistered task never matches. */
static __always_inline bool wake_hinted(__u64 enq_flags, __u32 user_ctx_word) {
  return (enq_flags & SCX_ENQ_WAKEUP) &&
         user_slow_path_pending(user_ctx_word) &&
         user_token_consumed(user_ctx_word);
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

static __always_inline s32 select_cpu_without_admission(struct task_struct *p,
                                                        s32 prev_cpu,
                                                        u64 wake_flags) {
  bool is_idle = false;
  s32 cpu;

  cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);
  if (!valid_cpu(cpu) || !task_cpumask_allows(p, (__u32)cpu)) {
    is_idle = false;
    cpu = pick_task_cpu(p, prev_cpu);
  }

  if (is_idle && valid_cpu(cpu) && task_cpumask_allows(p, (__u32)cpu) &&
      !inactive_dispatch_needed() && !normal_dispatch_needed())
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

  return cpu;
}

static __always_inline void enqueue_normal_dsq_recorded(
    struct task_struct *p, struct task_scx_ctx *task_ctx, __u64 enq_flags,
    __u32 lock_id, __u32 user_ctx_word) {
  record_enqueue_state(task_ctx, NORMAL_DSQ_ID, lock_id, ENQ_PATH_NORMAL_DSQ,
                       ADMISSION_CPU_NONE, user_ctx_word,
                       wake_hinted(enq_flags, user_ctx_word));
  scx_bpf_dsq_insert(p, NORMAL_DSQ_ID, SCX_SLICE_DFL, enq_flags);
  record_normal_enqueue();
}

static __always_inline void enqueue_normal_dsq(struct task_struct *p,
                                               __u64 enq_flags) {
  enqueue_normal_dsq_recorded(p, 0, enq_flags, UNMANAGED_LOCK_ID, 0);
}

static __always_inline bool enqueue_normal_local_fast_recorded(
    struct task_struct *p, struct task_scx_ctx *task_ctx, __u64 enq_flags,
    __u32 lock_id, __u32 user_ctx_word) {
  __u32 cpu;

  if (inactive_dispatch_needed() || normal_dispatch_needed())
    return false;

  cpu = pick_task_cpu(p, -1);
  if (!valid_cpu(cpu) || !task_cpumask_allows(p, cpu))
    return false;

  record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | cpu, lock_id,
                       ENQ_PATH_NORMAL_LOCAL_FAST, cpu, user_ctx_word,
                       wake_hinted(enq_flags, user_ctx_word));
  scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
  return true;
}

static __always_inline bool enqueue_normal_local_fast(struct task_struct *p,
                                                      __u64 enq_flags) {
  return enqueue_normal_local_fast_recorded(p, 0, enq_flags, UNMANAGED_LOCK_ID,
                                            0);
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
                                                   __u64 enq_flags,
                                                   __u32 user_ctx_word) {
  if (!valid_lock_id(lock_id))
    return false;

  if (cpu >= MAX_CPUS || !task_cpu_allowed(p, cpu)) {
    enqueue_normal_dsq_recorded(p, task_ctx, enq_flags, lock_id,
                                user_ctx_word);
    return true;
  }

  task_ctx->admission_cpu = cpu;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->force_inactive_wait = 0;
  record_enqueue_state(task_ctx, inactive_dsq_id(lock_id), lock_id,
                       ENQ_PATH_FORCE_INACTIVE, cpu, user_ctx_word,
                       wake_hinted(enq_flags, user_ctx_word));
  scx_bpf_dsq_insert(p, inactive_dsq_id(lock_id), SCX_SLICE_DFL, enq_flags);
  record_inactive_enqueue();
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

  if (!registered_threads_active())
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
      cpu < MAX_CPUS && task_cpu_allowed(p, cpu)) {
    if (grant_admission(p, task_ctx, lock_id, cpu))
      bump_debug_counter(&running_pending_grant_success);
    else
      bump_debug_counter(&running_pending_grant_failure);
  }

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
  bool debug_counters;
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

  if (!registered_threads_active()) {
    return select_cpu_without_admission(p, prev_cpu, wake_flags);
  }

  debug_counters = debug_counters_enabled();
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
      !wants_slow_path && !inactive_dispatch_needed() &&
      !normal_dispatch_needed()) {
    /* A hinted task always wants the slow path, so this shortcut never carries
     * a wake_consumed_* class. */
    record_enqueue_state(task_ctx, SCX_DSQ_LOCAL,
                         have_user ? effective_lock_id(user_ctx_word)
                                   : UNMANAGED_LOCK_ID,
                         ENQ_PATH_SELECT_LOCAL_DIRECT, (__u32)cpu,
                         user_ctx_word, false);
    if (debug_counters)
      bump_counter(&select_local_direct);
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);
  }

  return cpu;
}

void BPF_STRUCT_OPS(accordin_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  __u32 lock_id = UNMANAGED_LOCK_ID;
  bool have_user = false;
  bool debug_counters;
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

  if (!registered_threads_active()) {
    if (enqueue_normal_local_fast(p, enq_flags))
      return;

    enqueue_normal_dsq(p, enq_flags);
    return;
  }

  task_ctx = get_task_ctx(p);
  if (!task_ctx) {
    if (enqueue_normal_local_fast(p, enq_flags))
      return;

    enqueue_normal_dsq(p, enq_flags);
    return;
  }

  have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
  if (have_user)
    lock_id = effective_lock_id(user_ctx_word);

  debug_counters = debug_counters_enabled();
  if (debug_counters) {
    bool wake = enq_flags & SCX_ENQ_WAKEUP;

    /* Only a task that already published a user word can fail the read; tasks
     * that never registered are not part of the protocol. */
    if (wake && !have_user && task_ctx->user_ctx_ptr)
      bump_counter(&wake_read_fail);
    /* Every wakeup counted here reaches exactly one wake_consumed_* class in
     * record_enqueue_state below. */
    if (wake_hinted(enq_flags, user_ctx_word))
      bump_counter(&wake_consumed_seen);
  }

  clear_invalid_admission_cpu(p, task_ctx);

  if (consumed_token_reuse_requested(task_ctx, have_user, user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    release_admission(p, task_ctx);
    enqueue_force_inactive(p, task_ctx, lock_id, cpu, enq_flags, user_ctx_word);
    return;
  }

  if (should_release_from_user(task_ctx, have_user, user_ctx_word))
    release_admission(p, task_ctx);

  if (task_ctx->force_inactive_wait && have_user &&
      user_slow_path_pending(user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    enqueue_force_inactive(p, task_ctx, lock_id, cpu, enq_flags, user_ctx_word);
    return;
  }

  if (task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu)) {
    record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | task_ctx->admission_cpu,
                         lock_id, ENQ_PATH_ADMISSION_LOCAL,
                         task_ctx->admission_cpu, user_ctx_word,
                         wake_hinted(enq_flags, user_ctx_word));
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | task_ctx->admission_cpu,
                       SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (slow_path_requested(task_ctx, have_user, user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    if (cpu >= MAX_CPUS) {
      enqueue_normal_dsq_recorded(p, task_ctx, enq_flags, lock_id,
                                  user_ctx_word);
      return;
    }
    task_ctx->admission_cpu = cpu;

    if (grant_admission(p, task_ctx, lock_id, cpu)) {
      record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | cpu, lock_id,
                           ENQ_PATH_SLOW_GRANTED_LOCAL, cpu, user_ctx_word,
                           wake_hinted(enq_flags, user_ctx_word));
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
      return;
    }

    record_enqueue_state(task_ctx, inactive_dsq_id(lock_id), lock_id,
                         ENQ_PATH_SLOW_INACTIVE, cpu, user_ctx_word,
                         wake_hinted(enq_flags, user_ctx_word));
    scx_bpf_dsq_insert(p, inactive_dsq_id(lock_id), SCX_SLICE_DFL, enq_flags);
    record_inactive_enqueue();
    return;
  }

  if (enqueue_normal_local_fast_recorded(p, task_ctx, enq_flags, lock_id,
                                         user_ctx_word))
    return;

  if (p->nr_cpus_allowed == 1) {
    cpu = pick_task_cpu(p, -1);
    if (valid_cpu(cpu) && task_cpumask_allows(p, cpu)) {
      record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | cpu, lock_id,
                           ENQ_PATH_NORMAL_LOCAL_FAST, cpu, user_ctx_word,
                           wake_hinted(enq_flags, user_ctx_word));
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL,
                         enq_flags);
      return;
    }
  }

  enqueue_normal_dsq_recorded(p, task_ctx, enq_flags, lock_id, user_ctx_word);
}

void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
  __u32 inactive_seq;
  __u32 normal_seq;
  bool inactive_cpu_available;
  bool debug_counters;

  (void)prev;

  if (!valid_cpu(cpu))
    return;

  debug_counters = debug_counters_enabled();
  if (debug_counters)
    dispatch_calls += 1;

  if (stats_only_enabled()) {
    scx_bpf_dsq_move_to_local(SCX_DSQ_GLOBAL);
    return;
  }

  inactive_seq = inactive_enqueue_seq;
  normal_seq = normal_enqueue_seq;

  if (normal_dispatch_needed_for(normal_seq)) {
    if (debug_counters)
      dispatch_normal_attempts += 1;
    if (scx_bpf_dsq_move_to_local(NORMAL_DSQ_ID)) {
      reset_inactive_dispatch_budget((__u32)cpu);
      if (debug_counters)
        dispatch_normal_success += 1;
      return;
    }

    if (debug_counters)
      dispatch_normal_empty += 1;
    record_normal_scan_empty(normal_seq);
  } else if (debug_counters) {
    dispatch_normal_skip_seq += 1;
  }

  if (!inactive_dispatch_needed_for(inactive_seq))
    return;

  inactive_cpu_available = inactive_dispatch_cpu_available((__u32)cpu);
  if (debug_counters) {
    if (!inactive_cpu_available)
      dispatch_inactive_unavailable += 1;
    else if (!inactive_dispatch_budget_available((__u32)cpu))
      dispatch_inactive_budget_blocked += 1;
  }

  if (inactive_cpu_available && inactive_dispatch_budget_available((__u32)cpu)) {
    if (debug_counters)
      dispatch_inactive_attempts += 1;
    if (move_selected_inactive_to_local((__u32)cpu)) {
      record_inactive_dispatch((__u32)cpu);
      if (debug_counters)
        dispatch_inactive_success += 1;
      return;
    }

    if (debug_counters)
      dispatch_inactive_empty += 1;
    record_inactive_scan_empty(inactive_seq);
  }

  if (inactive_cpu_available) {
    if (debug_counters)
      dispatch_inactive_attempts += 1;
    if (move_selected_inactive_to_local((__u32)cpu)) {
      record_inactive_dispatch((__u32)cpu);
      if (debug_counters)
        dispatch_inactive_success += 1;
      return;
    }

    if (debug_counters)
      dispatch_inactive_empty += 1;
    record_inactive_scan_empty(inactive_seq);
  }
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

  if (!registered_threads_active())
    return;

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

  if (!runnable) {
    /* A block invalidates any routing decision made for a runnable re-enqueue,
     * so the wakeup is classified from scratch. */
    task_ctx->force_inactive_wait = 0;
    task_ctx->must_run_on_admission_cpu = 0;

    if (!task_ctx->holds_admission)
      return;

    /* Only a confirmed critical section justifies pinning the slot while the
     * owner sleeps; the lock it protects cannot progress without it. */
    if (task_in_critical_section(task_ctx, have_user, user_ctx_word)) {
      task_ctx->must_run_on_admission_cpu = 1;
      return;
    }

    /* Any other block gives the slot back and re-enters admission at wakeup.
     * An unreadable user word is treated as not-in-critical-section: a real
     * owner resumes without a slot and protect_critical_section re-pins it at
     * its next runnable stop. */
    if (!have_user)
      bump_debug_counter(&block_release_read_fail);

    release_admission(p, task_ctx);
    return;
  }

  if (consumed_token_reuse_requested(task_ctx, have_user, user_ctx_word)) {
    release_admission(p, task_ctx);
    task_ctx->force_inactive_wait = 1;
    return;
  }

  if (task_in_critical_section(task_ctx, have_user, user_ctx_word)) {
    protect_critical_section(p, task_ctx, lock_id);
    return;
  }

  if (!task_ctx->holds_admission)
    return;

  task_ctx->must_run_on_admission_cpu = 1;
}

void BPF_STRUCT_OPS(accordin_dump, struct scx_dump_ctx *dump_ctx) {
  __u32 lock_id;

  (void)dump_ctx;

  scx_bpf_dump(
      "accordin_global debug=%u reg_threads=%u normal_seq=%u normal_empty=%u "
      "normal_q=%d inactive_seq=%u inactive_empty=%u\n",
      debug_counters_mode, registered_thread_count, normal_enqueue_seq,
      normal_empty_seq, scx_bpf_dsq_nr_queued(NORMAL_DSQ_ID),
      inactive_enqueue_seq, inactive_empty_seq);
  if (debug_counters_enabled()) {
    scx_bpf_dump(
        "accordin_dispatch calls=%llu normal_skip_seq=%llu normal_attempts=%llu "
        "normal_success=%llu normal_empty=%llu inactive_unavail=%llu "
        "inactive_budget_blocked=%llu inactive_attempts=%llu "
        "inactive_success=%llu inactive_empty=%llu\n",
        dispatch_calls, dispatch_normal_skip_seq, dispatch_normal_attempts,
        dispatch_normal_success, dispatch_normal_empty,
        dispatch_inactive_unavailable, dispatch_inactive_budget_blocked,
        dispatch_inactive_attempts, dispatch_inactive_success,
        dispatch_inactive_empty);
    scx_bpf_dump(
        "accordin_routing select_local_direct=%llu wake_consumed_seen=%llu "
        "wake_consumed_granted=%llu wake_consumed_inactive=%llu "
        "wake_consumed_normal=%llu wake_read_fail=%llu "
        "running_pending_grant_success=%llu "
        "running_pending_grant_failure=%llu block_release_read_fail=%llu\n",
        select_local_direct, wake_consumed_seen, wake_consumed_granted,
        wake_consumed_inactive, wake_consumed_normal, wake_read_fail,
        running_pending_grant_success, running_pending_grant_failure,
        block_release_read_fail);
  }

#pragma unroll
  for (lock_id = 0; lock_id < MAX_LOCK_CLASSES; lock_id++) {
    __u64 dsq_id = inactive_dsq_id(lock_id);
    s32 queued = scx_bpf_dsq_nr_queued(dsq_id);

    if (queued)
      scx_bpf_dump("accordin_inactive_q lock=%u dsq=0x%llx queued=%d\n",
                   lock_id, dsq_id, queued);
  }
}

void BPF_STRUCT_OPS(accordin_dump_task, struct scx_dump_ctx *dump_ctx,
                    struct task_struct *p) {
  struct task_scx_ctx *task_ctx;

  (void)dump_ctx;

  task_ctx = bpf_task_storage_get(&task_ctx_map, p, 0, 0);
  if (!task_ctx)
    return;

  scx_bpf_dump(
      "accordin_task pid=%d tgid=%d holds=%u adm_cpu=%u must=%u force=%u\n",
      p->pid, p->tgid, task_ctx->holds_admission, task_ctx->admission_cpu,
      task_ctx->must_run_on_admission_cpu, task_ctx->force_inactive_wait);
  if (debug_counters_enabled())
    scx_bpf_dump(
        "accordin_task_enqueue pid=%d last_path=%u last_lock=%u last_cpu=%u "
        "last_dsq=0x%llx user_word=0x%x\n",
        p->pid, task_ctx->last_enqueue_path, task_ctx->last_enqueue_lock_id,
        task_ctx->last_enqueue_cpu, task_ctx->last_enqueue_dsq,
        task_ctx->last_user_ctx_word);
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
               .dump = (void *)accordin_dump,
               .dump_task = (void *)accordin_dump_task,
               .exit_task = (void *)accordin_exit_task,
               .init = (void *)accordin_init, .exit = (void *)accordin_exit,
               .name = "accordin");
