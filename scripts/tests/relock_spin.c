// SPDX-License-Identifier: GPL-2.0-only
/* Drive the real spin/epoch transitions with deterministic scheduler events.
 * In particular, a notification racing withdrawal must never be erased. */
#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <assert.h>
#include <stdio.h>
#include <time.h>
#include <sched.h>

static int test_yield(void);
static int test_cpu(void) { return 0; }
static int test_clock(clockid_t clock, struct timespec *now);
#define sched_yield test_yield
#define sched_getcpu test_cpu
#define clock_gettime test_clock
#include "../../src/direct.c"

_Thread_local struct thread_state thread_state;
static struct admission_state state;
struct admission_state *scheduler_admission = &state;
bool admission_enabled = true;
uint64_t cv_spin_ns = 1000;
void register_thread(void) { thread_state.registered = true; }

enum event { NO_SLOT, EXPIRE, NOTIFY_AT_YIELD, NOTIFY_SPIN, NOTIFY_ONLY, REVOKE };
static enum event event;
static unsigned int yields, clocks;
static accordin_relock_request_t request;
static uint32_t wake, score;

static void notify_waiter(void)
{
    API(relock_wake)(&request);
    __atomic_store_n(&wake, 1, __ATOMIC_RELEASE);
}

static int test_yield(void)
{
    yields++;
    if (event == NOTIFY_AT_YIELD)
        notify_waiter();
    else if (event != NO_SLOT)
        state.owners[0] = ((uint64_t)request.epoch << 32) | thread_state.tid;
    return 0;
}

static int test_clock(clockid_t clock, struct timespec *now)
{
    (void)clock;
    *now = (struct timespec){.tv_sec = 100, .tv_nsec = clocks++ * 2000};
    if (clocks == 2) {
        if (event == NOTIFY_SPIN) {
            notify_waiter();
            now->tv_nsec = 0;
        } else if (event == NOTIFY_ONLY) {
            /* Logical notification arrived, but the relock baton is pending. */
            wake = 1;
            now->tv_nsec = 0;
        } else if (event == REVOKE) {
            state.owners[0] = 0;
            now->tv_nsec = 0;
        }
    }
    return 0;
}

static void reset(enum event next, uint32_t failures, unsigned int demand)
{
    thread_state = (struct thread_state){.tid = 123, .registered = true};
    state = (struct admission_state){.enabled = 1, .demand = demand};
    event = next;
    wake = yields = clocks = 0;
    score = failures;
    API(relock_prepare)(&request);
}

static void spin(void)
{
    API(relock_spin)(&request, &wake, &score, CLOCK_MONOTONIC, NULL);
}

int main(void)
{
    reset(NO_SLOT, 0, 0);
    spin();
    assert(yields == 1 && score == 0 && thread_state.word == request.epoch);

    reset(EXPIRE, 0, 1);
    spin();
    assert(yields == 1 && score == 2 && thread_state.word == request.epoch);
    reset(EXPIRE, 2, 1);
    spin();
    assert(yields == 1 && score == 4);
    reset(EXPIRE, 4, 1);
    spin();
    assert(yields == 0 && score == 4 && thread_state.word == request.epoch);

    reset(EXPIRE, 7, 0);
    spin();
    assert(yields == 1 && score == 8);
    reset(NOTIFY_SPIN, 8, 0);
    spin();
    assert(yields == 1 && score == 7 && wake == 1);
    assert(thread_state.word == (request.epoch | USER_WAITING));
    reset(NOTIFY_SPIN, 0, 1);
    spin();
    assert(yields == 1 && score == 0 && wake == 1);
    reset(NOTIFY_ONLY, 8, 0);
    spin();
    assert(yields == 1 && score == 7 && wake == 1);
    assert(thread_state.word == request.epoch);

    reset(REVOKE, 0, 0);
    spin();
    assert(yields == 1 && score == 2 && thread_state.word == request.epoch);
    reset(NOTIFY_AT_YIELD, 0, 0);
    spin();
    assert(yields == 1 && wake == 1);
    assert(thread_state.word == (request.epoch | USER_WAITING));
    struct MUTEX *mutex = API(create)();
    assert(mutex && API(relock)(mutex, &request) == 0);
    assert(thread_state.word == (request.epoch | USER_HELD));
    assert(API(unlock)(mutex) == 0 && thread_state.word == request.epoch);
    assert(API(destroy)(mutex) == 0);

    reset(EXPIRE, 0, 0);
    struct timespec expired = {.tv_sec = 99};
    API(relock_spin)(&request, &wake, &score, CLOCK_REALTIME, &expired);
    assert(yields == 0 && thread_state.word == request.epoch);
    state.enabled = 0;
    spin();
    assert(yields == 0);
    state.enabled = 1;
    thread_state.depth = 1;
    API(relock_prepare)(&request);
    assert(request.nested && !request.word);
    spin();
    assert(yields == 0 && thread_state.depth == 1);
    puts("PASS adaptive spin: demand/history, grant loss, racing wake, same epoch, deadlines/nesting");
    return 0;
}
