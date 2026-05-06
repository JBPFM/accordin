#ifndef MCS_TAS_ACCORDIN_DIRECT_H
#define MCS_TAS_ACCORDIN_DIRECT_H

#ifdef __cplusplus
extern "C" {
#endif

typedef struct mcs_tas_accordin_direct_mutex
    mcs_tas_accordin_direct_mutex_t;

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

#ifdef __cplusplus
}
#endif

#endif
