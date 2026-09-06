/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stddef.h>

#include <gcrmcs.h>

#include "directalgo.h"
#include "interpose.h"

const size_t directalgo_instance_size = sizeof(gcr_mcs_mutex_t);

void directalgo_instance_init(void *instance) {
    gcr_mcs_init((gcr_mcs_mutex_t *)instance);
}

void directalgo_instance_fini(void *instance) {
    gcr_mcs_destroy((gcr_mcs_mutex_t *)instance);
}

int directalgo_lock(void *instance) {
    gcr_mcs_lock((gcr_mcs_mutex_t *)instance);
    return 0;
}

/* The concurrency restriction admission path has no non-blocking entry, so a
 * try-acquire reports contention rather than bypassing admission. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    gcr_mcs_unlock((gcr_mcs_mutex_t *)instance);
}
