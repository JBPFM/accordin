/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbmcs.hpp>

#include "directalgo.h"

/* LiTL adapter for the MCS lock of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbMcsLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbMcsLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbMcsLock *>(instance)->~MbMcsLock();
}

static MbMcsLock::Context &context_for(MbMcsLock *lock) {
    return *static_cast<MbMcsLock::Context *>(
        directlock_context(lock, sizeof(MbMcsLock::Context)));
}

int directalgo_lock(void *instance) {
    MbMcsLock *lock = static_cast<MbMcsLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbMcsLock *lock = static_cast<MbMcsLock *>(instance);
    lock->unlock(context_for(lock));
}
