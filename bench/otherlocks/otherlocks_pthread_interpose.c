#define _GNU_SOURCE

#include <errno.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#include <linux/futex.h>

#if defined(OTHERLOCK_KIND_GCR)
#include "gcr_mcs.h"
#elif defined(OTHERLOCK_KIND_CNA)
#include <vsync/spinlock/cnalock.h>
#else
#error "Define one OTHERLOCK_KIND_* variant"
#endif

#define INIT_EMPTY 0u
#define INIT_BUSY 1u
#define INIT_READY 2u
#define TLS_LOCK_SLOTS 128u
#define CPU_NODE_CACHE_SIZE 4096
#define MAX_NUMA_NODE_SCAN 256

typedef struct hook_mutex_state {
  atomic_uchar status;
  void *lock;
} hook_mutex_state_t;

typedef struct hook_cond_state {
  atomic_uchar status;
  void *cond;
} hook_cond_state_t;

typedef struct hook_seq_cond {
  atomic_uint seq;
  atomic_uint target;
} hook_seq_cond_t;

#if defined(OTHERLOCK_KIND_GCR)
typedef gcr_mcs_mutex_t hook_lock_t;
typedef struct tls_lock_slot {
  void *owner;
} tls_lock_slot_t;
#else
typedef cnalock_t hook_lock_t;
typedef struct tls_lock_slot {
  void *owner;
  cna_node_t node;
} tls_lock_slot_t;
#endif

_Static_assert(sizeof(pthread_mutex_t) >= sizeof(hook_mutex_state_t),
               "pthread_mutex_t is too small for otherlocks hook state");
_Static_assert(sizeof(pthread_cond_t) >= sizeof(hook_cond_state_t),
               "pthread_cond_t is too small for otherlocks cond state");

static _Thread_local tls_lock_slot_t tls_slots[TLS_LOCK_SLOTS];
#if defined(OTHERLOCK_KIND_CNA)
static atomic_int cpu_node_cache[CPU_NODE_CACHE_SIZE];
#endif

static int (*real_pthread_mutex_init_fn)(pthread_mutex_t *,
                                         const pthread_mutexattr_t *);
static int (*real_pthread_mutex_destroy_fn)(pthread_mutex_t *);
static int (*real_pthread_mutex_lock_fn)(pthread_mutex_t *);
static int (*real_pthread_mutex_trylock_fn)(pthread_mutex_t *);
static int (*real_pthread_mutex_unlock_fn)(pthread_mutex_t *);

static inline void cpu_relax_hook(void) {
#if defined(__x86_64__) || defined(__i386__)
  __builtin_ia32_pause();
#else
  __asm__ __volatile__("" ::: "memory");
#endif
}

static void load_real_pthread_symbols(void) {
  static atomic_uchar done;
  unsigned char expected = INIT_EMPTY;
  if (!atomic_compare_exchange_strong_explicit(
          &done, &expected, INIT_BUSY, memory_order_acq_rel,
          memory_order_acquire)) {
    while (atomic_load_explicit(&done, memory_order_acquire) == INIT_BUSY) {
      cpu_relax_hook();
    }
    return;
  }

  real_pthread_mutex_init_fn = dlsym(RTLD_NEXT, "pthread_mutex_init");
  real_pthread_mutex_destroy_fn = dlsym(RTLD_NEXT, "pthread_mutex_destroy");
  real_pthread_mutex_lock_fn = dlsym(RTLD_NEXT, "pthread_mutex_lock");
  real_pthread_mutex_trylock_fn = dlsym(RTLD_NEXT, "pthread_mutex_trylock");
  real_pthread_mutex_unlock_fn = dlsym(RTLD_NEXT, "pthread_mutex_unlock");

  atomic_store_explicit(&done, INIT_READY, memory_order_release);
}

static int futex_wait_private(atomic_uint *addr, unsigned expected,
                              const struct timespec *timeout) {
  return (int)syscall(SYS_futex, (int *)addr, FUTEX_WAIT_PRIVATE,
                      (int)expected, timeout, NULL, 0);
}

