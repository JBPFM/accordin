// SPDX-License-Identifier: GPL-2.0-only
#define _GNU_SOURCE
#include <assert.h>
#include <pthread.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdio.h>
#include <time.h>
#include "../../include/accordin_relock.h"
#include "../../src/bpf/intf.h"
#include "backend.h"

enum { THREADS = 6, ITERATIONS = 1000 };
static void *shared;
static void (*prepare)(accordin_relock_request_t *);
static int (*relock)(void *, accordin_relock_request_t *);
static int (*park)(accordin_relock_request_t *);
static _Atomic unsigned started;
static unsigned counter;
static sem_t prepared, resume;

static void *worker(void *unused) {
    (void)unused;
    atomic_fetch_add(&started, 1);
    for (unsigned i = 0; i < ITERATIONS; ++i) {
        assert(!mutex_lock(shared));
        ++counter;
        assert(!mutex_unlock(shared));
    }
    return NULL;
}

static void *old_relock(void *unused) {
    (void)unused;
    accordin_relock_request_t request;
    assert(!mutex_lock(shared));
    assert(!mutex_unlock(shared));
    prepare(&request);
    assert(request.word && !request.nested);
    assert(!park(&request));
    assert(!sem_post(&prepared));
    assert(!sem_wait(&resume));
    assert(!relock(shared, &request));
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) ==
           (request.epoch | USER_HELD));
    assert(!mutex_unlock(shared));
    return NULL;
}

int main(int argc, char **argv) {
    assert(argc == 3);
    backend_pin_cpu_pair();
    load_backend(argv[1], argv[2]);
    prepare = symbol("mutex_relock_prepare");
    relock = symbol("mutex_relock");
    park = symbol("mutex_relock_park");
    shared = mutex_create();
    void *outer = mutex_create();
    assert(shared && outer);

    /* Below capacity, a CV request can be prepared but custody stays off. */
    assert(!mutex_lock(shared));
    assert(!mutex_unlock(shared));
    accordin_relock_request_t probe;
    prepare(&probe);
    assert(probe.word && !probe.nested);
    assert(!park(&probe));
    assert(!relock(shared, &probe));
    assert(!mutex_unlock(shared));
    assert(!sem_init(&prepared, 0, 0));
    assert(!sem_init(&resume, 0, 0));
    pthread_t old;
    assert(!pthread_create(&old, NULL, old_relock, NULL));
    assert(!sem_wait(&prepared));

    /* Hold two locks before overload. The first queued MCS successor may
     * also predate activation; it must be allowed to finish its handoff. */
    assert(!mutex_lock(outer));
    assert(!mutex_lock(shared));
    pthread_t workers[THREADS];
    for (unsigned i = 0; i < THREADS; ++i)
        assert(!pthread_create(&workers[i], NULL, worker, NULL));
    while (atomic_load(&started) != THREADS) {
        const struct timespec pause = {.tv_nsec = 1000000};
        assert(!nanosleep(&pause, NULL));
    }
    assert(!mutex_unlock(shared));
    assert(!mutex_unlock(outer));
    /* A request prepared off-mode confirms its epoch after activation. */
    assert(!sem_post(&resume));
    assert(!pthread_join(old, NULL));
    assert(!sem_destroy(&prepared));
    assert(!sem_destroy(&resume));
    for (unsigned i = 0; i < THREADS; ++i)
        assert(!pthread_join(workers[i], NULL));
    assert(counter == THREADS * ITERATIONS);

    /* Once triggered, low load does not revoke grants or restart episodes. */
    prepare(&probe);
    assert(probe.word && !probe.nested);
    assert(!relock(shared, &probe));
    assert(__atomic_load_n((uint32_t *)probe.word, __ATOMIC_ACQUIRE) ==
           (probe.epoch | USER_HELD));
    assert(!mutex_lock(outer));
    assert(!mutex_unlock(shared));
    assert(!mutex_unlock(outer));
    assert(__atomic_load_n((uint32_t *)probe.word, __ATOMIC_ACQUIRE) == probe.epoch);
    assert(!mutex_destroy(shared));
    assert(!mutex_destroy(outer));
    puts("auto admission ok: inactive, overload, old holder/node/relock, latched active");
    return 0;
}
