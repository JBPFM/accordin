/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <linux/futex.h>
#include <sched.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <accordin-internal.h>
#include "interpose.h"

struct accordin_mutex {
    ACCORDIN_DIRECT(t) *direct;
    uint32_t park_guard, park_pending;
    struct accordin_park_waiter *head, *tail, *selected;
    pthread_t relock_owner;
    unsigned int relock_owned;
};

static void park_lock(struct accordin_mutex *mutex) {
    while (__atomic_exchange_n(&mutex->park_guard, 1, __ATOMIC_ACQUIRE)) {
        while (__atomic_load_n(&mutex->park_guard, __ATOMIC_RELAXED))
            sched_yield();
    }
}

static void park_unlock(struct accordin_mutex *mutex) {
    __atomic_store_n(&mutex->park_guard, 0, __ATOMIC_RELEASE);
}

static void park_remove(struct accordin_mutex *mutex,
                        struct accordin_park_waiter *waiter) {
    if (waiter->prev)
        waiter->prev->next = waiter->next;
    else
        mutex->head = waiter->next;
    if (waiter->next)
        waiter->next->prev = waiter->prev;
    else
        mutex->tail = waiter->prev;
    waiter->queued = 0;
}

/* Keep park_guard through the syscall and the waiter must pass this guard
 * before returning: neither the futex nor its TLS request can disappear. */
static void park_wake(struct accordin_park_waiter *waiter) {
    ACCORDIN_DIRECT(relock_wake)(&waiter->request);
    /* Pair with the waiter's parked publication and wake recheck. Either it
     * sees wake before sleeping or we see parked and issue FUTEX_WAKE. */
    __atomic_store_n(&waiter->wake, 1, __ATOMIC_SEQ_CST);
    if (__atomic_load_n(&waiter->parked, __ATOMIC_SEQ_CST) &&
        syscall(SYS_futex, &waiter->wake, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0) < 0)
        abort();
}

static void park_start(struct accordin_mutex *mutex) {
    if (!mutex->selected && !mutex->relock_owned && mutex->head &&
        mutex->head->armed) {
        mutex->selected = mutex->head;
        park_wake(mutex->selected);
    }
    __atomic_store_n(&mutex->park_pending, mutex->head || mutex->relock_owned,
                     __ATOMIC_RELEASE);
}

/* All-zero PTHREAD_MUTEX_INITIALIZER is lazily replaced with this pointer.
 * may_alias allows accessing pthread storage through the pointer slot. */
typedef struct accordin_mutex *mutex_slot_t __attribute__((may_alias));
_Static_assert(sizeof(pthread_mutex_t) >= sizeof(mutex_slot_t), "mutex pointer storage");
_Static_assert(_Alignof(pthread_mutex_t) >= _Alignof(mutex_slot_t), "mutex pointer alignment");

static void require_success(int result) {
    if (result != 0)
        abort();
}

static struct accordin_mutex *create_mutex(const pthread_mutexattr_t *attr) {
    struct accordin_mutex *impl = calloc(1, sizeof(*impl));
    if (!impl)
        return NULL;
    impl->direct = ACCORDIN_DIRECT(create)();
    if (!impl->direct) {
        free(impl);
        return NULL;
    }
    return impl;
}

static void free_mutex(struct accordin_mutex *impl) {
    require_success(ACCORDIN_DIRECT(destroy)(impl->direct));
    free(impl);
}

static struct accordin_mutex *get_mutex(pthread_mutex_t *mutex) {
    mutex_slot_t *slot = (mutex_slot_t *)mutex;
    struct accordin_mutex *impl = __atomic_load_n(slot, __ATOMIC_ACQUIRE);
    if (!impl) {
        struct accordin_mutex *candidate = create_mutex(NULL);
        if (!candidate)
            return NULL;
        if (__atomic_compare_exchange_n(slot, &impl, candidate, 0,
                                        __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE))
            impl = candidate;
        else
            free_mutex(candidate);
    }
    return impl;
}

int accordin_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr) {
    if (attr) {
        int type, shared, robust, protocol;
        int ret = pthread_mutexattr_gettype(attr, &type);
        if (ret) return ret;
        if ((ret = pthread_mutexattr_getpshared(attr, &shared))) return ret;
        if ((ret = pthread_mutexattr_getrobust(attr, &robust))) return ret;
        if ((ret = pthread_mutexattr_getprotocol(attr, &protocol))) return ret;
        if (type != PTHREAD_MUTEX_NORMAL || shared != PTHREAD_PROCESS_PRIVATE ||
            robust != PTHREAD_MUTEX_STALLED || protocol != PTHREAD_PRIO_NONE)
            return ENOTSUP;
    }
    struct accordin_mutex *impl = create_mutex(attr);
    if (!impl)
        return errno ? errno : ENOMEM;
    memset(mutex, 0, sizeof(*mutex));
    __atomic_store_n((mutex_slot_t *)mutex, impl, __ATOMIC_RELEASE);
    return 0;
}

