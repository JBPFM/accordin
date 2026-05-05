/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#ifndef __kptr
#define __kptr
#endif

#define READY_DSQ_ID 0x100ULL
#define SSC_DSQ_ID 0x5CCULL
#define INACTIVE_DSQ_BASE 0x10000ULL
#define DISTRIBUTED_INACTIVE_DSQ_BASE 0x20000ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U
#define MAX_LOCK_DOMAINS 8U
#define MAX_INACTIVE_SHARDS 8U
#define DISTRIBUTED_STEAL_SCAN 4U
#define DOMAIN_CPU_SLOTS (MAX_LOCK_DOMAINS * MAX_CPUS)
#define ADMISSION_CPU_NONE MAX_CPUS
#define ADMISSION_OWNER_RESERVED 0xffffffffU
#define USER_ADMISSION_IN_CRITICAL_SECTION (1U << 0)
#define USER_ADMISSION_SLOW_PATH_PENDING (1U << 1)

struct user_admission_ctx {
  unsigned int flags;
  unsigned int lock_domain;
  unsigned long long lock_id;
  unsigned int tracked_lock_depth;
};

struct task_scx_ctx {
  unsigned int admitted;
  unsigned int initialized;
  unsigned int holds_admission;
  unsigned int admission_cpu;
  unsigned int must_run_on_admission_cpu;
  unsigned int inactive_wait;
  unsigned int lock_domain;
  unsigned long long
      user_ctx_ptr; /* cached pointer to user-space admission context */
};

struct lock_domain_state {
  unsigned int active_count;
  unsigned int rr_cursor;
};

enum stat_key {
  STAT_SSC_WAITERS = 0,
  STAT_DBG_ACCT_CALLS = 1,
  STAT_DBG_ACCT_READOK = 2,
  STAT_DISTRIBUTED_ENQUEUE = 3,
  STAT_DISTRIBUTED_LOCAL_MOVE = 4,
  STAT_DISTRIBUTED_STEAL_MOVE = 5,
  STAT_DISTRIBUTED_FALLBACK = 6,
  STAT_DISTRIBUTED_RESERVE_FAIL = 7,
  STAT_DISTRIBUTED_RESCUE_MOVE = 8,
  STAT_NR = 9,
};

#endif
