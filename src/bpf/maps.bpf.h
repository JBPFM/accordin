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
  __type(value, __u32); /* current admission owner pid, or 0 if empty */
} cpu_admission_owner_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, MAX_CPUS);
  __type(key, __u32);
  __type(value, __u32);
} cpu_dispatch_seq_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, STAT_NR);
  __type(key, __u32);
  __type(value, __u64);
} stats_map SEC(".maps");

struct active_cpumask_slot {
  struct bpf_cpumask __kptr *mask;
};

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, 1);
  __type(key, __u32);
  __type(value, struct active_cpumask_slot);
} active_cpumask_map SEC(".maps");

volatile __u32 stats_only_mode = 0;
volatile __u64 dbg_acct_calls = 0;
volatile __u64 dbg_acct_read_ok = 0;

#endif /* __MAPS_BPF_H */
