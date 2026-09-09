/* SPDX-License-Identifier: GPL-2.0-only */
#include <scx/common.bpf.h>
#include "maps.bpf.h"

char _license[] SEC("license") = "GPL";
UEI_DEFINE(uei);

/* The lockless read of a dispatch queue's head postdates the vendored scx
 * headers, so the kfunc is declared here and, like the __COMPAT helpers, used
 * only where the running kernel exports it. */
struct task_struct *scx_bpf_dsq_peek(u64 dsq_id) __ksym __weak;

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
  return cpu < MAX_CPUS ? &admission.owners[cpu].ticket : 0;
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

/* The word and the slot the runtime keeps for a thread, taken together: the
 * scheduler has to see the slot a thread recorded beside the request it
 * recorded it for, and one read of the aligned pair is what gives it both. */
static __always_inline bool user_state(struct task_struct *p,
                                       struct admission_word *word) {
  __u32 tid = p->pid;
  __u64 *address = bpf_map_lookup_elem(&thread_ctx_addr_map, &tid);
  word->state = 0;
  word->slot = 0;
  if (!address)
    return true;
  /* A failed read is not a release, and must not revoke a spinning waiter. */
  return !bpf_probe_read_user(word, sizeof(*word), (const void *)*address);
}

static __always_inline void release_slot(struct task_struct *p,
                                        struct task_scx_ctx *tctx) {
  __u32 assigned = tctx->admission_cpu;
  volatile __u64 *owner;
  bool freed = false;

  if (!assigned)
    return;
  owner = owner_slot(assigned - 1);
  if (owner)
    freed = __sync_val_compare_and_swap(owner, tctx->ticket, 0) == tctx->ticket;
  tctx->admission_cpu = 0;
  /* A record adopted from the runtime may name a slot this task no longer owns.
   * The kick exists to make the freed slot's CPU look for a waiter, so it
   * belongs to the exchange that freed it and to no other. */
  if (freed)
    scx_bpf_kick_cpu(assigned - 1, 0);
}

static __always_inline __u64 request_ticket(struct task_struct *p, __u32 state) {
  return ((__u64)(state & ~USER_META) << 32) | (__u32)p->pid;
}

/* Take over the slot the runtime records for this thread, so that pinning,
 * local routing and reclaim all read one record whichever side wrote the table.
 * The slot is user-writable, hence the bound: an out-of-range CPU handed to a
 * kick takes the whole scheduler down.
 *
 * The entry is read once and adopted only if it names this thread and carries a
 * request no newer than the word it came with. The word and the slot are
 * written separately, so an old word may arrive beside a fresh slot; without the
 * request comparison the idle-word rule below would then retire an entry the
 * thread has only just taken. Request numbers only grow, which is what makes
 * the comparison decide it. */
static __always_inline void adopt_slot(struct task_struct *p,
                                       struct task_scx_ctx *tctx,
                                       struct admission_word word) {
  volatile __u64 *owner;
  __u64 value;

  if (!word.slot || word.slot - 1 >= waiting_queues)
    return;
  owner = owner_slot(word.slot - 1);
  if (!owner)
    return;
  value = *owner;
  if ((__u32)value != (__u32)p->pid ||
      (s32)((__u32)(value >> 32) - (word.state & ~USER_META)) > 0 ||
      (tctx->admission_cpu == word.slot && tctx->ticket == value))
    return;
  /* A record left on another CPU is released here or nowhere: its ticket is
   * still in that entry, so this exchange is what frees it. */
  if (tctx->admission_cpu != word.slot)
    release_slot(p, tctx);
  tctx->admission_cpu = word.slot;
  tctx->ticket = value;
  __sync_fetch_and_add(&claims_adopted, 1);
}

/* Unless renewed at yield, a new request retires the old slot.
 * A changed affinity cannot park an existing MCS node behind its successor.
 * Adoption comes first: a slot has to be on the record before the rules that
 * weigh it, the affinity rule included. */
