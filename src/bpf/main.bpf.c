/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * scx_ulock BPF scheduler
 *
 * Scheduling plane for the user-space-lock-driven SSC scheduler.
 *
 * This BPF program is intentionally kept thin:
 *   - Routes tasks to DSQ_SSC or DSQ_NORMAL based on task_class.
 *   - SSC CPUs consume only DSQ_SSC; normal CPUs consume only DSQ_NORMAL.
 *   - Implements lazy migration via ssc_gen / last_ssc_gen.
 *   - Tracks per-task on-CPU time for controller use.
 *
 * All policy decisions (classification, SSC width search, topology) live in
 * the user-space controller.  The BPF hot path reads already-computed results.
 *
 * NOT implemented here:
 *   - Futex tracing or any hot-path lock instrumentation.
 *   - Kernel lock contention tracking.
 *   - Complex scheduling policy logic.
 */
#include "vmlinux.h"
#include <scx/common.bpf.h>
#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

/* Shared DSQ identifiers.  SSC CPUs consume DSQ_SSC; others consume DSQ_NORMAL. */
#define DSQ_SSC    1ULL
#define DSQ_NORMAL 2ULL

/* --------------------------------------------------------------------------
 * BPF Maps
 * -------------------------------------------------------------------------- */

/*
 * Global scheduler configuration.
 * Written by the controller; read by BPF callbacks.
 * Single-element array so the controller can do a simple map_update_elem.
 */
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__type(key, u32);
	__type(value, struct ulock_config);
	__uint(max_entries, 1);
} ulock_config_map SEC(".maps");

/*
 * SSC CPU bitmask.
 * Written by the controller after each SSC width change.
 * BPF dispatch and select_cpu use this to determine SSC membership.
 */
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__type(key, u32);
	__type(value, struct ulock_cpumask);
	__uint(max_entries, 1);
} ssc_cpumask SEC(".maps");

/*
 * Task classification result.
 * Written by the controller after each control period.
 * BPF reads this in enqueue to route the task to the correct DSQ.
 * Keyed by pid (u32).
 */
struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__type(key, u32);
	__type(value, u32); /* enum task_class */
	__uint(max_entries, 32768);
} task_class_map SEC(".maps");

/*
 * Per-task context stored in BPF task storage.
 * Allocated at init_task; freed automatically at task exit.
 */
struct {
	__uint(type, BPF_MAP_TYPE_TASK_STORAGE);
	__uint(map_flags, BPF_F_NO_PREALLOC);
	__type(key, int);
	__type(value, struct task_ctx);
} task_ctx_map SEC(".maps");

/*
 * Per-thread epoch aggregate slots.
 * Written by the user-space lock library (mcs_pthread_hook).
 * Read by the controller to compute wait_ratio and classify tasks.
 * BPF_F_MMAPABLE allows the controller to access the array via mmap
 * without going through syscalls on every read.
 */
struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__type(key, u32);
	__type(value, struct epoch_slot);
	__uint(max_entries, MAX_SLOTS);
	__uint(map_flags, BPF_F_MMAPABLE);
} epoch_slots SEC(".maps");

/* --------------------------------------------------------------------------
 * Helpers
 * -------------------------------------------------------------------------- */

/* Fetch the global configuration.  Returns NULL if the map is missing. */
static __always_inline struct ulock_config *get_config(void)
{
	u32 key = 0;
	return bpf_map_lookup_elem(&ulock_config_map, &key);
}

/*
 * Check whether @cpu belongs to the current SSC.
 * Returns false if the config or mask is unavailable, treating all CPUs as
 * non-SSC until the controller publishes a mask.
 */
static __always_inline bool cpu_in_ssc(u32 cpu)
{
	u32 key = 0;
	struct ulock_cpumask *mask;

	if (cpu >= MAX_CPUS)
		return false;

	mask = bpf_map_lookup_elem(&ssc_cpumask, &key);
	if (!mask)
		return false;

	return !!(mask->bits[cpu / 64] & (1ULL << (cpu % 64)));
}

/*
 * Refresh the task class from task_class_map if the SSC generation has
 * changed.  This implements lazy migration: tasks update their routing
 * one at a time at their next scheduling event rather than all at once.
 *
 * Must be called with a valid tctx and cfg.
 */
static __always_inline void refresh_task_cls(struct task_ctx *tctx,
					     struct task_struct *p,
					     const struct ulock_config *cfg)
{
	u32 pid;
	u32 *cls_p;

	if (tctx->last_ssc_gen == cfg->ssc_gen)
		return;

	pid = p->pid;
	cls_p = bpf_map_lookup_elem(&task_class_map, &pid);
	tctx->cls = cls_p ? *cls_p : TASK_NORMAL;
	tctx->last_ssc_gen = cfg->ssc_gen;
}

/* --------------------------------------------------------------------------
 * Scheduler callbacks
 * -------------------------------------------------------------------------- */

/*
 * select_cpu - Hint at a CPU for the waking task.
 *
 * We let the kernel's default heuristic pick the CPU.  Routing is done in
 * enqueue / dispatch, not here, to ensure tasks always land in the correct DSQ.
 *
 * We intentionally do NOT insert into SCX_DSQ_LOCAL here because that would
 * bypass enqueue and break DSQ separation.
 */
s32 BPF_STRUCT_OPS(ulock_select_cpu, struct task_struct *p, s32 prev_cpu,
		   u64 wake_flags)
{
	bool is_idle = false;

	return scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);
}

