/* SPDX-License-Identifier: MIT */
#ifndef LITL_ACCORDIN_H
#define LITL_ACCORDIN_H

#include <pthread.h>
#include "padding.h"

#if defined(MCSTASACCORDIN)
#include <mcs_tas_accordin_direct.h>
#define LOCK_ALGORITHM "MCS-TAS-Accordin"
#define ACCORDIN_DIRECT(name) mcs_tas_accordin_direct_mutex_##name
#else
#include <mcs_accordin_direct.h>
#define LOCK_ALGORITHM "MCS-Accordin"
#define ACCORDIN_DIRECT(name) mcs_accordin_direct_mutex_##name
#endif

/* The direct runtime owns its TLS queue nodes. Store the adapter pointer in
 * the intercepted pthread mutex instead of allocating LiTL's CLHT/context. */
#define NO_INDIRECTION 1
#define NEED_CONTEXT 0
#define SUPPORT_WAITING 0
typedef pthread_mutex_t lock_mutex_t;
typedef pthread_cond_t lock_cond_t;
typedef void lock_context_t;

int accordin_mutex_init(lock_mutex_t *mutex, const pthread_mutexattr_t *attr);
int accordin_mutex_lock(lock_mutex_t *mutex, lock_context_t *context);
int accordin_mutex_trylock(lock_mutex_t *mutex, lock_context_t *context);
void accordin_mutex_unlock(lock_mutex_t *mutex, lock_context_t *context);
int accordin_mutex_destroy(lock_mutex_t *mutex);
int accordin_cond_init(lock_cond_t *cond, const pthread_condattr_t *attr);
int accordin_cond_wait(lock_cond_t *cond, lock_mutex_t *mutex, lock_context_t *context);
int accordin_cond_timedwait(lock_cond_t *cond, lock_mutex_t *mutex,
                          lock_context_t *context, const struct timespec *ts);
int accordin_cond_clockwait(lock_cond_t *cond, lock_mutex_t *mutex,
                          clockid_t clock, const struct timespec *ts);
int accordin_cond_signal(lock_cond_t *cond);
int accordin_cond_broadcast(lock_cond_t *cond);
int accordin_cond_destroy(lock_cond_t *cond);

#define lock_mutex_init accordin_mutex_init
#define lock_mutex_lock accordin_mutex_lock
#define lock_mutex_trylock accordin_mutex_trylock
#define lock_mutex_unlock accordin_mutex_unlock
#define lock_mutex_destroy accordin_mutex_destroy
#define lock_cond_init accordin_cond_init
#define lock_cond_wait accordin_cond_wait
#define lock_cond_timedwait accordin_cond_timedwait
#define lock_cond_signal accordin_cond_signal
#define lock_cond_broadcast accordin_cond_broadcast
#define lock_cond_destroy accordin_cond_destroy
#define lock_thread_start() ((void)0)
#define lock_thread_exit() ((void)0)
#define lock_application_init() ((void)0)
#define lock_application_exit() ((void)0)

#endif
