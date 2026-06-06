/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#ifndef __kptr
#define __kptr
#endif

#define READY_DSQ_ID 0x100ULL
#define SSC_DSQ_ID 0x5CCULL
#define INACTIVE_DSQ_ID 0x10000ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U
#define ADMISSION_CPU_NONE MAX_CPUS

/* Stored in user thread-local memory and read from BPF via bpf_probe_read_user.
 */
struct lock_sched_thread_ctx {
  unsigned long long thread_start_ns;        /* thread stats start timestamp */
  unsigned long long thread_elapsed_ns_total; /* latest cumulative thread time */
  unsigned long long wait_ns_total; /* cumulative wait time */
  unsigned long long wait_start_ns; /* current wait start timestamp */
  unsigned long long hold_ns_total; /* cumulative hold time */
  unsigned long long hold_start_ns; /* current hold start timestamp */
  unsigned long long lock_count;    /* completed lock hold intervals */
  unsigned int admission_owned;     /* userspace cached admission bit */
  unsigned int admission_cpu;       /* CPU cached when admission was granted */
  unsigned int admission_requeue_home; /* sticky-home hint for reenqueue */
  unsigned int in_critical_section; /* true while inside lock critical section */
  unsigned int slow_path_pending;   /* true while waiting for slow-path admission */
};

/* Kept only so existing task-storage userspace bindings still generate cleanly. */
struct task_scx_ctx {
  unsigned long long last_wait_ns;
  unsigned long long pending_wait_ns;
  unsigned long long run_start_ns;
  unsigned long long
      user_ctx_ptr; /* cached pointer to user-space lock_sched_thread_ctx */
  unsigned int admitted;
  unsigned int initialized;
  unsigned int holds_admission;
  unsigned int admission_cpu;
  unsigned int must_run_on_admission_cpu;
  unsigned int inactive_wait;
  unsigned int slow_path_pending;
  unsigned int in_critical_section;
};

enum stat_key {
  STAT_SSC_WAITERS = 0,
  STAT_DBG_ACCT_CALLS = 1,
  STAT_DBG_ACCT_READOK = 2,
  STAT_NR = 3,
};

#endif
