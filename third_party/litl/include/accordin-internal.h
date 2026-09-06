/* SPDX-License-Identifier: MIT */
#ifndef LITL_ACCORDIN_INTERNAL_H
#define LITL_ACCORDIN_INTERNAL_H

#include <stdatomic.h>
#include "accordin.h"

/* How a waiter is currently sleeping. PARK_CUSTODY means the scheduler holds
 * it and no futex sleep is in progress; PARK_FUTEX means the private futex
 * loop is running or about to run. */
#define PARK_NONE 0
#define PARK_CUSTODY 1
#define PARK_FUTEX 2

struct accordin_mutex;
struct accordin_park_waiter {
    struct accordin_park_waiter *prev, *next;
    struct accordin_mutex *mutex;
    accordin_relock_request_t request;
    uint32_t wake;
    _Atomic uint32_t mode;
    unsigned int armed, queued;
};

/* The condition queue guard may nest the parking guard, never the reverse.
 * No caller may hold either guard across a raw mutex acquisition. */
void accordin_wait_init(pthread_mutex_t *mutex, struct accordin_park_waiter *waiter);
void accordin_wait_arm(struct accordin_park_waiter *waiter);
void accordin_wait_notify(struct accordin_park_waiter *waiter);
void accordin_wait_cancel(struct accordin_park_waiter *waiter);
void accordin_wait_relock(struct accordin_park_waiter *waiter);

#endif
