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
/* Park one prepared request in the scheduler instead of sleeping on it.
 * At most one park per prepared request: it returns 0 without yielding
 * when the request is already notified or custody is unavailable, and
 * leaves the word in the waiting state in either case. */
int mcs_accordin_direct_mutex_relock_park(accordin_relock_request_t *request);
/* True while the scheduler can hold a wait instead of letting it sleep, so a
 * caller can decide up front which wakeup path a wait will need. */
int mcs_accordin_direct_mutex_cv_custody_ready(void);
/* Hand parked, notified requests back to lock admission. Returns how many
 * moved, zero without a scheduler, or -1 on error. */
int mcs_accordin_direct_mutex_cv_flush(unsigned int width, unsigned int flags);
int mcs_accordin_direct_mutex_relock(mcs_accordin_direct_mutex_t *mutex,
                                   accordin_relock_request_t *request);

#ifdef __cplusplus
}
#endif
#endif
