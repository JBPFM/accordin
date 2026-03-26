/*
 * Adapted from bench/flexguard/include/flexguard_bpf.h.
 */
#ifndef _FLEXGUARD_BPF_H_
#define _FLEXGUARD_BPF_H_

#include "platform_defs.h"
typedef struct flexguard_qnode_t {
  union {
    struct {
      volatile unsigned char waiting;
      volatile struct flexguard_qnode_t *volatile next;
      volatile unsigned char cs_counter;
    };

    unsigned char padding[CACHE_LINE_SIZE];
  };
} flexguard_qnode_t;
typedef volatile flexguard_qnode_t *flexguard_qnode_ptr;

enum {
  FLEXGUARD_CRITICAL_STATE_NONE = 0,
  FLEXGUARD_CRITICAL_STATE_HELD = 1u << 0,
  FLEXGUARD_CRITICAL_STATE_HANDOFF = 1u << 1,
};

static inline int flexguard_is_critical_state(unsigned char cs_counter) {
  return (cs_counter & (FLEXGUARD_CRITICAL_STATE_HELD |
                        FLEXGUARD_CRITICAL_STATE_HANDOFF)) != 0;
}

typedef volatile long long num_preempted_cs_t;

#endif
