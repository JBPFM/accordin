/* SPDX-License-Identifier: GPL-2.0-only */
#include <scx/common.bpf.h>

#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

#include "maps.bpf.h"

#define INACTIVE_STEAL_SCAN 8U

static __always_inline bool valid_cpu(s32 cpu) {
  return cpu >= 0 && cpu < MAX_CPUS;
}

static __always_inline bool stats_only_enabled(void) {
  return stats_only_mode != 0;
}

static __always_inline struct cpu_inactive_hint *lookup_inactive_hint(__u32 cpu) {
  if (cpu >= MAX_CPUS)
    return 0;

  return bpf_map_lookup_elem(&cpu_inactive_hint_map, &cpu);
}

static __always_inline bool cpu_is_active(__u32 cpu) {
  __u64 word;

  if (cpu >= MAX_CPUS)
    return false;

  if (active_cpus_all)
    return true;

  if (cpu < 64)
    word = active_cpu_word0;
  else if (cpu < 128)
    word = active_cpu_word1;
  else if (cpu < 192)
    word = active_cpu_word2;
  else
    word = active_cpu_word3;

  return word & (1ULL << (cpu & 63));
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

  if (!task_cpumask_allows(p, cpu))
    return false;

  return cpu_is_active(cpu);
}

static __always_inline bool valid_lock_id(__u32 lock_id) {
  return lock_id != UNMANAGED_LOCK_ID && lock_id < MAX_LOCK_CLASSES;
}

static __always_inline __u32 lock_id_bit(__u32 lock_id) {
  if (!valid_lock_id(lock_id))
    return 0;

  return 1U << lock_id;
}

static __always_inline __u32 user_admission_lock_id(__u32 user_ctx_word) {
  return (user_ctx_word & ~USER_ADMISSION_FLAG_MASK) >>
         USER_ADMISSION_LOCK_ID_SHIFT;
}

static __always_inline __u32 owner_key(__u32 lock_id, __u32 cpu) {
  return lock_id * MAX_CPUS + cpu;
}

static __always_inline __u64 inactive_dsq_id(__u32 lock_id, __u32 cpu) {
  return INACTIVE_DSQ_BASE + ((__u64)lock_id * MAX_CPUS) + cpu;
}

static __always_inline void record_inactive_enqueue(__u32 lock_id, __u32 cpu) {
  struct cpu_inactive_hint *hint;
  __u32 bit = lock_id_bit(lock_id);

  if (!bit || cpu >= MAX_CPUS)
    return;

  hint = lookup_inactive_hint(cpu);
  if (!hint)
    return;

  __sync_fetch_and_or(&hint->lock_mask, bit);
  __sync_fetch_and_add(&hint->total, 1);
}

static __always_inline void record_inactive_dequeue(__u32 lock_id, __u32 cpu) {
  struct cpu_inactive_hint *hint;

  if (!valid_lock_id(lock_id) || cpu >= MAX_CPUS)
    return;

  hint = lookup_inactive_hint(cpu);
  if (!hint || !hint->total)
    return;

  __sync_fetch_and_sub(&hint->total, 1);
}

static __always_inline bool active_words_want_cpu(__u64 wanted0, __u64 wanted1,
                                                  __u64 wanted2, __u64 wanted3,
                                                  __u32 cpu, __u32 nr_cpus) {
  __u64 word;

  if (cpu >= nr_cpus || cpu >= MAX_CPUS)
    return false;

  if (cpu < 64)
    word = wanted0;
  else if (cpu < 128)
    word = wanted1;
  else if (cpu < 192)
    word = wanted2;
  else
    word = wanted3;

  return word & (1ULL << (cpu & 63));
}

static __always_inline bool active_words_cover_online_cpus(__u64 wanted0,
                                                           __u64 wanted1,
                                                           __u64 wanted2,
                                                           __u64 wanted3,
                                                           __u32 nr_cpus) {
  __u32 online = scx_bpf_nr_cpu_ids();
  __u32 i;

  if (online > MAX_CPUS)
    online = MAX_CPUS;

  for (i = 0; i < MAX_CPUS; i++) {
    if (i >= online)
      break;
    if (!active_words_want_cpu(wanted0, wanted1, wanted2, wanted3, i, nr_cpus))
      return false;
  }

  return true;
}

