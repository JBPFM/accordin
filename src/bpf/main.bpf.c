/* SPDX-License-Identifier: GPL-2.0-only */
#include <scx/common.bpf.h>
#include "maps.bpf.h"

char _license[] SEC("license") = "GPL";
UEI_DEFINE(uei);

/* The admission queue a waiter is filed in. The index is a CPU id at the
 * enqueue and release sites and a queue number when the bank itself is walked;
 * the wrap keeps it inside the bank whatever the caller reports. */
static __always_inline __u64 waiting_dsq(__u32 index) {
  return WAITING_DSQ + index % MAX_CPUS;
}

static __always_inline bool is_waiting(__u64 dsq) {
  return dsq >= WAITING_DSQ && dsq < WAITING_DSQ + MAX_CPUS;
}

/* The bank owns a contiguous id range that the other queues stay out of. */
_Static_assert(NORMAL_DSQ < WAITING_DSQ || NORMAL_DSQ >= WAITING_DSQ + MAX_CPUS,
               "NORMAL_DSQ overlaps the admission queues");
_Static_assert(WAITFORSIGNAL_DSQ < WAITING_DSQ ||
                   WAITFORSIGNAL_DSQ >= WAITING_DSQ + MAX_CPUS,
               "WAITFORSIGNAL_DSQ overlaps the admission queues");

static __always_inline __u32 nr_waiting(void) {
  __u32 total = 0, index;

  bpf_for(index, 0, waiting_queues) {
    s32 nr = scx_bpf_dsq_nr_queued(waiting_dsq(index));

    if (nr > 0)
      total += nr;
  }
  return total;
}

static __always_inline volatile __u64 *owner_slot(__u32 cpu) {
  barrier_var(cpu);
  return cpu < MAX_CPUS ? &admission.owners[cpu] : 0;
}

static __always_inline bool allowed(struct task_struct *p, __u32 cpu) {
  return cpu < MAX_CPUS && bpf_cpumask_test_cpu(cpu, p->cpus_ptr);
}

static __always_inline struct task_scx_ctx *task_ctx(struct task_struct *p) {
  return bpf_task_storage_get(&task_ctx_map, p, 0, 0);
}

static __always_inline bool lock_routing(void) {
  return !stats_only_mode && (!auto_admission || admission.active);
}

/* Runnable/quiescent are paired across sleep and CPU migration; enqueue is
 * not a runnable transition and must not inflate the count on every slice.
 * The per-task mark also makes initial attachment and cleanup idempotent. */
void BPF_STRUCT_OPS(accordin_runnable, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *tctx;
  __u32 count, capacity;

  (void)enq_flags;
  if (!auto_admission || admission.active || p->tgid != auto_tgid)
    return;
  tctx = bpf_task_storage_get(&task_ctx_map, p, 0, BPF_LOCAL_STORAGE_GET_F_CREATE);
  if (!tctx) {
    admission.active = 1;
    return;
  }
  if (__sync_lock_test_and_set(&tctx->auto_runnable, 1))
    return;
  count = __sync_fetch_and_add(&auto_runnable, 1) + 1;
  capacity = auto_capacity;
  /* A narrower affinity is handled conservatively: never use the full-host
   * CPU count to decide that a pinned group cannot be oversubscribed. */
  if (p->nr_cpus_allowed < capacity)
    capacity = p->nr_cpus_allowed;
  if (count > capacity && __sync_bool_compare_and_swap(&admission.active, 0, 1)) {
    auto_trigger_runnable = count;
    auto_activated_at = scx_bpf_now();
  }
}

void BPF_STRUCT_OPS(accordin_quiescent, struct task_struct *p, u64 deq_flags) {
  struct task_scx_ctx *tctx;

  (void)deq_flags;
  if (!auto_admission || admission.active || p->tgid != auto_tgid)
    return;
  tctx = task_ctx(p);
  if (tctx && __sync_lock_test_and_set(&tctx->auto_runnable, 0))
    __sync_fetch_and_sub(&auto_runnable, 1);
}

