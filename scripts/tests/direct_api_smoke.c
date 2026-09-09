// SPDX-License-Identifier: GPL-2.0-only
#define _GNU_SOURCE
#include <assert.h>
#include <ctype.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <time.h>
#include "../../include/accordin_relock.h"
#include "../../src/bpf/intf.h"
#include "backend.h"

/* Exercise the shipped C ABI, including concurrent first-use registration and
 * TLS cleanup. Run under timeout so a stalled lock handoff fails the test. */
enum { THREADS = 8, ITERATIONS = 2000, MIXED_ITERATIONS = 20000 };
static int (*mutex_trylock)(void *);
static void (*relock_prepare)(accordin_relock_request_t *);
static void (*relock_wake)(accordin_relock_request_t *);
static int (*relock_park)(accordin_relock_request_t *);
static int (*cv_flush)(unsigned int, unsigned int);
static int (*mutex_relock)(void *, accordin_relock_request_t *);
static void *primary, *secondary;
static unsigned counter, nested_counter;
static pthread_barrier_t barrier;

static void *publish_relock(void *arg) {
    relock_wake(arg);
    return NULL;
}

static void relock_test(void) {
    accordin_relock_request_t request;
    assert(mutex_lock(primary) == 0);
    assert(mutex_unlock(primary) == 0);
    relock_prepare(&request);
    assert(request.word && !request.nested);
    assert(!(request.epoch & USER_META));
    uint32_t first_epoch = request.epoch;
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) == request.epoch);
    pthread_t notifier;
    assert(pthread_create(&notifier, NULL, publish_relock, &request) == 0);
    assert(pthread_join(notifier, NULL) == 0);
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) ==
           (request.epoch | USER_WAITING));
    assert(mutex_relock(primary, &request) == 0);
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) ==
           (request.epoch | USER_HELD));
    assert(mutex_unlock(primary) == 0);
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) == request.epoch);
    /* Timeout/cancel consumes a dormant request without a notifier. */
    relock_prepare(&request);
    assert(!(request.epoch & USER_META) && request.epoch == first_epoch + 8);
    assert(mutex_relock(primary, &request) == 0);
    uint32_t held = __atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE);
    assert(held == (request.epoch | USER_HELD));
    assert(mutex_lock(secondary) == 0);
    assert(mutex_unlock(primary) == 0);
    accordin_relock_request_t nested;
    relock_prepare(&nested);
    assert(nested.nested && !nested.word);
    relock_wake(&nested);
    assert(mutex_relock(primary, &nested) == 0);
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) == held);
    assert(mutex_unlock(secondary) == 0);
    assert(mutex_unlock(primary) == 0);
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) == request.epoch);
    puts("direct relock ok: cross-thread publication, same epoch, dormant/nested requests");
}

/* Custody hands the thread to the scheduler until the wait is notified or its
 * limit passes. Nothing notifies here, so the park must end through expiry. */
static void park_test(int scheduled) {
    accordin_relock_request_t request;
    assert(mutex_lock(primary) == 0);
    assert(mutex_unlock(primary) == 0);
    relock_prepare(&request);
    assert(request.word && !request.nested);
    struct timespec before, after;
    assert(clock_gettime(CLOCK_MONOTONIC, &before) == 0);
    int parked = relock_park(&request);
    assert(clock_gettime(CLOCK_MONOTONIC, &after) == 0);
    assert(parked == scheduled);
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) ==
           (scheduled ? (request.epoch | USER_WAITING) : request.epoch));
    /* A park that returns without waiting never reached the scheduler. */
    long long elapsed_us = (after.tv_sec - before.tv_sec) * 1000000LL +
                           (after.tv_nsec - before.tv_nsec) / 1000;
    assert(!scheduled || elapsed_us >= 1000);
    assert(mutex_relock(primary, &request) == 0);
    assert(mutex_unlock(primary) == 0);

    /* A request notified before the park is never handed to the scheduler. */
    relock_prepare(&request);
    pthread_t notifier;
    assert(pthread_create(&notifier, NULL, publish_relock, &request) == 0);
    assert(pthread_join(notifier, NULL) == 0);
    assert(relock_park(&request) == 0);
    assert(__atomic_load_n((uint32_t *)request.word, __ATOMIC_ACQUIRE) ==
           (request.epoch | USER_WAITING));
    assert(mutex_relock(primary, &request) == 0);
    assert(mutex_unlock(primary) == 0);

    /* Both passes run here: neither has a wait of this thread's to release. */
    assert(cv_flush(0, CV_FLUSH_MOVE | CV_FLUSH_EXPIRE) >= 0);
    printf("direct park ok: custody %s, notified request skips the park\n",
           scheduled ? "expired" : "unavailable");
}

