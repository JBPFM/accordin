/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_RUNTIME_H
#define ACCORDIN_RUNTIME_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdatomic.h>
#include <sched.h>
#include "bpf/intf.h"

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
};

_Static_assert(offsetof(struct thread_state, word) ==
                   offsetof(struct admission_word, state),
               "admission word must open the thread record");
_Static_assert(offsetof(struct thread_state, slot) ==
                   offsetof(struct admission_word, slot),
               "the slot must follow the word as the scheduler reads it");
_Static_assert(_Alignof(struct thread_state) == 8,
               "the word and the slot must share an eight-byte read");
_Static_assert(offsetof(struct admission_state, owners) % 8 == 0,
               "owner records must be eight-byte aligned");

extern _Thread_local struct thread_state thread_state;
extern struct admission_state *scheduler_admission;
extern bool admission_enabled;
extern bool auto_admission;
/* Whether a contender may take a slot out of the table itself. */
extern bool user_claim;
extern bool cv_counters_on;
extern uint64_t claim_renews, claim_claims, claim_undone, claim_queued;
void register_thread(void);

/* True while the scheduler can hold a condvar wait instead of a futex sleep. */
bool accordin_cv_custody_ready(void);
/* Hand notified condvar waits back to lock admission. Returns how many moved,
 * zero without a scheduler, or -1 on error. Zero width or flags select the
 * values configured through the environment. */
int accordin_cv_flush_now(unsigned int width, unsigned int flags);

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
    if (cv_counters_on)
        __atomic_fetch_add(counter, 1, __ATOMIC_RELAXED);
}

/* Take the slot this request needs without entering the kernel: consume a grant
 * the table already carries, renew the slot this thread holds, or claim a free
 * one on this CPU. Returns false when the request has to be queued instead.
 * Every write is a compare-and-swap against a value only this thread writes, so
 * it can neither displace a grant the scheduler made nor a slot another thread
 * holds. The caller must have published SPINNING first. */
static inline bool admission_take_slot(struct admission_state *state,
                                       uint64_t ticket)
{
    unsigned int cpu = sched_getcpu();
    unsigned long long expected;

    /* A condvar wake, or a wait a flush filed in the bank and dispatch then
     * served, arrives with the grant already written. */
    if (cpu < MAX_CPUS &&
        __atomic_load_n(&state->owners[cpu], __ATOMIC_RELAXED) == ticket) {
        /* Record the grant where the scheduler reads it, so its task record
         * and this table entry name the same slot. */
        thread_state.slot = cpu + 1;
        thread_state.ticket = ticket;
        return true;
    }
    /* Taking a slot outside the queue order is only fair while nothing is
     * queued: demand counts the ordinary queue together with the whole bank,
     * and an undercount lasts no longer than one correction period. */
    if (!user_claim || __atomic_load_n(&state->demand, __ATOMIC_RELAXED) > 0)
        return false;
    if (thread_state.slot) {
        expected = thread_state.ticket;
        if (__atomic_compare_exchange_n(&state->owners[thread_state.slot - 1],
                                        &expected, ticket, false,
                                        __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) {
            thread_state.ticket = ticket;
            claim_count(&claim_renews);
            return true;
        }
        /* The entry was reclaimed, or it already belongs to someone else. */
        thread_state.slot = 0;
    }
    cpu = sched_getcpu();
    if (cpu >= MAX_CPUS ||
        __atomic_load_n(&state->owners[cpu], __ATOMIC_RELAXED))
        return false;
    /* The scheduler adopts a slot through this field, so name the entry before
     * the entry names this thread. */
    thread_state.slot = cpu + 1;
    expected = 0;
    if (__atomic_compare_exchange_n(&state->owners[cpu], &expected, ticket,
                                    false, __ATOMIC_ACQ_REL, __ATOMIC_RELAXED)) {
        if ((unsigned int)sched_getcpu() == cpu) {
            thread_state.ticket = ticket;
            claim_count(&claim_claims);
            return true;
        }
        /* Migrated between the read and the claim: left in place the slot would
         * reserve a CPU nobody runs on while the new CPU carries two spinners.
         * Only this thread writes this value, so withdrawing it is safe, and a
         * scheduler that already adopted it fails its later release and drops
         * the record. */
        expected = ticket;
        __atomic_compare_exchange_n(&state->owners[cpu], &expected, 0, false,
                                    __ATOMIC_ACQ_REL, __ATOMIC_RELAXED);
        claim_count(&claim_undone);
    }
    thread_state.slot = 0;
    return false;
}

static inline void admission_wait(bool prequeued)
{
    /* Only the contended path consults the policy. An off-mode contender
     * publishes SPINNING before entering the raw queue, so activation cannot
     * mistake an existing queue predecessor for a new admission request. */
    if (!admission_active()) {
        uint32_t word = atomic_load_explicit(&thread_state.word, memory_order_relaxed);
        atomic_store_explicit(&thread_state.word, (word & ~USER_META) | USER_SPINNING,
                              memory_order_relaxed);
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
    atomic_store_explicit(&thread_state.word, request | USER_SPINNING,
                          memory_order_relaxed);
    /* With no mapping, or none the scheduler still reads, there is no slot to
     * take; the loop below already ends on a scheduler that is gone. */
    if (state && __atomic_load_n(&state->enabled, __ATOMIC_ACQUIRE)) {
        if (admission_take_slot(state, ticket))
            return;
        claim_count(&claim_queued);
    }
    atomic_store_explicit(&thread_state.word, request | USER_WAITING,
                          memory_order_relaxed);

    /* Normal contention submits through yield. A condvar wake may already
     * carry a grant: consume it before yielding, retaining the same epoch.
     * A yield need not dispatch; confirm this request, never an older grant. */
    for (;;) {
        if (!prequeued)
            sched_yield();
        prequeued = false;
        if (!state || !__atomic_load_n(&state->enabled, __ATOMIC_ACQUIRE))
            break;
        unsigned int cpu = sched_getcpu();
        if (cpu < MAX_CPUS &&
            __atomic_load_n(&state->owners[cpu], __ATOMIC_RELAXED) == ticket) {
            /* Record the grant where the scheduler reads it, so its task
             * record and this table entry name the same slot. */
            thread_state.slot = cpu + 1;
            thread_state.ticket = ticket;
            break;
        }
    }
    atomic_store_explicit(&thread_state.word, request | USER_SPINNING,
                          memory_order_relaxed);
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
