/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#ifndef __kptr
#define __kptr
#endif

#define CONTROLLED_DSQ_ID 0x100ULL
#define SSC_DSQ_ID 0x5CCULL
#define INACTIVE_DSQ_BASE 0x10000ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U
#define ADMISSION_CPU_NONE MAX_CPUS
#define USER_ADMISSION_IN_CRITICAL_SECTION (1U << 0)
#define USER_ADMISSION_SLOW_PATH_PENDING (1U << 1)
#define USER_ADMISSION_SLOW_PATH_SEEN (1U << 2)
#define USER_ADMISSION_FLAG_MASK 0x7U
#define USER_ADMISSION_LOCK_ID_SHIFT 3U
#define MAX_LOCK_CLASSES 8U
#define UNMANAGED_LOCK_ID 0U
#define ADMISSION_TOKEN_MAX_USES 1U

struct accordin_active_cpus_args {
  unsigned long long wanted0;
  unsigned long long wanted1;
  unsigned long long wanted2;
  unsigned long long wanted3;
  unsigned int nr_cpus;
};

struct accordin_cpu_nudge_args {
  unsigned int cpu;
  unsigned int drain_inactive;
};

struct task_scx_ctx {
  unsigned int slow_path_seen;
  unsigned int initialized;
  unsigned int holds_admission;
  unsigned int admission_cpu;
  unsigned int must_run_on_admission_cpu;
  unsigned int inactive_wait;
  unsigned int admission_token_uses;
  unsigned int admission_token_cooldown;
  unsigned long long
      user_ctx_ptr; /* cached pointer to user-space admission word */
  unsigned int admission_lock_id; /* 0 = no admission held */
};

struct cpu_inactive_hint {
  unsigned int total;
};

struct cpu_admission_debug {
  unsigned long long inactive_enqueue;
  unsigned long long inactive_local_dequeue;
  unsigned long long inactive_steal_dequeue;
  unsigned long long inactive_controlled_dequeue;
  unsigned long long direct_grant;
  unsigned long long token_limit_reject;
  unsigned long long owner_busy_reject;
  unsigned int current_inactive_total;
  unsigned int max_inactive_total;
};

enum stat_key {
  STAT_SSC_WAITERS = 0,
  STAT_DBG_ACCT_CALLS = 1,
  STAT_DBG_ACCT_READOK = 2,
  STAT_NR = 3,
};

#endif
