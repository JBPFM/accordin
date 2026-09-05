// SPDX-License-Identifier: GPL-2.0-only
#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

/* Exercise the shipped C ABI, including concurrent first-use registration and
 * TLS cleanup. Run under timeout so a stalled lock handoff fails the test. */
enum { THREADS = 8, ITERATIONS = 2000 };
static void *library;
static const char *prefix;
static void *(*mutex_create)(void);
static int (*mutex_destroy)(void *);
static int (*mutex_lock)(void *);
static int (*mutex_trylock)(void *);
static int (*mutex_unlock)(void *);
static void *primary, *secondary;
static unsigned counter, nested_counter;
static pthread_barrier_t barrier;

static void *symbol(const char *suffix) {
    char name[160];
    snprintf(name, sizeof(name), "%s_%s", prefix, suffix);
    void *result = dlsym(library, name);
    if (!result) {
        fprintf(stderr, "missing symbol %s: %s\n", name, dlerror());
        exit(1);
    }
    return result;
}

static void *contender(void *unused) {
    (void)unused;
    pthread_barrier_wait(&barrier);
    for (unsigned i = 0; i < ITERATIONS; ++i) {
        assert(mutex_lock(primary) == 0);
        ++counter;
        if (i % 4 == 0) {
            assert(mutex_lock(secondary) == 0);
            ++nested_counter;
            assert(mutex_unlock(secondary) == 0);
        }
        /* A holder must resume even when contenders are parked on its CPU. */
        if (i % 128 == 0)
            sched_yield();
        if (i % 512 == 0) {
            const struct timespec delay = {.tv_nsec = 100000};
            assert(nanosleep(&delay, NULL) == 0);
        }
        assert(mutex_unlock(primary) == 0);
    }
    return NULL;
}

int main(int argc, char **argv) {
    assert(argc == 3);
    prefix = argv[2];
    library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (!library) {
        fprintf(stderr, "dlopen: %s\n", dlerror());
        return 1;
    }
    mutex_create = symbol("mutex_create");
    mutex_destroy = symbol("mutex_destroy");
    mutex_lock = symbol("mutex_lock");
    mutex_trylock = symbol("mutex_trylock");
    mutex_unlock = symbol("mutex_unlock");
    assert(mutex_lock(NULL) == EINVAL);
    assert(mutex_trylock(NULL) == EINVAL);
    assert(mutex_unlock(NULL) == EINVAL);
    assert(mutex_destroy(NULL) == EINVAL);
    primary = mutex_create();
    secondary = mutex_create();
    assert(primary && secondary);
    assert(mutex_trylock(primary) == 0);
    assert(mutex_trylock(primary) == EBUSY);
    assert(mutex_unlock(primary) == 0);

    /* Out-of-order unlocks still share one admission episode. */
    assert(mutex_lock(primary) == 0);
    assert(mutex_lock(secondary) == 0);
    assert(mutex_unlock(primary) == 0);
    assert(mutex_unlock(secondary) == 0);

    pthread_t threads[THREADS];
    assert(pthread_barrier_init(&barrier, NULL, THREADS) == 0);
    for (unsigned i = 0; i < THREADS; ++i)
        assert(pthread_create(&threads[i], NULL, contender, NULL) == 0);
    if (getenv("DIRECT_SMOKE_MIGRATE")) {
        cpu_set_t allowed;
        int cpus[2], count = 0;
        assert(sched_getaffinity(0, sizeof(allowed), &allowed) == 0);
        for (int cpu = 0; cpu < CPU_SETSIZE && count < 2; ++cpu)
            if (CPU_ISSET(cpu, &allowed))
                cpus[count++] = cpu;
        assert(count == 2);
        /* Move queued/spinning threads onto CPUs occupied by other waiters. */
        for (unsigned round = 0; round < 16; ++round) {
            cpu_set_t mask;
            CPU_ZERO(&mask);
            CPU_SET(cpus[round % 2], &mask);
            for (unsigned i = 0; i < THREADS; ++i) {
                int result = pthread_setaffinity_np(threads[i], sizeof(mask), &mask);
                assert(result == 0 || result == ESRCH);
            }
            const struct timespec delay = {.tv_nsec = 1000000};
            assert(nanosleep(&delay, NULL) == 0);
        }
    }
    for (unsigned i = 0; i < THREADS; ++i)
        assert(pthread_join(threads[i], NULL) == 0);
    assert(counter == THREADS * ITERATIONS);
    assert(nested_counter == THREADS * ITERATIONS / 4);
    assert(pthread_barrier_destroy(&barrier) == 0);

    assert(mutex_destroy(secondary) == 0);
    assert(mutex_destroy(primary) == 0);
    printf("direct smoke ok: %s acquisitions=%u nested=%u\n",
           prefix, counter, nested_counter);
    /* Direct libraries and their registered TLS state live until process exit. */
    return 0;
}
