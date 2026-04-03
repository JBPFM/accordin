/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __MAPS_BPF_H
#define __MAPS_BPF_H

/* Keep this header self-contained for standalone clangd parsing. */
#include <scx/common.bpf.h>

#include "intf.h"

/* ------------------------------------------------------------------ */
/*  Maps                                                               */
/* ------------------------------------------------------------------ */

struct {
  __uint(type, BPF_MAP_TYPE_TASK_STORAGE);
  __uint(map_flags, BPF_F_NO_PREALLOC);
  __type(key, int);
  __type(value, struct task_scx_ctx);
} task_ctx_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_HASH);
  __uint(max_entries, MAX_TASKS);
  __type(key, __u32);   /* pid (tid) */
  __type(value, __u64); /* user-space pointer to lock_sched_thread_ctx */
} thread_ctx_addr_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, STAT_NR);
  __type(key, __u32);
  __type(value, __u64);
} stats_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, MAX_CPUS);
  __type(key, __u32);
  __type(value, __u32); /* NUMA node id */
} cpu_to_node SEC(".maps");

/* ------------------------------------------------------------------ */
/*  Globals (mutable .bss)                                             */
/* ------------------------------------------------------------------ */

// /* Window parameters */
// volatile __u64 window_ns       = 200000000ULL;  /* 200ms */
// volatile __u64 window_start_ns = 0;
//
// /* Thresholds (x1000 fixed-point, e.g. 350 = 0.35) */
// volatile __u32 p_high     = 350;
// volatile __u32 p_low      = 200;
// volatile __u32 p_w_ewma   = 0;
// volatile __u32 ewma_alpha = 300;  /* 0.3 x 1000 */
//
// /* Admission targets */
// volatile __s64 target_local  = 1024;
// volatile __s64 target_remote = 1024;
// volatile __s64 max_target_local  = 1024;
// volatile __s64 max_target_remote = 1024;
// volatile __s64 active_local  = 0;
// volatile __s64 active_remote = 0;
//
// /* SSC parameters */
// volatile __u64 max_ssc_wait_ns  = 50000000ULL;  /* 50ms */
// volatile __u64 min_ssc_dwell_ns = 1000000ULL;   /* 1ms */

/*
 * Per-CPU aggregation accumulators — eliminates cross-core atomic
 * contention on every stopping()/tick().
 */
struct agg_percpu {
  __u64 epoch;
  __u64 run_ns;
  __u64 wait_ns;
  __u64 unlock_count;
};

struct {
  __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
  __uint(max_entries, 1);
  __type(key, __u32);
  __type(value, struct agg_percpu);
} agg_percpu_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, MAX_CPUS);
  __type(key, __u32);
  __type(value, struct ssc_vote_slot);
} ssc_vote_slot_map SEC(".maps");

/* Hysteresis counters */
volatile __u32 consec_high = 0;
volatile __u32 consec_low = 0;
volatile __u32 H_persist = 2;
volatile __u32 L_persist = 3;
volatile __u64 ssc_vote_window_ns = 200000000ULL;

/* NUMA */
volatile __s32 dominant_node = 0;
volatile __u32 ssc_active_count = 2;
volatile __u32 ssc_cpu_count = 0;
volatile __u32 ssc_cpu_list[MAX_CPUS] = {};
volatile __u16 ssc_cpu_rank[MAX_CPUS] = {};

/* Stats */
volatile __u64 forced_release_cnt = 0;
volatile __u32 stats_only_mode = 0;
volatile __u64 ssc_vote_epoch = 0;
volatile __u64 ssc_vote_start_ns = 0;
volatile __u64 ssc_vote_decided_epoch = 0;
volatile __u64 ssc_vote_sum_run = 0;
volatile __u64 ssc_vote_sum_wait = 0;
volatile __u64 ssc_vote_sum_unlock_count = 0;
volatile __u32 ssc_vote_publish_count = 0;
volatile __u64 ssc_vote_last_score = 0;
volatile __u64 ssc_vote_last_effective_score = 0;
volatile __u32 ssc_bootstrap_mature_windows = 0;
volatile __u32 ssc_pending_capped_grow = 0;
volatile __u32 ssc_vote_consec_grow = 0;
volatile __u32 ssc_vote_consec_shrink = 0;
volatile __u32 ssc_search_phase = SSC_SEARCH_SEEK;
volatile __u32 ssc_best_count = 2;
volatile __u64 ssc_best_score = 0;
volatile __u32 ssc_best_candidate_count = 0;
volatile __u32 ssc_best_candidate_streak = 0;
volatile __u32 ssc_refine_low = 2;
volatile __u32 ssc_refine_high = 2;

/* Per-window debug stats — updated only when dbg_counters_enabled=1 */
volatile __u32 dbg_counters_enabled = 0; /* 0=off (production), 1=on (debug) */
volatile __u64 dbg_win_run = 0;
volatile __u64 dbg_win_wait = 0;
volatile __u64 dbg_acct_calls = 0;   /* total account_task_activity calls */
volatile __u64 dbg_acct_read_ok = 0; /* bpf_probe_read_user succeeded */
volatile __u64 dbg_refine_entries = 0;
volatile __u64 dbg_refine_single_point = 0;
volatile __u64 dbg_refine_noop_targets = 0;
volatile __u64 dbg_noop_resizes = 0;
volatile __u64 dbg_active_count_changes = 0;
volatile __u64 dbg_bad_steady_rebases = 0;
volatile __u64 dbg_task_ctx_creates = 0;
volatile __u64 dbg_task_ctx_misses = 0;
volatile __u64 dbg_grow_uses_capped_step = 0;
volatile __u64 dbg_last_grow_target = 0;

#endif /* __MAPS_BPF_H */
