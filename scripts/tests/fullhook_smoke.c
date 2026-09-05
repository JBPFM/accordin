// SPDX-License-Identifier: GPL-2.0-only
#define _GNU_SOURCE
#include <assert.h>
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

/* Exercise an unmodified pthread program under the LD_PRELOAD interposer:
 * static and initialised mutexes, recursive mutexes, nesting, trylock and a
 * bounded-buffer producer/consumer over condition variables. Run under
 * timeout so a stalled handoff or a lost wakeup fails the test. */
enum {
    WORKERS = 8,
    ITERATIONS = 2000,
    PRODUCERS = 4,
    CONSUMERS = 4,
    ITEMS = 2000,
    CAPACITY = 4,
    TIMEOUT_MS = 50,
};

static pthread_mutex_t static_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t recursive_static = PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP;
static pthread_mutex_t nested_mutex;
static pthread_mutex_t queue_mutex;
static pthread_cond_t not_empty = PTHREAD_COND_INITIALIZER;
static pthread_cond_t not_full;
static pthread_barrier_t barrier;

static unsigned long counter, nested_counter;
static unsigned queued, produced, consumed;
static int finished;
static int recursive_probe;

static void check_interposed(const char *name)
{
    void *address = dlsym(RTLD_DEFAULT, name);
    Dl_info info;

    if (!address) {
        fprintf(stderr, "%s is not resolvable\n", name);
        exit(1);
    }
    if (!dladdr(address, &info) || !info.dli_fname ||
        !strstr(info.dli_fname, "fullhook")) {
        fprintf(stderr, "%s resolves to %s, not to the preloaded interposer\n",
                name, info.dli_fname ? info.dli_fname : "an unknown object");
        exit(1);
    }
}

static void *contender(void *unused)
{
    (void)unused;
    pthread_barrier_wait(&barrier);
    for (unsigned i = 0; i < ITERATIONS; ++i) {
        assert(pthread_mutex_lock(&static_mutex) == 0);
        ++counter;
        if (i % 4 == 0) {
            assert(pthread_mutex_lock(&nested_mutex) == 0);
            ++nested_counter;
            assert(pthread_mutex_unlock(&nested_mutex) == 0);
        }
        if (i % 8 == 0) {
            assert(pthread_mutex_lock(&recursive_static) == 0);
            assert(pthread_mutex_lock(&recursive_static) == 0);
            assert(pthread_mutex_trylock(&recursive_static) == 0);
            assert(pthread_mutex_unlock(&recursive_static) == 0);
            assert(pthread_mutex_unlock(&recursive_static) == 0);
            assert(pthread_mutex_unlock(&recursive_static) == 0);
        }
        if (i % 512 == 0) {
            const struct timespec delay = {.tv_nsec = 100000};
            assert(nanosleep(&delay, NULL) == 0);
        }
        assert(pthread_mutex_unlock(&static_mutex) == 0);
    }
    return NULL;
}

static void *producer(void *unused)
{
    (void)unused;
    for (unsigned i = 0; i < ITEMS; ++i) {
        assert(pthread_mutex_lock(&queue_mutex) == 0);
        while (queued == CAPACITY)
            assert(pthread_cond_wait(&not_full, &queue_mutex) == 0);
        ++queued;
        ++produced;
        assert(pthread_cond_signal(&not_empty) == 0);
        assert(pthread_mutex_unlock(&queue_mutex) == 0);
    }
    return NULL;
}

static void *consumer(void *unused)
{
    (void)unused;
    for (;;) {
        assert(pthread_mutex_lock(&queue_mutex) == 0);
        while (!queued && !finished)
            assert(pthread_cond_wait(&not_empty, &queue_mutex) == 0);
        if (!queued) {
            assert(pthread_mutex_unlock(&queue_mutex) == 0);
            return NULL;
        }
        --queued;
        ++consumed;
        assert(pthread_cond_signal(&not_full) == 0);
        assert(pthread_mutex_unlock(&queue_mutex) == 0);
    }
}

/* A recursive mutex must stay unavailable to every other thread. */
static void *recursive_intruder(void *unused)
{
    (void)unused;
    recursive_probe = pthread_mutex_trylock(&recursive_static);
    if (!recursive_probe)
        assert(pthread_mutex_unlock(&recursive_static) == 0);
    return NULL;
}

