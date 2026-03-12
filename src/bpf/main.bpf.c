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

/* ------------------------------------------------------------------ */
/*  Maps                                                               */
/* ------------------------------------------------------------------ */

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, MAX_TASKS);
	__type(key, __u32);   /* pid (tid) */
	__type(value, struct task_scx_ctx);
} task_ctx_map SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_HASH);
	__uint(max_entries, MAX_TASKS);
	__type(key, __u32);   /* pid (tid) */
	__type(value, __u64); /* user-space pointer to lock_sched_thread_ctx */
} thread_ctx_addr_map SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, STAT_NR);
	__type(key, __u32);
	__type(value, __u64);
} stats_map SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, MAX_CPUS);
	__type(key, __u32);
	__type(value, __u32); /* NUMA node id */
} cpu_to_node SEC(".maps");

/* ------------------------------------------------------------------ */
/*  Globals (mutable .bss)                                             */
/* ------------------------------------------------------------------ */

/* Window parameters */
volatile __u64 window_ns       = 4000000ULL;  /* 4ms */
volatile __u64 window_start_ns = 0;

/* Thresholds (x1000 fixed-point, e.g. 350 = 0.35) */
volatile __u32 p_high     = 350;
volatile __u32 p_low      = 200;
volatile __u32 p_w_ewma   = 0;
volatile __u32 ewma_alpha = 200;  /* 0.2 x 1000 */

/* Admission targets */
volatile __s64 target_local  = 1024;
volatile __s64 target_remote = 1024;
volatile __s64 max_target_local  = 1024;
volatile __s64 max_target_remote = 1024;
volatile __s64 active_local  = 0;
volatile __s64 active_remote = 0;

/* SSC parameters */
volatile __u64 max_ssc_wait_ns  = 50000000ULL;  /* 50ms */
volatile __u64 min_ssc_dwell_ns = 1000000ULL;   /* 1ms */

/* Aggregation accumulators (fed by stopping(), consumed by window rollover) */
volatile __u64 agg_run_ns  = 0;
volatile __u64 agg_wait_ns = 0;

/* Hysteresis counters */
volatile __u32 consec_high  = 0;
volatile __u32 consec_low   = 0;
volatile __u32 H_persist    = 2;
volatile __u32 L_persist    = 2;

/* NUMA */
volatile __s32 dominant_node = 0;

/* Stats */
volatile __u64 forced_release_cnt = 0;

/*
 * Per-CPU timestamp tracking for SSC dwell time.
 * When dispatch() releases from SSC, we record the release time.
 * Since we can't peek at SSC head task, we use the ssc_enter_ts
 * from task_ctx_map looked up after move_to_local succeeds.
 */

/* ------------------------------------------------------------------ */
/*  Helpers                                                            */
/* ------------------------------------------------------------------ */

/*
 * Read a user-space lock_sched_thread_ctx with seqcount protection.
 * Returns true on success.
 */
static __always_inline bool read_thread_ctx(__u64 user_ptr,
					    struct lock_sched_thread_ctx *out)
{
	__u32 seq1 = 0, seq2 = 0;
	__u64 seq_addr = user_ptr + offsetof(struct lock_sched_thread_ctx, seq);

	if (!user_ptr)
		return false;

	if (bpf_probe_read_user(&seq1, sizeof(seq1), (void *)(unsigned long)seq_addr))
		return false;
	if (seq1 & 1)
		return false;  /* writer active */

	if (bpf_probe_read_user(out, sizeof(*out), (void *)(unsigned long)user_ptr))
		return false;

	if (bpf_probe_read_user(&seq2, sizeof(seq2), (void *)(unsigned long)seq_addr))
		return false;
	if ((seq2 & 1) || seq1 != seq2)
		return false;  /* torn read */

	return true;
}

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
	if (!bpf_map_lookup_elem(&thread_ctx_addr_map, &pid))
		return NULL;

	struct task_scx_ctx new_ctx = {
		.admitted = 1,
		.last_node = -1,
	};
	bpf_map_update_elem(&task_ctx_map, &pid, &new_ctx, BPF_NOEXIST);
	return bpf_map_lookup_elem(&task_ctx_map, &pid);
}

