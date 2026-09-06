/* SPDX-License-Identifier: MIT */
/* A library that takes a lock while the scheduler library is still loading.
 * Allocators with their own locks do this through the interposed mutex, so the
 * thread reaches the runtime before there is a registry to publish its
 * admission word into. A thread left unpublished reads to the scheduler as
 * idle, is never admitted, and waits for a grant that cannot arrive. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>

static pthread_mutex_t early = PTHREAD_MUTEX_INITIALIZER;

int pthread_key_create(pthread_key_t *key, void (*destructor)(void *)) {
    static int (*next)(pthread_key_t *, void (*)(void *));
    static int taken;

    if (!next)
        next = dlsym(RTLD_NEXT, "pthread_key_create");
    /* Set before the lock: the runtime creates a key of its own, and this must
     * hold exactly one lock, on the loading thread, however deep it recurses. */
    if (!taken) {
        taken = 1;
        pthread_mutex_lock(&early);
        pthread_mutex_unlock(&early);
    }
    return next(key, destructor);
}
