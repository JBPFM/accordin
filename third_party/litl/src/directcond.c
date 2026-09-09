/* SPDX-License-Identifier: MIT */
#define _GNU_SOURCE

#include <errno.h>
#include <limits.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <linux/futex.h>
#include <sys/syscall.h>

#include "directalgo.h"
#include "interpose.h"

/*
 * Sequence condition variable shared by the direct lock algorithms.
 *
 * The state lives in the intercepted pthread_cond_t, so a statically
 * initialized condition variable needs no allocation. A waiter claims a ticket
 * in target, releases the mutex, and waits until seq reaches its ticket.
 */
typedef struct __attribute__((may_alias)) {
    unsigned int seq;
    unsigned int target;
} directcond_t;

_Static_assert(sizeof(pthread_cond_t) >= sizeof(directcond_t),
               "cond state storage");
_Static_assert(_Alignof(pthread_cond_t) >= _Alignof(unsigned int),
               "cond state alignment");

static directcond_t *cond_state(pthread_cond_t *cond) {
    return (directcond_t *)cond;
}

static int futex_wait(unsigned int *word, unsigned int expected,
                      const struct timespec *timeout) {
    return (int)syscall(SYS_futex, word, FUTEX_WAIT_PRIVATE, (int)expected,
                        timeout, NULL, 0);
}

static int futex_wake(unsigned int *word, int count) {
    return (int)syscall(SYS_futex, word, FUTEX_WAKE_PRIVATE, count, NULL, NULL,
                        0);
}

static int relative_timeout(clockid_t clock, const struct timespec *deadline,
                            struct timespec *timeout) {
    struct timespec now;
    if (clock_gettime(clock, &now) != 0)
        return errno;
    timeout->tv_sec = deadline->tv_sec - now.tv_sec;
    timeout->tv_nsec = deadline->tv_nsec - now.tv_nsec;
    if (timeout->tv_nsec < 0) {
        timeout->tv_nsec += 1000000000L;
        timeout->tv_sec -= 1;
    }
    if (timeout->tv_sec < 0)
        return ETIMEDOUT;
    return 0;
}

/* Gives back the ticket of a waiter that leaves without being signalled. */
static void cancel_wait(directcond_t *state, unsigned int ticket) {
    for (;;) {
        unsigned int seq = __atomic_load_n(&state->seq, __ATOMIC_ACQUIRE);
        if ((int)(seq - ticket) >= 0)
            return;
        if (__atomic_compare_exchange_n(&state->seq, &seq, seq + 1, 1,
                                        __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            futex_wake(&state->seq, INT_MAX);
            return;
        }
    }
}

static int cond_wait_until(pthread_cond_t *cond, pthread_mutex_t *mutex,
                           clockid_t clock, const struct timespec *deadline) {
    directcond_t *state = cond_state(cond);
    unsigned int ticket =
        __atomic_add_fetch(&state->target, 1, __ATOMIC_ACQ_REL);
    unsigned int seq = __atomic_load_n(&state->seq, __ATOMIC_ACQUIRE);
    int result = 0;
    int expired = 0;

    directlock_mutex_unlock(mutex, NULL);
    while ((int)(ticket - seq) > 0) {
        if (!deadline) {
            futex_wait(&state->seq, seq, NULL);
        } else {
            struct timespec timeout;
            result = relative_timeout(clock, deadline, &timeout);
            if (result != 0) {
                expired = 1;
                break;
            }
            if (futex_wait(&state->seq, seq, &timeout) != 0 &&
                errno == ETIMEDOUT) {
                result = ETIMEDOUT;
                expired = 1;
                break;
            }
        }
        seq = __atomic_load_n(&state->seq, __ATOMIC_ACQUIRE);
    }
    if (expired)
        cancel_wait(state, ticket);
    directlock_mutex_lock(mutex, NULL);
    return result;
}

int directlock_cond_init(pthread_cond_t *cond,
                         const pthread_condattr_t *attr) {
    (void)attr;
    memset(cond, 0, sizeof(*cond));
    return 0;
}

int directlock_cond_destroy(pthread_cond_t *cond) {
    (void)cond;
    return 0;
}

int directlock_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                         void *context) {
    (void)context;
    return cond_wait_until(cond, mutex, CLOCK_REALTIME, NULL);
}

int directlock_cond_timedwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                              void *context, const struct timespec *ts) {
    (void)context;
    return cond_wait_until(cond, mutex, CLOCK_REALTIME, ts);
}

int directlock_cond_clockwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                              clockid_t clock, const struct timespec *ts) {
    return cond_wait_until(cond, mutex, clock, ts);
}

int directlock_cond_signal(pthread_cond_t *cond) {
    directcond_t *state = cond_state(cond);
    unsigned int seq = __atomic_load_n(&state->seq, __ATOMIC_ACQUIRE);
    unsigned int target = __atomic_load_n(&state->target, __ATOMIC_ACQUIRE);
    if ((int)(target - seq) <= 0)
        return 0;
    __atomic_add_fetch(&state->seq, 1, __ATOMIC_ACQ_REL);
    futex_wake(&state->seq, 1);
    return 0;
}

int directlock_cond_broadcast(pthread_cond_t *cond) {
    directcond_t *state = cond_state(cond);
    unsigned int target = __atomic_load_n(&state->target, __ATOMIC_ACQUIRE);
    for (;;) {
        unsigned int seq = __atomic_load_n(&state->seq, __ATOMIC_ACQUIRE);
        if ((int)(target - seq) <= 0)
            break;
        if (__atomic_compare_exchange_n(&state->seq, &seq, target, 1,
                                        __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE))
            break;
    }
    futex_wake(&state->seq, INT_MAX);
    return 0;
}
