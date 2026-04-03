/*
 * File: flexguard_userspace_state.bpf.c
 * Author: Victor Laforet <victor.laforet@inria.fr>
 *
 * Description:
 *      Implementation of a hybrid lock with MCS, CLH or Ticket spin locks.
 * 			BPF preemptions detection driven by userspace
 * critical-state tracking.
 *
 * The MIT License (MIT)
 *
 * Copyright (c) 2023 Victor Laforet
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

/*
 * Cache line size in bytes.
 */
#define CACHE_LINE_SIZE 128

/*
 * Maximum number of threads allowed per process.
 */
#define MAX_NUMBER_THREADS 1600

/*
 * Maximum number of locks allowed per application.
 */
#define MAX_NUMBER_LOCKS 1000

#include "flexguard.bpf.h"
#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

#include <bpf/bpf_core_read.h>
#include <vmlinux.h>

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

#ifdef DEBUG
#define DPRINT(args...) bpf_printk(args);
#else
#define DPRINT(...)
#endif

flexguard_qnode_t qnodes[MAX_NUMBER_THREADS];

preempted_flag_t preempted_flags[MAX_NUMBER_THREADS];

num_preempted_holders_t num_preempted_holders = 0;

struct {
  __uint(type, BPF_MAP_TYPE_HASH);
  __type(key, u32);
  __type(value, int);
  __uint(max_entries, MAX_NUMBER_THREADS);
} nodes_map SEC(".maps");

SEC("tp_btf/sched_switch")
int BPF_PROG(sched_switch_btf, bool preempt, struct task_struct *prev,
             struct task_struct *next) {
  u32 key;
  flexguard_qnode_ptr qnode;
  int *thread_id;
  int thread_index;
  unsigned char state;

  /*
   * Clear preempted status of next thread.
   * Optimization: skip if next is a kernel thread.
   */
  if (!(next->flags & 0x00200000)) // PF_KTHREAD
  {
    key = next->pid;
    thread_id = bpf_map_lookup_elem(&nodes_map, &key);
    if (thread_id && *thread_id >= 0 && *thread_id < MAX_NUMBER_THREADS) {
      thread_index = *thread_id;
      if (preempted_flags[thread_index]) {
        state = qnodes[thread_index].cs_counter;
        preempted_flags[thread_index] = 0;
        if (flexguard_is_holder_state(state))
          __sync_fetch_and_add(&num_preempted_holders, -1);
      }
    }
  }

  /*
   * Optimization: No map lookup if prev is a kernel thread.
   */
  if (prev->flags & 0x00200000) // PF_KTHREAD
    return 0;

  /*
   * Retrieve prev's qnode.
   */
  key = prev->pid;
  thread_id = bpf_map_lookup_elem(&nodes_map, &key);
  if (!thread_id || *thread_id < 0 || *thread_id >= MAX_NUMBER_THREADS)
    return 0;
  thread_index = *thread_id;
  qnode = &qnodes[thread_index];

  if (get_task_state(prev) &
      ((((TASK_INTERRUPTIBLE | TASK_UNINTERRUPTIBLE | TASK_STOPPED |
          TASK_TRACED | EXIT_DEAD | EXIT_ZOMBIE | TASK_PARKED) +
         1)
        << 1) -
       1))
    return 0;

  state = qnode->cs_counter;
  if (!preempted_flags[thread_index] && flexguard_is_critical_state(state)) {
    DPRINT("Detected preemption: %s (%d) -> %s (%d)", prev->comm, prev->pid,
           next->comm, next->pid);
    preempted_flags[thread_index] = 1;
    if (flexguard_is_holder_state(state))
      __sync_fetch_and_add(&num_preempted_holders, 1);
  }

  return 0;
}
