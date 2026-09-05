/* SPDX-License-Identifier: GPL-2.0-only */
#include "raw_lock.h"

struct raw_lock {
    _Alignas(RAW_LOCK_ALIGN) _Atomic(struct node *) tail;
    _Alignas(RAW_LOCK_ALIGN) _Atomic bool locked;
};

static _Thread_local struct node thread_node;

RAW_FN bool raw_trylock(struct raw_lock *lock)
{
    bool expected = false;
    return atomic_compare_exchange_strong_explicit(&lock->locked, &expected, true,
                                                   memory_order_acquire, memory_order_relaxed);
}

RAW_FN void raw_lock(struct raw_lock *lock)
{
    struct node *node = &thread_node;
    atomic_store_explicit(&node->next, NULL, memory_order_relaxed);
    atomic_store_explicit(&node->waiting, false, memory_order_relaxed);
    struct node *prev = atomic_exchange_explicit(&lock->tail, node, memory_order_acq_rel);
    if (prev) {
        atomic_store_explicit(&node->waiting, true, memory_order_relaxed);
        atomic_store_explicit(&prev->next, node, memory_order_release);
        while (atomic_load_explicit(&node->waiting, memory_order_acquire))
            spin_pause();
    }
    /* Test before test-and-set: a queued waiter reads the line shared until
     * the holder releases it. */
    while (atomic_load_explicit(&lock->locked, memory_order_relaxed) ||
           atomic_exchange_explicit(&lock->locked, true, memory_order_acquire))
        spin_pause();
    /* This node is reusable as soon as acquisition finishes, before unlock. */
    queue_release(&lock->tail, node);
}

RAW_FN void raw_unlock(struct raw_lock *lock)
{
    atomic_store_explicit(&lock->locked, false, memory_order_release);
}
