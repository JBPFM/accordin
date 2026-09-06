/* SPDX-License-Identifier: MIT */
#define _GNU_SOURCE

#include <errno.h>
#include <sched.h>
#include <stddef.h>
#include <stdio.h>
#include <unistd.h>

#include <vsync/spinlock/cnalock.h>

#include "directalgo.h"
#include "interpose.h"

#define CPU_NODE_CACHE_SIZE 4096
#define MAX_NUMA_NODE_SCAN 256

/*
 * Compact NUMA-aware lock. The algorithm comes from libvsync; this file only
 * supplies the queue node, which is private to one (thread, lock) pair, and the
 * NUMA node of the running CPU.
 */

const size_t directalgo_instance_size = sizeof(cnalock_t);

static int cpu_node_cache[CPU_NODE_CACHE_SIZE];

static unsigned int current_numa_node(void) {
    int cpu = sched_getcpu();
    if (cpu < 0 || cpu >= CPU_NODE_CACHE_SIZE)
        return 0;

    int cached = __atomic_load_n(&cpu_node_cache[cpu], __ATOMIC_ACQUIRE);
    if (cached > 0)
        return (unsigned int)(cached - 1);

    char path[128];
    for (int node = 0; node < MAX_NUMA_NODE_SCAN; node++) {
        snprintf(path, sizeof(path), "/sys/devices/system/node/node%d/cpu%d",
                 node, cpu);
        if (access(path, F_OK) == 0) {
            __atomic_store_n(&cpu_node_cache[cpu], node + 1, __ATOMIC_RELEASE);
            return (unsigned int)node;
        }
    }
    __atomic_store_n(&cpu_node_cache[cpu], 1, __ATOMIC_RELEASE);
    return 0;
}

static cna_node_t *queue_node(void *instance) {
    return directlock_context(instance, sizeof(cna_node_t));
}

void directalgo_instance_init(void *instance) {
    cnalock_init((cnalock_t *)instance);
}

void directalgo_instance_fini(void *instance) {
    (void)instance;
}

int directalgo_lock(void *instance) {
    cnalock_acquire((cnalock_t *)instance, queue_node(instance),
                    current_numa_node());
    return 0;
}

/* The queue lock has no non-blocking acquire. */
int directalgo_trylock(void *instance) {
    (void)instance;
    return EBUSY;
}

void directalgo_unlock(void *instance) {
    cnalock_release((cnalock_t *)instance, queue_node(instance),
                    current_numa_node());
}
