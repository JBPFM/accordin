/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __ADMISSION_BPF_H
#define __ADMISSION_BPF_H

/* Keep this header self-contained for standalone clangd parsing. */
#include <scx/common.bpf.h>

#include "intf.h"
#include "maps.bpf.h"

/*
 * Admission control and NUMA helpers.
 *
 * Manages per-task scheduling context creation, NUMA node lookups,
 * and the admit_task() routine that maintains active_local/active_remote
 * counters with NUMA-aware placement.
 */

/* ------------------------------------------------------------------ */
/*  Task context management                                            */
/* ------------------------------------------------------------------ */

/*
 * Lookup the per-task scheduling context via task local storage.
 * Returns NULL for tasks without a context.
 */
static __always_inline struct task_scx_ctx *
lookup_task_ctx(struct task_struct *p) {
  return bpf_task_storage_get(&task_ctx_map, p, 0, 0);
}

/*
 * Lookup or create a per-task scheduling context via task local storage.
 * Only creates for threads that registered a userspace lock context.
 * New tasks start admitted.
 *
 * Uses BPF_LOCAL_STORAGE_GET_F_CREATE for atomic create-if-absent,
 * replacing the old lookup+insert+lookup triple with a single call.
 */
static __always_inline struct task_scx_ctx *
get_or_create_task_ctx(struct task_struct *p) {
  struct task_scx_ctx *tc;

  tc = bpf_task_storage_get(&task_ctx_map, p, 0, 0);
  if (tc)
    return tc;

  __u32 pid = p->pid;
  __u64 *user_ptr_p = bpf_map_lookup_elem(&thread_ctx_addr_map, &pid);
  if (!user_ptr_p)
    return NULL;

  tc =
      bpf_task_storage_get(&task_ctx_map, p, 0, BPF_LOCAL_STORAGE_GET_F_CREATE);
  if (!tc)
    return NULL;

  tc->admitted = 1;
  tc->user_ctx_ptr = *user_ptr_p;

  return tc;
}

/* ------------------------------------------------------------------ */
/*  NUMA helpers                                                       */
/* ------------------------------------------------------------------ */

static __always_inline __s32 get_cpu_node(__s32 cpu) {
  if (cpu < 0 || cpu >= MAX_CPUS)
    return 0;

  return (__s32)ssc_cpu_node[cpu];
}

static __always_inline bool is_local_node(__s32 node) {
  return node == dominant_node;
}

static __always_inline __s32 get_ssc_cpu_by_index(__u32 idx) {
  if (idx >= ssc_cpu_count || idx >= MAX_CPUS)
    return -1;

  return (__s32)ssc_cpu_list[idx];
}

static __always_inline struct ssc_claim_state *lookup_ssc_claim_state(void) {
  __u32 key = 0;

  return bpf_map_lookup_elem(&ssc_claim_state_map, &key);
}

static __always_inline bool is_cpu_ssc_candidate(__s32 cpu) {
  if (cpu < 0 || cpu >= MAX_CPUS)
    return false;

  return ssc_cpu_rank[cpu] < ssc_cpu_count;
}

static __always_inline __u32 get_ssc_node_capacity(__s32 node) {
  if (node < 0 || node >= MAX_NODES)
    return 0;

  return ssc_node_capacity[node];
}

static __always_inline void reset_ssc_claim_state(struct ssc_claim_state *state) {
  if (!state)
    return;

  state->epoch = ssc_claim_epoch;
  state->claimed_count = 0;
  state->anchor_node = -1;
  state->anchor_capacity = 0;
}

static __always_inline void trim_ssc_claims_to_active_count(
    struct ssc_claim_state *state) {
  while (state->claimed_count > ssc_active_count) {
    __u32 claimed = state->claimed_count;
    __u32 slot;
    __u32 cpu;

    if (!claimed)
      break;

    slot = claimed - 1;
    if (slot >= MAX_CPUS) {
      state->claimed_count = MAX_CPUS;
      continue;
    }

    cpu = state->slot_cpu[slot];

    if (cpu < MAX_CPUS && state->cpu_epoch[cpu] == state->epoch &&
        state->cpu_slot[cpu] == slot)
      state->cpu_epoch[cpu] = 0;

    state->slot_cpu[slot] = 0;
    state->claimed_count = slot;
  }
}

