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
            admission_wait();
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
#endif
