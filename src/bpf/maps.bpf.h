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
  __type(value, __u64); /* user-space pointer to admission word */
} thread_ctx_addr_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, STAT_NR);
  __type(key, __u32);
  __type(value, __u64);
} stats_map SEC(".maps");

volatile __u32 stats_only_mode = 0;
volatile __u32 single_lock_mode = 0;
volatile __u32 debug_counters_mode = 0;
volatile __u32 cpu_admission_owner[MAX_CPUS];
volatile __u32 cpu_last_inactive_lock[MAX_CPUS];
volatile __u32 cpu_inactive_dispatch_count[MAX_CPUS];
volatile __u32 width_control_enabled;
volatile __u32 class_width[MAX_LOCK_CLASSES]; /* 0 = unlimited */
volatile __s32 class_active[MAX_LOCK_CLASSES];
volatile __u32 class_active_peak[MAX_LOCK_CLASSES];
volatile __u32 class_inactive_depth[MAX_LOCK_CLASSES];
volatile __u64 class_active_underflow_events;
volatile __u32 inactive_previous_lock_percent;
volatile __u32 inactive_enqueue_seq;
volatile __u32 inactive_empty_seq;
volatile __u32 normal_enqueue_seq;
volatile __u32 normal_empty_seq;
volatile __u32 registered_thread_count;
volatile __u64 dispatch_calls;
volatile __u64 dispatch_normal_skip_seq;
volatile __u64 dispatch_normal_attempts;
volatile __u64 dispatch_normal_success;
volatile __u64 dispatch_normal_empty;
volatile __u64 dispatch_inactive_unavailable;
volatile __u64 dispatch_inactive_budget_blocked;
volatile __u64 dispatch_inactive_attempts;
volatile __u64 dispatch_inactive_success;
volatile __u64 dispatch_inactive_empty;
volatile __u64 dbg_acct_calls = 0;
volatile __u64 dbg_acct_read_ok = 0;

#endif /* __MAPS_BPF_H */
