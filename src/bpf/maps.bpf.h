/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __MAPS_BPF_H
#define __MAPS_BPF_H

#include <scx/common.bpf.h>
#include "intf.h"

struct {
  __uint(type, BPF_MAP_TYPE_TASK_STORAGE);
  __uint(map_flags, BPF_F_NO_PREALLOC);
  __type(key, int);
  __type(value, struct task_scx_ctx);
} task_ctx_map SEC(".maps");

struct {
  __uint(type, BPF_MAP_TYPE_HASH);
  __uint(max_entries, MAX_TASKS);
  __type(key, __u32);
  __type(value, __u64);
} thread_ctx_addr_map SEC(".maps");

volatile __u32 stats_only_mode;
struct admission_state admission;

#endif
