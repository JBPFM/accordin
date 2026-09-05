#define _GNU_SOURCE
#include "gcr_mcs.h"

#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef GCR_MCS_TLS_SLOTS
#define GCR_MCS_TLS_SLOTS 64
#endif

static inline void cpu_relax(void) {
#if defined(__x86_64__) || defined(__i386__)
  __builtin_ia32_pause();
#else
  __asm__ __volatile__("" ::: "memory");
#endif
}

static inline int futex_wait_private(_Atomic int *addr, int expected) {
  return (int)syscall(SYS_futex, (int *)addr, FUTEX_WAIT_PRIVATE, expected,
                      NULL, NULL, 0);
}

static inline int futex_wake_private(_Atomic int *addr, int n) {
  return (int)syscall(SYS_futex, (int *)addr, FUTEX_WAKE_PRIVATE, n, NULL, NULL,
                      0);
}

/* ---------------- MCS lock ---------------- */

static inline void mcs_init(mcs_lock_t *l) {
  atomic_store_explicit(&l->tail, NULL, memory_order_relaxed);
}

static inline void mcs_lock_acquire(mcs_lock_t *l, mcs_node_t *me) {
  atomic_store_explicit(&me->next, NULL, memory_order_relaxed);
  atomic_store_explicit(&me->locked, 1, memory_order_relaxed);

  mcs_node_t *pred =
      atomic_exchange_explicit(&l->tail, me, memory_order_acq_rel);
  if (pred == NULL) {
    /* No predecessor: own the lock immediately. */
    atomic_store_explicit(&me->locked, 0, memory_order_relaxed);
    return;
  }

  atomic_store_explicit(&pred->next, me, memory_order_release);
  while (atomic_load_explicit(&me->locked, memory_order_acquire)) {
    cpu_relax();
  }
}

static inline void mcs_lock_release(mcs_lock_t *l, mcs_node_t *me) {
  mcs_node_t *succ = atomic_load_explicit(&me->next, memory_order_acquire);
  if (succ == NULL) {
    mcs_node_t *expected = me;
    if (atomic_compare_exchange_strong_explicit(&l->tail, &expected, NULL,
                                                memory_order_release,
                                                memory_order_relaxed)) {
      return;
    }
    do {
      cpu_relax();
      succ = atomic_load_explicit(&me->next, memory_order_acquire);
    } while (succ == NULL);
  }
  atomic_store_explicit(&succ->locked, 0, memory_order_release);
}

/* ---------- Thread-local MCS nodes ---------- */

typedef struct tls_slot {
  void *owner;
  int in_use;
  mcs_node_t node;
} tls_slot_t;

static _Thread_local tls_slot_t tls_slots[GCR_MCS_TLS_SLOTS];

static mcs_node_t *tls_mcs_node_acquire(gcr_mcs_mutex_t *m) {
  for (unsigned i = 0; i < GCR_MCS_TLS_SLOTS; ++i) {
    if (!tls_slots[i].in_use) {
      tls_slots[i].in_use = 1;
      tls_slots[i].owner = m;
      atomic_store_explicit(&tls_slots[i].node.next, NULL,
                            memory_order_relaxed);
      atomic_store_explicit(&tls_slots[i].node.locked, 0, memory_order_relaxed);
      return &tls_slots[i].node;
    }
  }
  fprintf(stderr,
          "gcr_mcs: TLS MCS node pool exhausted; increase GCR_MCS_TLS_SLOTS\n");
  abort();
}

static tls_slot_t *tls_mcs_node_find(gcr_mcs_mutex_t *m) {
  for (unsigned i = 0; i < GCR_MCS_TLS_SLOTS; ++i) {
    if (tls_slots[i].in_use && tls_slots[i].owner == m) {
      return &tls_slots[i];
    }
  }
  fprintf(stderr, "gcr_mcs: unlock without a matching lock in this thread\n");
  abort();
}

/* ---------- Passive GCR queue ---------- */

static inline void wait_until_nonzero(_Atomic int *word, uint32_t spins) {
  for (uint32_t i = 0; i < spins; ++i) {
    if (atomic_load_explicit(word, memory_order_acquire) != 0)
      return;
    cpu_relax();
  }

  while (atomic_load_explicit(word, memory_order_acquire) == 0) {
    int rc = futex_wait_private(word, 0);
    if (rc == -1 && errno != EAGAIN && errno != EINTR) {
      /* Futex failure should not break correctness; fall back to yielding. */
      sched_yield();
    }
  }
}

static inline gcr_passive_node_t *gcr_push_self(gcr_mcs_mutex_t *m,
                                                gcr_passive_node_t *me) {
  atomic_store_explicit(&me->next, NULL, memory_order_relaxed);
  atomic_store_explicit(&me->event, 0, memory_order_relaxed);

  gcr_passive_node_t *pred =
      atomic_exchange_explicit(&m->tail, me, memory_order_acq_rel);
  if (pred != NULL) {
    atomic_store_explicit(&pred->next, me, memory_order_release);
  } else {
    /* An empty queue makes the pusher the head right away, so it never waits
     * on its own event. */
    atomic_store_explicit(&m->top, me, memory_order_release);
    atomic_store_explicit(&me->event, 1, memory_order_release);
  }
  return me;
}

