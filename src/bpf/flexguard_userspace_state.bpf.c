/*
 * File: flexguard_userspace_state.bpf.c
 * Author: Victor Laforet <victor.laforet@inria.fr>
 *
 * Description:
 *      Implementation of a hybrid lock with MCS, CLH or Ticket spin locks.
 * 			BPF preemptions detection driven by userspace critical-state tracking.
 *
 * The MIT License (MIT)
 *
 * Copyright (c) 2023 Victor Laforet
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy of
 * this software and associated documentation files (the "Software"), to deal in
 * the Software without restriction, including without limitation the rights to
 * use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
 * the Software, and to permit persons to whom the Software is furnished to do so,
 * subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
 * FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
 * COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
 * IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
 * CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

#include <scx/common.bpf.h>
#include "platform_defs.h"
#include "flexguard_bpf.h"
#include "bpf_fixes.bpf.h"

#ifdef DEBUG
#define DPRINT(args...) bpf_printk(args);
#else
#define DPRINT(...)
#endif

#define LOWPRI_DSQ_ID 1
#define LOWPRI_CREDIT_MAX 1
#define LOWPRI_REFILL_INTERVAL_NS (1000 * 1000)

flexguard_qnode_t qnodes[MAX_NUMBER_THREADS];

num_preempted_cs_t num_preempted_cs = 0;
u32 lowpri_credit = 0;
u64 lowpri_last_refill_ns = 0;

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

struct
{
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, u32);
	__type(value, int);
	__uint(max_entries, MAX_NUMBER_THREADS);
} nodes_map SEC(".maps");

struct
{
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, u32);
	__type(value, u32);
	__uint(max_entries, MAX_NUMBER_THREADS);
} is_preempted_map SEC(".maps");

SEC("tp_btf/sched_switch")
int BPF_PROG(sched_switch_btf, bool preempt, struct task_struct *prev, struct task_struct *next)
{
	u32 key;
	flexguard_qnode_ptr qnode;
	int *thread_id;

	/*
	 * Clear preempted status of next thread.
	 * Optimization: skip if next is a kernel thread.
	 */
	if (!(next->flags & 0x00200000)) // PF_KTHREAD
	{
		key = next->pid;
		if (bpf_map_delete_elem(&is_preempted_map, &key) == 0)
			__sync_fetch_and_add(&num_preempted_cs, -1);
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
	if (!thread_id || *thread_id < 0 || *thread_id >= MAX_NUMBER_THREADS || !(qnode = &qnodes[*thread_id]))
		return 0;

	if (get_task_state(prev) & ((((TASK_INTERRUPTIBLE | TASK_UNINTERRUPTIBLE | TASK_STOPPED | TASK_TRACED | EXIT_DEAD | EXIT_ZOMBIE | TASK_PARKED) + 1) << 1) - 1))
		return 0;

	if (flexguard_is_critical_state(qnode->cs_counter))
	{
		DPRINT("Detected preemption: %s (%d) -> %s (%d)", prev->comm, prev->pid, next->comm, next->pid);
		bpf_map_update_elem(&is_preempted_map, &key, &key, BPF_NOEXIST);
		__sync_fetch_and_add(&num_preempted_cs, 1);
	}

	return 0;
}

static __always_inline void refill_lowpri_credit(u64 now)
{
	if (!scx_bpf_dsq_nr_queued(LOWPRI_DSQ_ID))
	{
		lowpri_credit = 0;
		return;
	}

	if (lowpri_credit >= LOWPRI_CREDIT_MAX)
		return;

	lowpri_credit++;
	lowpri_last_refill_ns = now;
}

s32 BPF_STRUCT_OPS(lb_simple_select_cpu, struct task_struct *p, s32 prev_cpu,
		   u64 wake_flags)
{
	bool is_idle = false;
	s32 cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

	if (is_idle)
		scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

	return cpu;
}

void BPF_STRUCT_OPS(lb_simple_enqueue, struct task_struct *p, u64 enq_flags)
{
	scx_bpf_dsq_insert(p, SCX_DSQ_GLOBAL, SCX_SLICE_DFL, enq_flags);
}

bool BPF_STRUCT_OPS(lb_simple_yield, struct task_struct *from, struct task_struct *to)
{
	if (to)
		return false;

	scx_bpf_dsq_insert(from, LOWPRI_DSQ_ID, SCX_SLICE_DFL, 0);
	return true;
}

void BPF_STRUCT_OPS(lb_simple_stopping, struct task_struct *p, bool runnable)
{
	refill_lowpri_credit(scx_bpf_now());
}

void BPF_STRUCT_OPS(lb_simple_dispatch, s32 cpu, struct task_struct *prev)
{
	u64 now;

	if (scx_bpf_dsq_move_to_local(SCX_DSQ_GLOBAL))
		return;

	now = scx_bpf_now();
	if (lowpri_last_refill_ns == 0 || now - lowpri_last_refill_ns >= LOWPRI_REFILL_INTERVAL_NS)
		refill_lowpri_credit(now);

	if (lowpri_credit > 0 && scx_bpf_dsq_move_to_local(LOWPRI_DSQ_ID))
		lowpri_credit--;
}

s32 BPF_STRUCT_OPS_SLEEPABLE(lb_simple_init)
{
	return scx_bpf_create_dsq(LOWPRI_DSQ_ID, -1);
}

void BPF_STRUCT_OPS(lb_simple_exit, struct scx_exit_info *ei)
{
	UEI_RECORD(uei, ei);
}

SCX_OPS_DEFINE(lb_simple_ops,
	       .select_cpu = (void *)lb_simple_select_cpu,
	       .enqueue = (void *)lb_simple_enqueue,
	       .stopping = (void *)lb_simple_stopping,
	       .yield = (void *)lb_simple_yield,
	       .dispatch = (void *)lb_simple_dispatch,
	       .init = (void *)lb_simple_init,
	       .exit = (void *)lb_simple_exit,
	       .flags = SCX_OPS_SWITCH_PARTIAL,
	       .name = "lb_simple");
