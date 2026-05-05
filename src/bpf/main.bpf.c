/* SPDX-License-Identifier: GPL-2.0-only */
#include <scx/common.bpf.h>

#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

#include "maps.bpf.h"

#define INACTIVE_DRAIN_INTERVAL 64U
#define INACTIVE_STEAL_SCAN 8U
#define DOMAIN_ACTIVE_CAS_RETRIES 8U

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

static __always_inline __u32 inactive_shard_for_cpu(__u32 cpu) {
  return cpu & (MAX_INACTIVE_SHARDS - 1);
}

static __always_inline __u64 distributed_inactive_dsq_id(__u32 lock_domain,
                                                         __u32 shard) {
  return DISTRIBUTED_INACTIVE_DSQ_BASE +
         ((__u64)(lock_domain - 1) * MAX_INACTIVE_SHARDS) + shard;
}

static __always_inline bool valid_lock_domain(__u32 lock_domain) {
  return lock_domain >= 1 && lock_domain <= MAX_LOCK_DOMAINS;
}

static __always_inline __u32 lock_domain_slot(__u32 lock_domain) {
  return lock_domain - 1;
}

static __always_inline __u32 domain_cpu_slot(__u32 lock_domain, __u32 cpu) {
  return lock_domain_slot(lock_domain) * MAX_CPUS + cpu;
}

static __always_inline void inc_stat(__u32 key) {
  __u64 *value;

  value = bpf_map_lookup_elem(&stats_map, &key);
  if (value)
    __sync_fetch_and_add(value, 1);
}

