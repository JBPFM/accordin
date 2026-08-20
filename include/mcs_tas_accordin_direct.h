#ifndef MCS_TAS_ACCORDIN_DIRECT_H
#define MCS_TAS_ACCORDIN_DIRECT_H

#include <stddef.h>
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

/* Per-waiter wake event for a leader/follower queue.
 *
 * A waiter blocks on a reason code instead of a condition variable, so the two
 * reasons a queue wakes it for stay apart: a waiter whose work the queue head
 * already did returns without re-acquiring the mutex, and a waiter promoted to
 * head re-acquires it on the admission decision its wake carried.
 *
 * The event lives in caller-owned storage of at least
 * mcs_tas_accordin_direct_writer_event_size() bytes, aligned to
 * mcs_tas_accordin_direct_writer_event_align(); there is nothing to release.
 * Call order: bind the mutex once, then per waiter init, then arm while holding
 * the mutex, then release the mutex, then wait; a wait reporting 1 re-acquires
 * through relock.
 *
 * A post is the last access its caller may make to the event: the waiter it
 * releases may destroy the storage the instant the reason lands. */
size_t mcs_tas_accordin_direct_writer_event_size(void);

size_t mcs_tas_accordin_direct_writer_event_align(void);

typedef struct mcs_tas_accordin_direct_writer_event
    mcs_tas_accordin_direct_writer_event_t;

/* Resolves the mutex to the binding its events are initialized from. The answer
 * never changes for a given mutex and stays valid for its lifetime, so it is
 * asked once rather than per waiter. */
int mcs_tas_accordin_direct_writer_event_bind(
    mcs_tas_accordin_direct_mutex_t* mutex, void** out_binding);

int mcs_tas_accordin_direct_writer_event_init(
    mcs_tas_accordin_direct_writer_event_t* event, void* binding);

int mcs_tas_accordin_direct_writer_event_arm(
    mcs_tas_accordin_direct_writer_event_t* event);

/* Returns 0 once the work is done and the result published, 1 once the waiter
 * is the new queue head, and -1 for a null event. */
int mcs_tas_accordin_direct_writer_event_wait(
    mcs_tas_accordin_direct_writer_event_t* event);

int mcs_tas_accordin_direct_writer_event_relock(
    mcs_tas_accordin_direct_writer_event_t* event);

int mcs_tas_accordin_direct_writer_event_post_completed(
    mcs_tas_accordin_direct_writer_event_t* event);

int mcs_tas_accordin_direct_writer_event_post_leader(
    mcs_tas_accordin_direct_writer_event_t* event);

#ifdef __cplusplus
}
#endif

#endif
