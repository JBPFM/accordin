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

#define GCR_MCS_DEFAULT_ACTIVE_LIMIT 4u
#define GCR_MCS_DEFAULT_SIGNAL_PERIOD 0x4000u
#define GCR_MCS_DEFAULT_PASSIVE_SPINS 1024u

/* Upper bound of the head's active-count sampling interval. */
#define GCR_MCS_MAX_CHECK_INTERVAL (1u << 20)

typedef struct gcr_mcs_mutex gcr_mcs_mutex_t;

/* Environment overrides consulted by gcr_mcs_init(). */
#define GCR_MCS_ENV_ACTIVE_LIMIT "GCR_MCS_ACTIVE_LIMIT"
#define GCR_MCS_ENV_SIGNAL_PERIOD "GCR_MCS_SIGNAL_PERIOD"
#define GCR_MCS_ENV_PASSIVE_SPINS "GCR_MCS_PASSIVE_SPINS"

/* Stable prefix of the one-line report emitted when the tunables resolve. */
#define GCR_MCS_CONFIG_REPORT_PREFIX "gcr_mcs: effective_config"

/*
 * Initialize with the paper defaults:
 *   active_limit  = 4      // a joining thread turns passive while more than
 *                          // four threads are active
 *   signal_period = 0x4000 // periodically admit passive head for fairness
 *   passive_spins = 1024   // spin before futex parking
 *
 * The two thresholds of the paper are coupled: the rejoin threshold is derived
 * as max(active_limit / 2, 1), and the passive-queue head rejoins the active
 * set once it observes the active count below that threshold.
 *
 * Each default may be overridden through the environment variable of the same
 * tunable.  Values are decimal, or hexadecimal with a "0x" prefix.  A value
 * that does not parse, is negative, exceeds uint32_t or is zero is reported on
 * stderr and the compile-time default is used for that tunable instead.  The
 * environment is read and parsed once per process; the resolved values, plus
 * the derived rejoin threshold, are reported on one stderr line prefixed with
 * GCR_MCS_CONFIG_REPORT_PREFIX and are readable through
 * gcr_mcs_effective_config().
 */
void gcr_mcs_init(gcr_mcs_mutex_t *m);

typedef struct gcr_mcs_config {
  uint32_t active_limit;
  uint32_t rejoin_limit;
  uint32_t signal_period;
  uint32_t passive_spins;
} gcr_mcs_config_t;

/*
 * Report the process-wide tunables that gcr_mcs_init() applies, resolving the
 * environment first if no lock has been initialized yet.
 */
void gcr_mcs_effective_config(gcr_mcs_config_t *out);

/*
 * active_limit:  passivize on lock() when num_active > active_limit.  Half of
 *                it, at least 1, is the rejoin threshold: the passive-queue
 *                head leaves its spin loop once it reads num_active below that
 *                value.
 * signal_period: every signal_period unlocks, approve the passive-queue head.
 * passive_spins: spin iterations before futex parking while waiting for the
 *                head of the passive queue.
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

  /*
   * Sampling interval of the passive-queue head over num_active, in spin
   * iterations.  It starts at 1, doubles up to GCR_MCS_MAX_CHECK_INTERVAL
   * while the head finds the active set still crowded, and is reset to 1 by
   * the head that leaves on a low active count.
   */
  _Atomic uint32_t next_check_active;

  /* Number of times a passive-queue head has sampled num_active. */
  _Atomic uint64_t head_active_polls;

  uint32_t active_limit;

  /* Active count below which the passive-queue head joins the active set. */
  uint32_t rejoin_limit;

  uint32_t signal_period;
  uint32_t passive_spins;

  char pad[64];
} __attribute__((aligned(64)));

#ifdef __cplusplus
}
#endif

#endif /* GCR_MCS_H */
