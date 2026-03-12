/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#ifndef __kptr
#define __kptr
#endif

#define READY_DSQ_ID    0x100ULL
#define SSC_DSQ_ID      0x5CCULL
#define MAX_TASKS       65536U
#define MAX_CPUS        256U
#define MAX_NODES       8U

enum thread_role {
    ROLE_NONE  = 0,
    ROLE_OWNER = 1,
};

/* Stored in user thread-local memory and read from BPF via bpf_probe_read_user. */
struct lock_sched_thread_ctx {
    unsigned long long wait_ns_total;  /* monotonic cumulative */
    unsigned int       state;          /* enum thread_role */
    unsigned int       seq;            /* seqcount (odd=writing) */
};

/* Per-task scheduling context stored in BPF task_ctx_map. */
struct task_scx_ctx {
    unsigned long long last_wait_ns;
    unsigned long long run_start_ns;
    unsigned long long run_ns_window;
    unsigned long long wait_ns_window;
    unsigned int       role;
    unsigned int       admitted;
    unsigned int       counted;     /* 1 if task is counted in active_local/remote */
    unsigned int       counted_local; /* 1 if counted in active_local */
    int                last_node;
    unsigned long long ssc_enter_ts;
};

/* Stats exported via stats_map for userspace monitoring. */
enum stat_key {
    STAT_P_W_EWMA       = 0,
    STAT_TARGET_LOCAL    = 1,
    STAT_TARGET_REMOTE   = 2,
    STAT_ACTIVE_LOCAL    = 3,
    STAT_ACTIVE_REMOTE   = 4,
    STAT_SSC_WAITERS     = 5,
    STAT_CONSEC_HIGH     = 6,
    STAT_CONSEC_LOW      = 7,
    STAT_FORCED_RELEASE  = 8,
    STAT_NR              = 9,
};

#endif
