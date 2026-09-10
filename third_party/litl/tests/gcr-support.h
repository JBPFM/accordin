#ifndef GCR_SUPPORT_H
#define GCR_SUPPORT_H

#include <gcrmcs.h>

#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/*
 * Scaffold shared by the direct GCR tests: the lock under test, the waiter
 * thread that reports its own admission, and the bounded poll every test uses
 * to observe a state change without hanging when the change never comes.
 */

#define TEST_TIMEOUT_SECONDS 10

static gcr_mcs_mutex_t lock;
static atomic_int waiter_admitted;

static inline double monotonic_seconds(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

static inline void sleep_briefly(void) {
  struct timespec ts = {0, 200000};
  nanosleep(&ts, NULL);
}

static inline void futex_wake_one(_Atomic int *addr) {
  syscall(SYS_futex, (int *)addr, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
}

/* Acquire the lock, record the admission, and release it again. */
static inline void *waiter_main(void *arg) {
  (void)arg;
  gcr_mcs_lock(&lock);
  atomic_store_explicit(&waiter_admitted, 1, memory_order_release);
  gcr_mcs_unlock(&lock);
  return NULL;
}

/*
 * Poll `condition` until it holds and evaluate to zero, or evaluate to one
 * after TEST_TIMEOUT_SECONDS and print the remaining arguments as an fprintf
 * message on stderr.  `condition` is re-evaluated on every poll, so it may
 * hand the value it observed back to the caller.
 */
#define wait_until(condition, ...)                                             \
  ({                                                                           \
    double wait_deadline_ = monotonic_seconds() + TEST_TIMEOUT_SECONDS;        \
    int wait_timed_out_ = 0;                                                   \
    while (!(condition)) {                                                     \
      if (monotonic_seconds() > wait_deadline_) {                              \
        fprintf(stderr, __VA_ARGS__);                                          \
        wait_timed_out_ = 1;                                                   \
        break;                                                                 \
      }                                                                        \
      sleep_briefly();                                                         \
    }                                                                          \
    wait_timed_out_;                                                           \
  })

#endif /* GCR_SUPPORT_H */
