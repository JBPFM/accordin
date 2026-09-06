/* SPDX-License-Identifier: GPL-2.0-only */
#include "raw_lock.h"

struct raw_lock {
    _Alignas(64) _Atomic(struct node *) tail;
};

/* A node stays live until unlock; four simultaneously held MCS locks per thread. */
static _Thread_local struct {
    struct node nodes[4];
    struct raw_lock *owners[4];
} pool;

static inline struct node *node_acquire(struct raw_lock *lock, bool waiting)
{
    for (unsigned int i = 0; i < 4; i++) {
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
    /* Publish next = NULL before a successor can link itself to this node.
     * Acquire alone allows that initialization to overwrite the successor's
     * link on weakly ordered CPUs, stranding the owner in queue_release. */
    if (atomic_compare_exchange_strong_explicit(&lock->tail, &expected, node,
                                                memory_order_acq_rel, memory_order_relaxed))
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
    for (unsigned int i = 0; i < 4; i++) {
        if (pool.owners[i] == lock) {
            queue_release(&lock->tail, &pool.nodes[i]);
            node_release(&pool.nodes[i]);
            return;
        }
    }
    abort();
}