static __always_inline bool user_state(struct task_struct *p, __u32 *state) {
  __u32 tid = p->pid;
  __u64 *address = bpf_map_lookup_elem(&thread_ctx_addr_map, &tid);
  *state = 0;
  if (!address)
    return true;
  /* A failed read is not a release, and must not revoke a spinning waiter. */
  return !bpf_probe_read_user(state, sizeof(*state), (const void *)*address);
}

static __always_inline void release_slot(struct task_struct *p,
                                        struct task_scx_ctx *tctx) {
  __u32 assigned = tctx->admission_cpu;
  volatile __u64 *owner;

  if (!assigned)
    return;
  owner = owner_slot(assigned - 1);
  if (owner)
    __sync_val_compare_and_swap(owner, tctx->ticket, 0);
  tctx->admission_cpu = 0;
  scx_bpf_kick_cpu(assigned - 1, 0);
}

static __always_inline __u64 request_ticket(struct task_struct *p, __u32 state) {
  return ((__u64)(state & ~USER_META) << 32) | (__u32)p->pid;
}

/* Unless renewed at yield, a new request retires the old slot.
 * A changed affinity cannot park an existing MCS node behind its successor. */
static __always_inline void refresh_episode(struct task_struct *p,
                                            struct task_scx_ctx *tctx,
                                            __u32 state) {
  if (!(state & USER_FLAGS) || tctx->ticket != request_ticket(p, state)) {
    release_slot(p, tctx);
  } else if (tctx->admission_cpu && !allowed(p, tctx->admission_cpu - 1)) {
    release_slot(p, tctx);
  }
}

s32 BPF_STRUCT_OPS(accordin_select_cpu, struct task_struct *p, s32 prev_cpu,
                   u64 wake_flags) {
  bool inactive = auto_admission && !admission.active;
  struct task_scx_ctx *tctx = inactive ? 0 : task_ctx(p);
  bool idle = false;
  s32 cpu;

  if (!stats_only_mode && tctx && tctx->admission_cpu &&
      allowed(p, tctx->admission_cpu - 1))
    return tctx->admission_cpu - 1;
  cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &idle);
  /* Before overload there are no grants to preserve. Send an idle wakeup
   * straight to its CPU, avoiding the normal DSQ and a redundant idle kick.
   * If activation races with this wakeup, a new WAITING request still confirms
   * its ticket in userspace before it can enter a raw lock queue. */
  if (inactive && idle)
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);
  return cpu;
}