static void single_thread_checks(void)
{
    pthread_mutexattr_t attr;
    pthread_mutex_t recursive_object;
    pthread_t intruder;

    assert(pthread_mutex_lock(&static_mutex) == 0);
    assert(pthread_mutex_trylock(&static_mutex) == EBUSY);
    assert(pthread_mutex_lock(&nested_mutex) == 0);
    assert(pthread_mutex_unlock(&nested_mutex) == 0);
    assert(pthread_mutex_unlock(&static_mutex) == 0);
    assert(pthread_mutex_trylock(&static_mutex) == 0);
    assert(pthread_mutex_unlock(&static_mutex) == 0);

    assert(pthread_mutex_lock(&recursive_static) == 0);
    assert(pthread_mutex_lock(&recursive_static) == 0);
    assert(pthread_create(&intruder, NULL, recursive_intruder, NULL) == 0);
    assert(pthread_join(intruder, NULL) == 0);
    assert(recursive_probe == EBUSY);
    assert(pthread_mutex_unlock(&recursive_static) == 0);
    assert(pthread_mutex_unlock(&recursive_static) == 0);
    assert(pthread_create(&intruder, NULL, recursive_intruder, NULL) == 0);
    assert(pthread_join(intruder, NULL) == 0);
    assert(recursive_probe == 0);

    assert(pthread_mutexattr_init(&attr) == 0);
    assert(pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_RECURSIVE) == 0);
    assert(pthread_mutex_init(&recursive_object, &attr) == 0);
    assert(pthread_mutexattr_destroy(&attr) == 0);
    assert(pthread_mutex_lock(&recursive_object) == 0);
    assert(pthread_mutex_trylock(&recursive_object) == 0);
    assert(pthread_mutex_unlock(&recursive_object) == 0);
    assert(pthread_mutex_unlock(&recursive_object) == 0);
    assert(pthread_mutex_destroy(&recursive_object) == 0);
}

/* A timed wait that expires must return ETIMEDOUT holding the mutex again. */
static void timedwait_check(void)
{
    pthread_cond_t lonely;
    struct timespec deadline;

    assert(pthread_cond_init(&lonely, NULL) == 0);
    assert(clock_gettime(CLOCK_REALTIME, &deadline) == 0);
    deadline.tv_nsec += (long)TIMEOUT_MS * 1000000L;
    if (deadline.tv_nsec >= 1000000000L) {
        deadline.tv_nsec -= 1000000000L;
        deadline.tv_sec += 1;
    }
    assert(pthread_mutex_lock(&static_mutex) == 0);
    assert(pthread_cond_timedwait(&lonely, &static_mutex, &deadline) == ETIMEDOUT);
    assert(pthread_mutex_trylock(&static_mutex) == EBUSY);
    assert(pthread_mutex_unlock(&static_mutex) == 0);
    assert(pthread_cond_destroy(&lonely) == 0);
}

int main(void)
{
    pthread_t workers[WORKERS];
    pthread_t producers[PRODUCERS], consumers[CONSUMERS];

    check_interposed("pthread_mutex_lock");
    check_interposed("pthread_mutex_unlock");
    check_interposed("pthread_cond_wait");
    check_interposed("pthread_cond_signal");

    assert(pthread_mutex_init(&nested_mutex, NULL) == 0);
    assert(pthread_mutex_init(&queue_mutex, NULL) == 0);
    assert(pthread_cond_init(&not_full, NULL) == 0);

    single_thread_checks();
    timedwait_check();

    assert(pthread_barrier_init(&barrier, NULL, WORKERS) == 0);
    for (unsigned i = 0; i < WORKERS; ++i)
        assert(pthread_create(&workers[i], NULL, contender, NULL) == 0);
    for (unsigned i = 0; i < WORKERS; ++i)
        assert(pthread_join(workers[i], NULL) == 0);
    assert(pthread_barrier_destroy(&barrier) == 0);
    assert(counter == (unsigned long)WORKERS * ITERATIONS);
    assert(nested_counter == (unsigned long)WORKERS * ITERATIONS / 4);

    for (unsigned i = 0; i < CONSUMERS; ++i)
        assert(pthread_create(&consumers[i], NULL, consumer, NULL) == 0);
    for (unsigned i = 0; i < PRODUCERS; ++i)
        assert(pthread_create(&producers[i], NULL, producer, NULL) == 0);
    for (unsigned i = 0; i < PRODUCERS; ++i)
        assert(pthread_join(producers[i], NULL) == 0);
    assert(pthread_mutex_lock(&queue_mutex) == 0);
    finished = 1;
    assert(pthread_cond_broadcast(&not_empty) == 0);
    assert(pthread_mutex_unlock(&queue_mutex) == 0);
    for (unsigned i = 0; i < CONSUMERS; ++i)
        assert(pthread_join(consumers[i], NULL) == 0);
    assert(produced == PRODUCERS * ITEMS);
    assert(consumed == produced);
    assert(queued == 0);

    assert(pthread_cond_destroy(&not_full) == 0);
    assert(pthread_mutex_destroy(&queue_mutex) == 0);
    assert(pthread_mutex_destroy(&nested_mutex) == 0);
    printf("fullhook smoke ok: acquisitions=%lu nested=%lu items=%u\n",
           counter, nested_counter, consumed);
    return 0;
}
