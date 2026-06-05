/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __INACTIVE_SELECT_H
#define __INACTIVE_SELECT_H

#include "intf.h"

#define INACTIVE_PROBABILITY_SCALE 100U
#define MANAGED_LOCK_COUNT (MAX_LOCK_CLASSES - 1U)

static __always_inline bool inactive_managed_lock_id(__u32 lock_id) {
  return lock_id != UNMANAGED_LOCK_ID && lock_id < MAX_LOCK_CLASSES;
}

static __always_inline bool inactive_prefer_previous_lock(__u32 random,
                                                          __u32 percent) {
  if (percent > INACTIVE_PROBABILITY_SCALE)
    percent = INACTIVE_PROBABILITY_SCALE;

  return (random % INACTIVE_PROBABILITY_SCALE) < percent;
}

static __always_inline __u32 inactive_other_lock_at(__u32 previous_lock_id,
                                                    __u32 start,
                                                    __u32 offset) {
  __u32 rank;
  __u32 lock_id;

  if (inactive_managed_lock_id(previous_lock_id)) {
    if (offset >= MANAGED_LOCK_COUNT - 1U)
      return UNMANAGED_LOCK_ID;

    rank = (start + offset) % (MANAGED_LOCK_COUNT - 1U);
    lock_id = rank + 1U;
    if (lock_id >= previous_lock_id)
      lock_id++;
    return lock_id;
  }

  if (offset >= MANAGED_LOCK_COUNT)
    return UNMANAGED_LOCK_ID;

  rank = (start + offset) % MANAGED_LOCK_COUNT;
  return rank + 1U;
}

#endif /* __INACTIVE_SELECT_H */
