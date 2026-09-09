// SPDX-License-Identifier: GPL-2.0-only
#define _GNU_SOURCE
#include <assert.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include "backend.h"

/* Contention shapes for the userspace admission slot. Each mode drives the
 * contended lock path a different way; the counters the runtime prints at
 * unload are what the surrounding script reads.
 *
 *   low       two threads on one mutex with a pause between episodes, so the
 *             bank stays empty and a contender takes its own slot
 *   overload  four threads with no pause, so the bank is never empty and every
 *             contender has to queue
 */

enum { MAX_WORKERS = 4, EPISODES_PER_CLOCK_READ = 64 };

static void *shared;
static unsigned long counter;
static double deadline;
static unsigned hold_rounds, gap_rounds;
static volatile unsigned sink;

static double monotonic(void) {
    struct timespec now;
    assert(!clock_gettime(CLOCK_MONOTONIC, &now));
    return (double)now.tv_sec + (double)now.tv_nsec / 1e9;
}

/* Occupies the CPU rather than sleeping: a thread that parks stops competing,
 * and the modes are defined by how much competition they keep alive. */
static void busy(unsigned rounds) {
    for (unsigned i = 0; i < rounds; ++i)
        sink = sink + 1;
}

static void *worker(void *unused) {
    (void)unused;
    while (monotonic() < deadline)
        for (unsigned i = 0; i < EPISODES_PER_CLOCK_READ; ++i) {
            assert(!mutex_lock(shared));
            busy(hold_rounds);
            ++counter;
            assert(!mutex_unlock(shared));
            busy(gap_rounds);
        }
    /* Slots are sticky, so the entry the last contended acquisition took is
     * still in the table with no lock left to release it. */
    return NULL;
}

int main(int argc, char **argv) {
    pthread_t threads[MAX_WORKERS];
    unsigned workers, i;
    double seconds;

    assert(argc == 4);
    if (!strcmp(argv[3], "low")) {
        workers = 2;
        seconds = 2.0;
        hold_rounds = 400;
        gap_rounds = 200;
    } else if (!strcmp(argv[3], "overload")) {
        workers = 4;
        seconds = 3.0;
        hold_rounds = 200;
        gap_rounds = 0;
    } else {
        fprintf(stderr, "usage: %s <library> <prefix> <low|overload>\n", argv[0]);
        return 2;
    }
    backend_pin_cpu_pair();
    load_backend(argv[1], argv[2]);
    shared = mutex_create();
    assert(shared);
    /* The main thread never contends, so every slot the table holds at unload
     * belongs to a worker and has to have been reclaimed with it. */
    deadline = monotonic() + seconds;
    for (i = 0; i < workers; ++i)
        assert(!pthread_create(&threads[i], NULL, worker, NULL));
    for (i = 0; i < workers; ++i)
        assert(!pthread_join(threads[i], NULL));
    assert(counter);
    assert(!mutex_destroy(shared));
    printf("user claim %s ok: %s, %lu acquisitions\n", argv[3], backend_prefix,
           counter);
    return 0;
}
