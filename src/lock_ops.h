/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_LOCK_OPS_H
#define ACCORDIN_LOCK_OPS_H

#include <time.h>
#include "runtime.h"

#ifdef MCS_TAS
#include "mcs_tas.h"
#else
#include "mcs.h"
#endif

/* Filled by lock_ops_lock when a caller asks how an acquisition went. Passing
 * NULL leaves no timing in the generated code. */
struct lock_trace {
    bool fast;
    unsigned int yields;
    uint64_t admission_ns;
    uint64_t spin_ns;
};

static inline uint64_t lock_ops_now_ns(void)
{
    struct timespec now;

    clock_gettime(CLOCK_MONOTONIC, &now);
    return (uint64_t)now.tv_sec * 1000000000ULL + (uint64_t)now.tv_nsec;
}

/* The fast path skips admission entirely; only a contended outer acquisition
 * asks the scheduler for a slot before joining the raw lock queue. One
 * timestamp ends the admission phase and starts the spin phase. */
static inline __attribute__((always_inline)) void
lock_ops_lock(struct raw_lock *lock, struct lock_trace *trace)
{
    ensure_registered();
    bool managed = admission_begin();
    if (trace)
        *trace = (struct lock_trace){0};
    if (raw_trylock(lock)) {
        if (trace)
            trace->fast = true;
    } else {
        uint64_t now = trace ? lock_ops_now_ns() : 0;
        if (managed) {
            admission_wait(trace ? &trace->yields : NULL);
            if (trace) {
                uint64_t admitted = lock_ops_now_ns();
                trace->admission_ns = admitted - now;
                now = admitted;
            }
        }
        raw_lock(lock);
        if (trace)
            trace->spin_ns = lock_ops_now_ns() - now;
    }
    admission_enter(managed);
}

static inline bool lock_ops_trylock(struct raw_lock *lock)
{
    ensure_registered();
    /* Failure leaves an existing admission episode untouched. */
    if (!raw_trylock(lock))
        return false;
    admission_enter(admission_begin());
    return true;
}

static inline void lock_ops_unlock(struct raw_lock *lock)
{
    raw_unlock(lock);
    admission_finish();
}

#endif
