/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#ifndef __kptr
#define __kptr
#endif

#define READY_DSQ_ID 0x100ULL
#define SSC_DSQ_ID 0x5CCULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U
#define MAX_NODES 8U
enum thread_role {
  ROLE_NONE = 0,
  ROLE_OWNER = 1,
};

/* Stored in user thread-local memory and read from BPF via bpf_probe_read_user.
 */
struct lock_sched_thread_ctx {
  unsigned long long wait_ns_total; /* cumulative wait time */
  unsigned long long wait_start_ns; /* current wait start timestamp */
  unsigned long long wait_end_ns;   /* latest completed wait end timestamp */
};

/* Per-task scheduling context stored in BPF task_ctx_map. */
struct task_scx_ctx {
  unsigned long long window_epoch;
  unsigned long long last_wait_ns;
  unsigned long long run_start_ns;
  unsigned long long run_ns_window;
  unsigned long long wait_ns_window;
  unsigned int admitted;
  unsigned long long
      user_ctx_ptr; /* cached pointer to user-space lock_sched_thread_ctx */
};

struct ssc_vote_slot {
  unsigned long long epoch;
  unsigned long long last_run_ns;
  unsigned long long last_wait_ns;
};

/* Stats exported via stats_map for userspace monitoring. */
enum stat_key {
  STAT_P_W_EWMA = 0,
  STAT_TARGET_LOCAL = 1,
  STAT_TARGET_REMOTE = 2,
  STAT_ACTIVE_LOCAL = 3,
  STAT_ACTIVE_REMOTE = 4,
  STAT_SSC_WAITERS = 5,
  STAT_CONSEC_HIGH = 6,
  STAT_CONSEC_LOW = 7,
  STAT_FORCED_RELEASE = 8,
  STAT_DBG_WIN_RUN = 9,   /* last window total_run (us) */
  STAT_DBG_WIN_WAIT = 10, /* last window total_wait (us) */
  STAT_DBG_WIN_PW = 11,   /* last window p_w */
  STAT_DBG_ACCT_CALLS = 12,
  STAT_DBG_ACCT_UPTR = 13,
  STAT_DBG_ACCT_READOK = 14,
  STAT_DBG_ACCT_WAITNZ = 15,
  STAT_NR = 16,
};

#endif
