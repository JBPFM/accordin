#ifndef GCR_MCS_H
#define GCR_MCS_H

/*
 * GCR-MCS: Generic Concurrency Restriction wrapper over an MCS queue lock.
 *
 * This is a direct, pthread-independent baseline implementation intended for
 * lock microbenchmarks and patched application call sites.  It implements the
 * core GCR idea: threads are either active, in which case they may enter the
 * underlying MCS lock, or passive, in which case they wait in a per-lock FIFO
 * passive queue before becoming active.
 *
 * Linux-only because passive wait uses futex.  Compile as C11:
 *   gcc -O3 -std=gnu11 -pthread example_gcr_mcs.c gcr_mcs.c
 */

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define GCR_MCS_DEFAULT_ACTIVE_LIMIT 1u
#define GCR_MCS_DEFAULT_SIGNAL_PERIOD 0x4000u
#define GCR_MCS_DEFAULT_PASSIVE_SPINS 1024u

typedef struct gcr_mcs_mutex gcr_mcs_mutex_t;

/*
 * Initialize with the paper-like defaults:
 *   active_limit  = 1      // passivize when >1 active thread is circulating
 *   signal_period = 0x4000 // periodically admit passive head for fairness
 *   passive_spins = 1024   // spin before futex parking
 */
void gcr_mcs_init(gcr_mcs_mutex_t *m);

/*
 * active_limit:  passivize on lock() when num_active > active_limit.
 * signal_period: every signal_period unlocks, approve the passive-queue head.
 * passive_spins: spin iterations before futex parking in passive waits.
 *
 * Passing 0 for any field selects the default for that field.
 */
void gcr_mcs_init_with(gcr_mcs_mutex_t *m, uint32_t active_limit,
                       uint32_t signal_period, uint32_t passive_spins);

void gcr_mcs_destroy(gcr_mcs_mutex_t *m);
void gcr_mcs_lock(gcr_mcs_mutex_t *m);
void gcr_mcs_unlock(gcr_mcs_mutex_t *m);

/* Optional diagnostics.  These are approximate under concurrency. */
uint32_t gcr_mcs_num_active(gcr_mcs_mutex_t *m);
uint64_t gcr_mcs_num_acquires(gcr_mcs_mutex_t *m);

/* Opaque definition is exposed so locks can be embedded without allocation. */
typedef struct mcs_node mcs_node_t;
struct mcs_node {
  _Atomic(mcs_node_t *) next;
  _Atomic int locked;
  char pad[64 - sizeof(_Atomic(mcs_node_t *)) - sizeof(_Atomic int) > 0
               ? 64 - sizeof(_Atomic(mcs_node_t *)) - sizeof(_Atomic int)
               : 1];
} __attribute__((aligned(64)));

typedef struct mcs_lock {
  _Atomic(mcs_node_t *) tail;
  char pad[64 - sizeof(_Atomic(mcs_node_t *)) > 0
               ? 64 - sizeof(_Atomic(mcs_node_t *))
               : 1];
} __attribute__((aligned(64))) mcs_lock_t;

typedef struct gcr_passive_node gcr_passive_node_t;
struct gcr_passive_node {
  _Atomic(gcr_passive_node_t *) next;
  _Atomic int event;
  char
      pad[64 - sizeof(_Atomic(gcr_passive_node_t *)) - sizeof(_Atomic int) > 0
              ? 64 - sizeof(_Atomic(gcr_passive_node_t *)) - sizeof(_Atomic int)
              : 1];
} __attribute__((aligned(64)));

struct gcr_mcs_mutex {
  mcs_lock_t inner;

  /* Passive FIFO queue, MCS-like. */
  _Atomic(gcr_passive_node_t *) top;
  _Atomic(gcr_passive_node_t *) tail;

  /* Signal consumed only by the current passive-queue head. */
  _Atomic int top_approved;

  /* Number of active threads allowed to call into the underlying MCS lock. */
  _Atomic uint32_t num_active;

  /* Acquisition counter used for periodic active/passive shuffling. */
  _Atomic uint64_t num_acqs;

  uint32_t active_limit;
  uint32_t signal_period;
  uint32_t passive_spins;

  char pad[64];
} __attribute__((aligned(64)));

#ifdef __cplusplus
}
#endif

#endif /* GCR_MCS_H */
