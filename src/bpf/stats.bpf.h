/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef __STATS_BPF_H
#define __STATS_BPF_H

/* Keep this header self-contained for standalone clangd parsing. */
#include <scx/common.bpf.h>

#include "intf.h"
#include "maps.bpf.h"

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
 * Read a user-space lock_sched_thread_ctx.
 *
 * The seqcount write-side is currently disabled in userspace (mcs_tas.rs),
 * so we skip the seq validation and do a single bpf_probe_read_user.
 * Returns true on success.
 */
static __always_inline bool read_thread_ctx(__u64 user_ptr,
					    struct lock_sched_thread_ctx *out)
{
	if (!user_ptr)
		return false;

	return bpf_probe_read_user(out, sizeof(*out),
				   (void *)(unsigned long)user_ptr) == 0;
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

static __always_inline __u64 scale_sampled_wait_ns(__u64 sampled_wait_ns)
{
#if WAIT_TIME_SAMPLE_STRIDE > 1
	if (sampled_wait_ns > (~0ULL / WAIT_TIME_SAMPLE_STRIDE))
		return ~0ULL;
	return sampled_wait_ns * WAIT_TIME_SAMPLE_STRIDE;
#else
	return sampled_wait_ns;
#endif
}

/* ------------------------------------------------------------------ */
/*  Per-task activity accounting                                       */
/* ------------------------------------------------------------------ */

/*
 * Account per-task run and wait time.
 *
 * Always reads userspace lock context and accumulates both run_ns
 * and wait_ns to the per-CPU aggregators symmetrically.
 */
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

	/* Read user-space lock context */
	bool _dbg = dbg_counters_enabled;
	if (_dbg)
		__sync_fetch_and_add((volatile __u64 *)&dbg_acct_calls, 1);
	if (tc->user_ctx_ptr) {
		if (_dbg)
			__sync_fetch_and_add((volatile __u64 *)&dbg_acct_has_uptr, 1);
		struct lock_sched_thread_ctx uctx = {};
		if (read_thread_ctx(tc->user_ctx_ptr, &uctx)) {
			if (_dbg)
				__sync_fetch_and_add((volatile __u64 *)&dbg_acct_read_ok, 1);
			tc->role = uctx.state;
			if (uctx.wait_ns_total >= tc->last_wait_ns) {
				wait_delta = scale_sampled_wait_ns(
					uctx.wait_ns_total - tc->last_wait_ns);
				tc->last_wait_ns = uctx.wait_ns_total;
				if (_dbg && wait_delta > 0)
					__sync_fetch_and_add((volatile __u64 *)&dbg_acct_wait_nz, 1);
			}
		}
	}

	if (tc->run_start_ns < window_start_ns) {
		tc->run_ns_window = 0;
		tc->wait_ns_window = 0;
	}
	tc->run_ns_window += run_delta;
	tc->wait_ns_window += wait_delta;

	/* Per-CPU accumulation — no atomics needed */
	if (run_delta || wait_delta) {
		__u32 key = 0;
		struct agg_percpu *agg = bpf_map_lookup_elem(&agg_percpu_map,
							     &key);
		if (agg) {
			agg->run_ns += run_delta;
			agg->wait_ns += wait_delta;
		}
	}

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

	/* CAS to elect a single winner across all CPUs */
	if (!__sync_bool_compare_and_swap(&window_start_ns, old_start,
					  old_start + ws))
		return false;

	/* Sum and zero all per-CPU accumulator slots */
	__u64 total_run = 0, total_wait = 0;
	__u32 agg_key = 0;
	int cpu;

	bpf_for(cpu, 0, MAX_CPUS) {
		struct agg_percpu *slot =
			bpf_map_lookup_percpu_elem(&agg_percpu_map, &agg_key,
						   cpu);
		if (slot) {
			total_run += slot->run_ns;
			total_wait += slot->wait_ns;
			slot->run_ns = 0;
			slot->wait_ns = 0;
		}
	}

	/*
	 * Compute p_w for this window (x1000 fixed-point).
	 *
	 * wait_ns is a SUBSET of run_ns (thread is on-CPU while spinning
	 * for the lock), so the correct ratio is wait/run, not
	 * wait/(run+wait) which double-counts and halves the signal.
	 * Cap at 1000 (can slightly exceed due to 8x sample scaling).
	 */
	__u32 p_w_sample = 0;
	if (total_run > 0) {
		__u64 ratio = total_wait * 1000 / total_run;
		p_w_sample = ratio > 1000 ? 1000 : (__u32)ratio;
	}

	/* Record per-window debug stats (in microseconds for readability) */
	dbg_win_run = total_run / 1000;
	dbg_win_wait = total_wait / 1000;

