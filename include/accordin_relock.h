/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_RELOCK_H
#define ACCORDIN_RELOCK_H

#include <stdint.h>

struct timespec;

/* One condition-wait reacquisition, owned by the waiting thread. Prepare only
 * after releasing its mutex. Publish wake before making the thread runnable;
 * consume with relock, including on timeout/cancellation. The caller must
 * serialize wake against consumption and keep the request and TLS alive.
 * Preparation reserves an epoch, never a CPU admission slot. Optional spin
 * borrows a slot for this epoch; wake may race spin, but must be serialized
 * against relock/consumption. Call spin with cancellation disabled, after
 * preparing and before parking; wake and spin_fail remain alive throughout. */
typedef struct accordin_relock_request {
    void *word;
    uint32_t epoch;
    unsigned int nested;
} accordin_relock_request_t;

#endif
