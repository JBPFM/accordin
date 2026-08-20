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
/* Managed rank the next deficit probe on this CPU starts from. The probe scans
 * a fixed window, so this cursor advancing between calls is what carries the
 * scan across the classes the window left out. A cursor left behind by a wider
 * span is reset by the probe, since the fallback span is the widest one and a
 * first publish can therefore narrow the span under a cursor. */
volatile __u32 cpu_inactive_probe_cursor[MAX_CPUS];
/* Managed class ranks the deficit probe rotates over, published by userspace.
 * Lock ids are dense from 1 upward, so the ranks past the ids handed out hold
 * no class and probing them only spends window. 0 means no span published yet;
 * the probe then rotates over the whole managed space. */
volatile __u32 inactive_probe_span;
/* Routes threads whose user word carries the cond-variable sleep bit through
 * the cvready queues. Userspace only publishes that bit under the same switch,
 * so with this off the bit is never seen and the routing never runs. */
volatile __u32 cv_route_enabled;
/* Consecutive cvready dispatches one CPU may take before it has to serve the
 * class queues again. 0 turns the priority off: cvready is then drained only by
 * the periodic forced drain and by the attempt that follows a class scan which
 * gave this CPU nothing, so a cond waiter never overtakes a lock waiter. */
volatile __u32 cv_priority_streak_limit;
volatile __u32 cpu_cvready_streak[MAX_CPUS];
/* Cvready class this CPU is expected to find work in: the one it last drained,
 * or the one a park just chose this CPU for. It is what the dispatch gate reads
 * the kernel queue count of, so a parked waiter always has one CPU that looks
 * at its class whatever the probe cursor is doing. */
volatile __u32 cpu_last_cvready_lock[MAX_CPUS];
/* Managed rank the next cvready probe on this CPU starts from. Kept apart from
 * the inactive cursor so the two sweeps do not drag each other's start rank. */
volatile __u32 cpu_cvready_probe_cursor[MAX_CPUS];
volatile __u32 cvready_enqueue_seq;
/* Threads taken off a cvready queue. Paired with cvready_enqueue_seq this is an
 * exact occupancy test, which the scan-derived empty gate the inactive queues
 * use cannot be here: the cvready selection reaches a window of the class space
 * rather than all of it, so an empty verdict from one scan says nothing about
 * the classes it never probed. */
volatile __u32 cvready_drained_count;
volatile __u32 width_control_enabled;
volatile __u32 class_width[MAX_LOCK_CLASSES]; /* 0 = unlimited */
/* Waiters currently admitted for the class. A task counts from the grant that
 * admitted it until it takes the lock: width bounds the queue in front of a
 * lock, and the holder runs regardless of it. */
volatile __s32 class_active[MAX_LOCK_CLASSES];
volatile __u32 class_active_peak[MAX_LOCK_CLASSES];
volatile __u32 class_inactive_depth[MAX_LOCK_CLASSES];
/* High-water mark of class_inactive_depth. Contention arrives in bursts far
 * shorter than a controller window, so a queue can fill and drain entirely
 * between two reads of the live depth; the mark keeps that demand visible.
 * BPF only ever raises it, the controller clears it by reading it. */
volatile __u32 class_inactive_depth_peak[MAX_LOCK_CLASSES];
volatile __u64 class_active_underflow_events;
volatile __u32 inactive_previous_lock_percent;
volatile __u32 inactive_enqueue_seq;
volatile __u32 inactive_empty_seq;
volatile __u32 normal_enqueue_seq;
volatile __u32 normal_empty_seq;
/* Deadline of the next inactive scan that ignores the gates guarding the
 * ordinary scan. Concurrent CPUs may both observe it expired and both force;
 * that costs one extra scan and is why no exclusion is taken here. */
volatile __u64 inactive_force_drain_at;
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
volatile __u64 dispatch_inactive_forced;
volatile __u64 dbg_acct_calls = 0;
volatile __u64 dbg_acct_read_ok = 0;
/* Admission-routing evidence: updated with atomic adds so the totals reconcile
 * (wake_consumed_seen == granted + inactive + normal). */
volatile __u64 select_local_direct;
volatile __u64 wake_consumed_seen;
volatile __u64 wake_consumed_granted;
volatile __u64 wake_consumed_inactive;
volatile __u64 wake_consumed_normal;
volatile __u64 wake_read_fail;
volatile __u64 running_pending_grant_success;
volatile __u64 running_pending_grant_failure;
volatile __u64 block_release_read_fail;
/* Cond-variable routing evidence: cv_wake_enq == cv_grant_at_enq + cv_parked
 * plus the enqueues whose class had no CPU to grant on. */
volatile __u64 cv_wake_enq;
volatile __u64 cv_grant_at_enq;
volatile __u64 cv_parked;
volatile __u64 cv_dispatch;
volatile __u64 cv_dispatch_forced;
volatile __u64 cv_word_read_fail;

#endif /* __MAPS_BPF_H */