static __always_inline bool is_cpu_ssc_core(__s32 cpu) {
  struct ssc_claim_state *state;

  if (!ssc_claim_epoch || !is_cpu_ssc_candidate(cpu))
    return false;

  state = lookup_ssc_claim_state();
  if (!state || state->epoch != ssc_claim_epoch)
    return false;

  if (state->cpu_epoch[cpu] != state->epoch)
    return false;

  return state->cpu_slot[cpu] < state->claimed_count &&
         state->cpu_slot[cpu] < ssc_active_count;
}

static __always_inline bool is_task_on_ssc_core(struct task_struct *p) {
  return is_cpu_ssc_core(scx_bpf_task_cpu(p));
}

static __always_inline bool try_claim_ssc_core(__s32 cpu) {
  struct ssc_claim_state *state;
  __s32 node;
  __u32 anchor_capacity;
  __u32 slot;
  bool claimed = false;

  if (!ssc_claim_epoch || !is_cpu_ssc_candidate(cpu))
    return false;

  state = lookup_ssc_claim_state();
  if (!state)
    return false;

  if (state->epoch == ssc_claim_epoch && state->cpu_epoch[cpu] == ssc_claim_epoch &&
      state->cpu_slot[cpu] < state->claimed_count &&
      state->cpu_slot[cpu] < ssc_active_count)
    return true;

  node = get_cpu_node(cpu);

  bpf_spin_lock(&state->lock);

  if (state->epoch != ssc_claim_epoch)
    reset_ssc_claim_state(state);

  trim_ssc_claims_to_active_count(state);

  if (state->cpu_epoch[cpu] == state->epoch &&
      state->cpu_slot[cpu] < state->claimed_count) {
    claimed = true;
    goto out_unlock;
  }

  slot = state->claimed_count;
  if (slot >= ssc_active_count || slot >= ssc_cpu_count || slot >= MAX_CPUS)
    goto out_unlock;

  if (state->anchor_node < 0) {
    state->anchor_node = node;
    state->anchor_capacity = get_ssc_node_capacity(node);
    if (!state->anchor_capacity)
      state->anchor_capacity = ssc_cpu_count;
  }

  anchor_capacity = state->anchor_capacity;
  if (node != state->anchor_node) {
    if (ssc_active_count <= anchor_capacity)
      goto out_unlock;
    if (slot < anchor_capacity)
      goto out_unlock;
  }

  state->cpu_epoch[cpu] = state->epoch;
  state->cpu_slot[cpu] = slot;
  state->slot_cpu[slot] = (__u32)cpu;
  state->claimed_count = slot + 1;
  claimed = true;

out_unlock:
  bpf_spin_unlock(&state->lock);
  return claimed;
}

static __always_inline bool ssc_claims_complete(void) {
  struct ssc_claim_state *state;

  if (!ssc_claim_epoch || !ssc_active_count)
    return false;

  state = lookup_ssc_claim_state();
  if (!state || state->epoch != ssc_claim_epoch)
    return false;

  return state->claimed_count >= ssc_active_count;
}

static __always_inline bool is_task_lock_protected(struct task_scx_ctx *tc) {
  if (!tc)
    return false;

  return tc->lock_state == LOCK_SCHED_STATE_SPINNER ||
         tc->lock_state == LOCK_SCHED_STATE_OWNER;
}

static __always_inline void keep_task_lock_protected(struct task_struct *p,
                                                     struct task_scx_ctx *tc) {
  if (!tc)
    return;

  tc->admitted = 1;
  if (p->scx.slice < SCX_SLICE_DFL)
    p->scx.slice = SCX_SLICE_DFL;
}

#endif /* __ADMISSION_BPF_H */
