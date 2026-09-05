/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_RAW_LOCK_H
#define ACCORDIN_RAW_LOCK_H

#include <stdatomic.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdlib.h>
#include <sched.h>

#ifdef PERF_SYMBOLS
#define RAW_FN static __attribute__((noinline))
#else
#define RAW_FN static inline __attribute__((always_inline))
#endif

/* The interposer stores a raw lock inside a 40-byte pthread_mutex_t, so it
 * drops the cache-line padding and accepts sharing a line, and it sizes the
 * MCS node pool for an unmodified program whose nesting depth is unknown. */
#ifdef ACCORDIN_FULLHOOK
#define RAW_LOCK_ALIGN 8
#define MCS_POOL_SIZE 8
#else
#define RAW_LOCK_ALIGN 64
#define MCS_POOL_SIZE 4
#endif

struct __attribute__((aligned(64))) node {
    _Atomic(struct node *) next;
    _Atomic bool waiting;
};

static inline void spin_pause(void)
{
#if defined(__aarch64__)
    /* Match Rust's std::hint::spin_loop() on AArch64. */
    __asm__ volatile("isb" ::: "memory");
#elif defined(__arm__)
    __asm__ volatile("yield" ::: "memory");
#elif defined(__x86_64__) || defined(__i386__)
    __asm__ volatile("pause" ::: "memory");
#else
    sched_yield();
#endif
}

/* Remove the head only after its successor has finished linking itself. */
static inline void queue_release(_Atomic(struct node *) *tail, struct node *node)
{
    struct node *next = atomic_load_explicit(&node->next, memory_order_acquire);
    struct node *expected = node;
    if (!next && !atomic_compare_exchange_strong_explicit(
            tail, &expected, NULL, memory_order_acq_rel, memory_order_acquire)) {
        do {
            next = atomic_load_explicit(&node->next, memory_order_acquire);
            if (!next)
                spin_pause();
        } while (!next);
    }
    if (next)
        atomic_store_explicit(&next->waiting, false, memory_order_release);
}

#endif
