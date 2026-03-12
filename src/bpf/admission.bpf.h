/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __ADMISSION_BPF_H
#define __ADMISSION_BPF_H

/* Keep this header self-contained for standalone clangd parsing. */
#include <scx/common.bpf.h>

#include "intf.h"
#include "maps.bpf.h"

/*
 * Admission control and NUMA helpers.
 *
 * Manages per-task scheduling context creation, NUMA node lookups,
 * and the admit_task() routine that maintains active_local/active_remote
 * counters with NUMA-aware placement.
 */

/* ------------------------------------------------------------------ */
/*  Task context management                                            */
/* ------------------------------------------------------------------ */

/*
 * Lookup or create a per-task scheduling context.
 * New tasks start admitted.
 */
static __always_inline struct task_scx_ctx *get_or_create_task_ctx(__u32 pid)
{
	struct task_scx_ctx *tc;

	tc = bpf_map_lookup_elem(&task_ctx_map, &pid);
	if (tc)
		return tc;

	/*
	 * Only track threads that registered a userspace lock context.
	 * Counting every sched_ext task in the machine dilutes wait ratio
	 * and prevents SSC admission from converging for the benchmark.
	 */
	__u64 *user_ptr_p = bpf_map_lookup_elem(&thread_ctx_addr_map, &pid);
	if (!user_ptr_p)
		return NULL;

	struct task_scx_ctx new_ctx = {
		.admitted = 1,
		.last_node = -1,
		.user_ctx_ptr = *user_ptr_p,
	};
	bpf_map_update_elem(&task_ctx_map, &pid, &new_ctx, BPF_NOEXIST);
	return bpf_map_lookup_elem(&task_ctx_map, &pid);
}

/* ------------------------------------------------------------------ */
/*  NUMA helpers                                                       */
/* ------------------------------------------------------------------ */

static __always_inline __s32 get_cpu_node(__s32 cpu)
{
	__u32 key = (__u32)cpu;
	__u32 *node = bpf_map_lookup_elem(&cpu_to_node, &key);
	if (node)
		return (__s32)*node;
	return 0;
}

static __always_inline bool is_local_node(__s32 node)
{
	return node == dominant_node;
}

/* ------------------------------------------------------------------ */
/*  Admission                                                          */
/* ------------------------------------------------------------------ */

static __always_inline void admit_task(struct task_scx_ctx *tc)
{
	tc->admitted = 1;
	tc->counted = 1;
	bool local = is_local_node(tc->last_node);
	tc->counted_local = local ? 1 : 0;
	if (local)
		__sync_fetch_and_add((volatile __s64 *)&active_local, 1);
	else
		__sync_fetch_and_add((volatile __s64 *)&active_remote, 1);
}

#endif /* __ADMISSION_BPF_H */
