/* SPDX-License-Identifier: MIT */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define CHECK(expr) do { if (!(expr)) { \
    fprintf(stderr, "%s:%d: %s failed\n", __FILE__, __LINE__, #expr); \
    abort(); \
} } while (0)
#define OK(expr) CHECK((expr) == 0)

static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static pthread_barrier_t start;
static long counter;
static int iterations;
static int ready, generation, acknowledged, waiter_count;

static void barrier_wait(void) {
    int ret = pthread_barrier_wait(&start);
    CHECK(ret == 0 || ret == PTHREAD_BARRIER_SERIAL_THREAD);
}

static void *increment(void *arg) {
    barrier_wait();
    for (int i = 0; i < iterations; i++) {
        if (i & 1) {
            int ret;
            while ((ret = pthread_mutex_trylock(&mutex)) == EBUSY)
                sched_yield();
            CHECK(ret == 0);
        } else {
            OK(pthread_mutex_lock(&mutex));
        }
        counter++;
        OK(pthread_mutex_unlock(&mutex));
    }
    return NULL;
}

static void counter_test(int threads) {
    pthread_t *ids = calloc(threads, sizeof(*ids));
    CHECK(ids);
    counter = 0;
    OK(pthread_barrier_init(&start, NULL, threads));
    for (int i = 0; i < threads; i++)
        OK(pthread_create(&ids[i], NULL, increment, NULL));
    for (int i = 0; i < threads; i++)
        OK(pthread_join(ids[i], NULL));
    CHECK(counter == (long)threads * iterations);
    OK(pthread_barrier_destroy(&start));
    free(ids);
    printf("PASS counter/static initialization: %d threads, %ld operations\n", threads, counter);
}

static void *try_busy(void *arg) {
    CHECK(pthread_mutex_trylock(arg) == EBUSY);
    return NULL;
}

static void lifecycle_test(void) {
    pthread_mutex_t locks[4];
    pthread_mutexattr_t attr;
    OK(pthread_mutexattr_init(&attr));
    for (int round = 0; round < 100; round++) {
        memset(locks, 0xa5, sizeof(locks));
        for (int i = 0; i < 4; i++) {
            OK(pthread_mutex_init(&locks[i], &attr));
            OK(pthread_mutex_trylock(&locks[i]));
        }
        for (int i = 3; i >= 0; i--) {
            OK(pthread_mutex_unlock(&locks[i]));
            OK(pthread_mutex_destroy(&locks[i]));
        }
    }
    OK(pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_RECURSIVE));
    CHECK(pthread_mutex_init(&locks[0], &attr) == ENOTSUP);
    OK(pthread_mutexattr_destroy(&attr));
    OK(pthread_mutex_init(&locks[0], NULL));
    struct timespec ts = {0};
    CHECK(pthread_mutex_timedlock(&locks[0], &ts) == ENOTSUP);
    OK(pthread_mutex_destroy(&locks[0]));
    pthread_t id;
    OK(pthread_mutex_lock(&mutex));
    OK(pthread_create(&id, NULL, try_busy, &mutex));
    OK(pthread_join(id, NULL));
    OK(pthread_mutex_unlock(&mutex));
    puts("PASS explicit initialization, reuse, nested locks, busy trylock");
}

static void native_locks_test(void) {
    pthread_spinlock_t spin;
    OK(pthread_spin_init(&spin, PTHREAD_PROCESS_PRIVATE));
    OK(pthread_spin_lock(&spin));
    CHECK(pthread_spin_trylock(&spin) == EBUSY);
    OK(pthread_spin_unlock(&spin));
    OK(pthread_spin_destroy(&spin));
    pthread_rwlock_t rwlock = PTHREAD_RWLOCK_INITIALIZER;
    OK(pthread_rwlock_rdlock(&rwlock));
    CHECK(pthread_rwlock_trywrlock(&rwlock) == EBUSY);
    OK(pthread_rwlock_unlock(&rwlock));
    OK(pthread_rwlock_wrlock(&rwlock));
    OK(pthread_rwlock_unlock(&rwlock));
    OK(pthread_rwlock_destroy(&rwlock));
    puts("PASS native spinlock/rwlock passthrough");
}

static struct timespec deadline(clockid_t clock, long ns) {
    struct timespec ts;
    OK(clock_gettime(clock, &ts));
    ts.tv_nsec += ns;
    ts.tv_sec += ts.tv_nsec / 1000000000;
    ts.tv_nsec %= 1000000000;
    return ts;
}

