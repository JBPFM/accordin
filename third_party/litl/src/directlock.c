/* SPDX-License-Identifier: MIT */
#include <errno.h>
#include <stdlib.h>
#include <string.h>

#include <topology.h>

#include "directalgo.h"
#include "interpose.h"

#define DIRECTLOCK_ALIGN L_CACHE_LINE_SIZE

struct directlock_header {
    unsigned int context_id;
};

/* All-zero PTHREAD_MUTEX_INITIALIZER is lazily replaced with this pointer.
 * may_alias allows accessing pthread storage through the pointer slot. */
typedef void *mutex_slot_t __attribute__((may_alias));
_Static_assert(sizeof(pthread_mutex_t) >= sizeof(mutex_slot_t),
               "mutex pointer storage");
_Static_assert(_Alignof(pthread_mutex_t) >= _Alignof(mutex_slot_t),
               "mutex pointer alignment");

static unsigned int next_context_id;

struct context_table {
    size_t length;
    void **slots;
};

static __thread struct context_table thread_contexts;

static size_t round_up(size_t value, size_t alignment) {
    return (value + alignment - 1) & ~(alignment - 1);
}

static struct directlock_header *instance_header(void *instance) {
    return (struct directlock_header *)((char *)instance - DIRECTLOCK_ALIGN);
}

static void *create_instance(void) {
    size_t bytes =
        DIRECTLOCK_ALIGN + round_up(directalgo_instance_size, DIRECTLOCK_ALIGN);
    void *block = NULL;
    if (posix_memalign(&block, DIRECTLOCK_ALIGN, bytes) != 0)
        return NULL;
    memset(block, 0, bytes);

    void *instance = (char *)block + DIRECTLOCK_ALIGN;
    instance_header(instance)->context_id =
        __atomic_fetch_add(&next_context_id, 1, __ATOMIC_RELAXED);
    directalgo_instance_init(instance);
    return instance;
}

static void destroy_instance(void *instance) {
    directalgo_instance_fini(instance);
    free((char *)instance - DIRECTLOCK_ALIGN);
}

void *directlock_attach(pthread_mutex_t *mutex) {
    mutex_slot_t *slot = (mutex_slot_t *)mutex;
    void *instance = __atomic_load_n(slot, __ATOMIC_ACQUIRE);
    if (instance)
        return instance;

    void *candidate = create_instance();
    if (!candidate)
        return NULL;
    if (__atomic_compare_exchange_n(slot, &instance, candidate, 0,
                                    __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE))
        return candidate;

    destroy_instance(candidate);
    return instance;
}

int directlock_reset(pthread_mutex_t *mutex) {
    void *instance = create_instance();
    if (!instance)
        return ENOMEM;

    memset(mutex, 0, sizeof(*mutex));
    void *previous =
        __atomic_exchange_n((mutex_slot_t *)mutex, instance, __ATOMIC_ACQ_REL);
    if (previous)
        destroy_instance(previous);
    return 0;
}

int directlock_release(pthread_mutex_t *mutex) {
    void *instance =
        __atomic_exchange_n((mutex_slot_t *)mutex, NULL, __ATOMIC_ACQ_REL);
    if (instance)
        destroy_instance(instance);
    return 0;
}

/* A queue node can be handed to another thread and stay reachable after its
 * allocating thread is gone, so contexts are kept until process exit. */
void *directlock_context(void *instance, size_t size) {
    unsigned int id = instance_header(instance)->context_id;

    if (id >= thread_contexts.length) {
        size_t length = thread_contexts.length ? thread_contexts.length : 16;
        while (id >= length)
            length *= 2;
        void **slots = realloc(thread_contexts.slots, length * sizeof(*slots));
        if (!slots)
            abort();
        memset(slots + thread_contexts.length, 0,
               (length - thread_contexts.length) * sizeof(*slots));
        thread_contexts.slots = slots;
        thread_contexts.length = length;
    }

    void *context = thread_contexts.slots[id];
    if (!context) {
        if (posix_memalign(&context, DIRECTLOCK_ALIGN,
                           round_up(size, DIRECTLOCK_ALIGN)) != 0)
            abort();
        memset(context, 0, round_up(size, DIRECTLOCK_ALIGN));
        thread_contexts.slots[id] = context;
    }
    return context;
}

/* Mutex attributes are ignored, as in the interposed baselines these
 * algorithms replace: only normal, process-private mutexes are modelled. */
int directlock_mutex_init(pthread_mutex_t *mutex,
                          const pthread_mutexattr_t *attr) {
    (void)attr;
    return directlock_reset(mutex);
}

int directlock_mutex_lock(pthread_mutex_t *mutex, void *context) {
    (void)context;
    void *instance = directlock_attach(mutex);
    if (!instance)
        return ENOMEM;
    return directalgo_lock(instance);
}

int directlock_mutex_trylock(pthread_mutex_t *mutex, void *context) {
    (void)context;
    void *instance = directlock_attach(mutex);
    if (!instance)
        return ENOMEM;
    return directalgo_trylock(instance);
}

void directlock_mutex_unlock(pthread_mutex_t *mutex, void *context) {
    (void)context;
    void *instance = directlock_attach(mutex);
    if (!instance)
        abort();
    directalgo_unlock(instance);
}

int directlock_mutex_destroy(pthread_mutex_t *mutex) {
    return directlock_release(mutex);
}
