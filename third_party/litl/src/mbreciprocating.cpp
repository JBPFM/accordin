/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbreciprocating.hpp>

#include "directalgo.h"

/* LiTL adapter for the reciprocating lock of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbReciprocatingLock);

void directalgo_instance_init(void *instance) {
    new (instance) MbReciprocatingLock();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbReciprocatingLock *>(instance)->~MbReciprocatingLock();
}

static MbReciprocatingLock::Context &context_for(MbReciprocatingLock *lock) {
    return *static_cast<MbReciprocatingLock::Context *>(
        directlock_context(lock, sizeof(MbReciprocatingLock::Context)));
}

int directalgo_lock(void *instance) {
    MbReciprocatingLock *lock = static_cast<MbReciprocatingLock *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbReciprocatingLock *lock = static_cast<MbReciprocatingLock *>(instance);
    lock->unlock();
}