static void timeout_test(clockid_t clock) {
    pthread_condattr_t attr;
    pthread_cond_t cv;
    pthread_mutex_t lock;
    OK(pthread_mutex_init(&lock, NULL));
    OK(pthread_condattr_init(&attr));
    OK(pthread_condattr_setclock(&attr, clock));
    OK(pthread_cond_init(&cv, &attr));
    OK(pthread_condattr_destroy(&attr));
    OK(pthread_mutex_lock(&lock));
    struct timespec ts = deadline(clock, 2000000);
    int ret;
    do {
        ret = pthread_cond_timedwait(&cv, &lock, &ts);
    } while (ret == 0);
    CHECK(ret == ETIMEDOUT);
    pthread_t id;
    OK(pthread_create(&id, NULL, try_busy, &lock));
    OK(pthread_join(id, NULL));
    ts.tv_nsec = 1000000000;
    CHECK(pthread_cond_timedwait(&cv, &lock, &ts) == EINVAL);
    OK(pthread_create(&id, NULL, try_busy, &lock));
    OK(pthread_join(id, NULL));
    OK(pthread_mutex_unlock(&lock));
    OK(pthread_cond_destroy(&cv));
    OK(pthread_mutex_destroy(&lock));
}

#define FIRST_WAIT_ROUNDS 32
static struct {
    pthread_mutex_t lock;
    pthread_cond_t cv;
    int arrived;
} first_wait[FIRST_WAIT_ROUNDS];
static int first_wait_threads;

static void *first_wait_worker(void *arg) {
    for (int i = 0; i < FIRST_WAIT_ROUNDS; i++) {
        barrier_wait();
        if ((long)arg & 1) {
            int ret;
            while ((ret = pthread_mutex_trylock(&first_wait[i].lock)) == EBUSY)
                sched_yield();
            CHECK(ret == 0);
        } else {
            OK(pthread_mutex_lock(&first_wait[i].lock));
        }
        first_wait[i].arrived++;
        if (first_wait[i].arrived == first_wait_threads)
            OK(pthread_cond_broadcast(&first_wait[i].cv));
        while (first_wait[i].arrived != first_wait_threads)
            OK(pthread_cond_wait(&first_wait[i].cv, &first_wait[i].lock));
        OK(pthread_mutex_unlock(&first_wait[i].lock));
    }
    return NULL;
}

static void first_wait_test(int threads) {
    pthread_t *ids = calloc(threads, sizeof(*ids));
    CHECK(ids);
    first_wait_threads = threads;
    for (int i = 0; i < FIRST_WAIT_ROUNDS; i++) {
        OK(pthread_mutex_init(&first_wait[i].lock, NULL));
        OK(pthread_cond_init(&first_wait[i].cv, NULL));
    }
    OK(pthread_barrier_init(&start, NULL, threads));
    for (long i = 0; i < threads; i++)
        OK(pthread_create(&ids[i], NULL, first_wait_worker, (void *)i));
    for (int i = 0; i < threads; i++)
        OK(pthread_join(ids[i], NULL));
    OK(pthread_barrier_destroy(&start));
    for (int i = 0; i < FIRST_WAIT_ROUNDS; i++) {
        CHECK(first_wait[i].arrived == threads);
        OK(pthread_cond_destroy(&first_wait[i].cv));
        OK(pthread_mutex_destroy(&first_wait[i].lock));
    }
    free(ids);
    printf("PASS first condvar wait with competing lock/trylock: %d threads, %d fresh mutexes\n",
           threads, FIRST_WAIT_ROUNDS);
}

static void *ping_pong(void *arg) {
    for (int i = 0; i < 1000; i++) {
        OK(pthread_mutex_lock(&mutex));
        while (generation != 1)
            OK(pthread_cond_wait(&cond, &mutex));
        generation = 0;
        OK(pthread_cond_signal(&cond));
        OK(pthread_mutex_unlock(&mutex));
    }
    return NULL;
}

static void signal_test(void) {
    pthread_t id;
    generation = 0;
    OK(pthread_create(&id, NULL, ping_pong, NULL));
    for (int i = 0; i < 1000; i++) {
        OK(pthread_mutex_lock(&mutex));
        while (generation != 0)
            OK(pthread_cond_wait(&cond, &mutex));
        generation = 1;
        OK(pthread_cond_signal(&cond));
        OK(pthread_mutex_unlock(&mutex));
    }
    OK(pthread_join(id, NULL));
    puts("PASS condvar signal: 1000 round trips");
}

static void *broadcast_waiter(void *arg) {
    OK(pthread_mutex_lock(&mutex));
    ready++;
    OK(pthread_cond_broadcast(&cond));
    for (int epoch = 1; epoch <= 50; epoch++) {
        while (generation < epoch) {
            struct timespec ts = deadline(CLOCK_REALTIME, 100000000);
            int ret = pthread_cond_timedwait(&cond, &mutex, &ts);
            CHECK(ret == 0 || ret == ETIMEDOUT);
        }
        acknowledged++;
        if (acknowledged == waiter_count)
            OK(pthread_cond_broadcast(&cond));
    }
    OK(pthread_mutex_unlock(&mutex));
    return NULL;
}

