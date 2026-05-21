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
  __uint(max_entries, MAX_CPUS);
  __type(key, __u32);
  __type(value, struct cpu_inactive_hint);
} cpu_inactive_hint_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, MAX_CPUS);
  __type(key, __u32);
  __type(value, struct cpu_admission_debug);
} cpu_adm_dbg_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, STAT_NR);
  __type(key, __u32);
  __type(value, __u64);
} stats_map SEC(".maps");

volatile __u32 stats_only_mode = 0;
volatile __u32 single_lock_mode = 0;
volatile __u32 admission_debug_mode = 0;
volatile __u32 active_cpus_all = 1;
volatile __u64 active_cpu_word0 = ~0ULL;
volatile __u64 active_cpu_word1 = ~0ULL;
volatile __u64 active_cpu_word2 = ~0ULL;
volatile __u64 active_cpu_word3 = ~0ULL;
volatile __u32 cpu_admission_owner[MAX_CPUS];
volatile __u32 lock_inactive_total[MAX_LOCK_CLASSES];
volatile __u32 lock_admit_cursor[MAX_LOCK_CLASSES];
volatile __u64 dbg_acct_calls = 0;
volatile __u64 dbg_acct_read_ok = 0;

#endif /* __MAPS_BPF_H */
