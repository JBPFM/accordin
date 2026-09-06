/* SPDX-License-Identifier: GPL-2.0-only */
#include <errno.h>
#include <string.h>
#include "runtime.h"

#ifdef MCS_TAS
#include "mcs_tas.h"
#include "mcs_tas_accordin_direct.h"
#define MUTEX mcs_tas_accordin_direct_mutex
#else
#include "mcs.h"
#include "mcs_accordin_direct.h"
#define MUTEX mcs_accordin_direct_mutex
#endif

#define CONCAT_(a, b) a##b
#define CONCAT(a, b) CONCAT_(a, b)
#define API(op) CONCAT(MUTEX, _##op)
#define EXPORT __attribute__((visibility("default")))

struct MUTEX {
    struct raw_lock raw;
};

EXPORT struct MUTEX *API(create)(void)
{
    struct MUTEX *mutex = aligned_alloc(_Alignof(struct MUTEX), sizeof(*mutex));
    if (mutex) {
        atomic_init(&mutex->raw.tail, NULL);
#ifdef MCS_TAS
        atomic_init(&mutex->raw.locked, false);
#endif
    }
    return mutex;
}

EXPORT int API(destroy)(struct MUTEX *mutex)
{
    if (!mutex)
        return EINVAL;
    free(mutex);
    return 0;
}

EXPORT int API(lock)(struct MUTEX *mutex)
{
    if (!mutex)
        return EINVAL;
    ensure_registered();
    bool managed = admission_begin();
    if (!raw_trylock(&mutex->raw)) {
        if (managed)
            admission_wait(false);
        raw_lock(&mutex->raw);
    }
    admission_enter(managed);
    return 0;
}

EXPORT int API(trylock)(struct MUTEX *mutex)
{
    if (!mutex)
        return EINVAL;
    ensure_registered();
    /* Failure leaves an existing admission episode untouched. */
    if (!raw_trylock(&mutex->raw))
        return EBUSY;
    admission_enter(admission_begin());
    return 0;
}

EXPORT int API(unlock)(struct MUTEX *mutex)
{
    if (!mutex)
        return EINVAL;
    raw_unlock(&mutex->raw);
    admission_finish();
    return 0;
}

EXPORT void API(relock_prepare)(accordin_relock_request_t *request)
{
    ensure_registered();
    *request = (accordin_relock_request_t){.nested = thread_state.depth != 0};
    if (!request->nested && admission_enabled) {
        uint32_t word = atomic_load_explicit(&thread_state.word, memory_order_relaxed);
        request->word = &thread_state.word;
        request->epoch = (word & ~USER_FLAGS) + 4;
        atomic_store_explicit(&thread_state.word, request->epoch, memory_order_release);
    }
}

EXPORT void API(relock_wake)(accordin_relock_request_t *request)
{
    if (request->word)
        atomic_store_explicit((_Atomic uint32_t *)request->word,
                              request->epoch | USER_WAITING, memory_order_release);
}

EXPORT int API(relock)(struct MUTEX *mutex, accordin_relock_request_t *request)
{
    if (!mutex || !request)
        return EINVAL;
    /* No admission_begin: the notifier published this exact request before
     * FUTEX_WAKE. Timeouts/cancellation publish it here if still dormant. */
    bool managed = request->word != NULL;
    thread_state.depth++;
    API(relock_wake)(request);
    if (!raw_trylock(&mutex->raw)) {
        if (managed)
            admission_wait(true);
        raw_lock(&mutex->raw);
    }
    admission_enter(managed);
    return 0;
}

#ifndef MCS_TAS
/* Historical MCS library aliases used by existing direct clients. */
#define TAS_ALIAS(op) \
    EXPORT __typeof__(mcs_accordin_direct_mutex_##op) mcs_tas_accordin_direct_mutex_##op \
        __attribute__((alias("mcs_accordin_direct_mutex_" #op)));
TAS_ALIAS(create)
TAS_ALIAS(destroy)
TAS_ALIAS(lock)
TAS_ALIAS(trylock)
TAS_ALIAS(unlock)
TAS_ALIAS(relock_prepare)
TAS_ALIAS(relock_wake)
TAS_ALIAS(relock)
#endif