static __always_inline void update_stat(__u32 key, __u64 val)
{
	__u64 *sp = bpf_map_lookup_elem(&stats_map, &key);
	if (sp)
		*sp = val;
}

static __always_inline void account_task_activity(struct task_scx_ctx *tc,
						      __u32 pid, __u64 now)
{
	__u64 run_delta = 0;
	__u64 wait_delta = 0;

	if (!tc->run_start_ns) {
		tc->run_start_ns = now;
		return;
	}

	if (now > tc->run_start_ns)
		run_delta = now - tc->run_start_ns;

	__u64 *user_ptr_p = bpf_map_lookup_elem(&thread_ctx_addr_map, &pid);
	if (user_ptr_p) {
		struct lock_sched_thread_ctx uctx = {};
		if (read_thread_ctx(*user_ptr_p, &uctx)) {
			tc->role = uctx.state;
			if (uctx.wait_ns_total >= tc->last_wait_ns) {
				wait_delta = uctx.wait_ns_total - tc->last_wait_ns;
				tc->last_wait_ns = uctx.wait_ns_total;
			}
		}
	}

	if (tc->run_start_ns < window_start_ns) {
		tc->run_ns_window = 0;
		tc->wait_ns_window = 0;
	}
	tc->run_ns_window += run_delta;
	tc->wait_ns_window += wait_delta;

	if (run_delta)
		__sync_fetch_and_add((volatile __u64 *)&agg_run_ns, run_delta);
	if (wait_delta)
		__sync_fetch_and_add((volatile __u64 *)&agg_wait_ns, wait_delta);

	tc->run_start_ns = now;
}

/*
 * Try to advance the aggregation window.  Uses cmpxchg to elect a single
 * leader across all CPUs.  The winner computes EWMA, updates targets, and
 * resets accumulators.
 */
static __always_inline bool try_advance_window(__u64 now)
{
	__u64 old_start = window_start_ns;
	__u64 ws = window_ns;

	if (now - old_start < ws)
		return false;

	__u64 new_start = old_start + ws;
	if (__sync_val_compare_and_swap((volatile __u64 *)&window_start_ns,
					old_start, new_start) != old_start)
		return false;

	/* --- Winner: we own this window transition --- */

	__u64 total_run  = __sync_lock_test_and_set((volatile __u64 *)&agg_run_ns, 0);
	__u64 total_wait = __sync_lock_test_and_set((volatile __u64 *)&agg_wait_ns, 0);

	/* Compute p_w for this window (x1000 fixed-point) */
	__u32 p_w_sample = 0;
	__u64 total = total_run + total_wait;
	if (total > 0)
		p_w_sample = (__u32)(total_wait * 1000 / total);

	/* EWMA update: ewma = alpha * sample + (1 - alpha) * ewma */
	__u32 alpha = ewma_alpha;
	__u32 old_ewma = p_w_ewma;
	__u32 new_ewma = (alpha * p_w_sample + (1000 - alpha) * old_ewma) / 1000;
	p_w_ewma = new_ewma;

	/* Hysteresis: consecutive high/low counters */
	__u32 ch = consec_high;
	__u32 cl = consec_low;
	__u32 hp = H_persist;
	__u32 lp = L_persist;

	if (new_ewma > p_high) {
		ch++;
		cl = 0;
	} else if (new_ewma < p_low) {
		cl++;
		ch = 0;
	} else {
		ch = 0;
		cl = 0;
	}

	/* Target adjustment with NUMA awareness */
	__s64 tl = target_local;
	__s64 tr = target_remote;

	if (ch >= hp) {
		/* Shrink: prefer shrinking remote first */
		if (tr > 0)
			tr--;
		else if (tl > 1)
			tl--;
		ch = 0;
	}
	if (cl >= lp) {
		/* Expand: prefer expanding local first */
		tl++;
		cl = 0;
	}

	consec_high  = ch;
	consec_low   = cl;
	if (tl > max_target_local)
		tl = max_target_local;
	if (tr > max_target_remote)
		tr = max_target_remote;
	target_local  = tl;
	target_remote = tr;

	/* Export stats */
	update_stat(STAT_P_W_EWMA, new_ewma);
	update_stat(STAT_TARGET_LOCAL, (__u64)tl);
	update_stat(STAT_TARGET_REMOTE, (__u64)tr);
	update_stat(STAT_ACTIVE_LOCAL, (__u64)active_local);
	update_stat(STAT_ACTIVE_REMOTE, (__u64)active_remote);
	update_stat(STAT_SSC_WAITERS, scx_bpf_dsq_nr_queued(SSC_DSQ_ID));
	update_stat(STAT_CONSEC_HIGH, ch);
	update_stat(STAT_CONSEC_LOW, cl);
	update_stat(STAT_FORCED_RELEASE, forced_release_cnt);

	return true;
}

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

