// SPDX-License-Identifier: GPL-2.0-only
#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include "../../include/accordin_relock.h"
#include "../../src/bpf/intf.h"

enum { THREADS = 6, ITERATIONS = 1000 };
static void *library, *shared;
static const char *prefix;
static void *(*create)(void);
static int (*destroy)(void *), (*lock)(void *), (*unlock)(void *);
static void (*prepare)(accordin_relock_request_t *);
static int (*relock)(void *, accordin_relock_request_t *);
static int (*park)(accordin_relock_request_t *);
static _Atomic unsigned started;
static unsigned counter;
static sem_t prepared, resume;

static void *symbol(const char *name) {
    char full[160];
    snprintf(full, sizeof(full), "%s_mutex_%s", prefix, name);
    void *value = dlsym(library, full);
    assert(value);
    return value;
}

static void *worker(void *unused) {
    (void)unused;
    atomic_fetch_add(&started, 1);
    for (unsigned i = 0; i < ITERATIONS; ++i) {
        assert(!lock(shared));
        ++counter;
        assert(!unlock(shared));
    }
    return NULL;
}

static void *old_relock(void *unused) {
    (void)unused;
    accordin_relock_request_t request;
    assert(!lock(shared));
    assert(!unlock(shared));
    prepare(&request);
    assert(request.word && !request.nested);
    assert(!park(&request));
    assert(!sem_post(&prepared));
    assert(!sem_wait(&resume));
    assert(!relock(shared, &request));
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) ==
           (request.epoch | USER_HELD));
    assert(!unlock(shared));
    return NULL;
}

int main(int argc, char **argv) {
    assert(argc == 3);
    cpu_set_t allowed, pair;
    assert(!sched_getaffinity(0, sizeof(allowed), &allowed));
    CPU_ZERO(&pair);
    for (int cpu = 0; cpu < CPU_SETSIZE && CPU_COUNT(&pair) < 2; ++cpu)
        if (CPU_ISSET(cpu, &allowed))
            CPU_SET(cpu, &pair);
    assert(CPU_COUNT(&pair) == 2);
    assert(!sched_setaffinity(0, sizeof(pair), &pair));
    prefix = argv[2];
    library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) {
        fprintf(stderr, "%s\n", dlerror());
        return 1;
    }
    create = symbol("create"); destroy = symbol("destroy");
    lock = symbol("lock"); unlock = symbol("unlock");
    prepare = symbol("relock_prepare"); relock = symbol("relock");
    park = symbol("relock_park");
    shared = create();
    void *outer = create();
    assert(shared && outer);

    /* Below capacity, a CV request can be prepared but custody stays off. */
    assert(!lock(shared));
    assert(!unlock(shared));
    accordin_relock_request_t probe;
    prepare(&probe);
    assert(probe.word && !probe.nested);
    assert(!park(&probe));
    assert(!relock(shared, &probe));
    assert(!unlock(shared));
    assert(!sem_init(&prepared, 0, 0));
    assert(!sem_init(&resume, 0, 0));
    pthread_t old;
    assert(!pthread_create(&old, NULL, old_relock, NULL));
    assert(!sem_wait(&prepared));

    /* Hold two locks before overload. The first queued MCS successor may
     * also predate activation; it must be allowed to finish its handoff. */
    assert(!lock(outer));
    assert(!lock(shared));
    pthread_t workers[THREADS];
    for (unsigned i = 0; i < THREADS; ++i)
        assert(!pthread_create(&workers[i], NULL, worker, NULL));
    while (atomic_load(&started) != THREADS) {
        const struct timespec pause = {.tv_nsec = 1000000};
        assert(!nanosleep(&pause, NULL));
    }
    assert(!unlock(shared));
    assert(!unlock(outer));
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
    assert(!lock(outer));
    assert(!unlock(shared));
    assert(!unlock(outer));
    assert(__atomic_load_n((uint32_t *)probe.word, __ATOMIC_ACQUIRE) == probe.epoch);
    assert(!destroy(shared));
    assert(!destroy(outer));
    puts("auto admission ok: inactive, overload, old holder/node/relock, latched active");
    return 0;
}
