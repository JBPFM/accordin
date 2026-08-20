#ifndef MCS_TAS_ACCORDIN_DIRECT_H
#define MCS_TAS_ACCORDIN_DIRECT_H

#include <time.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct mcs_tas_accordin_direct_mutex
    mcs_tas_accordin_direct_mutex_t;

typedef struct mcs_tas_accordin_direct_cond
    mcs_tas_accordin_direct_cond_t;

mcs_tas_accordin_direct_mutex_t*
mcs_tas_accordin_direct_mutex_create(void);

int mcs_tas_accordin_direct_mutex_destroy(
    mcs_tas_accordin_direct_mutex_t* mutex);

int mcs_tas_accordin_direct_mutex_lock(
    mcs_tas_accordin_direct_mutex_t* mutex);

int mcs_tas_accordin_direct_mutex_trylock(
    mcs_tas_accordin_direct_mutex_t* mutex);

int mcs_tas_accordin_direct_mutex_unlock(
    mcs_tas_accordin_direct_mutex_t* mutex);

mcs_tas_accordin_direct_cond_t*
mcs_tas_accordin_direct_cond_create(void);

int mcs_tas_accordin_direct_cond_destroy(
    mcs_tas_accordin_direct_cond_t* cond);

int mcs_tas_accordin_direct_cond_wait(
    mcs_tas_accordin_direct_cond_t* cond,
    mcs_tas_accordin_direct_mutex_t* mutex);

int mcs_tas_accordin_direct_cond_timedwait(
    mcs_tas_accordin_direct_cond_t* cond,
    mcs_tas_accordin_direct_mutex_t* mutex,
    const struct timespec* abstime);

int mcs_tas_accordin_direct_cond_signal(
    mcs_tas_accordin_direct_cond_t* cond);

int mcs_tas_accordin_direct_cond_broadcast(
    mcs_tas_accordin_direct_cond_t* cond);

#ifdef __cplusplus
}
#endif

#endif
