/* SPDX-License-Identifier: GPL-2.0-only */
#include <errno.h>
#include <string.h>
#include <time.h>
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
        request->epoch = (word & ~USER_META) + 8;
        atomic_store_explicit(&thread_state.word, request->epoch, memory_order_release);
    }
}

EXPORT void API(relock_wake)(accordin_relock_request_t *request)
{
    if (request->word)
        atomic_store_explicit((_Atomic uint32_t *)request->word,
                              request->epoch | USER_WAITING, memory_order_release);
}

static uint64_t monotonic_ns(void)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now))
        abort();
    return (uint64_t)now.tv_sec * 1000000000 + now.tv_nsec;
}

static bool deadline_passed(clockid_t clock, const struct timespec *deadline)
{
    if (!deadline)
        return false;
    struct timespec now;
    if (clock_gettime(clock, &now))
        return true;
    return now.tv_sec > deadline->tv_sec ||
           (now.tv_sec == deadline->tv_sec && now.tv_nsec >= deadline->tv_nsec);
}

EXPORT void API(relock_spin)(accordin_relock_request_t *request,
                             uint32_t *wake, uint32_t *spin_fail,
                             clockid_t clock, const struct timespec *deadline)
{
    struct admission_state *state = scheduler_admission;
    if (!request->word || !cv_spin_ns || !state ||
        !__atomic_load_n(&state->enabled, __ATOMIC_ACQUIRE) ||
        __atomic_load_n(wake, __ATOMIC_ACQUIRE) || deadline_passed(clock, deadline))
        return;
    bool demand_breaks = __atomic_load_n(spin_fail, __ATOMIC_RELAXED) >= 3;
    if (demand_breaks && admission_demand())
        return;

    _Atomic uint32_t *word = request->word;
    uint32_t expected = request->epoch;
    uint32_t waiting = request->epoch | USER_CV | USER_WAITING;
    uint32_t spinning = request->epoch | USER_CV | USER_SPINNING;
    uint64_t ticket = ((uint64_t)request->epoch << 32) | thread_state.tid;
    /* A notifier may already have published ordinary WAITING. Never overwrite
     * it, including when withdrawing an unsuccessful CV admission request. */
    if (!atomic_compare_exchange_strong_explicit(word, &expected, waiting,
                                                memory_order_acq_rel, memory_order_acquire))
        return;
    sched_yield();
    expected = waiting;
    if (admission_slot_held(ticket) &&
        atomic_compare_exchange_strong_explicit(word, &expected, spinning,
                                                memory_order_acq_rel, memory_order_acquire)) {
        uint64_t start = monotonic_ns();
        bool moved = false;
        for (unsigned int spins = 0;; spins++) {
            if (__atomic_load_n(wake, __ATOMIC_ACQUIRE)) {
                moved = true;
                break;
            }
            if (!(spins & 63) &&
                ((demand_breaks && admission_demand()) ||
                 monotonic_ns() - start >= cv_spin_ns ||
                 deadline_passed(clock, deadline) || !admission_slot_held(ticket)))
                break;
            spin_pause();
        }
        /* Shared condvars can finish spins concurrently. Keep the score
         * saturating without losing another waiter's update. */
        uint32_t score = __atomic_load_n(spin_fail, __ATOMIC_RELAXED), next;
        do {
            next = moved ? (score ? score - 1 : 0) : (score >= 6 ? 8 : score + 2);
        } while (!__atomic_compare_exchange_n(spin_fail, &score, next, 1,
                                              __ATOMIC_RELAXED, __ATOMIC_RELAXED));
    }
    expected = spinning;
    if (!atomic_compare_exchange_strong_explicit(word, &expected, request->epoch,
                                                 memory_order_acq_rel, memory_order_acquire)) {
        expected = waiting;
        atomic_compare_exchange_strong_explicit(word, &expected, request->epoch,
                                                memory_order_acq_rel, memory_order_acquire);
    }
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
TAS_ALIAS(relock_spin)
TAS_ALIAS(relock)
#endif