static __always_inline void init_task_ctx_if_needed(struct task_scx_ctx *task_ctx) {
  if (task_ctx->initialized)
    return;

  task_ctx->initialized = 1;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
  task_ctx->lock_domain = 0;
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

static __always_inline struct lock_domain_state *
lookup_domain_state(__u32 lock_domain) {
  __u32 slot;

  if (!valid_lock_domain(lock_domain))
    return 0;

  slot = lock_domain_slot(lock_domain);
  return bpf_map_lookup_elem(&lock_domain_state_map, &slot);
}

static __always_inline __u32 *lookup_domain_cpu_owner(__u32 lock_domain,
                                                      __u32 cpu) {
  __u32 slot;

  if (!valid_lock_domain(lock_domain) || cpu >= MAX_CPUS)
    return 0;

  slot = domain_cpu_slot(lock_domain, cpu);
  if (slot >= DOMAIN_CPU_SLOTS)
    return 0;

  return bpf_map_lookup_elem(&domain_cpu_owner_map, &slot);
}

static __always_inline __u32 effective_lock_budget(void) {
  __u32 nr_cpus = scx_bpf_nr_cpu_ids();
  __u32 budget = initial_lock_budget;

  if (nr_cpus > MAX_CPUS)
    nr_cpus = MAX_CPUS;
  if (!budget || budget > nr_cpus)
    budget = nr_cpus;
  if (!budget)
    budget = 1;
  return budget;
}

static __always_inline bool domain_owner_cas(__u32 *owner, __u32 old_owner,
                                             __u32 new_owner) {
  return __sync_val_compare_and_swap(owner, old_owner, new_owner) == old_owner;
}

static __always_inline bool try_take_domain_budget(
    struct lock_domain_state *domain, __u32 budget) {
  __u32 i;

#pragma clang loop unroll(full)
  for (i = 0; i < DOMAIN_ACTIVE_CAS_RETRIES; i++) {
    __u32 active_count = domain->active_count;

    if (active_count >= budget)
      return false;

    if (__sync_val_compare_and_swap(&domain->active_count, active_count,
                                    active_count + 1) == active_count)
      return true;
  }

  return false;
}

static __always_inline void release_domain_budget(
    struct lock_domain_state *domain) {
  __sync_fetch_and_sub(&domain->active_count, 1);
}

static __always_inline bool should_drain_inactive(__u32 cpu, __u32 *owner) {
  __u32 *seq;
  __u32 next;

  if (!owner || !*owner)
    return true;

  seq = bpf_map_lookup_elem(&cpu_dispatch_seq_map, &cpu);
  if (!seq)
    return false;

  next = *seq + 1;
  *seq = next;
  return (next & (INACTIVE_DRAIN_INTERVAL - 1)) == 0;
}

static __always_inline void set_task_admission(struct task_struct *p,
                                               struct task_scx_ctx *task_ctx,
                                               __u32 cpu,
                                               __u32 lock_domain) {
  task_ctx->admitted = 1;
  task_ctx->holds_admission = 1;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->inactive_wait = 0;
  task_ctx->admission_cpu = cpu;
  task_ctx->lock_domain = lock_domain;
  (void)p;
}

static __always_inline bool reserve_domain_cpu(__u32 lock_domain, __u32 cpu) {
  struct lock_domain_state *domain;
  __u32 *owner;
  __u32 budget;

  domain = lookup_domain_state(lock_domain);
  owner = lookup_domain_cpu_owner(lock_domain, cpu);
  if (!domain || !owner)
    return false;

  budget = effective_lock_budget();
  if (!try_take_domain_budget(domain, budget)) {
    inc_stat(STAT_DISTRIBUTED_RESERVE_FAIL);
    return false;
  }

  if (!domain_owner_cas(owner, 0, ADMISSION_OWNER_RESERVED)) {
    release_domain_budget(domain);
    return false;
  }

  return true;
}

static __always_inline void release_domain_cpu(__u32 lock_domain, __u32 cpu,
                                               __u32 pid,
                                               bool allow_reserved) {
  struct lock_domain_state *domain;
  __u32 *owner;

  domain = lookup_domain_state(lock_domain);
  owner = lookup_domain_cpu_owner(lock_domain, cpu);
  if (!domain || !owner)
    return;

  if (domain_owner_cas(owner, pid, 0))
    release_domain_budget(domain);
  else if (allow_reserved &&
           domain_owner_cas(owner, ADMISSION_OWNER_RESERVED, 0))
    release_domain_budget(domain);
}

static __always_inline bool grant_domain_admission(struct task_struct *p,
                                                   struct task_scx_ctx *task_ctx,
                                                   __u32 cpu,
                                                   __u32 lock_domain) {
  struct lock_domain_state *domain;
  __u32 pid = p->pid;
  __u32 *owner;
  __u32 budget;

  domain = lookup_domain_state(lock_domain);
  owner = lookup_domain_cpu_owner(lock_domain, cpu);
  if (!domain || !owner)
    return false;

  if (*owner == pid) {
    set_task_admission(p, task_ctx, cpu, lock_domain);
    return true;
  }

  if (domain_owner_cas(owner, ADMISSION_OWNER_RESERVED, pid)) {
    set_task_admission(p, task_ctx, cpu, lock_domain);
    return true;
  }

  if (*owner)
    return false;

  budget = effective_lock_budget();
  if (!try_take_domain_budget(domain, budget))
    return false;

  if (!domain_owner_cas(owner, 0, pid)) {
    release_domain_budget(domain);
    return false;
  }

  set_task_admission(p, task_ctx, cpu, lock_domain);
  return true;
}

static __always_inline bool try_move_domain_shard(__u32 lock_domain,
                                                  __u32 source_shard,
                                                  __u32 target_cpu,
                                                  bool steal) {
  if (source_shard >= MAX_INACTIVE_SHARDS)
    return false;

  if (!reserve_domain_cpu(lock_domain, target_cpu))
    return false;

  if (scx_bpf_dsq_move_to_local(
          distributed_inactive_dsq_id(lock_domain, source_shard))) {
    inc_stat(steal ? STAT_DISTRIBUTED_STEAL_MOVE
                   : STAT_DISTRIBUTED_LOCAL_MOVE);
    return true;
  }

  release_domain_cpu(lock_domain, target_cpu, ADMISSION_OWNER_RESERVED, true);
  return false;
}

static __always_inline bool try_move_domain_remote(__u32 lock_domain,
                                                   __u32 target_shard,
                                                   __u32 target_cpu) {
  struct lock_domain_state *domain;
  __u32 start;
  __u32 s;

  domain = lookup_domain_state(lock_domain);
  if (!domain)
    return false;

  start = domain->rr_cursor;
  if (start >= MAX_INACTIVE_SHARDS)
    start = 0;

#pragma clang loop unroll(disable)
  for (s = 0; s < DISTRIBUTED_STEAL_SCAN; s++) {
    __u32 shard = start + s;

    if (shard >= MAX_INACTIVE_SHARDS)
      shard -= MAX_INACTIVE_SHARDS;
    if (shard == target_shard)
      continue;

    if (try_move_domain_shard(lock_domain, shard, target_cpu, true)) {
      domain->rr_cursor = (shard + 1) & (MAX_INACTIVE_SHARDS - 1);
      return true;
    }
  }

  return false;
}

static __always_inline bool rescue_domain_shard(__u32 lock_domain,
                                                __u32 source_shard) {
  if (source_shard >= MAX_INACTIVE_SHARDS)
    return false;

  if (!scx_bpf_dsq_move_to_local(
          distributed_inactive_dsq_id(lock_domain, source_shard)))
    return false;

  inc_stat(STAT_DISTRIBUTED_RESCUE_MOVE);
  return true;
}

static __always_inline bool rescue_domain_remote(__u32 lock_domain,
                                                 __u32 target_shard) {
  struct lock_domain_state *domain;
  __u32 start;
  __u32 s;

  domain = lookup_domain_state(lock_domain);
  if (!domain)
    return false;

  start = domain->rr_cursor;
  if (start >= MAX_INACTIVE_SHARDS)
    start = 0;

#pragma clang loop unroll(disable)
  for (s = 0; s < MAX_INACTIVE_SHARDS; s++) {
    __u32 shard = start + s;

    if (shard >= MAX_INACTIVE_SHARDS)
      shard -= MAX_INACTIVE_SHARDS;
    if (shard == target_shard)
      continue;

    if (rescue_domain_shard(lock_domain, shard)) {
      domain->rr_cursor = (shard + 1) & (MAX_INACTIVE_SHARDS - 1);
      return true;
    }
  }

  return false;
}

static __always_inline void update_cpu_rr_cursor(__u32 cpu, __u32 lock_domain) {
  __u32 *cursor;
  __u32 next;

  if (cpu >= MAX_CPUS || !valid_lock_domain(lock_domain))
    return;

  cursor = bpf_map_lookup_elem(&cpu_lock_rr_cursor_map, &cpu);
  if (!cursor)
    return;

  next = lock_domain_slot(lock_domain) + 1;
  if (*cursor != next)
    *cursor = next;
}

static __always_inline bool dispatch_distributed_inactive(__u32 cpu) {
  __u32 *cursor;
  __u32 start = 0;
  __u32 nr_cpus = scx_bpf_nr_cpu_ids();
  __u32 local_shard;
  __u32 i;

  if (!distributed_inactive_pool || cpu >= MAX_CPUS)
    return false;

  if (nr_cpus > MAX_CPUS)
    nr_cpus = MAX_CPUS;
  if (cpu >= nr_cpus)
    return false;

  cursor = bpf_map_lookup_elem(&cpu_lock_rr_cursor_map, &cpu);
  if (cursor && *cursor < MAX_LOCK_DOMAINS)
    start = *cursor;

  local_shard = inactive_shard_for_cpu(cpu);
#pragma clang loop unroll(disable)
  for (i = 0; i < MAX_LOCK_DOMAINS; i++) {
    __u32 slot = start + i;
    __u32 lock_domain;

    if (slot >= MAX_LOCK_DOMAINS)
      slot -= MAX_LOCK_DOMAINS;

    lock_domain = slot + 1;
    if (try_move_domain_shard(lock_domain, local_shard, cpu, false)) {
      update_cpu_rr_cursor(cpu, lock_domain);
      return true;
    }

    if (try_move_domain_remote(lock_domain, local_shard, cpu)) {
      update_cpu_rr_cursor(cpu, lock_domain);
      return true;
    }
  }

  if (cursor)
    *cursor = start + 1 < MAX_LOCK_DOMAINS ? start + 1 : 0;
  return false;
}

static __always_inline bool rescue_distributed_inactive(__u32 cpu) {
  __u32 *cursor;
  __u32 start = 0;
  __u32 nr_cpus = scx_bpf_nr_cpu_ids();
  __u32 local_shard;
  __u32 i;

  if (!distributed_inactive_pool || cpu >= MAX_CPUS)
    return false;

  if (nr_cpus > MAX_CPUS)
    nr_cpus = MAX_CPUS;
  if (cpu >= nr_cpus)
    return false;

  cursor = bpf_map_lookup_elem(&cpu_lock_rr_cursor_map, &cpu);
  if (cursor && *cursor < MAX_LOCK_DOMAINS)
    start = *cursor;

  local_shard = inactive_shard_for_cpu(cpu);
#pragma clang loop unroll(disable)
  for (i = 0; i < MAX_LOCK_DOMAINS; i++) {
    __u32 slot = start + i;
    __u32 lock_domain;

    if (slot >= MAX_LOCK_DOMAINS)
      slot -= MAX_LOCK_DOMAINS;

    lock_domain = slot + 1;
    if (rescue_domain_shard(lock_domain, local_shard)) {
      update_cpu_rr_cursor(cpu, lock_domain);
      return true;
    }

    if (rescue_domain_remote(lock_domain, local_shard)) {
      update_cpu_rr_cursor(cpu, lock_domain);
      return true;
    }
  }

  if (cursor)
    *cursor = start + 1 < MAX_LOCK_DOMAINS ? start + 1 : 0;
  return false;
}

static __always_inline bool steal_inactive(__u32 cpu) {
  __u32 nr_cpus = scx_bpf_nr_cpu_ids();
  __u32 i;

  if (nr_cpus > MAX_CPUS)
    nr_cpus = MAX_CPUS;

  if (cpu >= nr_cpus || nr_cpus <= 1)
    return false;

#pragma unroll
  for (i = 1; i <= INACTIVE_STEAL_SCAN; i++) {
    __u32 victim = cpu + i;

    if (i >= nr_cpus)
      break;

    if (victim >= nr_cpus)
      victim -= nr_cpus;

    if (scx_bpf_dsq_move_to_local(inactive_dsq_id(victim)))
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
                                          struct user_admission_ctx *user_ctx) {
  if (!refresh_user_ctx_ptr(p, task_ctx))
    return false;

  if (bpf_probe_read_user(user_ctx, sizeof(*user_ctx),
                          (const void *)(unsigned long)task_ctx->user_ctx_ptr))
    return false;

  return true;
}

static __always_inline bool user_slow_path_pending(
    const struct user_admission_ctx *user_ctx) {
  return user_ctx->flags & USER_ADMISSION_SLOW_PATH_PENDING;
}

static __always_inline bool user_in_critical_section(
    const struct user_admission_ctx *user_ctx) {
  return user_ctx->flags & USER_ADMISSION_IN_CRITICAL_SECTION;
}

static __always_inline bool user_explicit_release(
    const struct user_admission_ctx *user_ctx) {
  return !user_slow_path_pending(user_ctx) &&
         !user_in_critical_section(user_ctx);
}

static __always_inline __u32 user_lock_domain(
    const struct user_admission_ctx *user_ctx) {
  if (!distributed_inactive_pool)
    return 0;
  if (!valid_lock_domain(user_ctx->lock_domain))
    return 0;
  return user_ctx->lock_domain;
}

static __always_inline void clear_admission_state(struct task_scx_ctx *task_ctx) {
  task_ctx->admitted = 0;
  task_ctx->holds_admission = 0;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->inactive_wait = 0;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
  task_ctx->lock_domain = 0;
}

static __always_inline void release_admission(struct task_struct *p,
                                              struct task_scx_ctx *task_ctx) {
  __u32 cpu = task_ctx->admission_cpu;
  __u32 lock_domain = task_ctx->lock_domain;
  __u32 pid = p->pid;
  __u32 *owner;

  if (distributed_inactive_pool && valid_lock_domain(lock_domain)) {
    release_domain_cpu(lock_domain, cpu, pid, false);
  } else {
    owner = lookup_cpu_owner(cpu);
    if (owner && *owner == pid)
      *owner = 0;
  }

  clear_admission_state(task_ctx);

  if (cpu < MAX_CPUS)
    scx_bpf_kick_cpu(cpu, 0);
}

static __always_inline bool grant_admission(struct task_struct *p,
                                            struct task_scx_ctx *task_ctx,
                                            __u32 cpu) {
  __u32 pid = p->pid;
  __u32 *owner;

  if (distributed_inactive_pool && valid_lock_domain(task_ctx->lock_domain))
    return grant_domain_admission(p, task_ctx, cpu, task_ctx->lock_domain);

  owner = lookup_cpu_owner(cpu);
  if (!owner)
    return false;

  if (*owner && *owner != pid)
    return false;

  *owner = pid;
  set_task_admission(p, task_ctx, cpu, 0);
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
    const struct user_admission_ctx *user_ctx) {
  if (task_ctx->holds_admission)
    return false;

  if (have_user)
    return user_slow_path_pending(user_ctx);

  return false;
}

static __always_inline bool should_release_from_user(
    const struct task_scx_ctx *task_ctx, bool have_user,
    const struct user_admission_ctx *user_ctx) {
  if (!task_ctx->holds_admission || !have_user)
    return false;

  return user_explicit_release(user_ctx);
}

static __always_inline bool task_in_critical_section(
    const struct task_scx_ctx *task_ctx, bool have_user,
    const struct user_admission_ctx *user_ctx) {
  if (have_user)
    return user_in_critical_section(user_ctx);

  return false;
}

static __always_inline void protect_critical_section(struct task_struct *p,
                                                     struct task_scx_ctx *task_ctx) {
  __u32 cpu = task_ctx->admission_cpu;

  if (cpu >= MAX_CPUS || !task_cpu_allowed(p, cpu))
    cpu = pick_allowed_cpu(p, bpf_get_smp_processor_id());

  if (cpu >= MAX_CPUS)
    return;

  task_ctx->admission_cpu = cpu;
  if (!task_ctx->holds_admission)
    grant_admission(p, task_ctx, cpu);

  task_ctx->must_run_on_admission_cpu = 1;
}

static __always_inline void refresh_running_state(struct task_struct *p) {
  struct task_scx_ctx *task_ctx;
  struct user_admission_ctx user_ctx = {};
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

  if (task_ctx->inactive_wait &&
      slow_path_requested(task_ctx, have_user, &user_ctx) &&
      task_cpu_allowed(p, cpu)) {
    if (distributed_inactive_pool && valid_lock_domain(task_ctx->lock_domain)) {
      task_ctx->admission_cpu = cpu;
      if (grant_admission(p, task_ctx, cpu))
        return;
    } else if (grant_admission(p, task_ctx, cpu)) {
      return;
    }
  }

  if (task_ctx->must_run_on_admission_cpu && task_ctx->admission_cpu == cpu)
    task_ctx->must_run_on_admission_cpu = 0;
}

s32 BPF_STRUCT_OPS(accordin_select_cpu, struct task_struct *p, s32 prev_cpu,
                   u64 wake_flags) {
  struct task_scx_ctx *task_ctx;
  struct user_admission_ctx user_ctx = {};
  bool is_idle = false;
  bool have_user = false;
  bool slow_path = false;
  __u32 lock_domain = 0;
  s32 cpu;

  task_ctx = get_task_ctx(p);
  if (task_ctx) {
    have_user = read_user_ctx(p, task_ctx, &user_ctx);
    clear_invalid_admission_cpu(p, task_ctx);
    slow_path = slow_path_requested(task_ctx, have_user, &user_ctx);
    if (slow_path)
      lock_domain = user_lock_domain(&user_ctx);
  }

  if (task_ctx && task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu))
    return (s32)task_ctx->admission_cpu;

  if (slow_path && valid_cpu(prev_cpu) &&
      task_cpu_allowed(p, (__u32)prev_cpu)) {
    task_ctx->lock_domain = lock_domain;
    task_ctx->admission_cpu = (__u32)prev_cpu;
    return prev_cpu;
  }

  cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

  if (slow_path && valid_cpu(cpu)) {
    task_ctx->lock_domain = lock_domain;
    task_ctx->admission_cpu = (__u32)cpu;
  }

  if (is_idle && !slow_path)
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

  return cpu;
}

void BPF_STRUCT_OPS(accordin_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *task_ctx;
  struct user_admission_ctx user_ctx = {};
  bool have_user = false;
  __u32 cpu;
  __u32 lock_domain;

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
    lock_domain = user_lock_domain(&user_ctx);
    task_ctx->lock_domain = lock_domain;

    if (grant_admission(p, task_ctx, cpu)) {
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
      return;
    }

    task_ctx->inactive_wait = 1;
    if (distributed_inactive_pool && valid_lock_domain(lock_domain)) {
      __u32 shard = inactive_shard_for_cpu(cpu);
      inc_stat(STAT_DISTRIBUTED_ENQUEUE);
      scx_bpf_dsq_insert(p, distributed_inactive_dsq_id(lock_domain, shard),
                         SCX_SLICE_DFL, enq_flags);
    } else {
      if (distributed_inactive_pool)
        inc_stat(STAT_DISTRIBUTED_FALLBACK);
      scx_bpf_dsq_insert(p, inactive_dsq_id(cpu), SCX_SLICE_DFL, enq_flags);
    }
    return;
  }

  task_ctx->inactive_wait = 0;
  task_ctx->lock_domain = 0;
  scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
}

