/* SPDX-License-Identifier: MIT */
#ifndef LITL_DIRECTALGO_H
#define LITL_DIRECTALGO_H

#include <pthread.h>
#include <stddef.h>
#include <time.h>

#include "directlock.h"

/*
 * Common LiTL front end for the baseline lock algorithms that manage their own
 * storage. Every such algorithm only implements the six entry points below;
 * mutex attachment, per-thread contexts, condition variables and the pthread
 * dispatch table are shared.
 */

#if defined(GCR)
#define LOCK_ALGORITHM "GCR-MCS"
#elif defined(CNA)
#define LOCK_ALGORITHM "CNA"
#elif defined(FLEXGUARD)
#define LOCK_ALGORITHM "FlexGuard"
#elif defined(MBMCS)
#define LOCK_ALGORITHM "MB-MCS"
#elif defined(MBMCSTAS)
#define LOCK_ALGORITHM "MB-MCS-TAS"
#elif defined(MBMCSTASTSE)
#define LOCK_ALGORITHM "MB-MCS-TAS-TSE"
#elif defined(MBMCSTASNEXT)
#define LOCK_ALGORITHM "MB-MCS-TAS-NEXT"
#elif defined(MBMCSTASNEXTTSE)
#define LOCK_ALGORITHM "MB-MCS-TAS-NEXT-TSE"
#elif defined(MBCLH)
#define LOCK_ALGORITHM "MB-CLH"
#elif defined(MBTWA)
#define LOCK_ALGORITHM "MB-TWA"
#elif defined(MBHAPAX)
#define LOCK_ALGORITHM "MB-HAPAX"
#elif defined(MBRECIPROCATING)
#define LOCK_ALGORITHM "MB-RECIPROCATING"
#else
#error "No direct lock algorithm selected"
#endif

/* The instance pointer lives in the intercepted mutex, the algorithm owns its
 * queue nodes, and spinlocks and rwlocks stay native. */
#define NO_INDIRECTION 1
#define NEED_CONTEXT 0
#define SUPPORT_WAITING 0
#define LITL_NATIVE_SPIN_RWLOCK 1
#define LITL_DIRECT_COND 1

typedef pthread_mutex_t lock_mutex_t;
typedef pthread_cond_t lock_cond_t;
typedef void lock_context_t;

#ifdef __cplusplus
extern "C" {
#endif

/* Provided by the selected algorithm. */
extern const size_t directalgo_instance_size;
void directalgo_instance_init(void *instance);
void directalgo_instance_fini(void *instance);
int directalgo_lock(void *instance);
int directalgo_trylock(void *instance);
void directalgo_unlock(void *instance);

/* Provided by directlock.c and directcond.c. */
int directlock_mutex_init(pthread_mutex_t *mutex,
                          const pthread_mutexattr_t *attr);
int directlock_mutex_lock(pthread_mutex_t *mutex, void *context);
int directlock_mutex_trylock(pthread_mutex_t *mutex, void *context);
void directlock_mutex_unlock(pthread_mutex_t *mutex, void *context);
int directlock_mutex_destroy(pthread_mutex_t *mutex);
int directlock_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr);
int directlock_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                         void *context);
int directlock_cond_timedwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                              void *context, const struct timespec *ts);
int directlock_cond_clockwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                              clockid_t clock, const struct timespec *ts);
int directlock_cond_signal(pthread_cond_t *cond);
int directlock_cond_broadcast(pthread_cond_t *cond);
int directlock_cond_destroy(pthread_cond_t *cond);

#ifdef __cplusplus
}
#endif

#define lock_mutex_init directlock_mutex_init
#define lock_mutex_lock directlock_mutex_lock
#define lock_mutex_trylock directlock_mutex_trylock
#define lock_mutex_unlock directlock_mutex_unlock
#define lock_mutex_destroy directlock_mutex_destroy
#define lock_cond_init directlock_cond_init
#define lock_cond_wait directlock_cond_wait
#define lock_cond_timedwait directlock_cond_timedwait
#define lock_cond_signal directlock_cond_signal
#define lock_cond_broadcast directlock_cond_broadcast
#define lock_cond_destroy directlock_cond_destroy
#define lock_thread_start() ((void)0)
#define lock_thread_exit() ((void)0)
#define lock_application_init() ((void)0)
#define lock_application_exit() ((void)0)

#endif /* LITL_DIRECTALGO_H */