	/*
	 * EWMA update.  When total_wait==0 the wait signal is missing
	 * (not proof of low contention), so decay slowly instead of
	 * slamming EWMA to 0.  This prevents drought windows from
	 * destroying the contention signal and causing target expansion.
	 */
	__u32 alpha = ewma_alpha;
	__u32 old_ewma = p_w_ewma;
	__u32 new_ewma;
	if (total_wait == 0 && old_ewma > 0) {
		/* Very slow linear decay: ~10/sec at 200ms windows */
		new_ewma = old_ewma > 2 ? old_ewma - 2 : 0;
	} else {
		new_ewma = (alpha * p_w_sample + (1000 - alpha) * old_ewma) / 1000;
	}
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
		/*
		 * Only count toward expansion when we have positive
		 * evidence of low contention (actual wait data observed).
		 * Drought windows (total_wait==0) decay EWMA but must
		 * NOT trigger expansion — missing signal != low contention.
		 */
		if (total_wait > 0)
			cl++;
		/* else: keep cl unchanged — drought doesn't count */
		ch = 0;
	} else {
		ch = 0;
		cl = 0;
	}

	/* Target adjustment with NUMA awareness */
	__s64 tl = target_local;
	__s64 tr = target_remote;

	if (ch >= hp) {
		/*
		 * Shrink by halving (bisection): high contention is
		 * actively harmful, so converge aggressively.  Overshoot
		 * is safe — target floor (tl>=2) prevents collapse, and
		 * the conservative expand side will recover slowly.
		 *
		 * Halve total, then distribute the reduction: shrink
		 * remote first, then local.
		 */
		__u64 total_t = (__u64)(tl + tr);
		__u64 step = total_t >> 1; /* halve */
		if (step < 1) step = 1;

		/* Shrink remote first, then local — no loop for BPF verifier */
		{
			__s64 shrink_remote = (__s64)step;
			if (shrink_remote > tr)
				shrink_remote = tr;
			tr -= shrink_remote;
			__s64 remain = (__s64)step - shrink_remote;
			if (remain > tl - 1)
				remain = tl - 1;
			if (remain > 0)
				tl -= remain;
		}
		ch = 0;
	}
	if (cl >= lp) {
		/*
		 * Expand with proportional step: the further below
		 * p_low, the larger the step.
		 * Use unsigned arithmetic for BPF verifier.
		 */
		__u32 under_delta = p_low > new_ewma ? p_low - new_ewma : 0;
		__u64 step = under_delta / 50;
		if (step < 1) step = 1;
		/*
		 * Cap expand step relative to current target (not remaining
		 * room) to prevent rapid expansion from near-zero.
		 */
		__u64 cur_total = (__u64)(tl + tr);
		__u64 max_step = cur_total > 4 ? cur_total >> 2 : 1;
		if (max_step < 1) max_step = 1;
		if (step > max_step) step = max_step;

		/*
		 * Expand: allocate step among local/remote based on
		 * which has more headroom.  No loop for BPF verifier.
		 */
		{
			__s64 local_headroom = tl - active_local;
			__s64 remote_headroom = tr - active_remote;
			__s64 expand_remote, expand_local;

			/* ~3/4 to the side with more headroom, ~1/4 to the other */
			__s64 major = (__s64)step - ((__s64)step >> 2);
			if (major < 1) major = 1;
			__s64 minor = (__s64)step - major;
			if (remote_headroom > local_headroom) {
				expand_remote = major;
				expand_local  = minor;
			} else {
				expand_local  = major;
				expand_remote = minor;
			}

			/* Clamp to max */
			if (tl + expand_local > max_target_local)
				expand_local = max_target_local - tl;
			if (expand_local < 0) expand_local = 0;
			if (tr + expand_remote > max_target_remote)
				expand_remote = max_target_remote - tr;
			if (expand_remote < 0) expand_remote = 0;

			tl += expand_local;
			tr += expand_remote;
		}
		cl = 0;
	}

	consec_high  = ch;
	consec_low   = cl;
	if (tl > max_target_local)
		tl = max_target_local;
	if (tr > max_target_remote)
		tr = max_target_remote;
	/* Minimum target: at N=2, p_w=(N-1)/(N+1)=0.33 sits between
	 * p_low and p_high, providing a natural equilibrium point. */
	if (tl < 2)
		tl = 2;
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
	update_stat(STAT_DBG_WIN_RUN, dbg_win_run);
	update_stat(STAT_DBG_WIN_WAIT, dbg_win_wait);
	update_stat(STAT_DBG_WIN_PW, p_w_sample);
	update_stat(STAT_DBG_ACCT_CALLS, dbg_acct_calls);
	update_stat(STAT_DBG_ACCT_UPTR, dbg_acct_has_uptr);
	update_stat(STAT_DBG_ACCT_READOK, dbg_acct_read_ok);
	update_stat(STAT_DBG_ACCT_WAITNZ, dbg_acct_wait_nz);

	return true;
}

#endif /* __STATS_BPF_H */
