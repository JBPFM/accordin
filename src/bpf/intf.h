/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#define NORMAL_DSQ 0x100ULL
#define WAITING_DSQ 0x101ULL
#define WAITFORSIGNAL_DSQ 0x102ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U

/* Admission word: bits 0-1 hold the state value (0 idle, USER_HELD,
 * USER_WAITING, USER_SPINNING), bit 2 marks a condvar wait, and bits 3 and
 * above count requests, advancing by 8. USER_FLAGS masks the value alone,
 * USER_META the value together with the condvar marker. */
#define USER_HELD 1U
#define USER_WAITING 2U
#define USER_SPINNING 3U
#define USER_FLAGS 3U
#define USER_CV 4U
#define USER_META 7U

/* Mapped read-only by the direct runtime to confirm admission after yielding. */
struct admission_state {
  unsigned int enabled;
  unsigned long long owners[MAX_CPUS];
};

struct task_scx_ctx {
  /* CPU + 1, or zero without an admission slot. */
  unsigned int admission_cpu;
  unsigned long long ticket;
  /* Time the condvar wait entered scheduler custody, zero outside custody. */
  unsigned long long parked_at;
  /* Request whose custody was withdrawn; it may not be granted again. */
  unsigned long long custody_denied;
};

/* Batched transfer of notified condvar waiters out of scheduler custody.
 * The caller fills the request fields and reads back the result fields. */
#define CV_FLUSH_EXPIRE 1U
#define CV_FLUSH_REV 2U
#define CV_FLUSH_TAIL 4U
#define CV_FLUSH_MOVE 8U

struct cv_flush_ctx {
  unsigned int width;
  unsigned int flags;
  unsigned long long now;
  unsigned int moved;
  unsigned int expired;
  unsigned int pending;
  unsigned int queued;
};

#endif
