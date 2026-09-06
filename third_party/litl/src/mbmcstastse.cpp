/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbmcstastse.hpp>

#include "directalgo.h"

/* LiTL adapter for the MCS lock with a test-and-set fast path and an rseq time-slice extension of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbMcsTasTseLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbMcsTasTseLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbMcsTasTseLock *>(instance)->~MbMcsTasTseLock();
}

static MbMcsTasTseLock::Context &context_for(MbMcsTasTseLock *lock) {
    return *static_cast<MbMcsTasTseLock::Context *>(
        directlock_context(lock, sizeof(MbMcsTasTseLock::Context)));
}

int directalgo_lock(void *instance) {
    MbMcsTasTseLock *lock = static_cast<MbMcsTasTseLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbMcsTasTseLock *lock = static_cast<MbMcsTasTseLock *>(instance);
    lock->unlock(context_for(lock));
}
