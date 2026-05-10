#define _GNU_SOURCE
#include "gcr_mcs.h"

#include <errno.h>
#include <linux/futex.h>
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

static inline void approve_passive_head(gcr_mcs_mutex_t *m) {
  atomic_store_explicit(&m->top_approved, 1, memory_order_release);
  futex_wake_private(&m->top_approved, 1);
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
    atomic_store_explicit(&m->top, me, memory_order_release);
    atomic_store_explicit(&me->event, 1, memory_order_release);
    if (atomic_load_explicit(&m->num_active, memory_order_acquire) == 0) {
      approve_passive_head(m);
    }
    futex_wake_private(&me->event, 1);
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

static inline void wait_as_passive_head(gcr_mcs_mutex_t *m) {
  uint32_t spins = m->passive_spins;

  for (;;) {
    if (atomic_load_explicit(&m->top_approved, memory_order_acquire) != 0) {
      return;
    }

    for (uint32_t i = 0; i < spins; ++i) {
      if (atomic_load_explicit(&m->top_approved, memory_order_acquire) != 0) {
        return;
      }
      cpu_relax();
    }

    if (atomic_load_explicit(&m->top_approved, memory_order_acquire) == 0) {
      int rc = futex_wait_private(&m->top_approved, 0);
      if (rc == -1 && errno != EAGAIN && errno != EINTR) {
        sched_yield();
      }
    }
  }
}

/* ---------------- Public API ---------------- */

void gcr_mcs_init(gcr_mcs_mutex_t *m) {
  gcr_mcs_init_with(m, GCR_MCS_DEFAULT_ACTIVE_LIMIT,
                    GCR_MCS_DEFAULT_SIGNAL_PERIOD,
                    GCR_MCS_DEFAULT_PASSIVE_SPINS);
}

void gcr_mcs_init_with(gcr_mcs_mutex_t *m, uint32_t active_limit,
                       uint32_t signal_period, uint32_t passive_spins) {
  mcs_init(&m->inner);
  atomic_store_explicit(&m->top, NULL, memory_order_relaxed);
  atomic_store_explicit(&m->tail, NULL, memory_order_relaxed);
  atomic_store_explicit(&m->top_approved, 0, memory_order_relaxed);
  atomic_store_explicit(&m->num_active, 0, memory_order_relaxed);
  atomic_store_explicit(&m->num_acqs, 0, memory_order_relaxed);

  m->active_limit = active_limit ? active_limit : GCR_MCS_DEFAULT_ACTIVE_LIMIT;
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

    /* Consume approval if that is what released us. If we got out because
     * num_active reached zero, this exchange is harmless. */
    atomic_exchange_explicit(&m->top_approved, 0, memory_order_acq_rel);
    atomic_fetch_add_explicit(&m->num_active, 1, memory_order_acq_rel);
    gcr_pop_self(m, &my_node);
  }

  mcs_node_t *node = tls_mcs_node_acquire(m);
  mcs_lock_acquire(&m->inner, node);
}

void gcr_mcs_unlock(gcr_mcs_mutex_t *m) {
  tls_slot_t *slot = tls_mcs_node_find(m);

  uint64_t acqs =
      atomic_fetch_add_explicit(&m->num_acqs, 1, memory_order_relaxed) + 1;
  if ((acqs % m->signal_period) == 0 &&
      atomic_load_explicit(&m->top, memory_order_acquire) != NULL) {
    approve_passive_head(m);
  }

  uint32_t old =
      atomic_fetch_sub_explicit(&m->num_active, 1, memory_order_acq_rel);
  if (old == 0) {
    fprintf(stderr, "gcr_mcs: num_active underflow\n");
    abort();
  }
  if (old == 1) {
    if (atomic_load_explicit(&m->top, memory_order_acquire) != NULL) {
      approve_passive_head(m);
    }
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