static int futex_wake_private(atomic_uint *addr, int n) {
  return (int)syscall(SYS_futex, (int *)addr, FUTEX_WAKE_PRIVATE, n, NULL, NULL,
                      0);
}

static hook_mutex_state_t *mutex_state(pthread_mutex_t *mutex) {
  return (hook_mutex_state_t *)mutex;
}

static hook_cond_state_t *cond_state(pthread_cond_t *cond) {
  return (hook_cond_state_t *)cond;
}

static int initialize_once(atomic_uchar *status) {
  unsigned char state = atomic_load_explicit(status, memory_order_acquire);
  if (state == INIT_READY) {
    return 0;
  }
  unsigned char expected = INIT_EMPTY;
  if (atomic_compare_exchange_strong_explicit(
          status, &expected, INIT_BUSY, memory_order_acq_rel,
          memory_order_acquire)) {
    return 1;
  }
  while (atomic_load_explicit(status, memory_order_acquire) == INIT_BUSY) {
    cpu_relax_hook();
  }
  return 0;
}

static void hook_lock_init(hook_lock_t *lock) {
#if defined(OTHERLOCK_KIND_GCR)
  gcr_mcs_init(lock);
#else
  cnalock_init(lock);
#endif
}

static void hook_lock_destroy(hook_lock_t *lock) {
#if defined(OTHERLOCK_KIND_GCR)
  gcr_mcs_destroy(lock);
#else
  (void)lock;
#endif
}

#if defined(OTHERLOCK_KIND_CNA)
static unsigned current_numa_node(void) {
  int cpu = sched_getcpu();
  if (cpu < 0 || cpu >= CPU_NODE_CACHE_SIZE) {
    return 0;
  }
  int cached = atomic_load_explicit(&cpu_node_cache[cpu], memory_order_acquire);
  if (cached > 0) {
    return (unsigned)(cached - 1);
  }

  char path[128];
  for (int node = 0; node < MAX_NUMA_NODE_SCAN; ++node) {
    snprintf(path, sizeof(path), "/sys/devices/system/node/node%d/cpu%d", node,
             cpu);
    if (access(path, F_OK) == 0) {
      atomic_store_explicit(&cpu_node_cache[cpu], node + 1,
                            memory_order_release);
      return (unsigned)node;
    }
  }
  atomic_store_explicit(&cpu_node_cache[cpu], 1, memory_order_release);
  return 0;
}
#endif

static tls_lock_slot_t *tls_slot_acquire(hook_lock_t *lock) {
  for (unsigned i = 0; i < TLS_LOCK_SLOTS; ++i) {
    if (tls_slots[i].owner == NULL) {
      tls_slots[i].owner = lock;
      return &tls_slots[i];
    }
  }
  fprintf(stderr, "otherlocks hook: TLS lock slot pool exhausted\n");
  abort();
}

static tls_lock_slot_t *tls_slot_find(hook_lock_t *lock) {
  for (unsigned i = 0; i < TLS_LOCK_SLOTS; ++i) {
    if (tls_slots[i].owner == lock) {
      return &tls_slots[i];
    }
  }
  fprintf(stderr, "otherlocks hook: unlock without matching lock\n");
  abort();
}

static void hook_lock_acquire(hook_lock_t *lock) {
#if defined(OTHERLOCK_KIND_GCR)
  (void)tls_slot_acquire(lock);
  gcr_mcs_lock(lock);
#else
  tls_lock_slot_t *slot = tls_slot_acquire(lock);
  cnalock_acquire(lock, &slot->node, current_numa_node());
#endif
}

static void hook_lock_release(hook_lock_t *lock) {
  tls_lock_slot_t *slot = tls_slot_find(lock);
#if defined(OTHERLOCK_KIND_GCR)
  gcr_mcs_unlock(lock);
#else
  cnalock_release(lock, &slot->node, current_numa_node());
#endif
  slot->owner = NULL;
}

