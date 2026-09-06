/* SPDX-License-Identifier: MIT */
#ifndef LITL_DIRECTLOCK_H
#define LITL_DIRECTLOCK_H

#include <pthread.h>
#include <stddef.h>

/*
 * Storage services shared by the lock algorithms that keep their state outside
 * the intercepted pthread objects.
 *
 * An algorithm instance is attached lazily to the intercepted pthread mutex:
 * the instance pointer is stored in the mutex itself, so a statically
 * initialized mutex needs no lookup table and the build needs no CLHT, ssmem
 * or PAPI. Per-thread queue nodes and per-acquisition state live in a
 * per-thread table indexed by a dense identifier assigned to every instance,
 * which keeps a lock context private to one (thread, lock) pair.
 */

#ifdef __cplusplus
extern "C" {
#endif

/* Attaches an instance, creating it on first use. Returns NULL on failure. */
void *directlock_attach(pthread_mutex_t *mutex);

/* Replaces the instance of an explicitly initialized mutex. */
int directlock_reset(pthread_mutex_t *mutex);

/* Detaches and releases the instance of a destroyed mutex. */
int directlock_release(pthread_mutex_t *mutex);

/*
 * Returns this thread's context for the given instance, zero filled on first
 * use and cache-line aligned. Contexts live until process exit because a queue
 * node can outlive the thread that allocated it.
 */
void *directlock_context(void *instance, size_t size);

#ifdef __cplusplus
}
#endif

#endif /* LITL_DIRECTLOCK_H */
