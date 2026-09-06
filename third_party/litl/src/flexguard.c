/* SPDX-License-Identifier: MIT */
#define _GNU_SOURCE

#include <errno.h>
#include <stddef.h>

#include <flexguard.h>

#include "directalgo.h"
#include "interpose.h"

/*
 * FlexGuard adapter.
 *
 * The reference implementation owns everything the algorithm needs: the lock
 * word and the MCS queue head live in flexguard_lock_t, the queue nodes live in
 * an array indexed by a thread-local identifier, and the preemption counter
 * that arbitrates between spinning and blocking is published by the BPF
 * runtime. Nothing here belongs in a per-thread, per-lock context.
 *
 * flexguard_init both initialises one lock and, exactly once per process,
 * brings up that runtime, so the first intercepted mutex reaches the
 * initialisation path. Loading this library on its own attaches nothing.
 */

const size_t directalgo_instance_size = sizeof(flexguard_lock_t);

void directalgo_instance_init(void *instance) {
    flexguard_init((flexguard_lock_t *)instance);
}

void directalgo_instance_fini(void *instance) {
    flexguard_destroy((flexguard_lock_t *)instance);
}

int directalgo_lock(void *instance) {
    flexguard_lock((flexguard_lock_t *)instance);
    return 0;
}

int directalgo_trylock(void *instance) {
    return flexguard_trylock((flexguard_lock_t *)instance);
}

void directalgo_unlock(void *instance) {
    flexguard_unlock((flexguard_lock_t *)instance);
}
