/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * scx_ulock shared interface between BPF and user-space controller.
 *
 * All structs here are written by either the controller or the lock library
 * and read by the BPF scheduler hot path.  Keep hot-path fields near the top
 * of each struct to fit them in a single cache line where possible.
 */
#pragma once

/* In BPF context vmlinux.h is included first and defines __VMLINUX_H__.
 * In user-space (bindgen, controller) we provide compatible type aliases. */
#ifndef __VMLINUX_H__
typedef unsigned int          u32;
typedef unsigned long long    u64;
#endif

/* Maximum number of CPUs and per-thread epoch slots supported. */
#define MAX_CPUS  1024
#define MAX_SLOTS 1024

/* --------------------------------------------------------------------------
 * Task classification
 * --------------------------------------------------------------------------
 * Written by the controller into task_class_map; read by BPF enqueue.
 */
enum task_class {
	TASK_NORMAL         = 0, /* not lock-intensive, scheduled on normal CPUs */
	TASK_CANDIDATE      = 1, /* accumulating hot epochs, not yet promoted    */
	TASK_LOCK_INTENSIVE = 2, /* in SSC, scheduled on SSC CPUs                */
};

/* --------------------------------------------------------------------------
 * Global scheduler configuration
 * --------------------------------------------------------------------------
 * Stored in ulock_config_map[0].  Written by the controller; read by BPF.
 * Update ssc_gen after any SSC width or mask change to trigger lazy migration.
 */
struct ulock_config {
	u64 epoch_ns;           /* epoch duration in nanoseconds (default 20 ms) */
	u64 control_period_ns;  /* controller wakeup period (default 100 ms)     */
	u32 enter_threshold_pct; /* wait_ratio % to enter SSC (default 10)       */
	u32 exit_threshold_pct;  /* wait_ratio % to leave SSC (default 5)        */
	u32 min_contended_acq;   /* minimum contended acquisitions (default 64)  */
	u32 hot_epochs_needed;   /* consecutive hot epochs before entering SSC   */
	u32 cool_epochs_needed;  /* consecutive cool epochs before leaving SSC   */
	u32 ssc_width;           /* current SSC CPU count (0 = SSC disabled)     */
	u32 max_ssc_width;       /* upper bound for SSC width search             */
	u32 partial_mode;        /* non-zero if SCX_OPS_SWITCH_PARTIAL is active */
	u64 ssc_gen;             /* generation counter; bump on every SSC change */
};

/* --------------------------------------------------------------------------
 * SSC CPU bitmask
 * --------------------------------------------------------------------------
 * Stored in ssc_cpumask[0].  Written by the controller; read by BPF dispatch
 * and select_cpu to determine whether a CPU belongs to the SSC.
 */
struct ulock_cpumask {
	u64 bits[MAX_CPUS / 64]; /* one bit per CPU; bit N = CPU N is in SSC */
};

/* --------------------------------------------------------------------------
 * Per-task scheduler context
 * --------------------------------------------------------------------------
 * Stored in task_ctx_map (BPF_MAP_TYPE_TASK_STORAGE).  Initialised by
 * ulock_init_task; updated by enqueue/running/stopping callbacks.
 * The controller does NOT write this struct directly; it updates
 * task_class_map and lets BPF refresh cls at the next scheduling event.
 */
struct task_ctx {
	u32 pid;
	u32 tgid;
	u32 cls;            /* current task_class; refreshed lazily on ssc_gen change */
	u32 epoch_id;       /* epoch this task was last classified in                  */
	u64 run_ns;         /* total on-CPU time accumulated this epoch               */
	u64 runnable_ns;    /* ktime_ns when task last entered running state          */
	u32 mig_count;      /* number of cross-DSQ migrations observed               */
	u32 lock_domain_id; /* lock domain affinity hint from lock library            */
	u64 last_ssc_gen;   /* ssc_gen value at last class refresh                   */
	u32 hot_epochs;     /* consecutive epochs above enter_threshold (BPF copy)   */
	u32 cool_epochs;    /* consecutive epochs below exit_threshold (BPF copy)    */
	u32 hotness_score;  /* reserved for future hotness ranking                   */
	u32 _pad;
};

/* --------------------------------------------------------------------------
 * Per-thread epoch aggregate slot
 * --------------------------------------------------------------------------
 * Stored in epoch_slots[slot_id] (BPF_MAP_TYPE_ARRAY, BPF_F_MMAPABLE).
 *
 * Ownership:
 *   - The lock library owns exactly one slot per live thread.
 *   - The lock library writes only to its own slot.
 *   - The controller reads all slots periodically.
 *
 * Consistency:
 *   Use a seqlock-style version field (seq):
 *     - Before starting a write: seq |= 1  (odd  → write in progress)
 *     - After completing a write: seq += 1 (even → snapshot committed)
 *   The controller accepts a snapshot only when seq is the same even value
 *   before and after reading the slot.
 */
struct epoch_slot {
	u32 tid;            /* thread ID that owns this slot             */
	u32 tgid;           /* process ID that owns this slot            */
	u32 slot_id;        /* index in epoch_slots map                  */
	u32 epoch_id;       /* epoch this snapshot belongs to            */
	u32 lock_domain_id; /* primary lock domain for this thread       */
	u32 _pad;
	u64 wait_ns;        /* total nanoseconds waiting for a lock      */
	u64 hold_ns;        /* total nanoseconds holding a lock          */
	u64 park_ns;        /* total nanoseconds sleeping in futex_wait  */
	u64 contended_acq;  /* number of lock acquisitions with contention */
	u64 park_count;     /* number of futex_wait calls                */
	u64 seq;            /* seqlock version: odd=writing, even=ready  */
	u64 flags;          /* reserved                                  */
};
