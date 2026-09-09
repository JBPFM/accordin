/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_RUNTIME_H
#define ACCORDIN_RUNTIME_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdatomic.h>
#include <sched.h>
#include "bpf/intf.h"
#include "rseq_slot.h"

struct thread_state {
    /* The word and the slot open the record because the scheduler reads them
     * together with one eight-byte probe. The pair is aligned to its own width:
     * a four-aligned pair may sit in the last four bytes of a page, and the
     * single read would then straddle into the next one and fail for the life
     * of the thread, leaving it neither queued nor reclaimed. */
    _Alignas(8) _Atomic uint32_t word;
    /* The CPU whose slot this thread holds, plus one; zero without one. */
    uint32_t slot;
    /* The value written in that slot. */
    uint64_t ticket;
    uint32_t depth;
    uint32_t tid;
    bool registered;
    bool auto_active;
    /* This thread's rseq area, resolved at registration, null without one. */
    void *rseq_area;
};

_Static_assert(offsetof(struct thread_state, word) ==
                   offsetof(struct admission_word, state),
               "admission word must open the thread record");
_Static_assert(offsetof(struct thread_state, slot) ==
                   offsetof(struct admission_word, slot),
               "the slot must follow the word as the scheduler reads it");

extern _Thread_local struct thread_state thread_state;
extern struct admission_state *scheduler_admission;
extern bool admission_enabled;
extern bool auto_admission;
/* Whether a contender may take a slot out of the table itself. */
extern bool user_claim;
/* Whether that write goes through a restartable sequence instead of an
 * exchange. Off without an rseq area, or when the switch denies it. */
extern bool user_rseq;
/* Whether the runtime keeps and reports its counters. */
extern bool runtime_diagnostics;
extern uint64_t claim_renews, claim_claims, claim_undone, claim_aborts,
    claim_queued;
void register_thread(void);

/* True while the scheduler can hold a condvar wait instead of a futex sleep. */
bool accordin_cv_custody_ready(void);
/* Hand notified condvar waits back to lock admission. Returns how many moved,
 * zero without a scheduler, or -1 on error. Zero flags ask for the move pass
 * alone. */
int accordin_cv_flush_now(unsigned int flags);

static inline void ensure_registered(void)
{
    if (!thread_state.registered)
        register_thread();
}

/* All nested locks share the outer episode, including out-of-order unlocks. */
static inline bool admission_active(void)
{
    if (!admission_enabled)
        return false;
    if (!auto_admission || thread_state.auto_active)
        return true;
    struct admission_state *state = scheduler_admission;
    if (state && __atomic_load_n(&state->active, __ATOMIC_ACQUIRE)) {
        /* Activation is monotonic for this library's lifetime. Once seen,
         * keep the steady overloaded path off the shared mapping. */
        thread_state.auto_active = true;
        return true;
    }
    return false;
}

static inline bool admission_begin(void)
{
    bool managed = thread_state.depth++ == 0 && admission_enabled;
    if (managed) {
        uint32_t word = atomic_load_explicit(&thread_state.word, memory_order_relaxed);
        atomic_store_explicit(&thread_state.word, (word & ~USER_META) + 8,
                              memory_order_relaxed);
    }
    return managed;
}

/* The lock path carries no shared atomic of its own unless the counters were
 * asked for. */
static inline void claim_count(uint64_t *counter)
{
    if (runtime_diagnostics)
        __atomic_fetch_add(counter, 1, __ATOMIC_RELAXED);
}

/* Publish the state this request is in, keeping the request number it carries. */
static inline void publish_state(uint32_t request, uint32_t state)
{
    atomic_store_explicit(&thread_state.word, request | state,
                          memory_order_relaxed);
}

/* The CPU the restartable path is bound to, or a negative value where that path
 * is unavailable and the exchange carries the writes instead. */
static inline int admission_slot_cpu(void)
{
#ifdef ACCORDIN_RSEQ_SLOT
    if (user_rseq && thread_state.rseq_area)
        return rseq_cpu(thread_state.rseq_area);
#endif
    return -1;
}

/* Whether the entry of cpu already carries this request's ticket. A condvar
 * wake, or a wait a flush filed in the bank and dispatch then served, arrives
 * with the grant already written. */
