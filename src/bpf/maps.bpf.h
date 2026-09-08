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

/* One-element holder for the periodic custody expiry timer. */
struct cv_timer_state {
  struct bpf_timer timer;
};

struct {
  __uint(type, BPF_MAP_TYPE_ARRAY);
  __uint(max_entries, 1);
  __type(key, __u32);
  __type(value, struct cv_timer_state);
} cv_timer_map SEC(".maps");

/* One flush pass counts into per-CPU memory: a running total held in a variable
 * has to be tracked exactly by the verifier, which then cannot fold the queue
 * walk. A syscall program runs pinned, so the entry belongs to one pass. */
struct cv_flush_tally {
  __u32 moved;
  __u32 expired;
  __u32 pending;
};

struct {
  __uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
  __uint(max_entries, 1);
  __type(key, __u32);
  __type(value, struct cv_flush_tally);
} cv_tally_map SEC(".maps");

volatile __u32 stats_only_mode;
struct admission_state admission;

/* Auto mode counts runnable tasks of the loading process, including tasks
 * waiting in a DSQ. Once activated, the detector stops updating the count. */
const volatile __u32 auto_admission;
const volatile __u32 auto_tgid;
const volatile __u32 auto_capacity;
__u32 auto_runnable;
__u32 auto_trigger_runnable;
__u64 auto_activated_at;

/* Custody configuration, published by the runtime before the scheduler loads. */
volatile __u32 cv_custody_enabled;
volatile __u64 cv_custody_limit_ns;
volatile __u64 cv_scan_period_ns;

/* Every park leaves custody exactly once: through a flush, through expiry, or
 * drained when the waiter or the scheduler goes away. */
__u64 cv_parked;
__u64 cv_parked_now;
__u64 cv_flush_calls;
__u64 cv_flushed;
__u64 cv_expired;
__u64 cv_flush_misses;
__u64 cv_drained;
__u32 flush_cursor;
__u32 admit_cursor;

#endif
