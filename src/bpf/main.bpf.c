/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * lb_simple - sched_ext lock-aware SSC admission control scheduler.
 *
 * Continuously observes per-thread lock wait statistics exported from
 * userspace, computes workload-level wait ratios, and uses a single
 * SSC (Scheduling Suppression Chamber) DSQ to throttle concurrency
 * via admission control.
 */
#include <scx/common.bpf.h>

#include "intf.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

#include "maps.bpf.h"
#include "stats.bpf.h"
#include "admission.bpf.h"

/* ------------------------------------------------------------------ */
/*  Callbacks                                                          */
/* ------------------------------------------------------------------ */

s32 BPF_STRUCT_OPS(lb_simple_select_cpu, struct task_struct *p, s32 prev_cpu,
		   u64 wake_flags)
{
	bool is_idle = false;
	s32 cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

	/*
	 * Fast path: if an idle CPU was found and the task is already
	 * admitted (or is a lock owner), dispatch directly to the local
	 * DSQ.  sched_ext will skip enqueue() entirely, avoiding the
	 * global READY_DSQ lock.  Correctness is preserved because
	 * stopping() still runs self-parking logic.
	 */
	if (is_idle) {
		struct task_scx_ctx *tc = lookup_task_ctx(p);
		if (tc && (tc->admitted || tc->role == ROLE_OWNER))
			scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);
		/* Untracked or suppressed tasks fall through to enqueue() */
	}

	return cpu;
}

void BPF_STRUCT_OPS(lb_simple_enqueue, struct task_struct *p, u64 enq_flags)
{
	struct task_scx_ctx *tc = lookup_task_ctx(p);
	if (!tc) {
		/* Untracked task (not yet seen in running()) — let it run. */
		scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
		return;
	}

	/* Owner is always admitted */
	if (tc->role == ROLE_OWNER) {
		if (!tc->admitted)
			admit_task(tc);
		scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
		return;
	}

	if (stats_only_mode || tc->admitted) {
		scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
		return;
	}

	/* Suppressed -> SSC */
	tc->ssc_enter_ts = bpf_ktime_get_ns();
	scx_bpf_dsq_insert(p, SSC_DSQ_ID, SCX_SLICE_DFL, enq_flags);
}

void BPF_STRUCT_OPS(lb_simple_dispatch, s32 cpu, struct task_struct *prev)
{
	if (stats_only_mode) {
		scx_bpf_dsq_move_to_local(READY_DSQ_ID);
		return;
	}

	__s64 al = active_local;
	__s64 ar = active_remote;
	__s64 tl = target_local;
	__s64 tr = target_remote;

	if (scx_bpf_dsq_nr_queued(SSC_DSQ_ID) > 0) {
		/*
		 * We cannot peek at the SSC head task directly.
		 * Use a two-pass approach:
		 * 1) If under target, try normal release (respecting dwell)
		 * 2) Safety valve: if SSC has tasks and we've been unable
		 *    to release, the ssc_enter_ts check in the task ctx
		 *    after move_to_local will catch stale tasks.
		 *
		 * Since we can't check dwell before moving, we always try
		 * to release when under target. The dwell time is best-effort
		 * via the min_ssc_dwell_ns in the ssc_enter_ts field.
		 */
		bool under_target = (al + ar) < (tl + tr);

		if (under_target) {
			if (scx_bpf_dsq_move_to_local(SSC_DSQ_ID))
				return;
		}

		/*
		 * Safety valve: only release from SSC when everything is parked
		 * and READY is empty. Releasing on every idle CPU defeats the
		 * admission cap and immediately re-expands CPU usage.
		 */
		if ((al + ar) <= 0 && scx_bpf_dsq_nr_queued(READY_DSQ_ID) == 0) {
			if (scx_bpf_dsq_move_to_local(SSC_DSQ_ID))
				return;
		}
	}

	/* Regular dispatch from READY_DSQ */
	scx_bpf_dsq_move_to_local(READY_DSQ_ID);
}

void BPF_STRUCT_OPS(lb_simple_running, struct task_struct *p)
{
	struct task_scx_ctx *tc = get_or_create_task_ctx(p);
	if (!tc)
		return;

	tc->run_start_ns = bpf_ktime_get_ns();

	/* NUMA node tracking — must happen before admit_task() so the
	 * active counter increments the correct (local vs remote) bucket. */
	__s32 this_cpu = scx_bpf_task_cpu(p);
	tc->last_node = get_cpu_node(this_cpu);

	if (tc->counted) {
		bool local = is_local_node(tc->last_node);
		bool counted_local = tc->counted_local;

		if (local != counted_local) {
			if (counted_local)
				__sync_fetch_and_sub((volatile __s64 *)&active_local, 1);
			else
				__sync_fetch_and_sub((volatile __s64 *)&active_remote, 1);

			tc->counted_local = local ? 1 : 0;
			if (local)
				__sync_fetch_and_add((volatile __s64 *)&active_local, 1);
			else
				__sync_fetch_and_add((volatile __s64 *)&active_remote, 1);
		}
	}

	/* If task was just released from SSC via dispatch, mark admitted.
	 * Also count newly created tasks that start admitted but uncounted. */
	if (!tc->admitted || !tc->counted) {
		admit_task(tc);
	}

	/* Role is read in stopping() via account_task_activity() using
	 * the cached user_ctx_ptr — no need to duplicate here. */
}

