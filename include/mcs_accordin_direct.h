#ifndef MCS_ACCORDIN_DIRECT_H
#define MCS_ACCORDIN_DIRECT_H

#include "accordin_relock.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct mcs_accordin_direct_mutex mcs_accordin_direct_mutex_t;

mcs_accordin_direct_mutex_t *mcs_accordin_direct_mutex_create(void);
int mcs_accordin_direct_mutex_destroy(mcs_accordin_direct_mutex_t *mutex);
int mcs_accordin_direct_mutex_lock(mcs_accordin_direct_mutex_t *mutex);
int mcs_accordin_direct_mutex_trylock(mcs_accordin_direct_mutex_t *mutex);
int mcs_accordin_direct_mutex_unlock(mcs_accordin_direct_mutex_t *mutex);
void mcs_accordin_direct_mutex_relock_prepare(accordin_relock_request_t *request);
void mcs_accordin_direct_mutex_relock_wake(accordin_relock_request_t *request);
int mcs_accordin_direct_mutex_relock(mcs_accordin_direct_mutex_t *mutex,
                                   accordin_relock_request_t *request);

#ifdef __cplusplus
}
#endif
#endif
