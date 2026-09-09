/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INTF_H
#define __INTF_H

#define NORMAL_DSQ 0x100ULL
#define WAITFORSIGNAL_DSQ 0x102ULL
#define MAX_TASKS 65536U
#define MAX_CPUS 256U

/* CPUs are collected into topology groups, each group a slice of one NUMA node
 * small enough to stay cache-friendly. A group never spans more CPUs than
 * MAX_GROUP_SIZE, and the worst case of one CPU per group needs as many groups
 * as there are CPUs. */
#define MAX_GROUPS MAX_CPUS
#define MAX_GROUP_SIZE 16U
#define CPU_NO_GROUP 0xffffffffU
/* Lock admission is served from a bank of queues, one per CPU, so that dispatch
 * on different CPUs rarely contends on the same queue lock. A waiter is filed
 * in the queue of the CPU it woke on. The bank spans every CPU id the kernel
 * may report; the scheduler creates the queues the machine actually has. */
#define WAITING_DSQ 0x200ULL

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

/* The pair the scheduler reads out of a registered thread with a single probe:
 * the admission word and the slot the thread was granted, a CPU id plus one,
 * zero without one. The halves are written separately and never merged into one
 * 64-bit atomic, because a notifier on another thread publishes the word alone.
 * The pair is eight-byte aligned so the single read never straddles a page. */
struct admission_word {
  unsigned int state;
  unsigned int slot;
} __attribute__((aligned(8)));

/* Mapped writable into the direct runtime, which confirms admission after
 * yielding and writes owners[] by compare-and-swap on its own ticket value. */
struct admission_state {
  unsigned int enabled;
  /* Auto admission latches this on at the first overload event. */
  unsigned int active;
  /* Tasks queued in the ordinary queue and in the whole admission bank.
   * Advisory: maintained without saturation, signed so a transient undercount
   * reads as empty rather than as an enormous queue, and corrected against the
   * queue depths by the periodic scan. */
  int demand;
  unsigned long long owners[MAX_CPUS];
};

struct task_scx_ctx {
  unsigned int auto_runnable;
  /* CPU + 1, or zero without an admission slot. */
  unsigned int admission_cpu;
  unsigned long long ticket;
  /* Time the condvar wait entered scheduler custody, zero outside custody. */
  unsigned long long parked_at;
  /* Request whose custody was withdrawn; it may not be granted again. */
  unsigned long long custody_denied;
};

/* Batched transfer of notified condvar waiters out of scheduler custody.
 * The caller fills the request fields and reads back the result fields.
 * MOVE hands notified waits to the admission queue and EXPIRE withdraws custody
 * from waits past their limit. The admission queue is ordered by the age stamp
 * a wait carries, so a released wait takes the place its park time gives it and
 * the walk order shows only under a width cap; REV then walks the custody queue
 * from its tail instead of from its head. SPREAD also wakes idle CPUs holding a
 * free admission slot for the moved waits, not only for the waits handed back
 * to the ordinary queue. Bit 4 carries no meaning. */
#define CV_FLUSH_EXPIRE 1U
#define CV_FLUSH_REV 2U
#define CV_FLUSH_MOVE 8U
#define CV_FLUSH_SPREAD 16U

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
