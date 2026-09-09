/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbmcstas.hpp>

#include "directalgo.h"

/* LiTL adapter for the MCS lock with a test-and-set fast path of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbMcsTasLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbMcsTasLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbMcsTasLock *>(instance)->~MbMcsTasLock();
}

static MbMcsTasLock::Context &context_for(MbMcsTasLock *lock) {
    return *static_cast<MbMcsTasLock::Context *>(
        directlock_context(lock, sizeof(MbMcsTasLock::Context)));
}

int directalgo_lock(void *instance) {
    MbMcsTasLock *lock = static_cast<MbMcsTasLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbMcsTasLock *lock = static_cast<MbMcsTasLock *>(instance);
    lock->unlock(context_for(lock));
}