static int interpose_mutex_init(pthread_mutex_t *mutex, bool force) {
  hook_mutex_state_t *state = mutex_state(mutex);
  if (force) {
    atomic_store_explicit(&state->status, INIT_EMPTY, memory_order_release);
    state->lock = NULL;
  }
  if (!initialize_once(&state->status)) {
    return 0;
  }

  hook_lock_t *lock = calloc(1, sizeof(*lock));
  if (lock == NULL) {
    atomic_store_explicit(&state->status, INIT_EMPTY, memory_order_release);
    return ENOMEM;
  }
  hook_lock_init(lock);
  state->lock = lock;
  atomic_store_explicit(&state->status, INIT_READY, memory_order_release);
  return 0;
}

static hook_lock_t *interpose_mutex_lock_ptr(pthread_mutex_t *mutex) {
  hook_mutex_state_t *state = mutex_state(mutex);
  if (atomic_load_explicit(&state->status, memory_order_acquire) != INIT_READY) {
    int rc = interpose_mutex_init(mutex, false);
    if (rc != 0) {
      errno = rc;
      return NULL;
    }
  }
  return (hook_lock_t *)state->lock;
}

static int interpose_mutex_destroy(pthread_mutex_t *mutex) {
  hook_mutex_state_t *state = mutex_state(mutex);
  if (atomic_load_explicit(&state->status, memory_order_acquire) == INIT_READY) {
    hook_lock_t *lock = (hook_lock_t *)state->lock;
    hook_lock_destroy(lock);
    free(lock);
    state->lock = NULL;
    atomic_store_explicit(&state->status, INIT_EMPTY, memory_order_release);
  }
  return 0;
}

static int interpose_cond_init(pthread_cond_t *cond, bool force) {
  hook_cond_state_t *state = cond_state(cond);
  if (force) {
    atomic_store_explicit(&state->status, INIT_EMPTY, memory_order_release);
    state->cond = NULL;
  }
  if (!initialize_once(&state->status)) {
    return 0;
  }
  hook_seq_cond_t *seq_cond = calloc(1, sizeof(*seq_cond));
  if (seq_cond == NULL) {
    atomic_store_explicit(&state->status, INIT_EMPTY, memory_order_release);
    return ENOMEM;
  }
  state->cond = seq_cond;
  atomic_store_explicit(&state->status, INIT_READY, memory_order_release);
  return 0;
}

static hook_seq_cond_t *interpose_cond_ptr(pthread_cond_t *cond) {
  hook_cond_state_t *state = cond_state(cond);
  if (atomic_load_explicit(&state->status, memory_order_acquire) != INIT_READY) {
    int rc = interpose_cond_init(cond, false);
    if (rc != 0) {
      errno = rc;
      return NULL;
    }
  }
  return (hook_seq_cond_t *)state->cond;
}

static int interpose_cond_destroy(pthread_cond_t *cond) {
  hook_cond_state_t *state = cond_state(cond);
  if (atomic_load_explicit(&state->status, memory_order_acquire) == INIT_READY) {
    free(state->cond);
    state->cond = NULL;
    atomic_store_explicit(&state->status, INIT_EMPTY, memory_order_release);
  }
  return 0;
}

static int relative_timeout_from_absolute(const struct timespec *abstime,
                                          struct timespec *timeout) {
  struct timespec now;
  if (clock_gettime(CLOCK_REALTIME, &now) != 0) {
    return errno;
  }
  timeout->tv_sec = abstime->tv_sec - now.tv_sec;
  timeout->tv_nsec = abstime->tv_nsec - now.tv_nsec;
  if (timeout->tv_nsec < 0) {
    timeout->tv_nsec += 1000000000L;
    timeout->tv_sec--;
  }
  if (timeout->tv_sec < 0) {
    return ETIMEDOUT;
  }
  return 0;
}

static void cancel_cond_wait(hook_seq_cond_t *cond, unsigned target) {
  for (;;) {
    unsigned seq = atomic_load_explicit(&cond->seq, memory_order_acquire);
    if (seq >= target) {
      return;
    }
    if (atomic_compare_exchange_weak_explicit(
            &cond->seq, &seq, seq + 1, memory_order_acq_rel,
            memory_order_acquire)) {
      futex_wake_private(&cond->seq, INT_MAX);
      return;
    }
  }
}