/*
 * enqueue - Place the task in the correct shared DSQ.
 *
 * LOCK_INTENSIVE tasks go to DSQ_SSC; all others go to DSQ_NORMAL.
 * Refreshes the task class if ssc_gen has advanced (lazy migration).
 */
void BPF_STRUCT_OPS(ulock_enqueue, struct task_struct *p, u64 enq_flags)
{
	struct task_ctx *tctx;
	struct ulock_config *cfg;
	u32 cls = TASK_NORMAL;

	tctx = bpf_task_storage_get(&task_ctx_map, p, NULL, 0);
	cfg  = get_config();

	if (tctx && cfg) {
		refresh_task_cls(tctx, p, cfg);
		cls = tctx->cls;
	}

	if (cls == TASK_LOCK_INTENSIVE)
		scx_bpf_dsq_insert(p, DSQ_SSC, SCX_SLICE_DFL, enq_flags);
	else
		scx_bpf_dsq_insert(p, DSQ_NORMAL, SCX_SLICE_DFL, enq_flags);
}

/*
 * dispatch - Move tasks from shared DSQs to the CPU's local DSQ.
 *
 * SSC CPUs pull exclusively from DSQ_SSC.
 * Normal CPUs pull exclusively from DSQ_NORMAL.
 * Cross-stealing is intentionally disabled (v1 requirement).
 */
void BPF_STRUCT_OPS(ulock_dispatch, s32 cpu, struct task_struct *prev)
{
	if (cpu_in_ssc((u32)cpu))
		scx_bpf_dsq_move_to_local(DSQ_SSC);
	else
		scx_bpf_dsq_move_to_local(DSQ_NORMAL);
}

/*
 * running - Record the wall-clock time when the task starts running.
 * Used by the controller to compute epoch_runtime_ns.
 */
void BPF_STRUCT_OPS(ulock_running, struct task_struct *p)
{
	struct task_ctx *tctx = bpf_task_storage_get(&task_ctx_map, p, NULL, 0);

	if (tctx)
		tctx->runnable_ns = bpf_ktime_get_ns();
}

/*
 * stopping - Accumulate on-CPU time when the task stops running.
 */
void BPF_STRUCT_OPS(ulock_stopping, struct task_struct *p, bool runnable)
{
	struct task_ctx *tctx = bpf_task_storage_get(&task_ctx_map, p, NULL, 0);

	if (tctx && tctx->runnable_ns) {
		tctx->run_ns += bpf_ktime_get_ns() - tctx->runnable_ns;
		tctx->runnable_ns = 0;
	}
}

/*
 * init_task - Allocate and zero-initialise the per-task context.
 * If the controller has already registered a class for this pid, adopt it.
 */
s32 BPF_STRUCT_OPS(ulock_init_task, struct task_struct *p,
		   struct scx_init_task_args *args)
{
	struct task_ctx *tctx;
	u32 pid = p->pid;
	u32 *cls_p;

	tctx = bpf_task_storage_get(&task_ctx_map, p, NULL,
				     BPF_LOCAL_STORAGE_GET_F_CREATE);
	if (!tctx)
		return -ENOMEM;

	tctx->pid          = pid;
	tctx->tgid         = p->tgid;
	tctx->cls          = TASK_NORMAL;
	tctx->epoch_id     = 0;
	tctx->run_ns       = 0;
	tctx->runnable_ns  = 0;
	tctx->mig_count    = 0;
	tctx->lock_domain_id = 0;
	tctx->last_ssc_gen = 0;
	tctx->hot_epochs   = 0;
	tctx->cool_epochs  = 0;
	tctx->hotness_score = 0;
	tctx->_pad         = 0;

	/* Inherit class if controller already knows about this task. */
	cls_p = bpf_map_lookup_elem(&task_class_map, &pid);
	if (cls_p)
		tctx->cls = *cls_p;

	return 0;
}

/*
 * exit_task - Remove the controller-side classification entry.
 * task_storage is freed automatically by the kernel.
 */
void BPF_STRUCT_OPS(ulock_exit_task, struct task_struct *p,
		    struct scx_exit_task_args *args)
{
	u32 pid = p->pid;

	bpf_map_delete_elem(&task_class_map, &pid);
}

/*
 * init - Create the two shared DSQs at scheduler load time.
 */
s32 BPF_STRUCT_OPS_SLEEPABLE(ulock_init)
{
	s32 ret;

	ret = scx_bpf_create_dsq(DSQ_SSC, -1);
	if (ret < 0)
		return ret;

	return scx_bpf_create_dsq(DSQ_NORMAL, -1);
}

/*
 * exit - Record exit information for the user-exit-info subsystem.
 */
void BPF_STRUCT_OPS(ulock_exit, struct scx_exit_info *ei)
{
	UEI_RECORD(uei, ei);
}

/*
 * Scheduler ops definition.
 * SCX_OPS_SWITCH_PARTIAL: only tasks that opt-in via SCHED_EXT are managed.
 */
SCX_OPS_DEFINE(ulock_ops,
	       .select_cpu = (void *)ulock_select_cpu,
	       .enqueue    = (void *)ulock_enqueue,
	       .dispatch   = (void *)ulock_dispatch,
	       .running    = (void *)ulock_running,
	       .stopping   = (void *)ulock_stopping,
	       .init_task  = (void *)ulock_init_task,
	       .exit_task  = (void *)ulock_exit_task,
	       .init       = (void *)ulock_init,
	       .exit       = (void *)ulock_exit,
	       .flags      = SCX_OPS_SWITCH_PARTIAL,
	       .name       = "ulock");