void BPF_STRUCT_OPS(accordin_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *tctx;
  __u64 dsq = NORMAL_DSQ;
  __u32 cpu = scx_bpf_task_cpu(p);
  __u32 state;

  if (lock_routing()) {
    bool known = user_state(p, &state);
    tctx = bpf_task_storage_get(&task_ctx_map, p, 0,
                              BPF_LOCAL_STORAGE_GET_F_CREATE);
    if (tctx) {
      if (known)
        refresh_episode(p, tctx, state);
      /* A task entering enqueue is queued nowhere, so a custody mark left on it
       * belongs to a wait the core has already taken out of the queue. Clearing
       * the mark claims the wait, the same way a scan claims one. */
      if (tctx->parked_at && __sync_lock_test_and_set(&tctx->parked_at, 0)) {
        __sync_fetch_and_add(&cv_drained, 1);
        __sync_fetch_and_sub(&cv_parked_now, 1);
      }
      if (tctx->admission_cpu) {
        cpu = tctx->admission_cpu - 1;
        dsq = SCX_DSQ_LOCAL_ON | cpu;
      } else if (known && cv_custody_enabled &&
                 (state & USER_CV) && (state & USER_FLAGS) == USER_WAITING &&
                 tctx->custody_denied != request_ticket(p, state)) {
        /* A condvar wait is held until it is notified or its custody expires,
         * never admitted onto a CPU slot. */
        tctx->ticket = request_ticket(p, state);
        /* The low bit keeps the mark non-zero whatever the clock reads, so an
         * unset mark is the only way to read as unparked. */
        tctx->parked_at = scx_bpf_now() | 1;
        dsq = WAITFORSIGNAL_DSQ;
        __sync_fetch_and_add(&cv_parked, 1);
        __sync_fetch_and_add(&cv_parked_now, 1);
      } else if (known && !(state & USER_CV) &&
                 (state & USER_FLAGS) == USER_WAITING) {
        tctx->ticket = request_ticket(p, state);
        dsq = waiting_dsq(cpu);
      }
    }
  }
  /* The core hands over the last runnable task of a CPU instead of keeping it,
   * so keep an ordinary task where it already is. A task bound for a managed
   * queue has to reach it: a slot is granted only from ops.dispatch, over the
   * admission queue, so a waiter kept on its CPU would yield forever without
   * ever being offered one. A slot holder is already routed to the local queue
   * of its admission CPU. */
  if ((enq_flags & SCX_ENQ_LAST) && dsq == NORMAL_DSQ)
    dsq = SCX_DSQ_LOCAL;
  scx_bpf_dsq_insert(p, dsq, SCX_SLICE_DFL, enq_flags);
  /* The last task of a CPU queued away from it leaves that CPU with nothing to
   * pick, and only this kick brings it back to dispatch. An idle kick may be
   * dropped while the task being queued is still the current one, so the
   * follow-up scheduling event the core asks for is an unconditional one. A
   * wait held for a signal wants no such event; its custody bounds it. */
  if ((enq_flags & SCX_ENQ_LAST) && is_waiting(dsq))
    scx_bpf_kick_cpu(cpu, 0);
  else
    scx_bpf_kick_cpu(cpu, SCX_KICK_IDLE);
}

/* Outcome of examining one queue: a waiter was granted this CPU's slot, the
 * queue held nothing for it, or the slot was taken while the grant was being
 * made and there is nothing left to hand out. */
#define ADMIT_NONE 0
#define ADMIT_GRANTED 1
#define ADMIT_SLOT_LOST 2

/* Reserve both the task and this CPU before moving a candidate. Different CPUs
 * may examine the same queue concurrently; a failed move rolls back both
 * reservations. There is no path that admits a new waiter onto an occupied CPU.
 * An empty queue costs one depth query and no iteration. */
static __always_inline int admit_from(__u32 cpu, volatile __u64 *owner,
                                      __u32 index) {
  __u64 dsq = waiting_dsq(index);
  struct task_struct *p;

  if (scx_bpf_dsq_nr_queued(dsq) <= 0)
    return ADMIT_NONE;
  bpf_for_each(scx_dsq, p, dsq, 0) {
    struct task_scx_ctx *tctx;

    if (!allowed(p, cpu))
      continue;
    tctx = task_ctx(p);
    if (!tctx || __sync_val_compare_and_swap(&tctx->admission_cpu, 0, cpu + 1))
      continue;
    if (__sync_val_compare_and_swap(owner, 0, tctx->ticket)) {
      tctx->admission_cpu = 0;
      return ADMIT_SLOT_LOST;
    }
    if (__COMPAT_scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, SCX_DSQ_LOCAL, 0))
      return ADMIT_GRANTED;
    release_slot(p, tctx);
  }
  return ADMIT_NONE;
}

/* The cursor is shared by every CPU and holds the queue after the last grant,
 * so every walk starts there. Rotating past the queue that granted keeps one
 * busy queue from starving the rest of the bank. The rotation covers every
 * queue whenever the own queue yields nothing, so a queue can only wait while
 * every dispatching CPU keeps finding work in its own queue; the home-CPU kick
 * on release and the tick keep that CPU dispatching. */