static __always_inline void init_task_ctx_if_needed(struct task_scx_ctx *task_ctx) {
  if (task_ctx->initialized)
    return;

  task_ctx->initialized = 1;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
  task_ctx->admission_lock_id = UNMANAGED_LOCK_ID;
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

static __always_inline __u32 *lookup_lock_cpu_owner(__u32 lock_id, __u32 cpu) {
  __u32 key;

  if (!valid_lock_id(lock_id) || cpu >= MAX_CPUS)
    return 0;

  key = owner_key(lock_id, cpu);
  return bpf_map_lookup_elem(&cpu_admission_owner_map, &key);
}

static __always_inline bool steal_inactive(__u32 cpu) {
  __u32 nr_cpus = scx_bpf_nr_cpu_ids();
  __u32 lock_id;
  __u32 i;

  if (nr_cpus > MAX_CPUS)
    nr_cpus = MAX_CPUS;

  if (nr_cpus <= 1)
    return false;

#pragma unroll
  for (i = 1; i <= INACTIVE_STEAL_SCAN; i++) {
    struct cpu_inactive_hint *hint;
    __u32 victim = cpu + i;

    if (i >= nr_cpus)
      break;

    if (victim >= nr_cpus)
      victim -= nr_cpus;

    hint = lookup_inactive_hint(victim);
    if (!hint || !hint->total)
      continue;

#pragma unroll
    for (lock_id = 1; lock_id < MAX_LOCK_CLASSES; lock_id++) {
      __u32 *owner;

      if (!(hint->lock_mask & lock_id_bit(lock_id)))
        continue;

      owner = lookup_lock_cpu_owner(lock_id, cpu);
      if (!owner || *owner)
        continue;

      if (scx_bpf_dsq_move_to_local(inactive_dsq_id(lock_id, victim))) {
        record_inactive_dequeue(lock_id, victim);
        return true;
      }
    }
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

static __always_inline bool user_slow_path_seen(__u32 user_ctx_word) {
  return user_ctx_word &
         (USER_ADMISSION_SLOW_PATH_PENDING | USER_ADMISSION_SLOW_PATH_SEEN);
}

static __always_inline bool user_in_critical_section(__u32 user_ctx_word) {
  return user_ctx_word & USER_ADMISSION_IN_CRITICAL_SECTION;
}

static __always_inline bool user_explicit_release(__u32 user_ctx_word) {
  return !user_slow_path_pending(user_ctx_word) &&
         !user_in_critical_section(user_ctx_word);
}

static __always_inline void clear_admission_state(struct task_scx_ctx *task_ctx) {
  task_ctx->holds_admission = 0;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->inactive_wait = 0;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
  task_ctx->admission_lock_id = UNMANAGED_LOCK_ID;
}

static __always_inline void release_admission(struct task_struct *p,
                                              struct task_scx_ctx *task_ctx) {
  __u32 cpu = task_ctx->admission_cpu;
  __u32 lock_id = task_ctx->admission_lock_id;
  __u32 pid = p->pid;
  __u32 *owner;

  owner = lookup_lock_cpu_owner(lock_id, cpu);
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
  __u32 *owner;

  owner = lookup_lock_cpu_owner(lock_id, cpu);
  if (!owner)
    return false;

  if (*owner && *owner != pid)
    return false;

  *owner = pid;
  task_ctx->slow_path_seen = 1;
  task_ctx->holds_admission = 1;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->inactive_wait = 0;
  task_ctx->admission_cpu = cpu;
  task_ctx->admission_lock_id = lock_id;
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
  if (valid_cpu(cpu) && cpu_is_active((__u32)cpu))
    return cpu;

  cpu = scx_bpf_pick_any_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu) && cpu_is_active((__u32)cpu))
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

  if (have_user)
    return user_slow_path_pending(user_ctx_word) &&
           valid_lock_id(user_admission_lock_id(user_ctx_word));

  return false;
}

static __always_inline bool refresh_slow_path_seen(
    struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (have_user && user_slow_path_seen(user_ctx_word))
    task_ctx->slow_path_seen = 1;

  return task_ctx->slow_path_seen;
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
    lock_id = user_admission_lock_id(user_ctx_word);
  refresh_slow_path_seen(task_ctx, have_user, user_ctx_word);
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
  bool slow_path_seen = false;
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
    slow_path_seen = refresh_slow_path_seen(task_ctx, have_user, user_ctx_word);
  }

  if (task_ctx)
    clear_invalid_admission_cpu(p, task_ctx);

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

  if (!wants_slow_path && slow_path_seen && valid_cpu(prev_cpu) &&
      task_cpu_allowed(p, (__u32)prev_cpu))
    return prev_cpu;

  cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

  if ((wants_slow_path || slow_path_seen) &&
      (!valid_cpu(cpu) || !task_cpu_allowed(p, (__u32)cpu))) {
    is_idle = false;
    cpu = pick_allowed_cpu(p, prev_cpu);
    if (cpu >= MAX_CPUS)
      cpu = pick_task_cpu(p, prev_cpu);
  } else if (!wants_slow_path && !slow_path_seen &&
             (!valid_cpu(cpu) || !task_cpumask_allows(p, (__u32)cpu))) {
    is_idle = false;
    cpu = pick_task_cpu(p, prev_cpu);
  }

  if ((wants_slow_path || slow_path_seen) && valid_cpu(cpu) &&
      task_cpu_allowed(p, (__u32)cpu))
    task_ctx->admission_cpu = (__u32)cpu;

  if (is_idle && valid_cpu(cpu) && task_cpumask_allows(p, (__u32)cpu) &&
      !wants_slow_path && !slow_path_seen)
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

  return cpu;
}