static inline void gcr_pop_self(gcr_mcs_mutex_t *m, gcr_passive_node_t *me) {
  gcr_passive_node_t *succ =
      atomic_load_explicit(&me->next, memory_order_acquire);
  if (succ == NULL) {
    gcr_passive_node_t *expected = me;
    if (atomic_compare_exchange_strong_explicit(&m->tail, &expected, NULL,
                                                memory_order_acq_rel,
                                                memory_order_relaxed)) {
      expected = me;
      atomic_compare_exchange_strong_explicit(
          &m->top, &expected, NULL, memory_order_acq_rel, memory_order_relaxed);
      return;
    }
    do {
      cpu_relax();
      succ = atomic_load_explicit(&me->next, memory_order_acquire);
    } while (succ == NULL);
  }

  atomic_store_explicit(&m->top, succ, memory_order_release);
  atomic_store_explicit(&succ->event, 1, memory_order_release);
  futex_wake_private(&succ->event, 1);
}

/*
 * The head of the passive queue spins until it is approved or until it reads
 * the active set below the rejoin threshold; it never parks, because nobody
 * signals the approval flag out of band.
 *
 * num_active is written by every lock and unlock, so reading it on every
 * iteration would drag its cache line away from the active threads.  The head
 * samples it on a deterministic back-off instead: once per iteration interval
 * that starts at 1, doubles on every sample that still finds the active set
 * crowded, and saturates at GCR_MCS_MAX_CHECK_INTERVAL.  A head that leaves on
 * a low active count resets the interval so that its successor starts by
 * monitoring closely again.
 */
static inline void wait_as_passive_head(gcr_mcs_mutex_t *m) {
  uint32_t interval =
      atomic_load_explicit(&m->next_check_active, memory_order_relaxed);
  uint64_t iterations = 0;

  if (interval == 0) {
    interval = 1u;
  }

  while (atomic_load_explicit(&m->top_approved, memory_order_acquire) == 0) {
    cpu_relax();

    if (++iterations % interval != 0) {
      continue;
    }

    atomic_fetch_add_explicit(&m->head_active_polls, 1, memory_order_relaxed);
    if (atomic_load_explicit(&m->num_active, memory_order_acquire) <
        m->rejoin_limit) {
      atomic_store_explicit(&m->next_check_active, 1, memory_order_relaxed);
      return;
    }

    if (interval < GCR_MCS_MAX_CHECK_INTERVAL) {
      interval *= 2u;
      atomic_store_explicit(&m->next_check_active, interval,
                            memory_order_relaxed);
    }
  }
}

/* ---------------- Public API ---------------- */

static inline uint32_t derive_rejoin_limit(uint32_t active_limit) {
  uint32_t rejoin = active_limit / 2u;
  return rejoin ? rejoin : 1u;
}

/*
 * Accept a decimal or "0x"-prefixed hexadecimal value in [1, UINT32_MAX].
 * Anything else keeps the default and is named on stderr so that a mistyped
 * sweep variable cannot pass for an intended configuration.
 */
static uint32_t resolve_env_u32(const char *name, uint32_t fallback) {
  const char *raw = getenv(name);
  if (raw == NULL) {
    return fallback;
  }

  const char *digits = raw;
  int base = 10;
  if (digits[0] == '0' && (digits[1] == 'x' || digits[1] == 'X')) {
    digits += 2;
    base = 16;
  }

  /* strtoull wraps a leading minus into a huge value, so refuse it up front. */
  int usable = digits[0] != '\0' && digits[0] != '-' && digits[0] != '+';

  unsigned long long value = 0;
  char *end = NULL;
  errno = 0;
  if (usable) {
    value = strtoull(digits, &end, base);
  }

  if (!usable || end == digits || *end != '\0' || errno == ERANGE ||
      value == 0ull || value > (unsigned long long)UINT32_MAX) {
    fprintf(stderr, "gcr_mcs: ignoring %s=\"%s\", using default %u\n", name, raw,
            fallback);
    return fallback;
  }
  return (uint32_t)value;
}

static gcr_mcs_config_t resolved_config;
static pthread_once_t resolve_once = PTHREAD_ONCE_INIT;