void BPF_STRUCT_OPS(lb_simple_stopping, struct task_struct *p, bool runnable)
{
	__u32 pid = p->pid;
	struct task_scx_ctx *tc = lookup_task_ctx(p);
	if (!tc)
		return;

	__u64 now = bpf_ktime_get_ns();
	account_task_activity(tc, pid, now);

	/* Window advance moved to tick() — only CPU 0 advances */

	if (stats_only_mode)
		return;

	/* Self-parking decision */
	__s64 al2 = active_local;
	__s64 ar2 = active_remote;
	__s64 tl2 = target_local;
	__s64 tr2 = target_remote;

	if (tc->role != ROLE_OWNER && tc->admitted) {
		bool local = is_local_node(tc->last_node);
		bool over_total = (al2 + ar2) > (tl2 + tr2);
		bool over_local = al2 > tl2;
		bool over_remote = ar2 > tr2;

		/* Remote-first suppression for NUMA */
		bool should_park = false;
		if (!local) {
			should_park = over_remote || over_total;
		} else {
			should_park = over_local || over_total;
		}

		if (should_park) {
			tc->admitted = 0;
			if (tc->counted) {
				tc->counted = 0;
				if (tc->counted_local)
					__sync_fetch_and_sub((volatile __s64 *)&active_local, 1);
				else
					__sync_fetch_and_sub((volatile __s64 *)&active_remote, 1);
			}
		}
	}
}

void BPF_STRUCT_OPS(lb_simple_tick, struct task_struct *p)
{
	__u32 pid = p->pid;
	struct task_scx_ctx *tc = lookup_task_ctx(p);
	__u64 now = bpf_ktime_get_ns();

	if (tc)
		account_task_activity(tc, pid, now);

	try_advance_window(now);

	if (stats_only_mode)
		return;

	/*
	 * If active count is above target, force a reschedule so the
	 * current task enters stopping() -> self-parking sooner.
	 */
	__s64 al = active_local;
	__s64 ar = active_remote;
	__s64 tl = target_local;
	__s64 tr = target_remote;
	if ((al + ar) > (tl + tr) || al > tl || ar > tr)
		p->scx.slice = 0;
}

void BPF_STRUCT_OPS(lb_simple_exit_task, struct task_struct *p,
		    struct scx_exit_task_args *args)
{
	__u32 pid = p->pid;
	struct task_scx_ctx *tc = lookup_task_ctx(p);

	/* Adjust active counts only if task was actually counted */
	if (tc && tc->counted) {
		if (tc->counted_local)
			__sync_fetch_and_sub((volatile __s64 *)&active_local, 1);
		else
			__sync_fetch_and_sub((volatile __s64 *)&active_remote, 1);
	}

	bpf_task_storage_delete(&task_ctx_map, p);
	bpf_map_delete_elem(&thread_ctx_addr_map, &pid);
}

s32 BPF_STRUCT_OPS_SLEEPABLE(lb_simple_init)
{
	s32 ret;

	ret = scx_bpf_create_dsq(READY_DSQ_ID, -1);
	if (ret)
		return ret;

	ret = scx_bpf_create_dsq(SSC_DSQ_ID, -1);
	if (ret)
		return ret;

	window_start_ns = bpf_ktime_get_ns();

	return 0;
}

void BPF_STRUCT_OPS(lb_simple_exit, struct scx_exit_info *ei)
{
	UEI_RECORD(uei, ei);
}

SCX_OPS_DEFINE(lb_simple_ops,
	       .select_cpu  = (void *)lb_simple_select_cpu,
	       .enqueue     = (void *)lb_simple_enqueue,
	       .dispatch    = (void *)lb_simple_dispatch,
	       .running     = (void *)lb_simple_running,
	       .stopping    = (void *)lb_simple_stopping,
	       .tick        = (void *)lb_simple_tick,
	       .exit_task   = (void *)lb_simple_exit_task,
	       .init        = (void *)lb_simple_init,
	       .exit        = (void *)lb_simple_exit,
	       .name        = "lb_simple");