static __always_inline void admit_waiter(__u32 cpu) {
  volatile __u64 *owner = owner_slot(cpu);
  __u32 queues = waiting_queues, start, probe;
  int own;

  if (!owner || *owner || !queues)
    return;
  /* A waiter that last ran here is filed in this CPU's queue, so that queue is
   * probed first for cache locality, and a grant there leaves the shared cursor
   * alone because a locality preference must not disturb the rotation. */
  own = admit_from(cpu, owner, cpu);
  if (own != ADMIT_NONE)
    return;
  start = admit_cursor;
  if (start >= queues)
    start = 0;
  bpf_for(probe, 0, queues) {
    __u32 index = start + probe;
    int result;

    if (index >= queues)
      index -= queues;
    if (index == cpu)
      continue;
    result = admit_from(cpu, owner, index);
    if (result == ADMIT_GRANTED) {
      index++;
      admit_cursor = index >= queues ? 0 : index;
      return;
    }
    if (result == ADMIT_SLOT_LOST)
      return;
  }
}

/* Withdraw custody from every wait that outlived its limit and hand it back to
 * the ordinary queue. Shared by the periodic scan and the flush program; both
 * run without an rq lock, the only contexts a queue-to-queue move is legal in.
 * The custody mark is the claim: whoever clears it owns the wait. A refused
 * move leaves the wait where it is, so the claim is put back and the wait is
 * scanned again; only a claim that cannot be put back means the wait already
 * left the queue by another path and was counted there. Every park is
 * accounted exactly once that way, which is why the count is never restated
 * from the queue depth: that depth is read while an enqueue may be publishing
 * the next park. */
__noinline int cv_expire_parked(__u64 now) {
  struct task_struct *p;
  int expired = 0;

  if (!now)
    now = scx_bpf_now();
  bpf_rcu_read_lock();
  bpf_for_each(scx_dsq, p, WAITFORSIGNAL_DSQ, 0) {
    struct task_scx_ctx *tctx = task_ctx(p);
    __u64 parked;

    if (!tctx)
      continue;
    parked = tctx->parked_at;
    /* The clock is per-CPU: a wait parked on a CPU running slightly ahead of
     * this one must not read as older than any limit. */
    if (!parked || (__s64)(now - parked) <= (__s64)cv_custody_limit_ns)
      continue;
    if (!__sync_bool_compare_and_swap(&tctx->parked_at, parked, 0))
      continue;
    tctx->custody_denied = tctx->ticket;
    __sync_fetch_and_sub(&cv_parked_now, 1);
    if (__COMPAT_scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, NORMAL_DSQ, 0)) {
      __sync_fetch_and_add(&cv_expired, 1);
      expired++;
    } else if (__sync_bool_compare_and_swap(&tctx->parked_at, 0, parked)) {
      __sync_fetch_and_add(&cv_parked_now, 1);
    } else {
      __sync_fetch_and_add(&cv_drained, 1);
    }
  }
  bpf_rcu_read_unlock();
  return expired;
}

/* Wake idle CPUs for tasks just handed back. The cursor only spreads the CPU
 * the walk starts from; which task lands where is the core's decision. */
__noinline int kick_free_slots(__u32 count) {
  __u32 probe, cursor = flush_cursor, kicked = 0, cpus = scx_bpf_nr_cpu_ids();

  if (!count)
    return 0;
  if (cpus > MAX_CPUS)
    cpus = MAX_CPUS;
  if (cursor >= cpus)
    cursor = 0;
  bpf_for(probe, 0, cpus) {
    __u32 index = cursor + probe;
    volatile __u64 *owner;

    if (index >= cpus)
      index -= cpus;
    owner = owner_slot(index);
    if (!owner || *owner)
      continue;
    scx_bpf_kick_cpu(index, SCX_KICK_IDLE);
    cursor = index + 1;
    if (++kicked >= count)
      break;
  }
  flush_cursor = cursor;
  return kicked;
}

void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
  (void)prev;
  /* Serve ordinary work alongside the reserved waiter. This also lets an
   * unadmitted fast-path holder run and unlock while the waiter is spinning. */
  scx_bpf_dsq_move_to_local(NORMAL_DSQ);
  if (!lock_routing() || cpu < 0 || cpu >= MAX_CPUS)
    return;
  admit_waiter((__u32)cpu);
}

