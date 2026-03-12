/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __STATS_BPF_H
#define __STATS_BPF_H

/*
 * Statistics layer: window-based EWMA wait ratio computation.
 *
 * Provides seqcount-protected user-space reads, per-task run/wait
 * accounting, and the elected-leader window rollover with EWMA
 * and hysteresis-driven target adjustment.
 */

/* ------------------------------------------------------------------ */
/*  User-space context read                                            */
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

/* ------------------------------------------------------------------ */
/*  Stats map helper                                                   */
/* ------------------------------------------------------------------ */

static __always_inline void update_stat(__u32 key, __u64 val)
{
	__u64 *sp = bpf_map_lookup_elem(&stats_map, &key);
	if (sp)
		*sp = val;
}

/* ------------------------------------------------------------------ */
/*  Per-task activity accounting                                       */
/* ------------------------------------------------------------------ */

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

/* ------------------------------------------------------------------ */
/*  Window rollover and EWMA                                           */
/* ------------------------------------------------------------------ */

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

#endif /* __STATS_BPF_H */
