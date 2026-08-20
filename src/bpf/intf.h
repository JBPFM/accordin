/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#ifndef __kptr
#define __kptr
#endif

#define NORMAL_DSQ_ID 0x100ULL
#define SSC_DSQ_ID 0x5CCULL
#define INACTIVE_DSQ_BASE 0x10000ULL
/* Per-class queues for threads parked on the condition-variable path. They are
 * kept apart from the inactive queues so a re-acquiring cond waiter, which the
 * lock's next holder is already waiting on, can be served ahead of the waiters
 * that only queued for the lock. */
#define CVREADY_DSQ_BASE 0x20000ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U
#define ADMISSION_CPU_NONE MAX_CPUS
#define USER_ADMISSION_IN_CRITICAL_SECTION (1U << 0)
#define USER_ADMISSION_SLOW_PATH_PENDING (1U << 1)
#define USER_ADMISSION_TOKEN_CONSUMED (1U << 2)
/* Set by a thread immediately before it releases a managed lock to sleep on a
 * condition variable: the lock-class field then names the lock the thread will
 * re-acquire on wake. USER_ADMISSION_SLOW_PATH_PENDING and
 * USER_ADMISSION_IN_CRITICAL_SECTION are never set while this bit is.
 *
 * Right after its futex wait returns the waiter retires the bit into
 * USER_ADMISSION_SLOW_PATH_PENDING for the same class rather than clearing it:
 * the re-acquisition that follows is an ordinary contended acquisition, and a
 * word carrying no flag at all would read as an explicit release and strip the
 * admission the wake was granted. The pending bit is cleared where every other
 * contender clears it, at USER_ADMISSION_IN_CRITICAL_SECTION. */
#define USER_ADMISSION_CV_SLEEP (1U << 3)
#define USER_ADMISSION_FLAG_MASK 0xFU
#define USER_ADMISSION_LOCK_ID_SHIFT 4U
#define MAX_LOCK_CLASSES 64U
#define UNMANAGED_LOCK_ID 0U
#define INACTIVE_PREVIOUS_LOCK_PERCENT_DEFAULT 0U
#define INACTIVE_DISPATCH_BURST 256U
#define CV_PRIORITY_STREAK_LIMIT_DEFAULT 2U
/* Upper bound the loader clamps the streak limit to: the streak is what hands
 * the CPU back to the class queues, so a limit far above the number of threads
 * a single signal can release lets a cond-variable storm keep a CPU on the
 * cvready queues for as long as the storm lasts. */
#define CV_PRIORITY_STREAK_LIMIT_MAX 16U

#define ENQ_PATH_UNKNOWN 0U
#define ENQ_PATH_NORMAL_DSQ 1U
#define ENQ_PATH_NORMAL_LOCAL_FAST 2U
#define ENQ_PATH_ADMISSION_LOCAL 3U
#define ENQ_PATH_SLOW_GRANTED_LOCAL 4U
#define ENQ_PATH_SLOW_INACTIVE 5U
#define ENQ_PATH_FORCE_INACTIVE 6U
#define ENQ_PATH_SELECT_LOCAL_DIRECT 7U
#define ENQ_PATH_CV_GRANTED_LOCAL 8U
#define ENQ_PATH_CV_PARKED 9U

struct task_scx_ctx {
  unsigned int initialized;
  unsigned int holds_admission;
  unsigned int admitted_class;
  /* Set while this admission episode is counted against the width of
   * admitted_class. It is the token that keeps the class counter balanced: the
   * grant that sets it is the only increment, the clear that follows is the
   * only decrement, and a holder never carries it. */
  unsigned int width_slot_held;
  /* Set by the grant a cond wake takes in the enqueue callback and cleared as
   * soon as the word stops naming a cond sleep. The woken thread only retires
   * USER_ADMISSION_CV_SLEEP once it runs again, so between the grant and that
   * update every hook that reads the word still sees the sleep state; without
   * this flag the first of them would hand the fresh grant straight back. */
  unsigned int cv_admitted;
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