static void broadcast_test(int threads) {
    pthread_t *ids = calloc(threads, sizeof(*ids));
    CHECK(ids);
    ready = generation = acknowledged = 0;
    waiter_count = threads;
    for (int i = 0; i < threads; i++)
        OK(pthread_create(&ids[i], NULL, broadcast_waiter, NULL));
    OK(pthread_mutex_lock(&mutex));
    while (ready < threads)
        OK(pthread_cond_wait(&cond, &mutex));
    for (int epoch = 1; epoch <= 50; epoch++) {
        acknowledged = 0;
        generation = epoch;
        OK(pthread_cond_broadcast(&cond));
        while (acknowledged < threads)
            OK(pthread_cond_wait(&cond, &mutex));
    }
    OK(pthread_mutex_unlock(&mutex));
    for (int i = 0; i < threads; i++)
        OK(pthread_join(ids[i], NULL));
    free(ids);
    printf("PASS condvar broadcast/timed wakeups: %d waiters, 50 rounds\n", threads);
}

/* Untimed waits are held by the scheduler rather than sleeping on a futex.
 * A notification arriving well after every waiter is held must still release
 * all of them, whether it reaches them directly or through custody expiry. */
#define CUSTODY_WAITERS 16
static pthread_cond_t custody_cv;
static int custody_ready, custody_go, custody_woken;

static void *custody_waiter(void *arg) {
    (void)arg;
    OK(pthread_mutex_lock(&mutex));
    custody_ready++;
    OK(pthread_cond_broadcast(&custody_cv));
    while (!custody_go)
        OK(pthread_cond_wait(&custody_cv, &mutex));
    CHECK(pthread_mutex_trylock(&mutex) == EBUSY);
    custody_woken++;
    OK(pthread_mutex_unlock(&mutex));
    return NULL;
}

static void custody_expiry_test(void) {
    pthread_t ids[CUSTODY_WAITERS];
    custody_ready = custody_go = custody_woken = 0;
    OK(pthread_cond_init(&custody_cv, NULL));
    for (int i = 0; i < CUSTODY_WAITERS; i++)
        OK(pthread_create(&ids[i], NULL, custody_waiter, NULL));
    OK(pthread_mutex_lock(&mutex));
    while (custody_ready < CUSTODY_WAITERS)
        OK(pthread_cond_wait(&custody_cv, &mutex));
    OK(pthread_mutex_unlock(&mutex));
    struct timespec delay = {.tv_nsec = 50000000};
    while (nanosleep(&delay, &delay) && errno == EINTR) {}
    OK(pthread_mutex_lock(&mutex));
    custody_go = 1;
    OK(pthread_cond_signal(&custody_cv));
    OK(pthread_cond_broadcast(&custody_cv));
    OK(pthread_mutex_unlock(&mutex));
    for (int i = 0; i < CUSTODY_WAITERS; i++)
        OK(pthread_join(ids[i], NULL));
    CHECK(custody_woken == CUSTODY_WAITERS);
    OK(pthread_cond_destroy(&custody_cv));
    printf("PASS condvar custody: %d untimed waiters released after a delayed notification\n",
           CUSTODY_WAITERS);
}

/* The suite runs with custody limits from a millisecond upward. A released wait
 * must never depend on that limit. */
static long custody_limit_ms(void) {
    const char *value = getenv("ACCORDIN_CV_CUSTODY_MS");
    long limit = value && *value ? strtol(value, NULL, 0) : 20;
    return limit > 0 ? limit : 0;
}

static long flush_width(void) {
    const char *value = getenv("ACCORDIN_CV_FLUSH_WIDTH");
    long width = value && *value ? strtol(value, NULL, 0) : 0;
    return width > 0 ? width : 0;
}

/* Under a short limit expiry releases every wait long before any deadline worth
 * asserting, so the deadline would hold whether or not a notification ever
 * reached the wait. It says something only when custody would otherwise hold
 * the wait well past it. A release narrower than the batch says nothing either:
 * it hands over as much as it is allowed to and leaves the rest to expiry. */
#define RELEASE_DEADLINE_MS 500
static int release_deadline_applies(int batch) {
    long width = flush_width();
    return custody_limit_ms() >= RELEASE_DEADLINE_MS && (!width || width >= batch);
}

static long elapsed_ms(const struct timespec *from) {
    struct timespec now;
    OK(clock_gettime(CLOCK_MONOTONIC, &now));
    return (now.tv_sec - from->tv_sec) * 1000 +
           (now.tv_nsec - from->tv_nsec) / 1000000;
}

static void settle(long ms) {
    struct timespec delay = {.tv_sec = ms / 1000, .tv_nsec = (ms % 1000) * 1000000};
    while (nanosleep(&delay, &delay) && errno == EINTR) {}
}

/* Every waiter is notified from inside the critical section, so every release
 * has to survive the notifier still holding the mutex. Alternating a broadcast
 * with one signal per waiter covers both notification shapes. */
#define HIT_WAITERS 32
#define HIT_ROUNDS 100
static pthread_cond_t hit_cv, hit_ack;
static int hit_generation, hit_waiting, hit_done;

static void *custody_hit_worker(void *arg) {
    (void)arg;
    OK(pthread_mutex_lock(&mutex));
    for (int round = 1; round <= HIT_ROUNDS; round++) {
        if (++hit_waiting == HIT_WAITERS)
            OK(pthread_cond_signal(&hit_ack));
        while (hit_generation < round)
            OK(pthread_cond_wait(&hit_cv, &mutex));
    }
    hit_done++;
    OK(pthread_mutex_unlock(&mutex));
    return NULL;
}