/* Mirrors the runtime's reading of ACCORDIN_CV_CUSTODY: on unless denied. */
static int custody_allowed(void) {
    const char *value = getenv("ACCORDIN_CV_CUSTODY");
    if (!value)
        return 1;
    return !(!strcasecmp(value, "0") || !strcasecmp(value, "false") ||
             !strcasecmp(value, "no") || !strcasecmp(value, "off"));
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

/* Reuse trylock nodes while other threads enqueue behind them. On weakly
 * ordered CPUs this stresses publication of the node's initialized next link. */
static void *mixed_contender(void *unused) {
    (void)unused;
    pthread_barrier_wait(&barrier);
    for (unsigned i = 0; i < MIXED_ITERATIONS; ++i) {
        if (i & 1) {
            int result;
            while ((result = mutex_trylock(primary)) == EBUSY)
                sched_yield();
            assert(result == 0);
        } else {
            assert(mutex_lock(primary) == 0);
        }
        ++counter;
        assert(mutex_unlock(primary) == 0);
    }
    return NULL;
}

int main(int argc, char **argv) {
    assert(argc == 3);
    char disable[96];
    snprintf(disable, sizeof(disable), "%s_DISABLE_BPF", argv[2]);
    for (char *c = disable; *c; ++c)
        *c = (char)toupper((unsigned char)*c);
    const char *setting = getenv(disable);
    /* Custody needs both the scheduler and the custody setting, which the
     * runtime reads as enabled unless it is explicitly denied. */
    int scheduled = setting && setting[0] == '0' && custody_allowed();
    load_backend(argv[1], argv[2]);
    mutex_trylock = symbol("mutex_trylock");
    relock_prepare = symbol("mutex_relock_prepare");
    relock_wake = symbol("mutex_relock_wake");
    mutex_relock = symbol("mutex_relock");
    relock_park = symbol("mutex_relock_park");
    cv_flush = symbol("mutex_cv_flush");
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

    relock_test();
    park_test(scheduled);

    pthread_t threads[THREADS];
    assert(pthread_barrier_init(&barrier, NULL, THREADS) == 0);
    for (unsigned i = 0; i < THREADS; ++i)
        assert(pthread_create(&threads[i], NULL, contender, NULL) == 0);
    if (getenv("DIRECT_SMOKE_MIGRATE")) {
        int cpus[2];
        backend_cpu_pair(cpus);
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
    for (unsigned i = 0; i < THREADS; ++i)
        assert(pthread_create(&threads[i], NULL, mixed_contender, NULL) == 0);
    for (unsigned i = 0; i < THREADS; ++i)
        assert(pthread_join(threads[i], NULL) == 0);
    assert(counter == THREADS * (ITERATIONS + MIXED_ITERATIONS));
    assert(pthread_barrier_destroy(&barrier) == 0);

    assert(mutex_destroy(secondary) == 0);
    assert(mutex_destroy(primary) == 0);
    printf("direct smoke ok: %s acquisitions=%u nested=%u\n",
           backend_prefix, counter, nested_counter);
    /* Direct libraries and their registered TLS state live until process exit. */
    return 0;
}
