// SPDX-License-Identifier: GPL-2.0-only
#ifndef ACCORDIN_TEST_BACKEND_H
#define ACCORDIN_TEST_BACKEND_H

#include <assert.h>
#include <dlfcn.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>

/* Loading of a direct backend under test. Every backend exports the same C ABI
 * behind its own prefix, so a test names a library and a prefix and reaches the
 * mutex entry points through these pointers. */

static void *backend_library;
static const char *backend_prefix;
static void *(*mutex_create)(void);
static int (*mutex_destroy)(void *);
static int (*mutex_lock)(void *);
static int (*mutex_unlock)(void *);

static inline void *symbol(const char *suffix)
{
    char name[160];

    snprintf(name, sizeof(name), "%s_%s", backend_prefix, suffix);
    void *value = dlsym(backend_library, name);
    if (!value) {
        fprintf(stderr, "missing symbol %s: %s\n", name, dlerror());
        exit(1);
    }
    return value;
}

static inline void load_backend(const char *path, const char *prefix)
{
    backend_prefix = prefix;
    backend_library = dlopen(path, RTLD_NOW | RTLD_LOCAL);
    if (!backend_library) {
        fprintf(stderr, "dlopen: %s\n", dlerror());
        exit(1);
    }
    mutex_create = symbol("mutex_create");
    mutex_destroy = symbol("mutex_destroy");
    mutex_lock = symbol("mutex_lock");
    mutex_unlock = symbol("mutex_unlock");
}

/* The two lowest CPUs the process is allowed to run on. */
static inline void backend_cpu_pair(int pair[2])
{
    cpu_set_t allowed;
    int count = 0;

    assert(!sched_getaffinity(0, sizeof(allowed), &allowed));
    for (int cpu = 0; cpu < CPU_SETSIZE && count < 2; ++cpu)
        if (CPU_ISSET(cpu, &allowed))
            pair[count++] = cpu;
    assert(count == 2);
}

/* Confine the process to that pair, so contention shapes have somewhere to
 * queue: with a CPU per thread every contender finds a free slot of its own.
 * The scheduler reads the loading thread's affinity, so this belongs before
 * the library is opened. */
static inline void backend_pin_cpu_pair(void)
{
    cpu_set_t mask;
    int pair[2];

    backend_cpu_pair(pair);
    CPU_ZERO(&mask);
    CPU_SET(pair[0], &mask);
    CPU_SET(pair[1], &mask);
    assert(!sched_setaffinity(0, sizeof(mask), &mask));
}

#endif