bool BPF_STRUCT_OPS(accordin_yield, struct task_struct *from, struct task_struct *to) {
  struct task_scx_ctx *tctx = lock_routing() ? task_ctx(from) : 0;
  __u32 cpu = bpf_get_smp_processor_id(), state;
  volatile __u64 *owner = owner_slot(cpu);

  (void)to;
  /* Renew only our existing slot; userspace still confirms the new ticket. */
  if (!stats_only_mode && tctx && owner && user_state(from, &state) &&
      !(state & USER_CV) && (state & USER_FLAGS) == USER_WAITING &&
      tctx->admission_cpu == cpu + 1) {
    __u64 next = request_ticket(from, state);
    if (__sync_val_compare_and_swap(owner, tctx->ticket, next) == tctx->ticket)
      tctx->ticket = next;
  }
  from->scx.slice = 0;
  return false;
}

void BPF_STRUCT_OPS(accordin_tick, struct task_struct *p) {
  struct task_scx_ctx *tctx = lock_routing() ? task_ctx(p) : 0;
  __u32 state;

  if (stats_only_mode || !tctx || !user_state(p, &state))
    return;
  refresh_episode(p, tctx, state);
  /* An admitted thread that spins never empties its CPU, so ops.dispatch is
   * never called there and the global queue is never consumed. Ending the
   * slice hands the CPU to one queued ordinary task; the spinner keeps its
   * slot and is re-enqueued behind it. */
  if (tctx->admission_cpu &&
      ((state & USER_FLAGS) == USER_WAITING ||
       (state & USER_FLAGS) == USER_SPINNING) &&
      scx_bpf_dsq_nr_queued(NORMAL_DSQ))
    p->scx.slice = 0;
}

void BPF_STRUCT_OPS(accordin_stopping, struct task_struct *p, bool runnable) {
  struct task_scx_ctx *tctx = lock_routing() ? task_ctx(p) : 0;
  __u32 state;

  if (stats_only_mode || !tctx || !user_state(p, &state))
    return;
  refresh_episode(p, tctx, state);
  /* A holder or an existing raw-lock node must be allowed to resume. */
  if (!runnable && (state & USER_FLAGS) != USER_HELD &&
      (state & USER_FLAGS) != USER_SPINNING)
    release_slot(p, tctx);
}

void BPF_STRUCT_OPS(accordin_exit_task, struct task_struct *p,
                    struct scx_exit_task_args *args) {
  struct task_scx_ctx *tctx = task_ctx(p);
  __u32 tid = p->pid;

  (void)args;
  if (tctx) {
    release_slot(p, tctx);
    /* A wait that leaves without a flush or an expiry still leaves custody. */
    if (tctx->parked_at && __sync_lock_test_and_set(&tctx->parked_at, 0)) {
      __sync_fetch_and_add(&cv_drained, 1);
      __sync_fetch_and_sub(&cv_parked_now, 1);
    }
  }
  bpf_map_delete_elem(&thread_ctx_addr_map, &tid);
  bpf_task_storage_delete(&task_ctx_map, p);
}

void BPF_STRUCT_OPS(accordin_dump, struct scx_dump_ctx *dump_ctx) {
  __u32 cpu;

  (void)dump_ctx;
  scx_bpf_dump("accordin normal=%d waiting=%u waitforsignal=%d\n",
               scx_bpf_dsq_nr_queued(NORMAL_DSQ), nr_waiting(),
               scx_bpf_dsq_nr_queued(WAITFORSIGNAL_DSQ));
  scx_bpf_dump("accordin cv parked=%llu now=%llu flushed=%llu expired=%llu\n",
               cv_parked, cv_parked_now, cv_flushed, cv_expired);
  scx_bpf_dump("accordin cv calls=%llu misses=%llu drained=%llu\n",
               cv_flush_calls, cv_flush_misses, cv_drained);
  bpf_for(cpu, 0, MAX_CPUS) {
    volatile __u64 *owner = owner_slot(cpu);
    if (owner && *owner)
      scx_bpf_dump("accordin cpu=%u owner=%u\n", cpu, (__u32)*owner);
  }
}

