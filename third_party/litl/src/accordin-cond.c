/* SPDX-License-Identifier: MIT */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <errno.h>
#include <linux/futex.h>
#include <sched.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <accordin-internal.h>

/* Notification removes a waiter from the condition queue and transfers it to
 * the mutex's parking queue. Its private futex becomes runnable only when that
 * queue hands it the relock baton; the notification itself is never deferred. */
struct cond_waiter {
    struct cond_waiter *prev, *next;
    struct accordin_park_waiter park;
    int queued;
    int signaled;
};

typedef struct __attribute__((may_alias)) {
    uint32_t guard;
    clockid_t clock;
    struct cond_waiter *head, *tail;
    unsigned int active;
} cond_state_t;

_Static_assert(sizeof(cond_state_t) <= sizeof(pthread_cond_t), "condvar storage");
_Static_assert(_Alignof(cond_state_t) <= _Alignof(pthread_cond_t), "condvar alignment");
_Static_assert(CLOCK_REALTIME == 0, "all-zero static condvars use realtime");

static cond_state_t *cond_state(pthread_cond_t *cond) {
    return (cond_state_t *)cond;
}

static void cond_require(int ret) {
    if (ret)
        abort();
}

static void queue_lock(cond_state_t *cond) {
    while (__atomic_exchange_n(&cond->guard, 1, __ATOMIC_ACQUIRE)) {
        while (__atomic_load_n(&cond->guard, __ATOMIC_RELAXED))
            sched_yield();
    }
}

static void queue_unlock(cond_state_t *cond) {
    __atomic_store_n(&cond->guard, 0, __ATOMIC_RELEASE);
}

static void dequeue(cond_state_t *cond, struct cond_waiter *waiter) {
    if (waiter->prev)
        waiter->prev->next = waiter->next;
    else
        cond->head = waiter->next;
    if (waiter->next)
        waiter->next->prev = waiter->prev;
    else
        cond->tail = waiter->prev;
    waiter->queued = 0;
}

static void wake_first(cond_state_t *cond, int signaled) {
    struct cond_waiter *waiter = cond->head;
    if (!waiter)
        return;
    dequeue(cond, waiter);
    waiter->signaled = signaled;
    accordin_wait_notify(&waiter->park);
}

struct wait_cleanup {
    cond_state_t *cond;
    struct cond_waiter waiter;
    volatile int unlocked;
};

static void cancel_wait(void *arg) {
    struct wait_cleanup *cleanup = arg;
    cond_require(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, NULL));
    cond_require(pthread_setcanceltype(PTHREAD_CANCEL_DEFERRED, NULL));
    queue_lock(cleanup->cond);
    if (cleanup->waiter.queued)
        dequeue(cleanup->cond, &cleanup->waiter);
    else if (cleanup->waiter.signaled)
        /* Cancellation must not consume a signal needed by another waiter. */
        wake_first(cleanup->cond, 1);
    accordin_wait_cancel(&cleanup->waiter.park);
    cleanup->cond->active--;
    queue_unlock(cleanup->cond);
    if (cleanup->unlocked)
        accordin_wait_relock(&cleanup->waiter.park);
}

