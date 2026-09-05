/* SPDX-License-Identifier: GPL-2.0-only */
#include "raw_lock.h"

struct raw_lock {
    _Alignas(RAW_LOCK_ALIGN) _Atomic(struct node *) tail;
};

/* A node stays live until unlock; MCS_POOL_SIZE MCS locks held at once per thread. */
static _Thread_local struct {
    struct node nodes[MCS_POOL_SIZE];
    struct raw_lock *owners[MCS_POOL_SIZE];
} pool;

static inline struct node *node_acquire(struct raw_lock *lock, bool waiting)
{
    for (unsigned int i = 0; i < MCS_POOL_SIZE; i++) {
        if (!pool.owners[i]) {
            pool.owners[i] = lock;
            atomic_store_explicit(&pool.nodes[i].next, NULL, memory_order_relaxed);
            atomic_store_explicit(&pool.nodes[i].waiting, waiting, memory_order_relaxed);
            return &pool.nodes[i];
        }
    }
    abort();
}

static inline void node_release(struct node *node)
{
    pool.owners[node - pool.nodes] = NULL;
}

RAW_FN bool raw_trylock(struct raw_lock *lock)
{
    struct node *node = node_acquire(lock, false);
    struct node *expected = NULL;
    if (atomic_compare_exchange_strong_explicit(&lock->tail, &expected, node,
                                                memory_order_acquire, memory_order_relaxed))
        return true;
    node_release(node);
    return false;
}

RAW_FN void raw_lock(struct raw_lock *lock)
{
    struct node *node = node_acquire(lock, true);
    struct node *prev = atomic_exchange_explicit(&lock->tail, node, memory_order_acq_rel);
    if (prev) {
        atomic_store_explicit(&prev->next, node, memory_order_release);
        while (atomic_load_explicit(&node->waiting, memory_order_acquire))
            spin_pause();
    }
}

RAW_FN void raw_unlock(struct raw_lock *lock)
{
    for (unsigned int i = 0; i < MCS_POOL_SIZE; i++) {
        if (pool.owners[i] == lock) {
            queue_release(&lock->tail, &pool.nodes[i]);
            node_release(&pool.nodes[i]);
            return;
        }
    }
    abort();
}