/* The runtime derives the scan period from the custody limit; the fallback only
 * covers a scheduler loaded without one. */
static __always_inline __u64 cv_scan_period(void) {
  __u64 period = cv_scan_period_ns;

  return period ? period : 10000000ULL;
}

static int cv_scan(void *map, int *key, struct cv_timer_state *value) {
  (void)map;
  (void)key;
  /* The queue depth, not a counter, decides: a drifted counter must never keep
   * a wait past its limit. */
  if (scx_bpf_dsq_nr_queued(WAITFORSIGNAL_DSQ))
    kick_free_slots(cv_expire_parked(0));
  bpf_timer_start(&value->timer, cv_scan_period(), 0);
  return 0;
}

static __always_inline struct cv_timer_state *cv_timer(void) {
  __u32 key = 0;

  return bpf_map_lookup_elem(&cv_timer_map, &key);
}

s32 BPF_STRUCT_OPS_SLEEPABLE(accordin_init) {
  struct cv_timer_state *timer;
  __u32 queues = scx_bpf_nr_cpu_ids(), index;
  s32 ret;

  ret = scx_bpf_create_dsq(NORMAL_DSQ, -1);
  if (ret)
    return ret;
  if (queues > MAX_CPUS)
    queues = MAX_CPUS;
  bpf_for(index, 0, queues) {
    ret = scx_bpf_create_dsq(waiting_dsq(index), -1);
    if (ret)
      return ret;
  }
  /* Every walk over the bank is bounded by the queues that exist. */
  waiting_queues = queues;
  ret = scx_bpf_create_dsq(WAITFORSIGNAL_DSQ, -1);
  if (ret)
    return ret;
  timer = cv_timer();
  if (!timer)
    return -ENOENT;
  ret = bpf_timer_init(&timer->timer, &cv_timer_map, CLOCK_MONOTONIC);
  if (ret)
    return ret;
  ret = bpf_timer_set_callback(&timer->timer, cv_scan);
  if (ret)
    return ret;
  ret = bpf_timer_start(&timer->timer, cv_scan_period(), 0);
  if (ret)
    return ret;
  admission.enabled = !stats_only_mode;
  admission.active = !auto_admission;
  return 0;
}

void BPF_STRUCT_OPS(accordin_exit, struct scx_exit_info *ei) {
  struct cv_timer_state *timer = cv_timer();

  admission.enabled = 0;
  if (timer)
    bpf_timer_cancel(&timer->timer);
  UEI_RECORD(uei, ei);
}

/* Hand every notified wait of the calling process to the lock admission queue
 * in one pass, and withdraw custody from the waits that outlived their limit.
 * Runs from a syscall, so it holds no rq lock and may move tasks between
 * queues. Head insertion keeps notified waits ahead of ordinary lock waiters;
 * reverse iteration then preserves the order they parked in. Each pass is
 * selected by the caller's flags. */