static void custody_hit_test(void) {
    pthread_t ids[HIT_WAITERS];
    hit_generation = hit_waiting = hit_done = 0;
    OK(pthread_cond_init(&hit_cv, NULL));
    OK(pthread_cond_init(&hit_ack, NULL));
    for (int i = 0; i < HIT_WAITERS; i++)
        OK(pthread_create(&ids[i], NULL, custody_hit_worker, NULL));
    OK(pthread_mutex_lock(&mutex));
    for (int round = 1; round <= HIT_ROUNDS; round++) {
        /* Counted under the mutex, so every waiter has reached its wait. */
        while (hit_waiting < HIT_WAITERS)
            OK(pthread_cond_wait(&hit_ack, &mutex));
        hit_waiting = 0;
        hit_generation = round;
        if (round & 1) {
            OK(pthread_cond_broadcast(&hit_cv));
        } else {
            for (int i = 0; i < HIT_WAITERS; i++)
                OK(pthread_cond_signal(&hit_cv));
        }
    }
    OK(pthread_mutex_unlock(&mutex));
    for (int i = 0; i < HIT_WAITERS; i++)
        OK(pthread_join(ids[i], NULL));
    CHECK(hit_done == HIT_WAITERS);
    OK(pthread_cond_destroy(&hit_ack));
    OK(pthread_cond_destroy(&hit_cv));
    printf("PASS condvar custody: %d waiters, %d rounds notified under the mutex\n",
           HIT_WAITERS, HIT_ROUNDS);
}

/* A notification issued with no mutex held has no unlock to defer to, so the
 * waiters have to be released there and then rather than by custody expiry. */
#define OUTSIDE_WAITERS 4
static pthread_cond_t outside_cv;
static int outside_ready, outside_go, outside_woken;

static void *outside_waiter(void *arg) {
    (void)arg;
    OK(pthread_mutex_lock(&mutex));
    outside_ready++;
    while (!outside_go)
        OK(pthread_cond_wait(&outside_cv, &mutex));
    outside_woken++;
    OK(pthread_mutex_unlock(&mutex));
    return NULL;
}

static void wait_for_count(const int *counter, int target) {
    for (;;) {
        OK(pthread_mutex_lock(&mutex));
        int seen = *counter;
        OK(pthread_mutex_unlock(&mutex));
        if (seen >= target)
            return;
        sched_yield();
    }
}

static void signal_outside_mutex_test(void) {
    pthread_t ids[OUTSIDE_WAITERS];
    struct timespec start;
    outside_ready = outside_go = outside_woken = 0;
    OK(pthread_cond_init(&outside_cv, NULL));
    for (int i = 0; i < OUTSIDE_WAITERS; i++)
        OK(pthread_create(&ids[i], NULL, outside_waiter, NULL));
    wait_for_count(&outside_ready, OUTSIDE_WAITERS);
    settle(50);
    OK(pthread_mutex_lock(&mutex));
    outside_go = 1;
    OK(pthread_mutex_unlock(&mutex));
    OK(clock_gettime(CLOCK_MONOTONIC, &start));
    for (int i = 0; i < OUTSIDE_WAITERS; i++)
        OK(pthread_cond_signal(&outside_cv));
    for (int i = 0; i < OUTSIDE_WAITERS; i++)
        OK(pthread_join(ids[i], NULL));
    long took = elapsed_ms(&start);
    CHECK(outside_woken == OUTSIDE_WAITERS);
    if (release_deadline_applies(OUTSIDE_WAITERS))
        CHECK(took < RELEASE_DEADLINE_MS);
    OK(pthread_cond_destroy(&outside_cv));
    printf("PASS condvar signal outside the mutex: %d waiters released in %ld ms\n",
           OUTSIDE_WAITERS, took);
}

/* One notification has to reach every held waiter, not just the first. */
#define MANY_WAITERS 64
static pthread_cond_t many_cv;
static int many_ready, many_go, many_woken;

static void *many_waiter(void *arg) {
    (void)arg;
    OK(pthread_mutex_lock(&mutex));
    many_ready++;
    while (!many_go)
        OK(pthread_cond_wait(&many_cv, &mutex));
    many_woken++;
    OK(pthread_mutex_unlock(&mutex));
    return NULL;
}

static void broadcast_many_test(void) {
    pthread_t ids[MANY_WAITERS];
    struct timespec start;
    many_ready = many_go = many_woken = 0;
    OK(pthread_cond_init(&many_cv, NULL));
    for (int i = 0; i < MANY_WAITERS; i++)
        OK(pthread_create(&ids[i], NULL, many_waiter, NULL));
    wait_for_count(&many_ready, MANY_WAITERS);
    settle(50);
    OK(clock_gettime(CLOCK_MONOTONIC, &start));
    OK(pthread_mutex_lock(&mutex));
    many_go = 1;
    OK(pthread_cond_broadcast(&many_cv));
    OK(pthread_mutex_unlock(&mutex));
    for (int i = 0; i < MANY_WAITERS; i++)
        OK(pthread_join(ids[i], NULL));
    long took = elapsed_ms(&start);
    CHECK(many_woken == MANY_WAITERS);
    if (release_deadline_applies(MANY_WAITERS))
        CHECK(took < RELEASE_DEADLINE_MS);
    OK(pthread_cond_destroy(&many_cv));
    printf("PASS condvar broadcast: %d waiters released by one notification in %ld ms\n",
           MANY_WAITERS, took);
}

