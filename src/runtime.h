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
        atomic_store_explicit(&thread_state.word, (word & ~USER_FLAGS) + 4,
                              memory_order_relaxed);
    }
    return managed;
}

static inline void admission_wait(void)
{
    uint32_t request = atomic_fetch_or_explicit(&thread_state.word, USER_WAITING,
                                               memory_order_relaxed) & ~USER_FLAGS;
    uint64_t ticket = ((uint64_t)request << 32) | thread_state.tid;
    struct admission_state *state = scheduler_admission;

    /* A yield need not dispatch. Confirm this request, never an older grant. */
    for (;;) {
        sched_yield();
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
        atomic_store_explicit(&thread_state.word, (word & ~USER_FLAGS) | USER_HELD,
                              memory_order_relaxed);
    }
}

static inline void admission_finish(void)
{
    if (--thread_state.depth == 0 && admission_enabled)
        atomic_fetch_and_explicit(&thread_state.word, ~USER_FLAGS, memory_order_relaxed);
}

#endif
