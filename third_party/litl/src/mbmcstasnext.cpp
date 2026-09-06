/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbmcstasnext.hpp>

#include "directalgo.h"

/* LiTL adapter for the MCS lock with a test-and-set fast path and an explicit next-in-line hand-off of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbMcsTasNextLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbMcsTasNextLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbMcsTasNextLock *>(instance)->~MbMcsTasNextLock();
}

static MbMcsTasNextLock::Context &context_for(MbMcsTasNextLock *lock) {
    return *static_cast<MbMcsTasNextLock::Context *>(
        directlock_context(lock, sizeof(MbMcsTasNextLock::Context)));
}

int directalgo_lock(void *instance) {
    MbMcsTasNextLock *lock = static_cast<MbMcsTasNextLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbMcsTasNextLock *lock = static_cast<MbMcsTasNextLock *>(instance);
    lock->unlock(context_for(lock));
}
