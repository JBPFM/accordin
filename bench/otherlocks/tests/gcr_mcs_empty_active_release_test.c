#define _GNU_SOURCE
#include "../gcr_mcs.h"

#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <time.h>

/*
 * The head of the passive queue must join the active set on its own once the
 * active set drains.  Nothing in the release path signals it: the acquisition
 * counter is seeded off the periodic-approval phase, so an approval published
 * by an unlock would show up as a nonzero top_approved and fail the test.
 */

#define TEST_TIMEOUT_SECONDS 10

static gcr_mcs_mutex_t lock;
static atomic_int waiter_admitted;

static double monotonic_seconds(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

static void sleep_briefly(void) {
  struct timespec ts = {0, 200000};
  nanosleep(&ts, NULL);
}

static void *waiter_main(void *arg) {
  (void)arg;
  gcr_mcs_lock(&lock);
  atomic_store_explicit(&waiter_admitted, 1, memory_order_release);
  gcr_mcs_unlock(&lock);
  return NULL;
}

int main(void) {
  pthread_t waiter;

  gcr_mcs_init(&lock);
  atomic_store_explicit(&waiter_admitted, 0, memory_order_relaxed);

  /* Off the periodic-approval phase: no unlock of this run may approve. */
  atomic_store_explicit(&lock.num_acqs, 1, memory_order_relaxed);

  /* More active threads than the active limit force the waiter to passivize. */
  atomic_store_explicit(&lock.num_active, 5, memory_order_release);

  if (pthread_create(&waiter, NULL, waiter_main, NULL) != 0) {
    fprintf(stderr, "failed to start waiter thread\n");
    return 1;
  }

  double deadline = monotonic_seconds() + TEST_TIMEOUT_SECONDS;
  while (atomic_load_explicit(&lock.top, memory_order_acquire) == NULL) {
    if (monotonic_seconds() > deadline) {
      fprintf(stderr, "waiter did not reach the head of the passive queue\n");
      return 1;
    }
    sleep_briefly();
  }

  if (atomic_load_explicit(&lock.top_approved, memory_order_acquire) != 0) {
    fprintf(stderr, "passive head was approved before the active set drained\n");
    return 1;
  }

  /* Drain the active set. */
  atomic_store_explicit(&lock.num_active, 0, memory_order_release);

  deadline = monotonic_seconds() + TEST_TIMEOUT_SECONDS;
  while (atomic_load_explicit(&waiter_admitted, memory_order_acquire) == 0) {
    if (monotonic_seconds() > deadline) {
      fprintf(stderr,
              "passive head was not released after the active set drained\n");
      return 1;
    }
    sleep_briefly();
  }

  pthread_join(waiter, NULL);

  if (atomic_load_explicit(&lock.top_approved, memory_order_acquire) != 0) {
    fprintf(stderr, "release path published an approval it must not publish\n");
    return 1;
  }
  if (gcr_mcs_num_active(&lock) != 0) {
    fprintf(stderr, "expected no active holders once the waiter is done\n");
    return 1;
  }
  if (atomic_load_explicit(&lock.top, memory_order_acquire) != NULL) {
    fprintf(stderr, "expected an empty passive queue once the waiter is done\n");
    return 1;
  }

  return 0;
}
