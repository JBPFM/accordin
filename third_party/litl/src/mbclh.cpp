/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbclh.hpp>

#include "directalgo.h"

/* LiTL adapter for the CLH lock of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbClhLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbClhLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbClhLock *>(instance)->~MbClhLock();
}

static MbClhLock::Context &context_for(MbClhLock *lock) {
    return *static_cast<MbClhLock::Context *>(
        directlock_context(lock, sizeof(MbClhLock::Context)));
}

int directalgo_lock(void *instance) {
    MbClhLock *lock = static_cast<MbClhLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbClhLock *lock = static_cast<MbClhLock *>(instance);
    lock->unlock(context_for(lock));
}
