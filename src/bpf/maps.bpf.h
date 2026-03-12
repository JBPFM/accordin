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
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, MAX_TASKS);
	__type(key, __u32);   /* pid (tid) */
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

/* Window parameters */
volatile __u64 window_ns       = 4000000ULL;  /* 4ms */
volatile __u64 window_start_ns = 0;

/* Thresholds (x1000 fixed-point, e.g. 350 = 0.35) */
volatile __u32 p_high     = 350;
volatile __u32 p_low      = 200;
volatile __u32 p_w_ewma   = 0;
volatile __u32 ewma_alpha = 200;  /* 0.2 x 1000 */

/* Admission targets */
volatile __s64 target_local  = 1024;
volatile __s64 target_remote = 1024;
volatile __s64 max_target_local  = 1024;
volatile __s64 max_target_remote = 1024;
volatile __s64 active_local  = 0;
volatile __s64 active_remote = 0;

/* SSC parameters */
volatile __u64 max_ssc_wait_ns  = 50000000ULL;  /* 50ms */
volatile __u64 min_ssc_dwell_ns = 1000000ULL;   /* 1ms */

/* Aggregation accumulators (fed by stopping(), consumed by window rollover) */
volatile __u64 agg_run_ns  = 0;
volatile __u64 agg_wait_ns = 0;

/* Hysteresis counters */
volatile __u32 consec_high  = 0;
volatile __u32 consec_low   = 0;
volatile __u32 H_persist    = 2;
volatile __u32 L_persist    = 2;

/* NUMA */
volatile __s32 dominant_node = 0;

/* Stats */
volatile __u64 forced_release_cnt = 0;

#endif /* __MAPS_BPF_H */
