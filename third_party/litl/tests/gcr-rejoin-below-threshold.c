#define _GNU_SOURCE
#include "gcr-support.h"

/*
 * A passive waiter must rejoin the active set as soon as the active count
 * falls below the rejoin threshold, which for the default active limit of 4
 * happens at an active count of 1.  The lock is seeded with a phantom active
 * count that never drains to zero, so the only event that can release the
 * waiter is the below-threshold drop, and the acquisition counter is seeded
 * off the periodic-approval phase so no unlock can approve the waiter either.
 */

int main(void) {
  gcr_passive_node_t placeholder_head;
  pthread_t waiter;

  gcr_mcs_init(&lock);
  atomic_store_explicit(&waiter_admitted, 0, memory_order_relaxed);

  if (lock.rejoin_limit != 2u || lock.active_limit != 4u) {
    fprintf(stderr, "expected default thresholds active=4 rejoin=2, got %u/%u\n",
            lock.active_limit, lock.rejoin_limit);
    return 1;
  }

  /*
   * Occupy the passive queue head so the waiter enqueues behind it and reaches
   * the passive wait without inspecting the active count itself.
   */
  atomic_store_explicit(&placeholder_head.next, NULL, memory_order_relaxed);
  atomic_store_explicit(&placeholder_head.event, 1, memory_order_relaxed);
  atomic_store_explicit(&lock.top, &placeholder_head, memory_order_release);
  atomic_store_explicit(&lock.tail, &placeholder_head, memory_order_release);
  atomic_store_explicit(&lock.top_approved, 0, memory_order_release);

  /* Off the periodic-approval phase: no unlock of this run may approve. */
  atomic_store_explicit(&lock.num_acqs, 1, memory_order_relaxed);

  /* More active threads than the active limit force the waiter to passivize. */
  atomic_store_explicit(&lock.num_active, 5, memory_order_release);

  if (pthread_create(&waiter, NULL, waiter_main, NULL) != 0) {
    fprintf(stderr, "failed to start waiter thread\n");
    return 1;
  }

  gcr_passive_node_t *waiter_node = NULL;
  if (wait_until((waiter_node = atomic_load_explicit(
                      &placeholder_head.next, memory_order_acquire)) != NULL,
                 "waiter did not enqueue on the passive queue\n")) {
    return 1;
  }

  /* Hand the passive queue head over to the waiter. */
  atomic_store_explicit(&lock.top, waiter_node, memory_order_release);
  atomic_store_explicit(&waiter_node->event, 1, memory_order_release);
  futex_wake_one(&waiter_node->event);

  /*
   * One active thread remains besides the one taken below, so the active count
   * bottoms out at 1 and never reaches zero.
   */
  atomic_store_explicit(&lock.num_active, 1, memory_order_release);

  gcr_mcs_lock(&lock);
  if (gcr_mcs_num_active(&lock) != 2) {
    fprintf(stderr, "expected two active threads while holding the lock\n");
    return 1;
  }
  gcr_mcs_unlock(&lock);

  if (gcr_mcs_num_active(&lock) == 0) {
    fprintf(stderr, "active count reached zero, test premise broken\n");
    return 1;
  }

  if (wait_until(
          atomic_load_explicit(&waiter_admitted, memory_order_acquire) != 0,
          "passive waiter was not admitted after the active count dropped "
          "below the rejoin threshold\n")) {
    return 1;
  }

  pthread_join(waiter, NULL);

  if (gcr_mcs_num_active(&lock) == 0) {
    fprintf(stderr, "active count reached zero, test premise broken\n");
    return 1;
  }
  if (atomic_load_explicit(&lock.top_approved, memory_order_acquire) != 0) {
    fprintf(stderr, "release path published an approval it must not publish\n");
    return 1;
  }

  return 0;
}
