#define _GNU_SOURCE
#include "gcr-support.h"

/*
 * The head of the passive queue samples the active count on a doubling
 * interval rather than on every spin iteration.  The interval must climb to
 * GCR_MCS_MAX_CHECK_INTERVAL and stop there, the sample rate over an
 * observation window must stay far below the iteration rate, and a head that
 * leaves because the active count is low must reset the interval to 1.
 */

#define OBSERVATION_WINDOW_SECONDS 0.02
#define MAX_POLLS_PER_WINDOW 256u

/*
 * Publish the current sampling interval and report whether the climb is over,
 * either because the interval reached the cap or because it left the powers of
 * two and the caller has to fail on it.
 */
static int interval_stopped_climbing(uint32_t *interval) {
  *interval =
      atomic_load_explicit(&lock.next_check_active, memory_order_relaxed);
  return *interval >= GCR_MCS_MAX_CHECK_INTERVAL || *interval == 0 ||
         (*interval & (*interval - 1u)) != 0;
}

int main(void) {
  pthread_t waiter;

  gcr_mcs_init(&lock);
  atomic_store_explicit(&waiter_admitted, 0, memory_order_relaxed);

  if (atomic_load_explicit(&lock.next_check_active, memory_order_relaxed) !=
      1u) {
    fprintf(stderr, "a fresh lock must start sampling on every iteration\n");
    return 1;
  }

  /* Off the periodic-approval phase, so the head keeps spinning. */
  atomic_store_explicit(&lock.num_acqs, 1, memory_order_relaxed);
  atomic_store_explicit(&lock.num_active, 5, memory_order_release);

  if (pthread_create(&waiter, NULL, waiter_main, NULL) != 0) {
    fprintf(stderr, "failed to start waiter thread\n");
    return 1;
  }

  uint32_t interval = 0;
  if (wait_until(interval_stopped_climbing(&interval),
                 "sampling interval stalled at %u\n", interval)) {
    return 1;
  }

  if (interval == 0 || (interval & (interval - 1u)) != 0) {
    fprintf(stderr, "sampling interval %u is not a power of two\n", interval);
    return 1;
  }
  if (interval != GCR_MCS_MAX_CHECK_INTERVAL) {
    fprintf(stderr, "sampling interval %u exceeds the cap %u\n", interval,
            GCR_MCS_MAX_CHECK_INTERVAL);
    return 1;
  }

  uint64_t polls_before =
      atomic_load_explicit(&lock.head_active_polls, memory_order_relaxed);
  double window_end = monotonic_seconds() + OBSERVATION_WINDOW_SECONDS;
  while (monotonic_seconds() < window_end) {
    sleep_briefly();
  }
  uint64_t polls_after =
      atomic_load_explicit(&lock.head_active_polls, memory_order_relaxed);

  if (polls_after - polls_before > MAX_POLLS_PER_WINDOW) {
    fprintf(stderr,
            "head sampled the active count %llu times in %.3fs, which is not a "
            "backed-off rate\n",
            (unsigned long long)(polls_after - polls_before),
            OBSERVATION_WINDOW_SECONDS);
    return 1;
  }

  /* Drain the active set: the head leaves and resets the interval. */
  atomic_store_explicit(&lock.num_active, 0, memory_order_release);

  if (wait_until(
          atomic_load_explicit(&waiter_admitted, memory_order_acquire) != 0,
          "passive head was not released after the active set drained\n")) {
    return 1;
  }

  pthread_join(waiter, NULL);

  if (atomic_load_explicit(&lock.next_check_active, memory_order_relaxed) !=
      1u) {
    fprintf(stderr,
            "sampling interval was not reset after a low active count, got %u\n",
            atomic_load_explicit(&lock.next_check_active, memory_order_relaxed));
    return 1;
  }
  if (atomic_load_explicit(&lock.head_active_polls, memory_order_relaxed) <=
      polls_after) {
    fprintf(stderr, "head left without sampling the active count\n");
    return 1;
  }

  return 0;
}