static __always_inline void refresh_episode(struct task_struct *p,
                                            struct task_scx_ctx *tctx,
                                            struct admission_word word) {
  adopt_slot(p, tctx, word);
  if (!(word.state & USER_FLAGS) ||
      tctx->ticket != request_ticket(p, word.state) ||
      (tctx->admission_cpu && !allowed(p, tctx->admission_cpu - 1)))
    release_slot(p, tctx);
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
  bool routing = lock_routing();
  struct task_scx_ctx *tctx;
  __u64 dsq = NORMAL_DSQ;
  __u32 cpu = scx_bpf_task_cpu(p);
  struct admission_word word;

  if (routing) {
    bool known = user_state(p, &word);
    tctx = bpf_task_storage_get(&task_ctx_map, p, 0,
                              BPF_LOCAL_STORAGE_GET_F_CREATE);
    if (tctx) {
      if (known)
        refresh_episode(p, tctx, word);
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
      } else if (known && cv_custody_enabled && (word.state & USER_CV) &&
                 (word.state & USER_FLAGS) == USER_WAITING &&
                 tctx->custody_denied != request_ticket(p, word.state)) {
        /* A condvar wait is held until it is notified or its custody expires,
         * never admitted onto a CPU slot. */
        __u64 parked = scx_bpf_now();

        tctx->ticket = request_ticket(p, word.state);
        /* The low bit keeps the mark non-zero whatever the clock reads, so an
         * unset mark is the only way to read as unparked. */
        tctx->parked_at = parked | 1;
        p->scx.dsq_vtime = parked;
        dsq = WAITFORSIGNAL_DSQ;
        __sync_fetch_and_add(&cv_parked, 1);
        __sync_fetch_and_add(&cv_parked_now, 1);
      } else if (known && !(word.state & USER_CV) &&
                 (word.state & USER_FLAGS) == USER_WAITING) {
        tctx->ticket = request_ticket(p, word.state);
        p->scx.dsq_vtime = scx_bpf_now();
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
  /* Everything the runtime treats as a competitor is queued in the ordinary
   * queue or in the bank, and nowhere else. The count is read only where the
   * routing it belongs to is on, and the final queue is settled by here. */
  if (routing && (dsq == NORMAL_DSQ || is_waiting(dsq)))
    __sync_fetch_and_add(&admission.demand, 1);
  /* The admission bank is ordered by the age stamp, which makes the head of a
   * queue its oldest request by construction. A queue holds either ordered or
   * plain insertions and never both, so the bank takes these and nothing else,
   * and every other queue takes the plain form. */
  if (is_waiting(dsq))
    scx_bpf_dsq_insert_vtime(p, dsq, SCX_SLICE_DFL, p->scx.dsq_vtime, enq_flags);
  else
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

/* Time between two stamps, floored at zero so a stamp taken slightly ahead of
 * the reader reads as no time at all. */
static __always_inline __u64 elapsed(__u64 now, __u64 stamp) {
  s64 span = (s64)(now - stamp);

  return span > 0 ? (__u64)span : 0;
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
 * An empty queue costs one depth query and no iteration. The queue is ordered
 * by the age stamp and the iterator walks that order, so the first candidate
 * offered is the oldest request the queue holds. */
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
    if (__COMPAT_scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, SCX_DSQ_LOCAL, 0)) {
      __sync_fetch_and_add(&admission.demand, -1);
      return ADMIT_GRANTED;
    }
    release_slot(p, tctx);
  }
  return ADMIT_NONE;
}

/* How long the waiter at the head of an admission queue has been in line. The
 * bank is ordered by the age stamp, so its head is the oldest request the queue
 * holds. Reading it through the head peek touches no queue lock; opening an
 * iterator would take the queue's raw spinlock and write its list twice, which
 * a group scan would pay for on every member of the group. The iterator is kept
 * for a kernel without the peek.
 *
 * A stamp is written by the clock of the CPU that queued the waiter, and only
 * the group scan compares stamps taken on different CPUs. The difference is
 * taken signed so a head stamped slightly ahead of this CPU reads as no wait at
 * all rather than as an enormous one; a persistent offset between two CPUs
 * wider than the slack would bias the order in favour of the CPU that runs
 * behind, which assumes a host whose clock is stable across CPUs. */
static __always_inline bool head_age(__u64 dsq, __u64 now, __u64 *age) {
  struct task_struct *p;
  bool found = false;

  if (bpf_ksym_exists(scx_bpf_dsq_peek)) {
    /* No head is how an empty queue reports itself, so no depth query. */
    p = scx_bpf_dsq_peek(dsq);
    if (!p)
      return false;
    *age = elapsed(now, p->scx.dsq_vtime);
    return true;
  }
  if (scx_bpf_dsq_nr_queued(dsq) <= 0)
    return false;
  bpf_for_each(scx_dsq, p, dsq, 0) {
    *age = elapsed(now, p->scx.dsq_vtime);
    found = true;
    break;
  }
  return found;
}

/* Grant order for a CPU that has a free slot. The CPU's own queue comes first,
 * for the cache footprint a waiter left behind on it, and is served while its
 * head is not younger than the oldest head of the CPU's topology group by more
 * than the slack; a count of consecutive own grants bounds that preference as
 * a second limiter. The oldest head of the group comes next, and is the
 * population balancer and the fairness rule in one: a waiter granted by another
 * CPU is filed under that CPU when it next waits, so populations drain from
 * crowded queues toward the CPUs that have slots, while ordering grants by
 * waiting time keeps no thread at the back of the line. The rotation over the
 * whole bank is the cross-group backstop; its cursor is shared by every CPU and
 * holds the queue after the last grant, so rotating past the queue that granted
 * keeps one busy queue from starving the rest of the bank. The rotation is
 * reached only once both the own queue and the group yield nothing, so how fast
 * a waiter is picked up from outside its group is set by the turnover of its
 * own group rather than by the rotation.
 */
static __always_inline void admit_waiter(__u32 cpu) {
  volatile __u64 *owner = owner_slot(cpu);
  __u32 queues = waiting_queues, start, probe;
  __u32 limit = own_limit, granted = own_grants[cpu];
  __u32 group = cpu_group[cpu], members = 0, best = 0, best_slot = 0;
  __u64 now, own_wait = 0, group_wait = 0;
  bool own_head, group_head = false;
  bool own_first;
  int result;

  if (!owner || *owner || !queues)
    return;
  now = scx_bpf_now();
  own_head = head_age(waiting_dsq(cpu), now, &own_wait);
  if (group < MAX_GROUPS) {
    __u32 cursor = group_cursor[group], slot;

    members = group_size[group];
    if (members > MAX_GROUP_SIZE)
      members = MAX_GROUP_SIZE;
    if (cursor >= members)
      cursor = 0;
    bpf_for(slot, 0, members) {
      __u32 pick = cursor + slot, member;
      __u64 age;

      if (pick >= members)
        pick -= members;
      if (pick >= MAX_GROUP_SIZE)
        continue;
      member = group_member[group][pick];
      /* The own queue is weighed on its own terms, not as a group member. */
      if (member == cpu || member >= queues)
        continue;
      if (!head_age(waiting_dsq(member), now, &age))
        continue;
      /* The scan starts at the cursor, so members whose heads are the same age
       * take turns instead of always losing to the earliest slot. */
      if (!group_head || age > group_wait) {
        group_wait = age;
        best = member;
        best_slot = pick;
        group_head = true;
      }
    }
  }
  own_first = own_head && (!group_head || own_wait + own_slack_ns >= group_wait);
  if (own_first && limit && granted >= limit)
    own_first = false;
  if (own_first) {
    result = admit_from(cpu, owner, cpu);
    if (result == ADMIT_GRANTED) {
      own_grants[cpu] = granted + 1;
      return;
    }
    if (result == ADMIT_SLOT_LOST)
      return;
  }
  if (group_head && group < MAX_GROUPS) {
    result = admit_from(cpu, owner, best);
    if (result == ADMIT_GRANTED) {
      own_grants[cpu] = 0;
      best_slot++;
      group_cursor[group] = best_slot >= members ? 0 : best_slot;
      return;
    }
    if (result == ADMIT_SLOT_LOST)
      return;
  }
  /* Age order and the count bound order the queues, they do not close the own
   * queue: with the group offering nothing this CPU may take, serving the own
   * queue still beats leaving the slot idle. The group gave nothing up, so a
   * grant here costs it nothing and the count starts over; leaving the count at
   * the bound would make every later dispatch repeat a scan of a group that has
   * nothing to give. */
  if (!own_first) {
    result = admit_from(cpu, owner, cpu);
    if (result == ADMIT_GRANTED) {
      own_grants[cpu] = 0;
      return;
    }
    if (result == ADMIT_SLOT_LOST)
      return;
  }
  start = admit_cursor;
  if (start >= queues)
    start = 0;
  bpf_for(probe, 0, queues) {
    __u32 index = start + probe;

    if (index >= queues)
      index -= queues;
    if (index == cpu)
      continue;
    result = admit_from(cpu, owner, index);
    if (result == ADMIT_GRANTED) {
      own_grants[cpu] = 0;
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
      __sync_fetch_and_add(&admission.demand, 1);
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
  /* Serve ordinary work alongside the reserved waiter. This also lets an
   * unadmitted fast-path holder run and unlock while the waiter is spinning. */
  bool moved = scx_bpf_dsq_move_to_local(NORMAL_DSQ);

  (void)prev;
  if (!lock_routing())
    return;
  if (moved)
    __sync_fetch_and_add(&admission.demand, -1);
  if (cpu < 0 || cpu >= MAX_CPUS)
    return;
  admit_waiter((__u32)cpu);
}

bool BPF_STRUCT_OPS(accordin_yield, struct task_struct *from, struct task_struct *to) {
  struct task_scx_ctx *tctx = lock_routing() ? task_ctx(from) : 0;
  __u32 cpu = bpf_get_smp_processor_id();
  volatile __u64 *owner = owner_slot(cpu);
  struct admission_word word;

  (void)to;
  /* Renew only our existing slot; userspace still confirms the new ticket. */
  if (!stats_only_mode && tctx && owner && user_state(from, &word) &&
      !(word.state & USER_CV) && (word.state & USER_FLAGS) == USER_WAITING &&
      tctx->admission_cpu == cpu + 1) {
    __u64 next = request_ticket(from, word.state);
    if (__sync_val_compare_and_swap(owner, tctx->ticket, next) == tctx->ticket)
      tctx->ticket = next;
  }
  from->scx.slice = 0;
  return false;
}

void BPF_STRUCT_OPS(accordin_tick, struct task_struct *p) {
  struct task_scx_ctx *tctx = lock_routing() ? task_ctx(p) : 0;
  struct admission_word word;

  if (stats_only_mode || !tctx || !user_state(p, &word))
    return;
  refresh_episode(p, tctx, word);
  /* An admitted thread that spins never empties its CPU, so ops.dispatch is
   * never called there and the global queue is never consumed. Ending the
   * slice hands the CPU to one queued ordinary task; the spinner keeps its
   * slot and is re-enqueued behind it. */
  if (tctx->admission_cpu &&
      ((word.state & USER_FLAGS) == USER_WAITING ||
       (word.state & USER_FLAGS) == USER_SPINNING) &&
      scx_bpf_dsq_nr_queued(NORMAL_DSQ))
    p->scx.slice = 0;
}

void BPF_STRUCT_OPS(accordin_stopping, struct task_struct *p, bool runnable) {
  struct task_scx_ctx *tctx = lock_routing() ? task_ctx(p) : 0;
  struct admission_word word;

  if (stats_only_mode || !tctx || !user_state(p, &word))
    return;
  refresh_episode(p, tctx, word);
  /* A holder or an existing raw-lock node must be allowed to resume. */
  if (!runnable && (word.state & USER_FLAGS) != USER_HELD &&
      (word.state & USER_FLAGS) != USER_SPINNING)
    release_slot(p, tctx);
}

/* Clear the entries a thread left behind, or the whole table when no tid is
 * named, and tally each of those into the counter that belongs to it.
 *
 * A thread may leave holding a slot the scheduler never recorded, and its
 * memory is gone by then, so the table itself is the only record left to clear.
 * Only the exchange that frees an entry asks its CPU to look for a waiter, and
 * only while a departing thread is what freed it: nothing follows a kick sent
 * as the scheduler is unloaded. */
__noinline int sweep_slots(__u32 tid) {
  __u32 cpu;

  /* Each tally goes straight to its counter: a running total in a variable has
   * to be tracked exactly by the verifier, which then cannot fold the walk. */
  bpf_for(cpu, 0, waiting_queues) {
    volatile __u64 *owner = owner_slot(cpu);
    __u64 value;

    if (!owner)
      continue;
    value = *owner;
    if (!value || (tid && (__u32)value != tid))
      continue;
    if (__sync_val_compare_and_swap(owner, value, 0) != value)
      continue;
    if (!tid) {
      __sync_fetch_and_add(&slots_left, 1);
      continue;
    }
    __sync_fetch_and_add(&slots_swept, 1);
    scx_bpf_kick_cpu(cpu, 0);
  }
  return 0;
}

void BPF_STRUCT_OPS(accordin_exit_task, struct task_struct *p,
                    struct scx_exit_task_args *args) {
  struct task_scx_ctx *tctx = task_ctx(p);
  __u32 tid = p->pid;

  (void)args;
  /* A wait that leaves without a flush or an expiry still leaves custody. */
  if (tctx && tctx->parked_at && __sync_lock_test_and_set(&tctx->parked_at, 0)) {
    __sync_fetch_and_add(&cv_drained, 1);
    __sync_fetch_and_sub(&cv_parked_now, 1);
  }
  /* The sweep frees every entry the recorded slot could name and the ones no
   * record ever reached, so a release of the record adds nothing; the record
   * itself goes with the storage below. A forked child holds its entries under
   * a tgid of its own, so the sweep is owed to every departing task. */
  sweep_slots(tid);
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
  scx_bpf_dump("accordin groups=%u own_limit=%u own_slack_ns=%llu peek=%u\n",
               group_count, own_limit, own_slack_ns, dsq_peek_ready);
  scx_bpf_dump("accordin demand=%d adopted=%llu swept=%llu left=%u\n",
               admission.demand, claims_adopted, slots_swept, slots_left);
  bpf_for(cpu, 0, waiting_queues) {
    volatile __u64 *owner = owner_slot(cpu);
    if (owner && *owner)
      scx_bpf_dump("accordin cpu=%u owner=%u\n", cpu, (__u32)*owner);
  }
  /* Only the CPUs a group claims are worth a line; the rest carry no mapping. */
  bpf_for(cpu, 0, MAX_CPUS) {
    __u32 group = cpu_group[cpu];
    if (group < MAX_GROUPS)
      scx_bpf_dump("accordin cpu=%u group=%u\n", cpu, group);
  }
}

/* The runtime derives the scan period from the custody limit; the fallback only
 * covers a scheduler loaded without one. */
static __always_inline __u64 cv_scan_period(void) {
  __u64 period = cv_scan_period_ns;

  return period ? period : 10000000ULL;
}

static int cv_scan(void *map, int *key, struct cv_timer_state *value) {
  int snapshot = admission.demand;
  int counted = scx_bpf_dsq_nr_queued(NORMAL_DSQ) + (int)nr_waiting();

  (void)map;
  (void)key;
  /* A task may leave a queue by a path that passes none of the sites which
   * discount it, so the advisory count is corrected against the queues here.
   * The correction is a difference and never a store: a store would erase the
   * enqueues made while the queues were being counted, leaving a populated bank
   * reading as empty. */
  __sync_fetch_and_add(&admission.demand, counted - snapshot);
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
  /* Which head read the loaded program took is only knowable here, so the
   * runtime is told rather than left to guess at the kernel's version. */
  dsq_peek_ready = bpf_ksym_exists(scx_bpf_dsq_peek);
  admission.enabled = !stats_only_mode;
  admission.active = !auto_admission;
  return 0;
}

void BPF_STRUCT_OPS(accordin_exit, struct scx_exit_info *ei) {
  struct cv_timer_state *timer = cv_timer();

  admission.enabled = 0;
  admission.demand = 0;
  /* Threads still running hold their slots legitimately at unload, so what the
   * sweep leaves in slots_left is the point of it; nothing reads the emptied
   * table afterwards, since the skeleton is destroyed and a fresh load starts
   * from a zeroed bss. */
  sweep_slots(0);
  if (timer)
    bpf_timer_cancel(&timer->timer);
  UEI_RECORD(uei, ei);
}

/* Hand every notified wait of the calling process to the lock admission queue
 * in one pass, and withdraw custody from the waits that outlived their limit.
 * Runs from a syscall, so it holds no rq lock and may move tasks between
 * queues. A released wait enters the admission queue at the place its park
 * stamp gives it, ahead of every lock request made after it parked. Each pass
 * is selected by the caller's flags. */
SEC("syscall")
int accordin_cv_flush(struct cv_flush_ctx *ctx) {
  struct cv_flush_tally *tally;
  struct task_struct *p;
  __u32 tgid = bpf_get_current_pid_tgid() >> 32;
  __u32 width = ctx->width, key = 0;
  __u64 iter_flags = (ctx->flags & CV_FLUSH_REV) ? SCX_DSQ_ITER_REV : 0;

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
      struct admission_word word;
      __u64 target, parked;
      bool moved;
      __u32 task_cpu;

      if (p->tgid != tgid)
        continue;
      tctx = task_ctx(p);
      if (!tctx || !tctx->parked_at)
        continue;
      task_cpu = scx_bpf_task_cpu(p);
      /* A released wait keeps the stamp it took when it parked, and the
       * admission queue is ordered by that stamp, so its place in line is the
       * wait it has already served rather than the moment it was handed over.
       * Where it goes in the queue is therefore no longer a choice the caller's
       * flags make. */
      target = waiting_dsq(task_cpu);
      /* Running in the process's own context, an unreadable word is not a
       * notification and cannot be confirmed later either. */
      if (!user_state(p, &word)) {
        target = NORMAL_DSQ;
      } else if ((word.state & USER_META) != USER_WAITING) {
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
        tctx->ticket = request_ticket(p, word.state);
      __sync_fetch_and_sub(&cv_parked_now, 1);
      /* A move can be refused while the wait is still queued, so put the claim
       * back and leave it to the next pass. Only a claim that cannot be put
       * back means the wait left the queue by another path, which counted it. */
      moved = target == NORMAL_DSQ
                  ? __COMPAT_scx_bpf_dsq_move(BPF_FOR_EACH_ITER, p, target, 0)
                  : __COMPAT_scx_bpf_dsq_move_vtime(BPF_FOR_EACH_ITER, p, target,
                                                    0);
      if (!moved) {
        if (__sync_bool_compare_and_swap(&tctx->parked_at, 0, parked))
          __sync_fetch_and_add(&cv_parked_now, 1);
        else
          __sync_fetch_and_add(&cv_drained, 1);
      } else if (target == NORMAL_DSQ) {
        __sync_fetch_and_add(&cv_expired, 1);
        __sync_fetch_and_add(&admission.demand, 1);
        tally->expired++;
      } else {
        __sync_fetch_and_add(&cv_flushed, 1);
        __sync_fetch_and_add(&admission.demand, 1);
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