/* ------------------------------------------------------------------ */
/*  Callbacks                                                          */
/* ------------------------------------------------------------------ */

s32 BPF_STRUCT_OPS(lb_simple_select_cpu, struct task_struct *p, s32 prev_cpu,
		   u64 wake_flags)
{
	bool is_idle = false;
	s32 cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

	/*
	 * Do NOT insert into SCX_DSQ_LOCAL here - all tasks must go through
	 * enqueue() -> dispatch() so SSC admission control is not bypassed.
	 */
	return cpu;
}

void BPF_STRUCT_OPS(lb_simple_enqueue, struct task_struct *p, u64 enq_flags)
{
	__u32 pid = p->pid;
	struct task_scx_ctx *tc = get_or_create_task_ctx(pid);
	if (!tc) {
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

	if (tc->admitted) {
		scx_bpf_dsq_insert(p, READY_DSQ_ID, SCX_SLICE_DFL, enq_flags);
		return;
	}

	/* Suppressed -> SSC */
	tc->ssc_enter_ts = bpf_ktime_get_ns();
	scx_bpf_dsq_insert(p, SSC_DSQ_ID, SCX_SLICE_DFL, enq_flags);
}

void BPF_STRUCT_OPS(lb_simple_dispatch, s32 cpu, struct task_struct *prev)
{
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
	__u32 pid = p->pid;
	struct task_scx_ctx *tc = get_or_create_task_ctx(pid);
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

	/* Read user-space role */
	__u64 *user_ptr_p = bpf_map_lookup_elem(&thread_ctx_addr_map, &pid);
	if (user_ptr_p) {
		struct lock_sched_thread_ctx uctx = {};
		if (read_thread_ctx(*user_ptr_p, &uctx))
			tc->role = uctx.state;
	}
}

void BPF_STRUCT_OPS(lb_simple_stopping, struct task_struct *p, bool runnable)
{
	__u32 pid = p->pid;
	struct task_scx_ctx *tc = bpf_map_lookup_elem(&task_ctx_map, &pid);
	if (!tc)
		return;

	__u64 now = bpf_ktime_get_ns();
	account_task_activity(tc, pid, now);

	/* Try to advance window (stopping path) */
	try_advance_window(now);

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
	struct task_scx_ctx *tc = bpf_map_lookup_elem(&task_ctx_map, &pid);
	__u64 now = bpf_ktime_get_ns();

	if (tc)
		account_task_activity(tc, pid, now);

	try_advance_window(now);

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
	struct task_scx_ctx *tc = bpf_map_lookup_elem(&task_ctx_map, &pid);

	/* Adjust active counts only if task was actually counted */
	if (tc && tc->counted) {
		if (tc->counted_local)
			__sync_fetch_and_sub((volatile __s64 *)&active_local, 1);
		else
			__sync_fetch_and_sub((volatile __s64 *)&active_remote, 1);
	}

	bpf_map_delete_elem(&task_ctx_map, &pid);
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