void BPF_STRUCT_OPS(accordin_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  __u32 lock_id = UNMANAGED_LOCK_ID;
  bool have_user = false;
  bool slow_path_seen = false;
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
    scx_bpf_dsq_insert(p, SCX_DSQ_GLOBAL, SCX_SLICE_DFL, enq_flags);
    return;
  }

  have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
  if (have_user)
    lock_id = user_admission_lock_id(user_ctx_word);
  slow_path_seen = refresh_slow_path_seen(task_ctx, have_user, user_ctx_word);

  clear_invalid_admission_cpu(p, task_ctx);

  if (should_release_from_user(task_ctx, have_user, user_ctx_word))
    release_admission(p, task_ctx);

  if (task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu)) {
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | task_ctx->admission_cpu,
                       SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (slow_path_requested(task_ctx, have_user, user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    if (cpu >= MAX_CPUS) {
      scx_bpf_dsq_insert(p, CONTROLLED_DSQ_ID, SCX_SLICE_DFL, enq_flags);
      return;
    }
    task_ctx->admission_cpu = cpu;

    if (grant_admission(p, task_ctx, lock_id, cpu)) {
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
      return;
    }

    task_ctx->inactive_wait = 1;
    task_ctx->admission_lock_id = lock_id;
    record_inactive_enqueue(lock_id, cpu);
    scx_bpf_dsq_insert(p, inactive_dsq_id(lock_id, cpu), SCX_SLICE_DFL, enq_flags);
    return;
  }

  task_ctx->inactive_wait = 0;
  if (slow_path_seen) {
    cpu = requested_cpu(p, task_ctx, -1);
    if (cpu >= MAX_CPUS) {
      scx_bpf_dsq_insert(p, CONTROLLED_DSQ_ID, SCX_SLICE_DFL, enq_flags);
      return;
    }

    task_ctx->admission_cpu = cpu;
    scx_bpf_dsq_insert(p, CONTROLLED_DSQ_ID, SCX_SLICE_DFL, enq_flags);
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

  scx_bpf_dsq_insert(p, SCX_DSQ_GLOBAL, SCX_SLICE_DFL, enq_flags);
}

static __always_inline bool drain_local_inactive(__u32 cpu) {
  struct cpu_inactive_hint *hint = lookup_inactive_hint(cpu);
  __u32 lock_id;

  if (!hint || !hint->total)
    return false;

#pragma unroll
  for (lock_id = 1; lock_id < MAX_LOCK_CLASSES; lock_id++) {
    __u32 *owner;

    if (!(hint->lock_mask & lock_id_bit(lock_id)))
      continue;

    owner = lookup_lock_cpu_owner(lock_id, cpu);
    if (!owner || *owner)
      continue;

    if (scx_bpf_dsq_move_to_local(inactive_dsq_id(lock_id, cpu))) {
      record_inactive_dequeue(lock_id, cpu);
      return true;
    }
  }

  return false;
}

void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
  bool active;
  (void)prev;

  if (!valid_cpu(cpu))
    return;

  active = cpu_is_active((__u32)cpu);
  if (!active)
    return;

  if (stats_only_enabled())
    return;

  if (drain_local_inactive((__u32)cpu))
    return;

  if (scx_bpf_dsq_move_to_local(CONTROLLED_DSQ_ID))
    return;

  steal_inactive((__u32)cpu);
}

static __always_inline void drain_inactive_to_controlled(__u32 cpu) {
  struct task_struct *p;
  __u32 lock_id;

#pragma unroll
  for (lock_id = 1; lock_id < MAX_LOCK_CLASSES; lock_id++) {
    bpf_rcu_read_lock();
    bpf_for_each(scx_dsq, p, inactive_dsq_id(lock_id, cpu), 0) {
      if (!scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, CONTROLLED_DSQ_ID, 0))
        break;
      record_inactive_dequeue(lock_id, cpu);
    }
    bpf_rcu_read_unlock();
  }
}

