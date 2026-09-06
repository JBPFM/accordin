/* SPDX-License-Identifier: MIT */
#ifndef LITL_ACCORDIN_INTERNAL_H
#define LITL_ACCORDIN_INTERNAL_H

#include "accordin.h"

struct accordin_mutex;
struct accordin_park_waiter {
    struct accordin_park_waiter *prev, *next;
    struct accordin_mutex *mutex;
    accordin_relock_request_t request;
    uint32_t wake;
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
