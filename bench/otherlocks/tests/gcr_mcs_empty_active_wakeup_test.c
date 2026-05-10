#include "../gcr_mcs.h"

#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>

int main(void) {
  gcr_mcs_mutex_t lock;
  gcr_passive_node_t passive_head;

  gcr_mcs_init(&lock);
  gcr_mcs_lock(&lock);

  atomic_store_explicit(&passive_head.next, NULL, memory_order_relaxed);
  atomic_store_explicit(&passive_head.event, 1, memory_order_relaxed);
  atomic_store_explicit(&lock.top, &passive_head, memory_order_release);
  atomic_store_explicit(&lock.tail, &passive_head, memory_order_release);
  atomic_store_explicit(&lock.top_approved, 0, memory_order_release);

  if (gcr_mcs_num_active(&lock) != 1) {
    fprintf(stderr, "expected one active holder before unlock\n");
    return 1;
  }

  gcr_mcs_unlock(&lock);

  if (gcr_mcs_num_active(&lock) != 0) {
    fprintf(stderr, "expected no active holders after unlock\n");
    return 1;
  }
  if (atomic_load_explicit(&lock.top_approved, memory_order_acquire) == 0) {
    fprintf(stderr,
            "expected unlock of last active holder to publish a nonzero "
            "passive-head wake token\n");
    return 1;
  }

  return 0;
}
