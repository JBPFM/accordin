/* SPDX-License-Identifier: MIT */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define CHECK(expr)                                                            \
    do {                                                                       \
        if (!(expr)) {                                                         \
            fprintf(stderr, "%s:%d: %s failed\n", __FILE__, __LINE__, #expr);  \
            abort();                                                           \
        }                                                                      \
    } while (0)
#define OK(expr) CHECK((expr) == 0)

static pthread_mutex_t static_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t dynamic_mutex;
static pthread_cond_t static_cond = PTHREAD_COND_INITIALIZER;
static pthread_barrier_t start;
static long counter;
static int iterations;
static int handoff, produced, consumed;

static void barrier_wait(void) {
    int ret = pthread_barrier_wait(&start);
    CHECK(ret == 0 || ret == PTHREAD_BARRIER_SERIAL_THREAD);
}

/* A concurrency-restricting algorithm may never grant a try-acquire, so the
 * test accepts a permanent EBUSY and falls back to a blocking acquisition. */
static void acquire_either_way(pthread_mutex_t *mutex, int prefer_try) {
    if (prefer_try) {
        for (int attempt = 0; attempt < 64; attempt++) {
            int ret = pthread_mutex_trylock(mutex);
            if (ret == 0)
                return;
            CHECK(ret == EBUSY);
            sched_yield();
        }
    }
    OK(pthread_mutex_lock(mutex));
}

static void *increment(void *arg) {
    pthread_mutex_t *mutex = arg;
    barrier_wait();
    for (int i = 0; i < iterations; i++) {
        acquire_either_way(mutex, i & 1);
        counter++;
        OK(pthread_mutex_unlock(mutex));
    }
    return NULL;
}

static void counter_test(pthread_mutex_t *mutex, int threads,
                         const char *label) {
    pthread_t *ids = calloc(threads, sizeof(*ids));
    CHECK(ids);
    counter = 0;
    OK(pthread_barrier_init(&start, NULL, threads));
    for (int i = 0; i < threads; i++)
        OK(pthread_create(&ids[i], NULL, increment, mutex));
    for (int i = 0; i < threads; i++)
        OK(pthread_join(ids[i], NULL));
    CHECK(counter == (long)threads * iterations);
    OK(pthread_barrier_destroy(&start));
    free(ids);
    printf("PASS counter/%s: %d threads, %ld operations\n", label, threads,
           counter);
}

static void *consumer(void *arg) {
    int rounds = *(int *)arg;
    for (int i = 0; i < rounds; i++) {
        OK(pthread_mutex_lock(&static_mutex));
        while (handoff == 0)
            OK(pthread_cond_wait(&static_cond, &static_mutex));
        handoff = 0;
        consumed++;
        OK(pthread_cond_broadcast(&static_cond));
        OK(pthread_mutex_unlock(&static_mutex));
    }
    return NULL;
}

static void condition_test(int rounds) {
    pthread_t id;
    handoff = produced = consumed = 0;
    OK(pthread_create(&id, NULL, consumer, &rounds));
    for (int i = 0; i < rounds; i++) {
        OK(pthread_mutex_lock(&static_mutex));
        while (handoff != 0)
            OK(pthread_cond_wait(&static_cond, &static_mutex));
        handoff = 1;
        produced++;
        OK(pthread_cond_signal(&static_cond));
        OK(pthread_mutex_unlock(&static_mutex));
    }
    OK(pthread_join(id, NULL));
    CHECK(produced == rounds && consumed == rounds);
    printf("PASS condition variable: %d handoffs\n", rounds);
}

static void timedwait_test(void) {
    struct timespec deadline;
    OK(clock_gettime(CLOCK_REALTIME, &deadline));
    deadline.tv_nsec += 2000000L;
    if (deadline.tv_nsec >= 1000000000L) {
        deadline.tv_nsec -= 1000000000L;
        deadline.tv_sec += 1;
    }

    OK(pthread_mutex_lock(&static_mutex));
    int ret = pthread_cond_timedwait(&static_cond, &static_mutex, &deadline);
    CHECK(ret == ETIMEDOUT);
    OK(pthread_mutex_unlock(&static_mutex));

    /* A timed-out waiter must not consume a later notification. */
    condition_test(4);
    printf("PASS condition variable timeout\n");
}

int main(int argc, char **argv) {
    if (argc != 4) {
        fprintf(stderr, "usage: %s LIBRARY THREADS ITERATIONS\n", argv[0]);
        return 2;
    }
    int threads = atoi(argv[2]);
    iterations = atoi(argv[3]);
    CHECK(threads > 0 && iterations > 0);

    Dl_info info;
    CHECK(dladdr((void *)(uintptr_t)&pthread_mutex_lock, &info));
    CHECK(info.dli_fname);
    CHECK(strstr(info.dli_fname, argv[1]));
    printf("PASS interposition: %s\n", argv[1]);

    counter_test(&static_mutex, threads, "static initialization");

    OK(pthread_mutex_init(&dynamic_mutex, NULL));
    counter_test(&dynamic_mutex, threads, "explicit initialization");
    OK(pthread_mutex_destroy(&dynamic_mutex));

    condition_test(iterations < 256 ? iterations : 256);
    timedwait_test();
    return 0;
}
