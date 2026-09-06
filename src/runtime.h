/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_RUNTIME_H
#define ACCORDIN_RUNTIME_H

#include <stdbool.h>
#include <stdint.h>
#include <stdatomic.h>
#include <sched.h>
#include "bpf/intf.h"

struct thread_state {
    _Atomic uint32_t word;
    uint32_t depth;
    uint32_t tid;
    bool registered;
};

extern _Thread_local struct thread_state thread_state;
extern struct admission_state *scheduler_admission;
extern bool admission_enabled;
extern uint64_t cv_spin_ns;
void register_thread(void);

static inline void ensure_registered(void)
{
    if (!thread_state.registered)
        register_thread();
}

/* All nested locks share the outer episode, including out-of-order unlocks. */
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

static inline void admission_wait(bool prequeued)
{
    uint32_t request = atomic_fetch_or_explicit(&thread_state.word, USER_WAITING,
                                               memory_order_relaxed) & ~USER_META;
    uint64_t ticket = ((uint64_t)request << 32) | thread_state.tid;
    struct admission_state *state = scheduler_admission;

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
            __atomic_load_n(&state->owners[cpu], __ATOMIC_RELAXED) == ticket)
            break;
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

static inline unsigned int admission_demand(void)
{
    struct admission_state *state = scheduler_admission;
    return state ? __atomic_load_n(&state->demand, __ATOMIC_RELAXED) : 0;
}

static inline bool admission_slot_held(uint64_t ticket)
{
    struct admission_state *state = scheduler_admission;
    if (!state || !__atomic_load_n(&state->enabled, __ATOMIC_ACQUIRE))
        return false;
    unsigned int cpu = sched_getcpu();
    return cpu < MAX_CPUS &&
           __atomic_load_n(&state->owners[cpu], __ATOMIC_RELAXED) == ticket;
}

#endif