/* A deadline shorter than the custody limit must still be the one that fires,
 * which it can only do if the wait never entered custody. */
static void timedwait_shorter_than_custody_test(void) {
    pthread_cond_t cv;
    struct timespec start;
    OK(pthread_cond_init(&cv, NULL));
    OK(pthread_mutex_lock(&mutex));
    OK(clock_gettime(CLOCK_MONOTONIC, &start));
    struct timespec ts = deadline(CLOCK_REALTIME, 10000000);
    CHECK(pthread_cond_timedwait(&cv, &mutex, &ts) == ETIMEDOUT);
    long took = elapsed_ms(&start);
    CHECK(pthread_mutex_trylock(&mutex) == EBUSY);
    OK(pthread_mutex_unlock(&mutex));
    OK(pthread_cond_destroy(&cv));
    CHECK(took >= 5);
    if (release_deadline_applies(1))
        CHECK(took < RELEASE_DEADLINE_MS);
    printf("PASS timed wait below the custody limit: returned in %ld ms\n", took);
}

/* A wait the scheduler holds takes its cancellation when it next runs, so the
 * deadline is the custody limit, not the moment of the request. */
static pthread_cond_t parked_cv;
static int parked_ready, parked_cleaned;

static void parked_cleanup(void *arg) {
    (void)arg;
    CHECK(pthread_mutex_trylock(&mutex) == EBUSY);
    parked_cleaned = 1;
    OK(pthread_mutex_unlock(&mutex));
}

static void *parked_waiter(void *arg) {
    (void)arg;
    OK(pthread_mutex_lock(&mutex));
    pthread_cleanup_push(parked_cleanup, NULL);
    parked_ready = 1;
    for (;;)
        OK(pthread_cond_wait(&parked_cv, &mutex));
    pthread_cleanup_pop(1);
    return NULL;
}

static void cancel_while_parked_test(void) {
    pthread_t id;
    struct timespec start;
    void *result;
    parked_ready = parked_cleaned = 0;
    OK(pthread_cond_init(&parked_cv, NULL));
    OK(pthread_create(&id, NULL, parked_waiter, NULL));
    wait_for_count(&parked_ready, 1);
    /* Short enough that the wait is still held when the request arrives. */
    settle(20);
    OK(clock_gettime(CLOCK_MONOTONIC, &start));
    OK(pthread_cancel(id));
    OK(pthread_join(id, &result));
    long took = elapsed_ms(&start);
    CHECK(result == PTHREAD_CANCELED);
    CHECK(parked_cleaned == 1);
    /* No notification reaches this wait, so it runs again only when its custody
     * ends. The limit is the contract and the periodic scan is what keeps it;
     * a request that waited longer means the scan stopped withdrawing custody. */
    CHECK(took <= custody_limit_ms() + RELEASE_DEADLINE_MS);
    OK(pthread_mutex_lock(&mutex));
    OK(pthread_mutex_unlock(&mutex));
    OK(pthread_cond_destroy(&parked_cv));
    printf("PASS cancellation of a held wait: cleanup ran and relocked in %ld ms\n",
           took);
}

/* Held and timed waits share one condvar and one mutex: the notifier must pick
 * the right release for each without stranding the other kind. */
#define MIXED_UNTIMED 8
#define MIXED_TIMED 8
#define MIXED_WAITERS (MIXED_UNTIMED + MIXED_TIMED)
#define MIXED_ROUNDS 20
static pthread_cond_t mixed_cv, mixed_ack;
static int mixed_generation, mixed_acknowledged, mixed_done;

static void *mixed_worker(void *arg) {
    int timed = (long)arg < MIXED_TIMED;
    OK(pthread_mutex_lock(&mutex));
    for (int round = 1; round <= MIXED_ROUNDS; round++) {
        while (mixed_generation < round) {
            if (timed) {
                struct timespec ts = deadline(CLOCK_REALTIME, 1000000000L);
                int ret = pthread_cond_timedwait(&mixed_cv, &mutex, &ts);
                CHECK(ret == 0 || ret == ETIMEDOUT);
            } else {
                OK(pthread_cond_wait(&mixed_cv, &mutex));
            }
        }
        if (++mixed_acknowledged == MIXED_WAITERS)
            OK(pthread_cond_signal(&mixed_ack));
    }
    mixed_done++;
    OK(pthread_mutex_unlock(&mutex));
    return NULL;
}

