/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_RELOCK_H
#define ACCORDIN_RELOCK_H

#include <stdint.h>

/* One condition-wait reacquisition, owned by the waiting thread. Prepare only
 * after releasing its mutex. Publish wake before making the thread runnable;
 * consume with relock, including on timeout/cancellation. The caller must
 * serialize wake against consumption and keep the request and TLS alive.
 * Preparation reserves an epoch, never a CPU admission slot. */
typedef struct accordin_relock_request {
    void *word;
    uint32_t epoch;
    unsigned int nested;
} accordin_relock_request_t;

#endif
