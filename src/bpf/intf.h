/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#ifndef __kptr
#define __kptr
#endif

#define SSC_DSQ_ID 0x5CCULL
#define INACTIVE_DSQ_BASE 0x10000ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U
#define ADMISSION_CPU_NONE MAX_CPUS
#define USER_ADMISSION_IN_CRITICAL_SECTION (1U << 0)
#define USER_ADMISSION_SLOW_PATH_PENDING (1U << 1)
#define USER_ADMISSION_FLAG_MASK 0x3U
#define USER_ADMISSION_LOCK_ID_SHIFT 2U
#define MAX_LOCK_CLASSES 16U
#define UNMANAGED_LOCK_ID 0U
#define INACTIVE_PREVIOUS_LOCK_PERCENT_DEFAULT 80U

struct task_scx_ctx {
  unsigned int initialized;
  unsigned int holds_admission;
  unsigned int admission_cpu;
  unsigned int must_run_on_admission_cpu;
  unsigned long long
      user_ctx_ptr; /* cached pointer to user-space admission word */
};

enum stat_key {
  STAT_SSC_WAITERS = 0,
  STAT_DBG_ACCT_CALLS = 1,
  STAT_DBG_ACCT_READOK = 2,
  STAT_NR = 3,
};

#endif