static inline bool admission_confirm(struct admission_state *state,
                                     unsigned int cpu, uint64_t ticket)
{
    if (cpu >= MAX_CPUS ||
        __atomic_load_n(&state->owners[cpu].ticket, __ATOMIC_RELAXED) != ticket)
        return false;
    /* Record the grant where the scheduler reads it, so its task record and
     * this table entry name the same slot. */
    thread_state.slot = cpu + 1;
    thread_state.ticket = ticket;
    return true;
}

/* Write this request's ticket into the entry of index, which must still carry
 * expect. A commit means the value is in the entry and the thread was still on
 * the CPU that entry belongs to, whichever sequence carried the write.
 *
 * The entry of the CPU the thread runs on needs no atomic instruction, only a
 * sequence the kernel restarts. While the thread runs on that CPU the only
 * writer that can turn the entry from free into a grant is that CPU's own
 * dispatch, and dispatch there requires the thread to be scheduled off it,
 * which restarts the sequence before its store. Every other writer exchanges
 * its own ticket value: another task's release, the exit sweep and the unload
 * scan all name a ticket this thread never wrote, so none of them can touch a
 * free entry or one this thread holds. A tick may retire the thread's own old
 * ticket without rescheduling; the store then lands on an entry that is free,
 * which is the outcome the renewal wanted, and the scheduler adopts the entry
 * again from the slot the thread records.
 *
 * Any other entry is outside that argument, because its CPU keeps dispatching
 * while this thread runs, so it takes the exchange and then reads the CPU for
 * itself. */