static void mixed_mode_test(void) {
    pthread_t ids[MIXED_WAITERS];
    mixed_generation = mixed_acknowledged = mixed_done = 0;
    OK(pthread_cond_init(&mixed_cv, NULL));
    OK(pthread_cond_init(&mixed_ack, NULL));
    for (long i = 0; i < MIXED_WAITERS; i++)
        OK(pthread_create(&ids[i], NULL, mixed_worker, (void *)i));
    OK(pthread_mutex_lock(&mutex));
    for (int round = 1; round <= MIXED_ROUNDS; round++) {
        mixed_acknowledged = 0;
        mixed_generation = round;
        if (round & 1) {
            OK(pthread_cond_broadcast(&mixed_cv));
        } else {
            for (int i = 0; i < MIXED_WAITERS; i++)
                OK(pthread_cond_signal(&mixed_cv));
        }
        while (mixed_acknowledged < MIXED_WAITERS) {
            struct timespec ts = deadline(CLOCK_REALTIME, 100000000);
            int ret = pthread_cond_timedwait(&mixed_ack, &mutex, &ts);
            CHECK(ret == 0 || ret == ETIMEDOUT);
            /* A timed waiter that left its wait between the signals misses one;
             * the condvar contract allows it, so hand out the round again. */
            if (ret == ETIMEDOUT)
                OK(pthread_cond_broadcast(&mixed_cv));
        }
    }
    OK(pthread_mutex_unlock(&mutex));
    for (int i = 0; i < MIXED_WAITERS; i++)
        OK(pthread_join(ids[i], NULL));
    CHECK(mixed_done == MIXED_WAITERS);
    OK(pthread_cond_destroy(&mixed_ack));
    OK(pthread_cond_destroy(&mixed_cv));
    printf("PASS mixed held and timed waits: %d waiters, %d rounds\n",
           MIXED_WAITERS, MIXED_ROUNDS);
}

static void cancel_cleanup(void *arg) {
    CHECK(pthread_mutex_trylock(&mutex) == EBUSY);
    ready = 2;
    OK(pthread_mutex_unlock(&mutex));
}

static void *cancel_waiter(void *arg) {
    OK(pthread_mutex_lock(&mutex));
    pthread_cleanup_push(cancel_cleanup, NULL);
    ready = 1;
    OK(pthread_cond_signal(&cond));
    for (;;)
        OK(pthread_cond_wait(&cond, &mutex));
    pthread_cleanup_pop(1);
    return NULL;
}

static void cancellation_test(void) {
    /* Cancellation must also work on the mutex's very first wait. The main thread polls the predicate. */
    OK(pthread_mutex_destroy(&mutex));
    OK(pthread_mutex_init(&mutex, NULL));
    pthread_t id;
    ready = 0;
    OK(pthread_create(&id, NULL, cancel_waiter, NULL));
    for (;;) {
        OK(pthread_mutex_lock(&mutex));
        int started = ready;
        OK(pthread_mutex_unlock(&mutex));
        if (started)
            break;
        sched_yield();
    }
    OK(pthread_cancel(id));
    void *result;
    OK(pthread_join(id, &result));
    CHECK(result == PTHREAD_CANCELED);
    CHECK(ready == 2);
    OK(pthread_mutex_lock(&mutex));
    OK(pthread_mutex_unlock(&mutex));
    puts("PASS cancellation restores mutex before caller cleanup");
}

static void cond_attributes_test(void) {
    pthread_condattr_t attr;
    pthread_cond_t cv;
    OK(pthread_condattr_init(&attr));
    OK(pthread_condattr_setpshared(&attr, PTHREAD_PROCESS_SHARED));
    CHECK(pthread_cond_init(&cv, &attr) == ENOTSUP);
    OK(pthread_condattr_destroy(&attr));
    OK(pthread_cond_init(&cv, NULL));
    /* Notifications with no waiters must not be saved for a later wait. */
    OK(pthread_cond_signal(&cv));
    OK(pthread_cond_signal(&cv));
    OK(pthread_cond_broadcast(&cv));
    OK(pthread_mutex_lock(&mutex));
    struct timespec ts = deadline(CLOCK_REALTIME, 2000000);
    CHECK(pthread_cond_timedwait(&cv, &mutex, &ts) == ETIMEDOUT);
    ts = (struct timespec){.tv_sec = -1};
    CHECK(pthread_cond_timedwait(&cv, &mutex, &ts) == ETIMEDOUT);
    /* Timed waits must preserve the caller's cancellation mode. */
    int state, type;
    OK(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, &state));
    CHECK(pthread_cond_timedwait(&cv, &mutex, &ts) == ETIMEDOUT);
    int previous;
    OK(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, &previous));
    CHECK(previous == PTHREAD_CANCEL_DISABLE);
    OK(pthread_setcanceltype(PTHREAD_CANCEL_DEFERRED, &type));
    CHECK(type == PTHREAD_CANCEL_DEFERRED);
    OK(pthread_setcancelstate(state, NULL));
    OK(pthread_mutex_unlock(&mutex));
    OK(pthread_cond_destroy(&cv));
    puts("PASS condvar attributes, no stored notifications, negative deadline, cancellation mode");
}