void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
  __u32 *owner;
  (void)prev;

  if (valid_cpu(cpu)) {
    if (distributed_inactive_pool) {
      if (dispatch_distributed_inactive((__u32)cpu))
        return;
    } else {
      owner = lookup_cpu_owner((__u32)cpu);
      if (should_drain_inactive((__u32)cpu, owner) &&
          scx_bpf_dsq_move_to_local(inactive_dsq_id((__u32)cpu)))
        return;
    }
  }

  if (scx_bpf_dsq_move_to_local(READY_DSQ_ID))
    return;

  if (!valid_cpu(cpu))
    return;

  if (distributed_inactive_pool && rescue_distributed_inactive((__u32)cpu))
    return;

  if (scx_bpf_dsq_move_to_local(inactive_dsq_id((__u32)cpu)))
    return;

  steal_inactive((__u32)cpu);
}

void BPF_STRUCT_OPS(accordin_running, struct task_struct *p) {
  refresh_running_state(p);
}

void BPF_STRUCT_OPS(accordin_tick, struct task_struct *p) {
  refresh_running_state(p);
}

void BPF_STRUCT_OPS(accordin_stopping, struct task_struct *p, bool runnable) {
  struct task_scx_ctx *task_ctx;
  struct user_admission_ctx user_ctx = {};
  bool have_user = false;

  task_ctx = lookup_task_ctx(p);
  if (!task_ctx)
    return;

  have_user = read_user_ctx(p, task_ctx, &user_ctx);

  if (should_release_from_user(task_ctx, have_user, &user_ctx)) {
    release_admission(p, task_ctx);
    return;
  }

  if (runnable && task_in_critical_section(task_ctx, have_user, &user_ctx)) {
    protect_critical_section(p, task_ctx);
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
  else if (task_ctx && distributed_inactive_pool &&
           valid_lock_domain(task_ctx->lock_domain) &&
           task_ctx->admission_cpu < MAX_CPUS)
    release_domain_cpu(task_ctx->lock_domain, task_ctx->admission_cpu, pid,
                       true);

  bpf_map_delete_elem(&thread_ctx_addr_map, &pid);
  bpf_task_storage_delete(&task_ctx_map, p);
}

s32 BPF_STRUCT_OPS_SLEEPABLE(accordin_init) {
  __u32 cpu;
  __u32 lock_domain;
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

  if (distributed_inactive_pool) {
#pragma clang loop unroll(disable)
    for (lock_domain = 1; lock_domain <= MAX_LOCK_DOMAINS; lock_domain++) {
      __u32 shard;
#pragma clang loop unroll(disable)
      for (shard = 0; shard < MAX_INACTIVE_SHARDS; shard++) {
        scx_bpf_create_dsq(distributed_inactive_dsq_id(lock_domain, shard), -1);
      }
    }
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