static void resolve_config(void) {
  resolved_config.active_limit =
      resolve_env_u32(GCR_MCS_ENV_ACTIVE_LIMIT, GCR_MCS_DEFAULT_ACTIVE_LIMIT);
  resolved_config.rejoin_limit =
      derive_rejoin_limit(resolved_config.active_limit);
  resolved_config.signal_period =
      resolve_env_u32(GCR_MCS_ENV_SIGNAL_PERIOD, GCR_MCS_DEFAULT_SIGNAL_PERIOD);
  resolved_config.passive_spins =
      resolve_env_u32(GCR_MCS_ENV_PASSIVE_SPINS, GCR_MCS_DEFAULT_PASSIVE_SPINS);

  fprintf(stderr,
          GCR_MCS_CONFIG_REPORT_PREFIX
          " active_limit=%u rejoin_limit=%u signal_period=%u (0x%x) "
          "passive_spins=%u\n",
          resolved_config.active_limit, resolved_config.rejoin_limit,
          resolved_config.signal_period, resolved_config.signal_period,
          resolved_config.passive_spins);
}

void gcr_mcs_effective_config(gcr_mcs_config_t *out) {
  pthread_once(&resolve_once, resolve_config);
  *out = resolved_config;
}

void gcr_mcs_init(gcr_mcs_mutex_t *m) {
  gcr_mcs_config_t config;
  gcr_mcs_effective_config(&config);
  gcr_mcs_init_with(m, config.active_limit, config.signal_period,
                    config.passive_spins);
}

void gcr_mcs_init_with(gcr_mcs_mutex_t *m, uint32_t active_limit,
                       uint32_t signal_period, uint32_t passive_spins) {
  mcs_init(&m->inner);
  atomic_store_explicit(&m->top, NULL, memory_order_relaxed);
  atomic_store_explicit(&m->tail, NULL, memory_order_relaxed);
  atomic_store_explicit(&m->top_approved, 0, memory_order_relaxed);
  atomic_store_explicit(&m->num_active, 0, memory_order_relaxed);
  atomic_store_explicit(&m->num_acqs, 0, memory_order_relaxed);
  atomic_store_explicit(&m->next_check_active, 1, memory_order_relaxed);
  atomic_store_explicit(&m->head_active_polls, 0, memory_order_relaxed);

  m->active_limit = active_limit ? active_limit : GCR_MCS_DEFAULT_ACTIVE_LIMIT;
  m->rejoin_limit = derive_rejoin_limit(m->active_limit);
  m->signal_period =
      signal_period ? signal_period : GCR_MCS_DEFAULT_SIGNAL_PERIOD;
  m->passive_spins =
      passive_spins ? passive_spins : GCR_MCS_DEFAULT_PASSIVE_SPINS;
}

void gcr_mcs_destroy(gcr_mcs_mutex_t *m) { (void)m; }

void gcr_mcs_lock(gcr_mcs_mutex_t *m) {
  uint32_t active = atomic_load_explicit(&m->num_active, memory_order_acquire);

  if (active <= m->active_limit) {
    atomic_fetch_add_explicit(&m->num_active, 1, memory_order_acq_rel);
  } else {
    gcr_passive_node_t my_node __attribute__((aligned(64)));
    gcr_push_self(m, &my_node);

    if (atomic_load_explicit(&my_node.event, memory_order_acquire) == 0) {
      wait_until_nonzero(&my_node.event, m->passive_spins);
    }

    wait_as_passive_head(m);

    /* Consume the approval when one is pending; a head that left on a low
     * active count leaves it for its successor. */
    if (atomic_load_explicit(&m->top_approved, memory_order_acquire) != 0) {
      atomic_store_explicit(&m->top_approved, 0, memory_order_release);
    }
    atomic_fetch_add_explicit(&m->num_active, 1, memory_order_acq_rel);
    gcr_pop_self(m, &my_node);
  }

  mcs_node_t *node = tls_mcs_node_acquire(m);
  mcs_lock_acquire(&m->inner, node);
}

void gcr_mcs_unlock(gcr_mcs_mutex_t *m) {
  tls_slot_t *slot = tls_mcs_node_find(m);

  /* Periodic approval keeps the passive queue moving even while the active set
   * stays crowded.  It is a plain store: the head spins on it, so no waiter
   * has to be woken and the release path stays free of system calls. */
  uint64_t acqs =
      atomic_fetch_add_explicit(&m->num_acqs, 1, memory_order_relaxed);
  if ((acqs % m->signal_period) == 0 &&
      atomic_load_explicit(&m->top, memory_order_acquire) != NULL) {
    atomic_store_explicit(&m->top_approved, 1, memory_order_release);
  }

  uint32_t old =
      atomic_fetch_sub_explicit(&m->num_active, 1, memory_order_acq_rel);
  if (old == 0) {
    fprintf(stderr, "gcr_mcs: num_active underflow\n");
    abort();
  }

  mcs_lock_release(&m->inner, &slot->node);
  slot->owner = NULL;
  slot->in_use = 0;
}

uint32_t gcr_mcs_num_active(gcr_mcs_mutex_t *m) {
  return atomic_load_explicit(&m->num_active, memory_order_relaxed);
}

uint64_t gcr_mcs_num_acquires(gcr_mcs_mutex_t *m) {
  return atomic_load_explicit(&m->num_acqs, memory_order_relaxed);
}
