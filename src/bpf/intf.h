/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#ifndef __kptr
#define __kptr
#endif

#define NORMAL_DSQ_ID 0x100ULL
#define SSC_DSQ_ID 0x5CCULL
#define INACTIVE_DSQ_BASE 0x10000ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U
#define ADMISSION_CPU_NONE MAX_CPUS
#define USER_ADMISSION_IN_CRITICAL_SECTION (1U << 0)
#define USER_ADMISSION_SLOW_PATH_PENDING (1U << 1)
#define USER_ADMISSION_TOKEN_CONSUMED (1U << 2)
#define USER_ADMISSION_FLAG_MASK 0x7U
#define USER_ADMISSION_LOCK_ID_SHIFT 3U
#define MAX_LOCK_CLASSES 64U
#define UNMANAGED_LOCK_ID 0U
#define INACTIVE_PREVIOUS_LOCK_PERCENT_DEFAULT 0U
#define INACTIVE_DISPATCH_BURST 256U

#define ENQ_PATH_UNKNOWN 0U
#define ENQ_PATH_NORMAL_DSQ 1U
#define ENQ_PATH_NORMAL_LOCAL_FAST 2U
#define ENQ_PATH_ADMISSION_LOCAL 3U
#define ENQ_PATH_SLOW_GRANTED_LOCAL 4U
#define ENQ_PATH_SLOW_INACTIVE 5U
#define ENQ_PATH_FORCE_INACTIVE 6U
#define ENQ_PATH_SELECT_LOCAL_DIRECT 7U

struct task_scx_ctx {
  unsigned int initialized;
  unsigned int holds_admission;
  unsigned int admitted_class;
  /* Set while this admission episode is counted against the width of
   * admitted_class. It is the token that keeps the class counter balanced: the
   * grant that sets it is the only increment, the clear that follows is the
   * only decrement, and a holder never carries it. */
  unsigned int width_slot_held;
  unsigned int admission_cpu;
  unsigned int must_run_on_admission_cpu;
  unsigned int force_inactive_wait;
  unsigned int last_enqueue_path;
  unsigned int last_enqueue_lock_id;
  unsigned int last_enqueue_cpu;
  unsigned int last_user_ctx_word;
  unsigned long long last_enqueue_dsq;
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