SEC("syscall")
int accordin_set_active_cpus(struct accordin_active_cpus_args *args) {
  __u64 wanted0, wanted1, wanted2, wanted3;
  __u32 all_active;
  __u32 n;

  if (!args)
    return -EINVAL;

  wanted0 = args->wanted0;
  wanted1 = args->wanted1;
  wanted2 = args->wanted2;
  wanted3 = args->wanted3;
  n = args->nr_cpus;
  if (n > MAX_CPUS)
    n = MAX_CPUS;

  all_active = active_words_cover_online_cpus(wanted0, wanted1, wanted2,
                                             wanted3, n);
  if (!all_active)
    active_cpus_all = 0;

  active_cpu_word0 = wanted0;
  active_cpu_word1 = wanted1;
  active_cpu_word2 = wanted2;
  active_cpu_word3 = wanted3;

  if (all_active)
    active_cpus_all = 1;

  return 0;
}

SEC("syscall")
int accordin_nudge_cpu(struct accordin_cpu_nudge_args *args) {
  __u32 cpu;

  if (!args)
    return -EINVAL;

  cpu = args->cpu;
  if (cpu >= MAX_CPUS)
    return -EINVAL;

  if (!stats_only_enabled() && args->drain_inactive)
    drain_inactive_to_controlled(cpu);

  scx_bpf_kick_cpu(cpu, 0);
  return 0;
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
    lock_id = user_admission_lock_id(user_ctx_word);
  refresh_slow_path_seen(task_ctx, have_user, user_ctx_word);

  if (should_release_from_user(task_ctx, have_user, user_ctx_word)) {
    release_admission(p, task_ctx);
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
  __u32 cpu;
  __u32 nr_cpus = scx_bpf_nr_cpu_ids();
  s32 ret;

  if (nr_cpus > MAX_CPUS)
    nr_cpus = MAX_CPUS;

  ret = scx_bpf_create_dsq(CONTROLLED_DSQ_ID, -1);
  if (ret)
    return ret;

  active_cpus_all = 1;
  active_cpu_word0 = ~0ULL;
  active_cpu_word1 = ~0ULL;
  active_cpu_word2 = ~0ULL;
  active_cpu_word3 = ~0ULL;

  for (cpu = 0; cpu < nr_cpus; cpu++) {
    ret = scx_bpf_create_dsq(inactive_dsq_id(1, cpu), -1);
    if (ret)
      return ret;
    ret = scx_bpf_create_dsq(inactive_dsq_id(2, cpu), -1);
    if (ret)
      return ret;
    ret = scx_bpf_create_dsq(inactive_dsq_id(3, cpu), -1);
    if (ret)
      return ret;
    ret = scx_bpf_create_dsq(inactive_dsq_id(4, cpu), -1);
    if (ret)
      return ret;
    ret = scx_bpf_create_dsq(inactive_dsq_id(5, cpu), -1);
    if (ret)
      return ret;
    ret = scx_bpf_create_dsq(inactive_dsq_id(6, cpu), -1);
    if (ret)
      return ret;
    ret = scx_bpf_create_dsq(inactive_dsq_id(7, cpu), -1);
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