static void clockwait_test(const char *library) {
    typedef int (*clockwait_fn)(pthread_cond_t *, pthread_mutex_t *, clockid_t,
                                const struct timespec *);
    const char *versions[] = {"GLIBC_2.30", "GLIBC_2.34"};
    pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
    for (unsigned i = 0; i < sizeof(versions) / sizeof(versions[0]); i++) {
        clockwait_fn wait = (clockwait_fn)dlvsym(RTLD_DEFAULT, "pthread_cond_clockwait", versions[i]);
        Dl_info info;
        CHECK(wait && dladdr((void *)wait, &info));
        CHECK(strstr(info.dli_fname, library));
        for (int monotonic = 0; monotonic <= 1; monotonic++) {
            clockid_t clock = monotonic ? CLOCK_MONOTONIC : CLOCK_REALTIME;
            OK(pthread_mutex_lock(&mutex));
            struct timespec ts = deadline(clock, 2000000);
            CHECK(wait(&cv, &mutex, clock, &ts) == ETIMEDOUT);
            CHECK(wait(&cv, &mutex, CLOCK_THREAD_CPUTIME_ID, &ts) == EINVAL);
            pthread_t id;
            OK(pthread_create(&id, NULL, try_busy, &mutex));
            OK(pthread_join(id, NULL));
            OK(pthread_mutex_unlock(&mutex));
        }
    }
    OK(pthread_cond_destroy(&cv));
    puts("PASS clockwait: both glibc symbol versions, explicit clocks, held mutex on return");
}

/* Exercise the relock parking interval, which outlives logical notification.
 * Alternate two condvars sharing one mutex; expiry and cancellation must not
 * lose a notification or strand the rest of the mutex's wake chain. */
#define RELOCK_WAITERS 8
static pthread_cond_t relock_cv[2];
static int relock_ready, relock_done, relock_cleaned, relock_nested;

static void relock_cleanup(void *outer) {
    CHECK(pthread_mutex_trylock(&mutex) == EBUSY);
    relock_cleaned++;
    OK(pthread_mutex_unlock(&mutex));
    if (outer) {
        OK(pthread_mutex_unlock(outer));
        OK(pthread_mutex_destroy(outer));
    }
}

static void *relock_waiter(void *arg) {
    long index = (long)arg;
    pthread_mutex_t outer;
    if (relock_nested) {
        OK(pthread_mutex_init(&outer, NULL));
        OK(pthread_mutex_lock(&outer));
    }
    OK(pthread_mutex_lock(&mutex));
    pthread_cleanup_push(relock_cleanup, relock_nested ? &outer : NULL);
    relock_ready++;
    struct timespec ts = deadline(CLOCK_REALTIME, 1000000000L);
    CHECK(pthread_cond_timedwait(&relock_cv[index % 2], &mutex, &ts) == 0);
    CHECK(pthread_mutex_trylock(&mutex) == EBUSY);
    relock_done++;
    pthread_cleanup_pop(0);
    OK(pthread_mutex_unlock(&mutex));
    if (relock_nested) {
        OK(pthread_mutex_unlock(&outer));
        OK(pthread_mutex_destroy(&outer));
    }
    return NULL;
}

static void relock_queue_test(int outside, int nested) {
    pthread_t ids[RELOCK_WAITERS];
    relock_ready = relock_done = relock_cleaned = 0;
    relock_nested = nested;
    for (int i = 0; i < 2; i++)
        OK(pthread_cond_init(&relock_cv[i], NULL));
    for (long i = 0; i < RELOCK_WAITERS; i++) {
        OK(pthread_create(&ids[i], NULL, relock_waiter, (void *)i));
        for (;;) {
            OK(pthread_mutex_lock(&mutex));
            if (relock_ready == i + 1)
                break;
            OK(pthread_mutex_unlock(&mutex));
            sched_yield();
        }
        OK(pthread_mutex_unlock(&mutex));
    }
    OK(pthread_mutex_lock(&mutex));
    if (outside)
        OK(pthread_mutex_unlock(&mutex));
    for (int i = 0; i < 2; i++)
        OK(pthread_cond_broadcast(&relock_cv[i]));
    if (!outside) {
        /* Empty cond queues still have active, notified relock waiters. */
        for (int i = 0; i < 2; i++)
            CHECK(pthread_cond_destroy(&relock_cv[i]) == EBUSY);
        /* For outermost waits, index 0 is selected and index 4 still parked.
         * Cancel both,
         * then let every notified waiter's deadline expire with mutex held. */
        OK(pthread_cancel(ids[0]));
        OK(pthread_cancel(ids[4]));
        struct timespec delay = {.tv_sec = 1, .tv_nsec = 100000000};
        while (nanosleep(&delay, &delay) && errno == EINTR) {}
        CHECK(relock_done == 0);
        OK(pthread_mutex_unlock(&mutex));
    }
    for (int i = 0; i < RELOCK_WAITERS; i++) {
        void *result;
        OK(pthread_join(ids[i], &result));
        CHECK(result == (!outside && (i == 0 || i == 4) ? PTHREAD_CANCELED : NULL));
    }
    CHECK(relock_done == RELOCK_WAITERS - (outside ? 0 : 2));
    CHECK(relock_cleaned == (outside ? 0 : 2));
    for (int i = 0; i < 2; i++)
        OK(pthread_cond_destroy(&relock_cv[i]));
    printf("PASS relock queues: outside=%d nested=%d, cancellation/expiry after notification\n",
           outside, nested);
}

