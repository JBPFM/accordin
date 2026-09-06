/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbtwa.hpp>

#include "directalgo.h"

/* LiTL adapter for the ticket lock with a waiting array of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbTwaLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbTwaLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbTwaLock *>(instance)->~MbTwaLock();
}

static MbTwaLock::Context &context_for(MbTwaLock *lock) {
    return *static_cast<MbTwaLock::Context *>(
        directlock_context(lock, sizeof(MbTwaLock::Context)));
}

int directalgo_lock(void *instance) {
    MbTwaLock *lock = static_cast<MbTwaLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbTwaLock *lock = static_cast<MbTwaLock *>(instance);
    lock->unlock(context_for(lock));
}