SEC("syscall")
int accordin_cv_flush(struct cv_flush_ctx *ctx) {
  struct cv_flush_tally *tally;
  struct task_struct *p;
  __u32 tgid = bpf_get_current_pid_tgid() >> 32;
  __u32 width = ctx->width, key = 0;
  __u64 iter_flags = (ctx->flags & CV_FLUSH_REV) ? SCX_DSQ_ITER_REV : 0;
  __u64 enq_flags = (ctx->flags & CV_FLUSH_TAIL) ? 0 : SCX_ENQ_HEAD;

  ctx->moved = ctx->expired = ctx->pending = ctx->queued = 0;
  if (stats_only_mode || !admission.enabled)
    return 0;
  tally = bpf_map_lookup_elem(&cv_tally_map, &key);
  if (!tally)
    return 0;
  tally->moved = tally->expired = tally->pending = 0;
  if (ctx->flags & CV_FLUSH_MOVE) {
    bpf_rcu_read_lock();
    bpf_for_each(scx_dsq, p, WAITFORSIGNAL_DSQ, iter_flags) {
      struct task_scx_ctx *tctx;
      __u64 move_flags = enq_flags;
      __u64 target, parked;
      __u32 task_cpu, state;

      if (p->tgid != tgid)
        continue;
      tctx = task_ctx(p);
      if (!tctx || !tctx->parked_at)
        continue;
      task_cpu = scx_bpf_task_cpu(p);
      target = waiting_dsq(task_cpu);
      /* Running in the process's own context, an unreadable word is not a
       * notification and cannot be confirmed later either. */
      if (!user_state(p, &state)) {
        target = NORMAL_DSQ;
        move_flags = 0;
      } else if ((state & USER_META) != USER_WAITING) {
        continue;
      } else if (width && tally->moved >= width) {
        tally->pending++;
        continue;
      }
      /* Clearing the custody mark claims the wait; an already cleared mark
       * means the wait belongs to someone else. */
      parked = __sync_lock_test_and_set(&tctx->parked_at, 0);
      if (!parked)
        continue;
      if (target == NORMAL_DSQ)
        tctx->custody_denied = tctx->ticket;
      else
        tctx->ticket = request_ticket(p, state);
      __sync_fetch_and_sub(&cv_parked_now, 1);
      /* A move can be refused while the wait is still queued, so put the claim
       * back and leave it to the next pass. Only a claim that cannot be put
       * back means the wait left the queue by another path, which counted it. */
      if (!__COMPAT_scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, target, move_flags)) {
        if (__sync_bool_compare_and_swap(&tctx->parked_at, 0, parked))
          __sync_fetch_and_add(&cv_parked_now, 1);
        else
          __sync_fetch_and_add(&cv_drained, 1);
      } else if (target == NORMAL_DSQ) {
        __sync_fetch_and_add(&cv_expired, 1);
        tally->expired++;
      } else {
        __sync_fetch_and_add(&cv_flushed, 1);
        tally->moved++;
        /* The wait is filed in the queue of the CPU it last ran on. Any CPU
         * with a free slot may grant it while walking the bank; waking that
         * CPU is the cheapest attempt, since it is the one most likely to still
         * hold the waiter's cache footprint. */
        scx_bpf_kick_cpu(task_cpu, SCX_KICK_IDLE);
      }
    }
    bpf_rcu_read_unlock();
  }
  if (ctx->flags & CV_FLUSH_EXPIRE)
    tally->expired += cv_expire_parked(ctx->now);
  /* Waits handed back to the ordinary queue may be served by any free CPU and
   * always need the sweep. A flushed wait has had one idle kick aimed at its
   * own CPU, which is dropped if that CPU is busy; sweeping the free slots for
   * it as well is what the spread flag asks for. */
  kick_free_slots((ctx->flags & CV_FLUSH_SPREAD) ? tally->moved + tally->expired
                                                 : tally->expired);
  __sync_fetch_and_add(&cv_flush_calls, 1);
  if (!tally->moved)
    __sync_fetch_and_add(&cv_flush_misses, 1);
  ctx->moved = tally->moved;
  ctx->expired = tally->expired;
  ctx->pending = tally->pending;
  ctx->queued = scx_bpf_dsq_nr_queued(WAITFORSIGNAL_DSQ);
  return tally->moved;
}

SCX_OPS_DEFINE(accordin_ops,
               .flags = SCX_OPS_ENQ_LAST,
               .select_cpu = (void *)accordin_select_cpu,
               .enqueue = (void *)accordin_enqueue,
               .dispatch = (void *)accordin_dispatch,
               .yield = (void *)accordin_yield,
               .tick = (void *)accordin_tick,
               .runnable = (void *)accordin_runnable,
               .quiescent = (void *)accordin_quiescent,
               .stopping = (void *)accordin_stopping,
               .exit_task = (void *)accordin_exit_task,
               .dump = (void *)accordin_dump,
               .init = (void *)accordin_init,
               .exit = (void *)accordin_exit,
               .name = "accordin");