static int interpose_cond_wait_common(pthread_cond_t *cond,
                                      pthread_mutex_t *mutex,
                                      const struct timespec *abstime) {
  hook_seq_cond_t *seq_cond = interpose_cond_ptr(cond);
  hook_lock_t *lock = interpose_mutex_lock_ptr(mutex);
  if (seq_cond == NULL || lock == NULL) {
    return errno ? errno : EINVAL;
  }

  unsigned target =
      atomic_fetch_add_explicit(&seq_cond->target, 1, memory_order_acq_rel) + 1;
  unsigned seq = atomic_load_explicit(&seq_cond->seq, memory_order_acquire);
  int rc = 0;
  bool timed_out = false;

  hook_lock_release(lock);
  while (target > seq) {
    if (abstime == NULL) {
      futex_wait_private(&seq_cond->seq, seq, NULL);
    } else {
      struct timespec timeout;
      rc = relative_timeout_from_absolute(abstime, &timeout);
      if (rc != 0) {
        timed_out = true;
        break;
      }
      if (futex_wait_private(&seq_cond->seq, seq, &timeout) != 0 &&
          errno == ETIMEDOUT) {
        rc = ETIMEDOUT;
        timed_out = true;
        break;
      }
    }
    seq = atomic_load_explicit(&seq_cond->seq, memory_order_acquire);
  }
  if (timed_out) {
    cancel_cond_wait(seq_cond, target);
  }
  hook_lock_acquire(lock);
  return rc;
}

__attribute__((constructor)) static void otherlocks_interpose_init(void) {
  load_real_pthread_symbols();
}

int pthread_mutex_init(pthread_mutex_t *mutex,
                       const pthread_mutexattr_t *attr) {
  (void)attr;
  return interpose_mutex_init(mutex, true);
}

int pthread_mutex_destroy(pthread_mutex_t *mutex) {
  return interpose_mutex_destroy(mutex);
}

int pthread_mutex_lock(pthread_mutex_t *mutex) {
  hook_lock_t *lock = interpose_mutex_lock_ptr(mutex);
  if (lock == NULL) {
    return errno ? errno : EINVAL;
  }
  hook_lock_acquire(lock);
  return 0;
}

int pthread_mutex_trylock(pthread_mutex_t *mutex) {
  (void)mutex;
  return EBUSY;
}

int pthread_mutex_timedlock(pthread_mutex_t *mutex,
                            const struct timespec *abstime) {
  (void)abstime;
  return pthread_mutex_lock(mutex);
}

int pthread_mutex_unlock(pthread_mutex_t *mutex) {
  hook_lock_t *lock = interpose_mutex_lock_ptr(mutex);
  if (lock == NULL) {
    return errno ? errno : EINVAL;
  }
  hook_lock_release(lock);
  return 0;
}

int pthread_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr) {
  (void)attr;
  return interpose_cond_init(cond, true);
}

int pthread_cond_destroy(pthread_cond_t *cond) {
  return interpose_cond_destroy(cond);
}

int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex) {
  return interpose_cond_wait_common(cond, mutex, NULL);
}

int pthread_cond_timedwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                           const struct timespec *abstime) {
  return interpose_cond_wait_common(cond, mutex, abstime);
}

int pthread_cond_signal(pthread_cond_t *cond) {
  hook_seq_cond_t *seq_cond = interpose_cond_ptr(cond);
  if (seq_cond == NULL) {
    return errno ? errno : EINVAL;
  }
  atomic_fetch_add_explicit(&seq_cond->seq, 1, memory_order_acq_rel);
  futex_wake_private(&seq_cond->seq, 1);
  return 0;
}

int pthread_cond_broadcast(pthread_cond_t *cond) {
  hook_seq_cond_t *seq_cond = interpose_cond_ptr(cond);
  if (seq_cond == NULL) {
    return errno ? errno : EINVAL;
  }
  unsigned target = atomic_load_explicit(&seq_cond->target, memory_order_acquire);
  atomic_store_explicit(&seq_cond->seq, target, memory_order_release);
  futex_wake_private(&seq_cond->seq, INT_MAX);
  return 0;
}
