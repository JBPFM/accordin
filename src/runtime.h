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
    bool auto_active;
};

extern _Thread_local struct thread_state thread_state;
extern struct admission_state *scheduler_admission;
extern bool admission_enabled;
extern bool auto_admission;
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

#endif
