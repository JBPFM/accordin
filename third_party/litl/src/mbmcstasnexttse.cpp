/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbmcstasnexttse.hpp>

#include "directalgo.h"

/* LiTL adapter for the MCS lock with a test-and-set fast path, an explicit next-in-line hand-off and an rseq time-slice extension of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbMcsTasNextTseLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbMcsTasNextTseLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbMcsTasNextTseLock *>(instance)->~MbMcsTasNextTseLock();
}

static MbMcsTasNextTseLock::Context &context_for(MbMcsTasNextTseLock *lock) {
    return *static_cast<MbMcsTasNextTseLock::Context *>(
        directlock_context(lock, sizeof(MbMcsTasNextTseLock::Context)));
}

int directalgo_lock(void *instance) {
    MbMcsTasNextTseLock *lock = static_cast<MbMcsTasNextTseLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbMcsTasNextTseLock *lock = static_cast<MbMcsTasNextTseLock *>(instance);
    lock->unlock(context_for(lock));
}