static inline int admission_put_slot(struct admission_state *state,
                                     unsigned int index,
                                     unsigned long long expect,
                                     unsigned long long value, int here)
{
    unsigned long long *entry = &state->owners[index].ticket;

#ifdef ACCORDIN_RSEQ_SLOT
    if (here >= 0 && index == (unsigned int)here)
        return rseq_cmpeqv_storev(thread_state.rseq_area, entry, expect, value,
                                  here);
#else
    (void)here;
#endif
    /* Acquire alone: the entry publishes nothing beyond its own value. */
    if (!__atomic_compare_exchange_n(entry, &expect, value, false,
                                     __ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
        return SLOT_MISMATCH;
    if ((unsigned int)sched_getcpu() == index)
        return SLOT_COMMITTED;
    /* Left in place after a migration the ticket would reserve a CPU nobody
     * runs on while the CPU the thread landed on carries two spinners. Only
     * this thread writes this value, so taking it back is safe, and a scheduler
     * that already adopted it fails its later release and drops the record. */
    expect = value;
    __atomic_compare_exchange_n(entry, &expect, 0, false, __ATOMIC_ACQUIRE,
                                __ATOMIC_RELAXED);
    return SLOT_MIGRATED;
}

/* A restart means a preemption, a migration or a signal landed inside the
 * sequence. A few more attempts cover the isolated one; a machine that keeps
 * interrupting is served better by the queue than by another attempt. */
#define ADMISSION_SLOT_ATTEMPTS 3

/* One write of an entry, retried while the sequence is restarted. Only a
 * restartable section can report a restart, so where there is none the write is
 * a single attempt and carries no loop at all. */
static inline int admission_write_slot(struct admission_state *state,
                                       unsigned int index,
                                       unsigned long long expect,
                                       unsigned long long value, int here)
{
#ifdef ACCORDIN_RSEQ_SLOT
    unsigned int attempt;

    /* Every copy of the section needs a descriptor of its own for the kernel to
     * find it by address, so unrolled attempts pay in the library and buy
     * nothing. */
#if defined(__clang__)
#pragma clang loop unroll(disable)
#else
#pragma GCC unroll 1
#endif
    for (attempt = 0; attempt < ADMISSION_SLOT_ATTEMPTS; attempt++) {
        int outcome = admission_put_slot(state, index, expect, value, here);

        if (outcome != SLOT_RESTARTED)
            return outcome;
        claim_count(&claim_aborts);
        here = admission_slot_cpu();
    }
    return SLOT_RESTARTED;
#else
    return admission_put_slot(state, index, expect, value, here);
#endif
}

/* Take the slot this request needs without entering the kernel: consume a grant
 * the table already carries, renew the slot this thread holds, or claim a free
 * one on this CPU. Returns false when the request has to be queued instead.
 * Every write names a value only this thread writes, so it can neither displace
 * a grant the scheduler made nor a slot another thread holds. The caller must
 * have published SPINNING first. */
static inline bool admission_take_slot(struct admission_state *state,
                                       uint64_t ticket)
{
    int here = admission_slot_cpu();
    unsigned int cpu = here < 0 ? (unsigned int)sched_getcpu() : (unsigned int)here;
    int outcome;

    if (admission_confirm(state, cpu, ticket))
        return true;
    /* Taking a slot outside the queue order is only fair while nothing is
     * queued: demand counts the ordinary queue together with the whole bank,
     * and an undercount lasts no longer than one correction period. */
    if (!user_claim || __atomic_load_n(&state->demand, __ATOMIC_RELAXED) > 0)
        return false;
    if (thread_state.slot) {
        outcome = admission_write_slot(state, thread_state.slot - 1,
                                       thread_state.ticket, ticket, here);
        if (outcome == SLOT_COMMITTED) {
            thread_state.ticket = ticket;
            claim_count(&claim_renews);
            return true;
        }
        if (outcome == SLOT_MIGRATED)
            claim_count(&claim_undone);
        /* The entry was reclaimed, taken back, or already belongs to someone
         * else; a free entry on this CPU is what is left to try. */
        thread_state.slot = 0;
    }
    if (cpu >= MAX_CPUS ||
        __atomic_load_n(&state->owners[cpu].ticket, __ATOMIC_RELAXED))
        return false;
    /* The scheduler adopts a slot through this field, so name the entry before
     * the entry names this thread. */
    thread_state.slot = cpu + 1;
    outcome = admission_write_slot(state, cpu, 0, ticket, here);
    if (outcome == SLOT_COMMITTED) {
        thread_state.ticket = ticket;
        claim_count(&claim_claims);
        return true;
    }
    thread_state.slot = 0;
    if (outcome == SLOT_MIGRATED)
        claim_count(&claim_undone);
    return false;
}

static inline void admission_wait(bool prequeued)
{
    /* Only the contended path consults the policy. An off-mode contender
     * publishes SPINNING before entering the raw queue, so activation cannot
     * mistake an existing queue predecessor for a new admission request. */
    if (!admission_active()) {
        uint32_t word = atomic_load_explicit(&thread_state.word, memory_order_relaxed);
        publish_state(word & ~USER_META, USER_SPINNING);
        return;
    }
    uint32_t request = atomic_load_explicit(&thread_state.word,
                                            memory_order_relaxed) & ~USER_META;
    uint64_t ticket = ((uint64_t)request << 32) | thread_state.tid;
    struct admission_state *state = scheduler_admission;

    /* SPINNING comes before any look at the table. WAITING would let a
     * preemption file this thread in the bank, where a second grant could race
     * the claim; an idle word would let a tick retire the entry the thread has
     * just taken. A spinner without a slot is the state the scheduler leaves
     * alone. */
    publish_state(request, USER_SPINNING);
    /* With no mapping, or none the scheduler still reads, there is no slot to
     * take; the loop below already ends on a scheduler that is gone. */
    if (state && __atomic_load_n(&state->enabled, __ATOMIC_ACQUIRE)) {
        if (admission_take_slot(state, ticket))
            return;
        claim_count(&claim_queued);
    }
    publish_state(request, USER_WAITING);

    /* Normal contention submits through yield. A condvar wake may already
     * carry a grant: consume it before yielding, retaining the same epoch.
     * A yield need not dispatch; confirm this request, never an older grant. */
    for (;;) {
        if (!prequeued)
            sched_yield();
        prequeued = false;
        if (!state || !__atomic_load_n(&state->enabled, __ATOMIC_ACQUIRE))
            break;
        if (admission_confirm(state, (unsigned int)sched_getcpu(), ticket))
            break;
    }
    publish_state(request, USER_SPINNING);
}

static inline void admission_enter(bool managed)
{
    if (managed) {
        uint32_t word = atomic_load_explicit(&thread_state.word, memory_order_relaxed);
        atomic_store_explicit(&thread_state.word, (word & ~USER_META) | USER_HELD,
                              memory_order_relaxed);
    }
}

static inline void admission_finish(void)
{
    if (--thread_state.depth == 0 && admission_enabled)
        atomic_fetch_and_explicit(&thread_state.word, ~USER_META, memory_order_relaxed);
}

#endif
