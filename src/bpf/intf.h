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

/* Stored in user thread-local memory and read from BPF via bpf_probe_read_user.
 */
struct lock_sched_thread_ctx {
  unsigned long long wait_ns_total; /* cumulative wait time */
  unsigned long long wait_start_ns; /* current wait start timestamp */
  unsigned long long wait_end_ns;   /* latest completed wait end timestamp */
};

/* Kept only so existing task-storage userspace bindings still generate cleanly. */
struct task_scx_ctx {
  unsigned long long last_wait_ns;
  unsigned long long pending_wait_ns;
  unsigned long long run_start_ns;
  unsigned int admitted;
  unsigned long long
      user_ctx_ptr; /* cached pointer to user-space lock_sched_thread_ctx */
};

enum stat_key {
  STAT_SSC_WAITERS = 0,
  STAT_DBG_ACCT_CALLS = 1,
  STAT_DBG_ACCT_READOK = 2,
  STAT_NR = 3,
};

#endif
