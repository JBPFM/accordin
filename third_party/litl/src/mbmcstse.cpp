/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbmcstse.hpp>

#include "directalgo.h"

/* LiTL adapter for the MCS lock with an rseq time-slice extension held across the critical section. */

const size_t directalgo_instance_size = sizeof(MbMcsTseLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbMcsTseLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbMcsTseLock *>(instance)->~MbMcsTseLock();
}

static MbMcsTseLock::Context &context_for(MbMcsTseLock *lock) {
    return *static_cast<MbMcsTseLock::Context *>(
        directlock_context(lock, sizeof(MbMcsTseLock::Context)));
}

int directalgo_lock(void *instance) {
    MbMcsTseLock *lock = static_cast<MbMcsTseLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbMcsTseLock *lock = static_cast<MbMcsTseLock *>(instance);
    lock->unlock(context_for(lock));
}
