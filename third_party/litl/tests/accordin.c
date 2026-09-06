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
    timeout_test(CLOCK_REALTIME);
    timeout_test(CLOCK_MONOTONIC);
    puts("PASS timeout/error returns with mutex held (realtime and monotonic)");
    cancellation_test();
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