static pthread_cond_t cancel_race_cv;
static int cancel_race_ready, cancel_race_go, cancel_race_cleaned, cancel_race_woken;

static void cancel_race_cleanup(void *arg) {
    CHECK(pthread_mutex_trylock(&mutex) == EBUSY);
    cancel_race_cleaned++;
    OK(pthread_mutex_unlock(&mutex));
}

static void *cancel_race_waiter(void *arg) {
    OK(pthread_mutex_lock(&mutex));
    pthread_cleanup_push(cancel_race_cleanup, NULL);
    cancel_race_ready++;
    while (!cancel_race_go) {
        struct timespec ts = deadline(CLOCK_REALTIME, 1000000000);
        /* The second waiter must receive the canceled first waiter's signal. */
        CHECK(pthread_cond_timedwait(&cancel_race_cv, &mutex, &ts) == 0);
    }
    cancel_race_woken++;
    pthread_cleanup_pop(0);
    OK(pthread_mutex_unlock(&mutex));
    return NULL;
}

static void lock_when_ready(int count) {
    for (;;) {
        OK(pthread_mutex_lock(&mutex));
        if (cancel_race_ready == count)
            return;
        OK(pthread_mutex_unlock(&mutex));
        sched_yield();
    }
}

static void cancel_signal_race_test(void) {
    for (int round = 0; round < 32; round++) {
        pthread_t ids[2];
        cancel_race_ready = cancel_race_go = cancel_race_cleaned = cancel_race_woken = 0;
        OK(pthread_cond_init(&cancel_race_cv, NULL));
        OK(pthread_create(&ids[0], NULL, cancel_race_waiter, NULL));
        lock_when_ready(1);
        /* First waiter is registered: it released mutex inside cond_wait. */
        CHECK(pthread_cond_destroy(&cancel_race_cv) == EBUSY);
        OK(pthread_mutex_unlock(&mutex));
        OK(pthread_create(&ids[1], NULL, cancel_race_waiter, NULL));
        lock_when_ready(2);
        cancel_race_go = 1;
        OK(pthread_cond_signal(&cancel_race_cv));
        /* Hold mutex so the selected waiter cannot return before cancellation. */
        OK(pthread_cancel(ids[0]));
        OK(pthread_mutex_unlock(&mutex));
        void *result;
        OK(pthread_join(ids[0], &result));
        CHECK(result == PTHREAD_CANCELED);
        OK(pthread_join(ids[1], &result));
        CHECK(result == NULL);
        CHECK(cancel_race_cleaned == 1 && cancel_race_woken == 1);
        OK(pthread_cond_destroy(&cancel_race_cv));
    }
    puts("PASS cancel/signal race transfers notification, busy destroy: 32 rounds");
}

int main(int argc, char **argv) {
    CHECK(argc == 4);
    setbuf(stdout, NULL);
    int threads = atoi(argv[2]);
    iterations = atoi(argv[3]);
    CHECK(threads > 0 && threads <= 256 && iterations > 0);
    /* Inspect the executable's versioned bindings, not just RTLD_DEFAULT:
     * recent glibc uses GLIBC_2.34 for trylock but older versions for lock. */
    void *symbols[] = {pthread_mutex_init, pthread_mutex_lock,
        pthread_mutex_trylock, pthread_mutex_unlock, pthread_mutex_destroy,
        pthread_cond_init, pthread_cond_wait, pthread_cond_timedwait,
        pthread_cond_signal, pthread_cond_broadcast, pthread_cond_destroy,
        pthread_cond_clockwait};
    for (unsigned i = 0; i < sizeof(symbols) / sizeof(symbols[0]); i++) {
        Dl_info info;
        CHECK(dladdr(symbols[i], &info));
        CHECK(strstr(info.dli_fname, argv[1]));
    }
    printf("PASS interposition: %s\n", argv[1]);
    counter_test(threads);
    lifecycle_test();
    native_locks_test();
    first_wait_test(threads);
    signal_test();
    broadcast_test(threads);
    custody_expiry_test();
    custody_hit_test();
    signal_outside_mutex_test();
    broadcast_many_test();
    mixed_mode_test();
    timedwait_shorter_than_custody_test();
    timeout_test(CLOCK_REALTIME);
    timeout_test(CLOCK_MONOTONIC);
    puts("PASS timeout/error returns with mutex held (realtime and monotonic)");
    cancellation_test();
    cancel_while_parked_test();
    cancel_signal_race_test();
    relock_queue_test(0, 0);
    relock_queue_test(1, 0);
    relock_queue_test(0, 1);
    relock_queue_test(1, 1);
    cond_attributes_test();
    clockwait_test(argv[1]);
    OK(pthread_cond_destroy(&cond));
    OK(pthread_mutex_destroy(&mutex));
    puts("PASS all LiTL Accordin tests");
    return 0;
}
