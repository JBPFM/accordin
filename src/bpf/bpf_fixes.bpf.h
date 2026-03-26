/*
 * Adapted from bench/flexguard/src/bpf_fixes.bpf.h.
 */
#ifndef __BPF_FIXES_BPF_H
#define __BPF_FIXES_BPF_H

#include <vmlinux.h>
#include <bpf/bpf_core_read.h>

#define TASK_RUNNING 0x00000000
#define TASK_INTERRUPTIBLE 0x00000001
#define TASK_UNINTERRUPTIBLE 0x00000002
#define TASK_STOPPED 0x00000004
#define TASK_TRACED 0x00000008
#define EXIT_DEAD 0x00000010
#define EXIT_ZOMBIE 0x00000020
#define TASK_PARKED 0x00000040

struct task_struct___o {
  volatile long int state;
} __attribute__((preserve_access_index));

struct task_struct___x {
  unsigned int __state;
} __attribute__((preserve_access_index));

static __always_inline __s64 get_task_state(void *task) {
  struct task_struct___x *t = task;

  if (bpf_core_field_exists(t->__state))
    return BPF_CORE_READ(t, __state);
  return BPF_CORE_READ((struct task_struct___o *)task, state);
}

#endif