static int acquire_mutex(struct accordin_mutex *impl, int trylock) {
    return trylock ? ACCORDIN_DIRECT(trylock)(impl->direct)
                   : ACCORDIN_DIRECT(lock)(impl->direct);
}

int accordin_mutex_lock(pthread_mutex_t *mutex, void *context) {
    struct accordin_mutex *impl = get_mutex(mutex);
    return impl ? acquire_mutex(impl, 0) : ENOMEM;
}

int accordin_mutex_trylock(pthread_mutex_t *mutex, void *context) {
    struct accordin_mutex *impl = get_mutex(mutex);
    return impl ? acquire_mutex(impl, 1) : ENOMEM;
}

void accordin_mutex_unlock(pthread_mutex_t *mutex, void *context) {
    struct accordin_mutex *impl = get_mutex(mutex);
    if (!impl)
        abort();
    if (!__atomic_load_n(&impl->park_pending, __ATOMIC_ACQUIRE)) {
        require_success(ACCORDIN_DIRECT(unlock)(impl->direct));
        return;
    }
    /* Serialize baton release with the successor's acquisition. A notification
     * racing the fast path starts its own first wake, even if the mutex is idle. */
    park_lock(impl);
    if (impl->relock_owned && pthread_equal(impl->relock_owner, pthread_self()))
        impl->relock_owned = 0;
    require_success(ACCORDIN_DIRECT(unlock)(impl->direct));
    park_start(impl);
    park_unlock(impl);
}

void accordin_wait_init(pthread_mutex_t *mutex, struct accordin_park_waiter *waiter) {
    waiter->mutex = get_mutex(mutex);
    if (!waiter->mutex)
        abort();
}

void accordin_wait_arm(struct accordin_park_waiter *waiter) {
    struct accordin_mutex *mutex = waiter->mutex;
    park_lock(mutex);
    /* Done after unlock: an early notifier must not overwrite USER_HELD or
     * have its request cleared by admission_finish on the old acquisition. */
    ACCORDIN_DIRECT(relock_prepare)(&waiter->request);
    waiter->armed = 1;
    if (waiter->queued && waiter->request.nested) {
        /* A wait retaining other mutexes retains its outer admission episode.
         * Do not delay its wake behind the serialized relock baton. */
        park_remove(mutex, waiter);
        park_wake(waiter);
    }
    park_start(mutex);
    park_unlock(mutex);
}

void accordin_wait_notify(struct accordin_park_waiter *waiter) {
    struct accordin_mutex *mutex = waiter->mutex;
    park_lock(mutex);
    /* Logical notification ends CV spinning even if a previous waiter still
     * owns the relock baton. A notified waiter must not burn a CPU waiting for
     * that separate queue; park_wake alone publishes its relock admission. */
    __atomic_store_n(&waiter->notified, 1, __ATOMIC_RELEASE);
    if (waiter->armed && waiter->request.nested) {
        park_wake(waiter);
    } else {
        waiter->prev = mutex->tail;
        if (mutex->tail)
            mutex->tail->next = waiter;
        else
            mutex->head = waiter;
        mutex->tail = waiter;
        waiter->queued = 1;
        park_start(mutex);
    }
    park_unlock(mutex);
}

void accordin_wait_cancel(struct accordin_park_waiter *waiter) {
    struct accordin_mutex *mutex = waiter->mutex;
    park_lock(mutex);
    if (waiter->queued) {
        park_remove(mutex, waiter);
        if (mutex->selected == waiter)
            mutex->selected = NULL;
        park_start(mutex);
    }
    park_unlock(mutex);
}

void accordin_wait_relock(struct accordin_park_waiter *waiter) {
    struct accordin_mutex *mutex = waiter->mutex;
    require_success(ACCORDIN_DIRECT(relock)(mutex->direct, &waiter->request));
    park_lock(mutex);
    if (waiter->queued) {
        if (mutex->selected != waiter || mutex->relock_owned)
            abort();
        park_remove(mutex, waiter);
        mutex->selected = NULL;
        mutex->relock_owner = pthread_self();
        mutex->relock_owned = 1;
        /* Retain the baton until this owner releases the user mutex, including
         * a release inside its next cond_wait. No stack pointer survives here. */
    }
    park_unlock(mutex);
}

int accordin_mutex_destroy(pthread_mutex_t *mutex) {
    struct accordin_mutex *impl = __atomic_exchange_n((mutex_slot_t *)mutex, NULL,
                                                     __ATOMIC_ACQ_REL);
    if (impl)
        free_mutex(impl);
    return 0;
}
