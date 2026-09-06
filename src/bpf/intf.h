/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#define NORMAL_DSQ 0x100ULL
#define WAITING_DSQ 0x101ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U

/* Bits 0-1 hold lock state, bit 2 marks revocable CV spinning, and the
 * request epoch occupies bits 3 and above. */
#define USER_HELD 1U
#define USER_WAITING 2U
#define USER_SPINNING 3U
#define USER_FLAGS 3U
#define USER_CV 4U
#define USER_META 7U

/* Mapped read-only by the direct runtime to confirm admission after yielding. */
struct admission_state {
  unsigned int enabled;
  /* Advisory presence of tasks queued for a CPU or an admission slot. */
  unsigned int demand;
  unsigned long long owners[MAX_CPUS];
};

struct task_scx_ctx {
  /* CPU + 1, or zero without an admission slot. */
  unsigned int admission_cpu;
  unsigned long long ticket;
};

#endif
