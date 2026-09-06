/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <new>

#include <mbhapax.hpp>

#include "directalgo.h"

/* LiTL adapter for the Hapax lock with visible waiters of the mutex microbenchmark. */

const size_t directalgo_instance_size = sizeof(MbHapaxVw);

void directalgo_instance_init(void *instance) {
    new (instance) MbHapaxVw();
}

void directalgo_instance_fini(void *instance) {
    static_cast<MbHapaxVw *>(instance)->~MbHapaxVw();
}

static MbHapaxVw::Context &context_for(MbHapaxVw *lock) {
    return *static_cast<MbHapaxVw::Context *>(
        directlock_context(lock, sizeof(MbHapaxVw::Context)));
}

int directalgo_lock(void *instance) {
    MbHapaxVw *lock = static_cast<MbHapaxVw *>(instance);
    lock->lock(context_for(lock));
    return 0;
}

/* The algorithm has no try-acquire path, so a try-acquire reports contention. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    MbHapaxVw *lock = static_cast<MbHapaxVw *>(instance);
    lock->unlock(context_for(lock));
}