static int wait_on_cond(pthread_cond_t *cond, pthread_mutex_t *mutex,
                        clockid_t clock, const struct timespec *ts) {
    if (clock != CLOCK_REALTIME && clock != CLOCK_MONOTONIC)
        return EINVAL;
    if (ts && (ts->tv_nsec < 0 || ts->tv_nsec >= 1000000000L))
        return EINVAL;

    pthread_testcancel();
    int old_state, old_type;
    cond_require(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, &old_state));
    struct wait_cleanup cleanup = {.cond = cond_state(cond)};
    int ret = 0;
    const struct timespec *deadline = ts;
    pthread_cleanup_push(cancel_wait, &cleanup);
    accordin_wait_init(mutex, &cleanup.waiter.park);
    queue_lock(cleanup.cond);
    cleanup.waiter.prev = cleanup.cond->tail;
    cleanup.waiter.queued = 1;
    if (cleanup.cond->tail)
        cleanup.cond->tail->next = &cleanup.waiter;
    else
        cleanup.cond->head = &cleanup.waiter;
    cleanup.cond->tail = &cleanup.waiter;
    cleanup.cond->active++;
    queue_unlock(cleanup.cond);

    /* Register before releasing the user mutex. An early signal queues our
     * notification; arming after unlock can then publish the relock request. */
    accordin_mutex_unlock(mutex, NULL);
    cleanup.unlocked = 1;
    accordin_wait_arm(&cleanup.waiter.park);

    /* syscall(SYS_futex) is not a libc cancellation point. Enable asynchronous
     * cancellation only around the resource-free futex wait loop, with the
     * cleanup handler installed and neither user mutex nor queue guard held.
     * The adapter is linked with -z now, avoiding lazy PLT binding here. */
    cond_require(pthread_setcanceltype(PTHREAD_CANCEL_ASYNCHRONOUS, &old_type));
    cond_require(pthread_setcancelstate(old_state, NULL));
    while (!__atomic_load_n(&cleanup.waiter.park.wake, __ATOMIC_ACQUIRE)) {
        int error = 0;
        int op = FUTEX_WAIT_BITSET_PRIVATE;
        if (clock == CLOCK_REALTIME)
            op |= FUTEX_CLOCK_REALTIME;
        if (deadline && deadline->tv_sec < 0)
            error = ETIMEDOUT;
        else if (syscall(SYS_futex, &cleanup.waiter.park.wake, op, 0, deadline,
                         NULL, FUTEX_BITSET_MATCH_ANY) < 0)
            error = errno;
        if (error && error != EINTR && error != EAGAIN) {
            cond_require(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, NULL));
            queue_lock(cleanup.cond);
            if (cleanup.waiter.queued) {
                /* Linearize expiry against notification before reacquiring.
                 * The notifier can no longer publish into this request. */
                dequeue(cleanup.cond, &cleanup.waiter);
                ret = error;
            } else {
                /* Already notified: the mutex wait has no deadline. Keeping
                 * the expired deadline would spin while awaiting the baton. */
                deadline = NULL;
            }
            queue_unlock(cleanup.cond);
            if (ret)
                break;
            cond_require(pthread_setcancelstate(old_state, NULL));
        }
    }
    cond_require(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, NULL));
    cond_require(pthread_setcanceltype(old_type, NULL));

    /* Reacquire with cancellation disabled. Keep the cleanup registered until
     * the final cancellation check, including a cancel racing with a signal
     * while this thread is waiting to reacquire the user mutex. */
    accordin_wait_relock(&cleanup.waiter.park);
    cleanup.unlocked = 0;
    cond_require(pthread_setcancelstate(old_state, NULL));
    pthread_testcancel();
    cond_require(pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, NULL));
    queue_lock(cleanup.cond);
    cleanup.cond->active--;
    queue_unlock(cleanup.cond);
    pthread_cleanup_pop(0);
    cond_require(pthread_setcancelstate(old_state, NULL));
    return ret;
}

int accordin_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex, void *context) {
    return wait_on_cond(cond, mutex, cond_state(cond)->clock, NULL);
}

int accordin_cond_timedwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                           void *context, const struct timespec *ts) {
    if (!ts)
        return EINVAL;
    return wait_on_cond(cond, mutex, cond_state(cond)->clock, ts);
}

int accordin_cond_clockwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                           clockid_t clock, const struct timespec *ts) {
    if (!ts)
        return EINVAL;
    return wait_on_cond(cond, mutex, clock, ts);
}

int accordin_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr) {
    clockid_t clock = CLOCK_REALTIME;
    if (attr) {
        int shared, ret = pthread_condattr_getpshared(attr, &shared);
        if (ret)
            return ret;
        if (shared != PTHREAD_PROCESS_PRIVATE)
            return ENOTSUP;
        if ((ret = pthread_condattr_getclock(attr, &clock)))
            return ret;
        if (clock != CLOCK_REALTIME && clock != CLOCK_MONOTONIC)
            return EINVAL;
    }
    memset(cond, 0, sizeof(*cond));
    cond_state(cond)->clock = clock;
    return 0;
}

int accordin_cond_signal(pthread_cond_t *cond) {
    cond_state_t *state = cond_state(cond);
    queue_lock(state);
    wake_first(state, 1);
    queue_unlock(state);
    return 0;
}

int accordin_cond_broadcast(pthread_cond_t *cond) {
    cond_state_t *state = cond_state(cond);
    queue_lock(state);
    while (state->head)
        wake_first(state, 0);
    queue_unlock(state);
    return 0;
}

int accordin_cond_destroy(pthread_cond_t *cond) {
    cond_state_t *state = cond_state(cond);
    queue_lock(state);
    int ret = state->active ? EBUSY : 0;
    queue_unlock(state);
    return ret;
}
