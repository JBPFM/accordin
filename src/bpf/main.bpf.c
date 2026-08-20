/* SPDX-License-Identifier: GPL-2.0-only */
#include <scx/common.bpf.h>

#include "intf.h"
#include "inactive_select.h"

char _license[] SEC("license") = "GPL";

UEI_DEFINE(uei);

#include "maps.bpf.h"

static __always_inline bool valid_cpu(s32 cpu) {
  return cpu >= 0 && cpu < MAX_CPUS;
}

static __always_inline bool stats_only_enabled(void) {
  return stats_only_mode != 0;
}

static __always_inline bool single_lock_enabled(void) {
  return single_lock_mode != 0;
}

static __always_inline bool debug_counters_enabled(void) {
  return debug_counters_mode != 0;
}

static __always_inline bool registered_threads_active(void) {
  return registered_thread_count != 0;
}

static __always_inline void bump_counter(volatile __u64 *counter) {
  __sync_fetch_and_add(counter, 1);
}

/* For sites that are not already inside a hoisted debug gate. */
static __always_inline void bump_debug_counter(volatile __u64 *counter) {
  if (debug_counters_enabled())
    bump_counter(counter);
}

static __always_inline bool inactive_dispatch_needed(void) {
  return inactive_empty_seq != inactive_enqueue_seq;
}

static __always_inline bool inactive_dispatch_needed_for(__u32 inactive_seq) {
  return inactive_empty_seq != inactive_seq;
}

/* Enqueue callbacks run concurrently on every CPU, so the bump has to be atomic
 * or a lost increment leaves the gate closed over a queued task. */
static __always_inline void record_inactive_enqueue(void) {
  __sync_fetch_and_add(&inactive_enqueue_seq, 1);
}

static __always_inline void record_inactive_scan_empty(__u32 inactive_seq) {
  inactive_empty_seq = inactive_seq;
}

static __always_inline bool normal_dispatch_needed_for(__u32 normal_seq) {
  return normal_empty_seq != normal_seq;
}

static __always_inline bool normal_dispatch_needed(void) {
  return normal_dispatch_needed_for(normal_enqueue_seq);
}

static __always_inline void record_normal_enqueue(void) {
  __sync_fetch_and_add(&normal_enqueue_seq, 1);
}

static __always_inline void record_normal_scan_empty(__u32 normal_seq) {
  normal_empty_seq = normal_seq;
}

static __always_inline bool cv_route_active(void) {
  return cv_route_enabled != 0;
}

/* Whether any thread sits on a cvready queue. A park counts before the drain
 * that takes the thread off the queue does, and nothing else removes a task
 * from these queues, so the two counters differ exactly while one is occupied;
 * a task whose insert has not landed yet is already counted, so it is never
 * missed. Being exact is what lets this gate stand alone: the cvready selection
 * probes a window of the class space, and an empty verdict from one scan cannot
 * speak for the classes it never looked at. */
static __always_inline bool cvready_queues_occupied(void) {
  return cvready_enqueue_seq != cvready_drained_count;
}

static __always_inline bool cvready_dispatch_needed(void) {
  return cv_route_active() && cvready_queues_occupied();
}

static __always_inline void record_cvready_enqueue(void) {
  __sync_fetch_and_add(&cvready_enqueue_seq, 1);
}

static __always_inline void record_cvready_drained(void) {
  __sync_fetch_and_add(&cvready_drained_count, 1);
}

static __always_inline bool task_cpumask_allows(struct task_struct *p,
                                                __u32 cpu) {
  if (cpu >= MAX_CPUS)
    return false;

  return bpf_cpumask_test_cpu(cpu, p->cpus_ptr);
}

static __always_inline bool task_cpu_allowed(struct task_struct *p, __u32 cpu) {
  if (cpu >= MAX_CPUS)
    return false;

  return task_cpumask_allows(p, cpu);
}

static __always_inline bool valid_lock_id(__u32 lock_id) {
  return lock_id != UNMANAGED_LOCK_ID && lock_id < MAX_LOCK_CLASSES;
}

static __always_inline __u32 user_lock_id(__u32 user_ctx_word) {
  return (user_ctx_word & ~USER_ADMISSION_FLAG_MASK) >>
         USER_ADMISSION_LOCK_ID_SHIFT;
}

static __always_inline __u32 effective_lock_id(__u32 user_ctx_word) {
  if (single_lock_enabled())
    return 1;

  return user_lock_id(user_ctx_word);
}

static __always_inline __u64 inactive_dsq_id(__u32 lock_id) {
  return INACTIVE_DSQ_BASE + lock_id;
}

static __always_inline __u64 cvready_dsq_id(__u32 lock_id) {
  return CVREADY_DSQ_BASE + lock_id;
}

static __always_inline void init_task_ctx_if_needed(struct task_scx_ctx *task_ctx) {
  if (task_ctx->initialized)
    return;

  task_ctx->initialized = 1;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
}

static __always_inline struct task_scx_ctx *lookup_task_ctx(struct task_struct *p) {
  struct task_scx_ctx *task_ctx;

  task_ctx = bpf_task_storage_get(&task_ctx_map, p, 0, 0);
  if (!task_ctx)
    return 0;

  init_task_ctx_if_needed(task_ctx);
  return task_ctx;
}

static __always_inline struct task_scx_ctx *get_task_ctx(struct task_struct *p) {
  struct task_scx_ctx *task_ctx;

  task_ctx =
      bpf_task_storage_get(&task_ctx_map, p, 0, BPF_LOCAL_STORAGE_GET_F_CREATE);
  if (!task_ctx)
    return 0;

  init_task_ctx_if_needed(task_ctx);
  return task_ctx;
}

/* Bounds a per-CPU array access for the verifier. barrier_var comes before the
 * bound so the compiler cannot fold the check away against a range it derived
 * elsewhere: the register the verifier then has bounds for is the one the index
 * is taken from, which a caller-side range check on a copy would not be. */
#define DEFINE_CPU_SLOT_LOOKUP(fn, array)                                      \
  static __always_inline volatile __u32 *fn(__u32 cpu) {                       \
    __u32 idx = cpu;                                                           \
                                                                               \
    barrier_var(idx);                                                          \
    if (idx >= MAX_CPUS)                                                       \
      return 0;                                                                \
                                                                               \
    return &array[idx];                                                        \
  }

DEFINE_CPU_SLOT_LOOKUP(lookup_cpu_owner, cpu_admission_owner)
DEFINE_CPU_SLOT_LOOKUP(lookup_cpu_last_inactive_lock, cpu_last_inactive_lock)
DEFINE_CPU_SLOT_LOOKUP(lookup_cpu_inactive_dispatch_count,
                       cpu_inactive_dispatch_count)
DEFINE_CPU_SLOT_LOOKUP(lookup_cpu_inactive_probe_cursor,
                       cpu_inactive_probe_cursor)
DEFINE_CPU_SLOT_LOOKUP(lookup_cpu_cvready_streak, cpu_cvready_streak)
DEFINE_CPU_SLOT_LOOKUP(lookup_cpu_last_cvready_lock, cpu_last_cvready_lock)
DEFINE_CPU_SLOT_LOOKUP(lookup_cpu_cvready_probe_cursor, cpu_cvready_probe_cursor)

/* Whether this CPU may still take a cvready task ahead of the class queues. A
 * limit of 0 disables the priority outright, so no CPU ever qualifies. Only the
 * preference is bounded here: the periodic forced pass moves a cvready task
 * without asking, and spends no budget when it does. */
static __always_inline bool cvready_priority_available(__u32 cpu) {
  volatile __u32 *streak;

  if (cv_priority_streak_limit == 0)
    return false;

  streak = lookup_cpu_cvready_streak(cpu);
  return !streak || *streak < cv_priority_streak_limit;
}

static __always_inline void record_cvready_priority_dispatch(__u32 cpu) {
  volatile __u32 *streak = lookup_cpu_cvready_streak(cpu);

  if (streak && *streak < cv_priority_streak_limit)
    *streak += 1;
}

/* Any dispatch that served the class queues or the normal queue ends the
 * streak: the budget exists to bound how long cvready keeps a CPU, and it has
 * just been handed back. */
static __always_inline void reset_cvready_streak(__u32 cpu) {
  volatile __u32 *streak = lookup_cpu_cvready_streak(cpu);

  if (streak)
    *streak = 0;
}

static __always_inline void record_cvready_dispatch_lock(__u32 cpu,
                                                         __u32 lock_id) {
  volatile __u32 *last_lock;

  if (!valid_lock_id(lock_id))
    return;

  last_lock = lookup_cpu_last_cvready_lock(cpu);
  if (last_lock)
    *last_lock = lock_id;
}

static __always_inline bool inactive_dispatch_budget_available(__u32 cpu) {
  volatile __u32 *count = lookup_cpu_inactive_dispatch_count(cpu);

  return !count || *count < INACTIVE_DISPATCH_BURST;
}

static __always_inline bool inactive_dispatch_cpu_available(__u32 cpu) {
  volatile __u32 *owner = lookup_cpu_owner(cpu);

  return owner && !*owner;
}

static __always_inline void record_inactive_dispatch(__u32 cpu) {
  volatile __u32 *count = lookup_cpu_inactive_dispatch_count(cpu);

  if (count && *count < INACTIVE_DISPATCH_BURST)
    *count += 1;
}

static __always_inline void reset_inactive_dispatch_budget(__u32 cpu) {
  volatile __u32 *count = lookup_cpu_inactive_dispatch_count(cpu);

  if (count)
    *count = 0;
}

static __always_inline void record_inactive_dispatch_lock(__u32 cpu,
                                                          __u32 lock_id) {
  volatile __u32 *last_lock;

  if (!valid_lock_id(lock_id))
    return;

  last_lock = lookup_cpu_last_inactive_lock(cpu);
  if (last_lock)
    *last_lock = lock_id;
}

/* Single sink for enqueue evidence: every routing decision lands here exactly
 * once, so a hinted wakeup contributes exactly one wake_consumed_* class. */
static __always_inline void
record_enqueue_state(struct task_scx_ctx *task_ctx, __u64 dsq_id,
                     __u32 lock_id, __u32 path, __u32 cpu,
                     __u32 user_ctx_word, bool hinted) {
  if (!debug_counters_enabled())
    return;

  if (hinted) {
    switch (path) {
    case ENQ_PATH_ADMISSION_LOCAL:
    case ENQ_PATH_SLOW_GRANTED_LOCAL:
      bump_counter(&wake_consumed_granted);
      break;
    case ENQ_PATH_SLOW_INACTIVE:
    case ENQ_PATH_FORCE_INACTIVE:
      bump_counter(&wake_consumed_inactive);
      break;
    case ENQ_PATH_NORMAL_DSQ:
    case ENQ_PATH_NORMAL_LOCAL_FAST:
      bump_counter(&wake_consumed_normal);
      break;
    default:
      break;
    }
  }

  if (!task_ctx)
    return;

  task_ctx->last_enqueue_dsq = dsq_id;
  task_ctx->last_enqueue_lock_id = lock_id;
  task_ctx->last_enqueue_path = path;
  task_ctx->last_enqueue_cpu = cpu;
  task_ctx->last_user_ctx_word = user_ctx_word;
}

static __always_inline bool refresh_user_ctx_ptr(struct task_struct *p,
                                                 struct task_scx_ctx *task_ctx) {
  __u32 pid = p->pid;
  __u64 *user_ctx_ptr;

  if (task_ctx->user_ctx_ptr)
    return true;

  user_ctx_ptr = bpf_map_lookup_elem(&thread_ctx_addr_map, &pid);
  if (!user_ctx_ptr || !*user_ctx_ptr)
    return false;

  task_ctx->user_ctx_ptr = *user_ctx_ptr;
  return true;
}

static __always_inline bool read_user_ctx(struct task_struct *p,
                                          struct task_scx_ctx *task_ctx,
                                          __u32 *user_ctx_word) {
  if (!refresh_user_ctx_ptr(p, task_ctx))
    return false;

  if (bpf_probe_read_user(user_ctx_word, sizeof(*user_ctx_word),
                          (const void *)(unsigned long)task_ctx->user_ctx_ptr))
    return false;

  return true;
}

static __always_inline bool user_slow_path_pending(__u32 user_ctx_word) {
  return user_ctx_word & USER_ADMISSION_SLOW_PATH_PENDING;
}

static __always_inline bool user_in_critical_section(__u32 user_ctx_word) {
  return user_ctx_word & USER_ADMISSION_IN_CRITICAL_SECTION;
}

static __always_inline bool user_token_consumed(__u32 user_ctx_word) {
  return user_ctx_word & USER_ADMISSION_TOKEN_CONSUMED;
}

static __always_inline bool user_cv_sleep(__u32 user_ctx_word) {
  return user_ctx_word & USER_ADMISSION_CV_SLEEP;
}

/* A word with neither flag set says the thread is done with the lock. The
 * cond-sleep bit is set with both flags clear by construction, so it would fall
 * into this shape and lose the class it names; it is excluded here and every
 * caller that wants the release for a cond sleeper asks for it explicitly. */
static __always_inline bool user_explicit_release(__u32 user_ctx_word) {
  return !user_slow_path_pending(user_ctx_word) &&
         !user_in_critical_section(user_ctx_word) &&
         !user_cv_sleep(user_ctx_word);
}

/* Wakeup carrying the cond-reacquire signature. A failed user-word read leaves
 * the word at zero, so an unregistered task never matches. */
static __always_inline bool wake_hinted(__u64 enq_flags, __u32 user_ctx_word) {
  return (enq_flags & SCX_ENQ_WAKEUP) &&
         user_slow_path_pending(user_ctx_word) &&
         user_token_consumed(user_ctx_word);
}

/* Bounds a per-class array access for the verifier. barrier_var comes before
 * the bound so the compiler cannot fold the check away when a caller proved
 * lock_id < MAX_LOCK_CLASSES on a different register. */
#define DEFINE_CLASS_SLOT_LOOKUP(fn, type, array)                              \
  static __always_inline volatile type *fn(__u32 lock_id) {                    \
    __u32 idx = lock_id;                                                       \
                                                                               \
    barrier_var(idx);                                                          \
    if (idx >= MAX_LOCK_CLASSES)                                               \
      return 0;                                                                \
                                                                               \
    return &array[idx];                                                        \
  }

DEFINE_CLASS_SLOT_LOOKUP(lookup_class_active, __s32, class_active)
DEFINE_CLASS_SLOT_LOOKUP(lookup_class_active_peak, __u32, class_active_peak)
DEFINE_CLASS_SLOT_LOOKUP(lookup_class_width, __u32, class_width)
DEFINE_CLASS_SLOT_LOOKUP(lookup_class_inactive_depth, __u32,
                         class_inactive_depth)
DEFINE_CLASS_SLOT_LOOKUP(lookup_class_inactive_depth_peak, __u32,
                         class_inactive_depth_peak)

static __always_inline void account_admission_grant(__u32 lock_id) {
  volatile __s32 *active = lookup_class_active(lock_id);
  volatile __u32 *peak;
  __s32 new_active;

  if (!active)
    return;

  /* Atomic: grant and release race across CPUs for the same class. */
  new_active = __sync_fetch_and_add(active, 1) + 1;

  /* Peak is advisory (controller reads-and-clears); a plain store is fine. */
  peak = lookup_class_active_peak(lock_id);
  if (peak && new_active > 0 && (__u32)new_active > *peak)
    *peak = (__u32)new_active;
}

static __always_inline void account_admission_release(__u32 lock_id) {
  volatile __s32 *active = lookup_class_active(lock_id);
  __s32 old;

  if (!active)
    return;

  /* Atomic decrement; balanced accounting keeps old >= 1, so the floor
   * fixup and underflow diagnostic fire only on genuine imbalance. */
  old = __sync_fetch_and_add(active, -1);
  if (old <= 0) {
    __sync_fetch_and_add(active, 1);
    __sync_fetch_and_add(&class_active_underflow_events, 1);
  }
}

/* The two width-slot helpers are the only pair that may move class_active, and
 * task_ctx->width_slot_held is the token that makes them alternate: taking a
 * slot is the sole increment of the class counter, returning it the sole
 * decrement, at most once each per admission episode. */
static __always_inline void take_width_slot(struct task_scx_ctx *task_ctx,
                                            __u32 lock_id) {
  task_ctx->width_slot_held = 1;
  account_admission_grant(lock_id);
}

static __always_inline void release_width_slot(struct task_scx_ctx *task_ctx) {
  if (!task_ctx->width_slot_held)
    return;

  task_ctx->width_slot_held = 0;
  account_admission_release(task_ctx->admitted_class);
}

/* The inactive-depth counters exist only for width control, so each one owns
 * its feature gate rather than relying on every call site to repeat it. */
static __always_inline void account_inactive_depth_enqueue(__u32 lock_id) {
  volatile __u32 *depth;
  volatile __u32 *peak;
  __u32 new_depth;

  if (!width_control_enabled)
    return;

  depth = lookup_class_inactive_depth(lock_id);
  if (!depth)
    return;

  new_depth = __sync_fetch_and_add(depth, 1) + 1;

  /* Peak is advisory (controller reads-and-clears); a plain store is fine. */
  peak = lookup_class_inactive_depth_peak(lock_id);
  if (peak && new_depth > *peak)
    *peak = new_depth;
}

static __always_inline void account_inactive_depth_release(__u32 lock_id) {
  volatile __u32 *depth;
  __u32 old;

  if (!width_control_enabled)
    return;

  depth = lookup_class_inactive_depth(lock_id);
  if (!depth)
    return;

  old = __sync_fetch_and_add(depth, -1);
  if (old == 0)
    __sync_fetch_and_add(depth, 1);
}

static __always_inline void reset_inactive_depth(__u32 lock_id) {
  volatile __u32 *depth;

  if (!width_control_enabled)
    return;

  depth = lookup_class_inactive_depth(lock_id);
  if (depth)
    *depth = 0;
}

/* Sole expression of the width rule: a class may take another CPU while fewer
 * waiters are admitted for it than its published width. Unlimited (width 0), an
 * unmanaged id, and the feature being off all mean "no cap". */
static __always_inline bool class_dispatch_eligible(__u32 lock_id) {
  volatile __u32 *width;
  volatile __s32 *active;
  __u32 w;

  if (!width_control_enabled || !valid_lock_id(lock_id))
    return true;

  width = lookup_class_width(lock_id);
  active = lookup_class_active(lock_id);
  if (!width || !active)
    return true;

  w = *width;
  if (w == 0)
    return true;

  return *active < (__s32)w;
}

/* Stand-in deficit for an unlimited class, large enough to outrank any real
 * width so an uncapped class with waiters is always preferred. */
#define INACTIVE_DEFICIT_UNLIMITED (1 << 24)

/* Widest-deficit class among the INACTIVE_DEFICIT_PROBE_WIDTH classes starting
 * at this CPU's probe cursor, or UNMANAGED_LOCK_ID when the window holds no
 * eligible class with waiters. The window rotates over the ranks userspace
 * reports as allocated rather than the whole class space, so it does not spend
 * its width on ranks that hold no class.
 *
 * The window is a preference, not the guarantee of service: the caller falls
 * back to a scan that visits every class, so a class the window skipped is
 * still dispatched, only without deficit ordering. The cursor advances on every
 * call, so a class stays outside the window for at most a full sweep. */
static __always_inline __u32 select_inactive_deficit_class(__u32 dispatch_cpu) {
  volatile __u32 *cursor = lookup_cpu_inactive_probe_cursor(dispatch_cpu);
  __u32 span = inactive_probe_ranks(inactive_probe_span);
  __u32 best_lock = UNMANAGED_LOCK_ID;
  __s32 best_deficit = 0;
  __u32 rank = 0;
  __u32 probe;

  if (cursor) {
    rank = *cursor;
    if (rank >= span)
      rank = 0;
    *cursor = inactive_next_probe_rank(rank, span);
  }

#pragma unroll
  for (probe = 0; probe < INACTIVE_DEFICIT_PROBE_WIDTH; probe++) {
    __u32 lock_id = inactive_lock_at_rank(rank);
    volatile __u32 *depth = lookup_class_inactive_depth(lock_id);
    volatile __u32 *width;
    volatile __s32 *active;
    __u32 w;
    __s32 in_flight;
    __s32 deficit;

    rank = inactive_wrap_rank(rank, span);

    /* Most classes are idle, so settle the cheap test before deriving the
     * width and active slots for the ones that are not. */
    if (!depth || *depth == 0)
      continue;

    width = lookup_class_width(lock_id);
    active = lookup_class_active(lock_id);
    if (!width || !active)
      continue;

    w = *width;
    in_flight = *active;
    if (w != 0 && in_flight >= (__s32)w)
      continue;

    deficit = (w == 0) ? INACTIVE_DEFICIT_UNLIMITED : ((__s32)w - in_flight);
    if (best_lock == UNMANAGED_LOCK_ID || deficit > best_deficit) {
      best_deficit = deficit;
      best_lock = lock_id;
    }
  }

  return best_lock;
}

/* Dispatch drain outcome. BLOCKED means a nonempty class DSQ was skipped only
 * for width-ineligibility; the caller must not record the inactive scan as
 * empty, or parked waiters stay stranded until the next enqueue bumps the seq. */
enum inactive_move_outcome {
  INACTIVE_MOVE_EMPTY = 0,
  INACTIVE_MOVE_MOVED = 1,
  INACTIVE_MOVE_BLOCKED = 2,
};

/* A class DSQ that still holds tasks was not skipped because it ran dry, so its
 * outcome is BLOCKED rather than EMPTY. Only EMPTY may arm the scan-empty
 * sequence gate, which is global and would otherwise suppress dispatch on the
 * CPUs that can still run those tasks. */
static __always_inline enum inactive_move_outcome
inactive_move_missed(__u64 dsq_id) {
  return scx_bpf_dsq_nr_queued(dsq_id) > 0 ? INACTIVE_MOVE_BLOCKED
                                           : INACTIVE_MOVE_EMPTY;
}

/* `force` waives width control for this attempt and nothing else: the
 * nr_queued/BLOCKED reporting stays as it is so a forced scan cannot arm the
 * scan-empty gate over a class that still holds tasks. */
static __always_inline enum inactive_move_outcome
move_inactive_to_local(__u32 dispatch_cpu, __u32 lock_id, bool force) {
  __u64 dsq_id;

  if (!valid_lock_id(lock_id))
    return INACTIVE_MOVE_EMPTY;

  dsq_id = inactive_dsq_id(lock_id);

  if (!force && !class_dispatch_eligible(lock_id))
    return inactive_move_missed(dsq_id);

  if (scx_bpf_dsq_move_to_local(dsq_id)) {
    record_inactive_dispatch_lock(dispatch_cpu, lock_id);
    account_inactive_depth_release(lock_id);
    return INACTIVE_MOVE_MOVED;
  }

  /* A failed move means either an empty queue or tasks whose cpumask excludes
   * this CPU; only the empty case may clear the class's tracked depth. */
  if (scx_bpf_dsq_nr_queued(dsq_id) > 0)
    return INACTIVE_MOVE_BLOCKED;

  reset_inactive_depth(lock_id);
  return INACTIVE_MOVE_EMPTY;
}

/* Attempts one class and reports whether it moved, latching BLOCKED into the
 * caller's flag. Every candidate in a scan goes through here so the tri-state
 * contract is honoured once instead of at each attempt. */
static __always_inline bool try_move_inactive(__u32 dispatch_cpu, __u32 lock_id,
                                              bool force, bool *blocked) {
  enum inactive_move_outcome outcome =
      move_inactive_to_local(dispatch_cpu, lock_id, force);

  if (outcome == INACTIVE_MOVE_BLOCKED)
    *blocked = true;

  return outcome == INACTIVE_MOVE_MOVED;
}

/* Global, so the verifier checks the unrolled class scan once against unknown
 * arguments instead of re-exploring it for every combination of dispatch-side
 * conditions that can reach a call site. The unrolled scan is the largest piece
 * of code dispatch runs and dispatch reaches it from several distinct states,
 * so inlining it multiplies those states into its own branch space, and that
 * product is what the instruction budget cannot pay for. Arguments and the
 * result are scalars because that is what a global subprogram may take and
 * return. */
__noinline enum inactive_move_outcome
move_selected_inactive_to_local(__u32 dispatch_cpu, bool force) {
  volatile __u32 *last_lock_ptr;
  __u32 previous_lock_id = UNMANAGED_LOCK_ID;
  __u32 random = bpf_get_prandom_u32();
  __u32 random_start = random / INACTIVE_PROBABILITY_SCALE;
  bool prefer_previous =
      inactive_prefer_previous_lock(random, inactive_previous_lock_percent);
  bool blocked = false;
  __u32 offset;

  last_lock_ptr = lookup_cpu_last_inactive_lock(dispatch_cpu);
  if (last_lock_ptr)
    previous_lock_id = *last_lock_ptr;

  if (valid_lock_id(previous_lock_id) && prefer_previous &&
      try_move_inactive(dispatch_cpu, previous_lock_id, force, &blocked))
    return INACTIVE_MOVE_MOVED;

  if (width_control_enabled) {
    __u32 best = select_inactive_deficit_class(dispatch_cpu);

    if (valid_lock_id(best) &&
        try_move_inactive(dispatch_cpu, best, force, &blocked))
      return INACTIVE_MOVE_MOVED;
  }

#pragma unroll
  for (offset = 0; offset < MAX_LOCK_CLASSES; offset++) {
    __u32 lock_id =
        inactive_other_lock_at(previous_lock_id, random_start, offset);

    if (!valid_lock_id(lock_id))
      break;

    if (try_move_inactive(dispatch_cpu, lock_id, force, &blocked))
      return INACTIVE_MOVE_MOVED;
  }

  if (valid_lock_id(previous_lock_id) && !prefer_previous &&
      try_move_inactive(dispatch_cpu, previous_lock_id, force, &blocked))
    return INACTIVE_MOVE_MOVED;

  return blocked ? INACTIVE_MOVE_BLOCKED : INACTIVE_MOVE_EMPTY;
}

/* One attempt on a cvready class queue. `force` waives width control only.
 * There is no empty-or-blocked verdict to report as the inactive drain has:
 * cvready occupancy is counted at the park and the drain, so no caller has to
 * tell a queue that ran dry from one this CPU may not run, and the queue count
 * that distinguishes them is not read.
 *
 * No inactive-depth accounting happens here: class_inactive_depth is what the
 * deficit probe and the width controller read as "waiters queued for the lock",
 * and a cond waiter parked here has not asked for admission yet. */
static __always_inline bool move_cvready_to_local(__u32 dispatch_cpu,
                                                  __u32 lock_id, bool force) {
  if (!valid_lock_id(lock_id))
    return false;

  if (!force && !class_dispatch_eligible(lock_id))
    return false;

  if (!scx_bpf_dsq_move_to_local(cvready_dsq_id(lock_id)))
    return false;

  record_cvready_dispatch_lock(dispatch_cpu, lock_id);
  record_cvready_drained();
  return true;
}

/* Selects the cvready class this CPU drains: the class it was last pointed at,
 * then a window of CVREADY_PROBE_WIDTH ranks from the CPU's rotating cursor.
 * Unlike the inactive selection there is no full-space fallback scan behind it,
 * and there must not be one: the measured cost of widening this window rules
 * out a second unrolled sweep over the class space beside the inactive one. The
 * rotating cursor is what still reaches every class, one window per call; the
 * class a park pointed this CPU at is reached by the first attempt whatever the
 * cursor is doing.
 *
 * Global for the same reason the inactive selection is: verified once against
 * unknown arguments instead of once per dispatch state that reaches it. */
__noinline bool move_selected_cvready_to_local(__u32 dispatch_cpu, bool force) {
  volatile __u32 *cursor = lookup_cpu_cvready_probe_cursor(dispatch_cpu);
  volatile __u32 *last_lock_ptr;
  __u32 previous_lock_id = UNMANAGED_LOCK_ID;
  __u32 span = inactive_probe_ranks(inactive_probe_span);
  __u32 rank = 0;
  __u32 probe;

  last_lock_ptr = lookup_cpu_last_cvready_lock(dispatch_cpu);
  if (last_lock_ptr)
    previous_lock_id = *last_lock_ptr;

  /* A signal usually releases several waiters of the same lock, so the class
   * this CPU served last is the likeliest to still hold one. */
  if (valid_lock_id(previous_lock_id) &&
      move_cvready_to_local(dispatch_cpu, previous_lock_id, force))
    return true;

  if (cursor) {
    rank = *cursor;
    if (rank >= span)
      rank = 0;
    *cursor = cvready_next_probe_rank(rank, span);
  }

#pragma unroll
  for (probe = 0; probe < CVREADY_PROBE_WIDTH; probe++) {
    __u32 lock_id = inactive_lock_at_rank(rank);

    rank = inactive_wrap_rank(rank, span);

    if (lock_id == previous_lock_id)
      continue;

    if (move_cvready_to_local(dispatch_cpu, lock_id, force))
      return true;
  }

  return false;
}

static __always_inline void clear_admission_state(struct task_scx_ctx *task_ctx) {
  task_ctx->holds_admission = 0;
  task_ctx->admitted_class = 0;
  task_ctx->cv_admitted = 0;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->force_inactive_wait = 0;
  task_ctx->admission_cpu = ADMISSION_CPU_NONE;
}

static __always_inline void release_admission(struct task_struct *p,
                                              struct task_scx_ctx *task_ctx) {
  __u32 cpu = task_ctx->admission_cpu;
  __u32 pid = p->pid;
  volatile __u32 *owner;

  owner = lookup_cpu_owner(cpu);
  if (owner && *owner == pid)
    *owner = 0;

  /* Returns the slot only if the episode still carries one; a task that reached
   * its critical section gave it back there. */
  release_width_slot(task_ctx);

  clear_admission_state(task_ctx);

  if (cpu < MAX_CPUS)
    scx_bpf_kick_cpu(cpu, 0);
}

/* `in_critical_section` says the task already owns the lock at grant time. Such
 * a task runs whatever the width says, so its episode starts without a slot;
 * every other grant is a waiter and takes one. */
static __always_inline bool grant_admission(struct task_struct *p,
                                            struct task_scx_ctx *task_ctx,
                                            __u32 lock_id, __u32 cpu,
                                            bool in_critical_section) {
  __u32 pid = p->pid;
  bool was_held = task_ctx->holds_admission;
  volatile __u32 *owner;

  if (!valid_lock_id(lock_id))
    return false;

  owner = lookup_cpu_owner(cpu);
  if (!owner)
    return false;

  if (*owner && *owner != pid)
    return false;

  *owner = pid;
  task_ctx->holds_admission = 1;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->force_inactive_wait = 0;
  task_ctx->admission_cpu = cpu;
  if (!was_held) {
    task_ctx->admitted_class = lock_id;
    task_ctx->width_slot_held = 0;
    if (!in_critical_section)
      take_width_slot(task_ctx, lock_id);
  }
  return true;
}

static __always_inline __u32 pick_allowed_cpu(struct task_struct *p,
                                              s32 fallback_cpu) {
  s32 cpu;

  if (valid_cpu(fallback_cpu) && task_cpu_allowed(p, (__u32)fallback_cpu))
    return fallback_cpu;

  cpu = scx_bpf_task_cpu(p);
  if (valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
    return cpu;

  cpu = scx_bpf_pick_idle_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
    return cpu;

  cpu = scx_bpf_pick_any_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
    return cpu;

  return ADMISSION_CPU_NONE;
}

static __always_inline __u32 pick_task_cpu(struct task_struct *p,
                                           s32 fallback_cpu) {
  s32 cpu;

  if (valid_cpu(fallback_cpu) && task_cpumask_allows(p, (__u32)fallback_cpu))
    return fallback_cpu;

  cpu = scx_bpf_task_cpu(p);
  if (valid_cpu(cpu) && task_cpumask_allows(p, (__u32)cpu))
    return cpu;

  cpu = scx_bpf_pick_idle_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu))
    return cpu;

  cpu = scx_bpf_pick_any_cpu(p->cpus_ptr, 0);
  if (valid_cpu(cpu))
    return cpu;

  return 0;
}

static __always_inline s32 select_cpu_without_admission(struct task_struct *p,
                                                        s32 prev_cpu,
                                                        u64 wake_flags) {
  bool is_idle = false;
  s32 cpu;

  cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);
  if (!valid_cpu(cpu) || !task_cpumask_allows(p, (__u32)cpu)) {
    is_idle = false;
    cpu = pick_task_cpu(p, prev_cpu);
  }

  if (is_idle && valid_cpu(cpu) && task_cpumask_allows(p, (__u32)cpu) &&
      !inactive_dispatch_needed() && !normal_dispatch_needed() &&
      !cvready_dispatch_needed())
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

  return cpu;
}

static __always_inline void enqueue_normal_dsq_recorded(
    struct task_struct *p, struct task_scx_ctx *task_ctx, __u64 enq_flags,
    __u32 lock_id, __u32 user_ctx_word) {
  record_enqueue_state(task_ctx, NORMAL_DSQ_ID, lock_id, ENQ_PATH_NORMAL_DSQ,
                       ADMISSION_CPU_NONE, user_ctx_word,
                       wake_hinted(enq_flags, user_ctx_word));
  scx_bpf_dsq_insert(p, NORMAL_DSQ_ID, SCX_SLICE_DFL, enq_flags);
  record_normal_enqueue();
}

static __always_inline void enqueue_normal_dsq(struct task_struct *p,
                                               __u64 enq_flags) {
  enqueue_normal_dsq_recorded(p, 0, enq_flags, UNMANAGED_LOCK_ID, 0);
}

static __always_inline bool enqueue_normal_local_fast_recorded(
    struct task_struct *p, struct task_scx_ctx *task_ctx, __u64 enq_flags,
    __u32 lock_id, __u32 user_ctx_word) {
  __u32 cpu;

  if (inactive_dispatch_needed() || normal_dispatch_needed() ||
      cvready_dispatch_needed())
    return false;

  cpu = pick_task_cpu(p, -1);
  if (!valid_cpu(cpu) || !task_cpumask_allows(p, cpu))
    return false;

  record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | cpu, lock_id,
                       ENQ_PATH_NORMAL_LOCAL_FAST, cpu, user_ctx_word,
                       wake_hinted(enq_flags, user_ctx_word));
  scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
  return true;
}

static __always_inline bool enqueue_normal_local_fast(struct task_struct *p,
                                                      __u64 enq_flags) {
  return enqueue_normal_local_fast_recorded(p, 0, enq_flags, UNMANAGED_LOCK_ID,
                                            0);
}

static __always_inline __u32 requested_cpu(struct task_struct *p,
                                           struct task_scx_ctx *task_ctx,
                                           s32 fallback_cpu) {
  if (task_ctx->admission_cpu < MAX_CPUS &&
      task_cpu_allowed(p, task_ctx->admission_cpu))
    return task_ctx->admission_cpu;

  return pick_allowed_cpu(p, fallback_cpu);
}

static __always_inline void clear_invalid_admission_cpu(
    struct task_struct *p, struct task_scx_ctx *task_ctx) {
  if (task_ctx->admission_cpu < MAX_CPUS &&
      !task_cpu_allowed(p, task_ctx->admission_cpu))
    release_admission(p, task_ctx);
}

static __always_inline bool slow_path_requested(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (task_ctx->holds_admission)
    return false;

  if (have_user && user_slow_path_pending(user_ctx_word)) {
    if (single_lock_enabled())
      return true;

    return valid_lock_id(user_lock_id(user_ctx_word));
  }

  return false;
}

static __always_inline bool consumed_token_reuse_requested(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  return task_ctx->holds_admission && have_user &&
         user_slow_path_pending(user_ctx_word) && user_token_consumed(user_ctx_word);
}

static __always_inline bool should_release_from_user(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (!task_ctx->holds_admission || !have_user)
    return false;

  return user_explicit_release(user_ctx_word);
}

/* A cond sleeper has already dropped the managed lock and is on its way into a
 * futex wait, so the token it still carries buys nothing and the CPU it pins is
 * wanted by whoever it woke. The class the word names describes the re-acquire
 * after the wait, not this episode, so the token goes back here and the wake is
 * routed from scratch.
 *
 * A token granted to the wake itself is exempt: the word still shows the sleep
 * until the woken thread reaches userspace and retires it, so reading it as a
 * sleeper again would revoke the grant before the thread ever ran on it. */
static __always_inline bool should_release_from_cv_sleep(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  return task_ctx->holds_admission && !task_ctx->cv_admitted && have_user &&
         user_cv_sleep(user_ctx_word);
}

/* Ends the exemption above. The word leaving the cond-sleep state is what says
 * the woken thread has run and published what it is doing now, so from here the
 * ordinary release rules describe it again. An unreadable word keeps the
 * exemption: nothing observed says the sleep is over. */
static __always_inline void note_cv_admission_state(
    struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (have_user && !user_cv_sleep(user_ctx_word))
    task_ctx->cv_admitted = 0;
}

/* Wake of a thread whose user word still shows the cond sleep it is returning
 * from. The has-token cases are settled before this is asked, so a match here
 * is a thread with no admission that named the lock it is about to re-acquire.
 */
static __always_inline bool cv_reacquire_requested(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (!cv_route_active() || task_ctx->holds_admission ||
      task_ctx->must_run_on_admission_cpu)
    return false;

  return have_user && user_cv_sleep(user_ctx_word) &&
         valid_lock_id(effective_lock_id(user_ctx_word));
}

static __always_inline bool task_in_critical_section(
    const struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (have_user)
    return user_in_critical_section(user_ctx_word);

  return false;
}

/* Width bounds the waiters queued in front of a lock, not the task holding it:
 * once the user word shows the lock taken, the slot goes back to the class so
 * the next waiter can be admitted while the critical section runs. Called
 * wherever BPF reads the word of a task that may hold admission; the slot token
 * makes every call after the first a no-op. */
static __always_inline void note_critical_section_entry(
    struct task_scx_ctx *task_ctx, bool have_user, __u32 user_ctx_word) {
  if (task_in_critical_section(task_ctx, have_user, user_ctx_word))
    release_width_slot(task_ctx);
}

static __always_inline bool enqueue_force_inactive(struct task_struct *p,
                                                   struct task_scx_ctx *task_ctx,
                                                   __u32 lock_id, __u32 cpu,
                                                   __u64 enq_flags,
                                                   __u32 user_ctx_word) {
  if (!valid_lock_id(lock_id))
    return false;

  if (cpu >= MAX_CPUS || !task_cpu_allowed(p, cpu)) {
    enqueue_normal_dsq_recorded(p, task_ctx, enq_flags, lock_id,
                                user_ctx_word);
    return true;
  }

  task_ctx->admission_cpu = cpu;
  task_ctx->must_run_on_admission_cpu = 0;
  task_ctx->force_inactive_wait = 0;
  record_enqueue_state(task_ctx, inactive_dsq_id(lock_id), lock_id,
                       ENQ_PATH_FORCE_INACTIVE, cpu, user_ctx_word,
                       wake_hinted(enq_flags, user_ctx_word));
  scx_bpf_dsq_insert(p, inactive_dsq_id(lock_id), SCX_SLICE_DFL, enq_flags);
  record_inactive_enqueue();
  if (width_control_enabled)
    account_inactive_depth_enqueue(lock_id);
  return true;
}

static __always_inline void protect_critical_section(struct task_struct *p,
                                                     struct task_scx_ctx *task_ctx,
                                                     __u32 lock_id) {
  __u32 cpu = task_ctx->admission_cpu;

  if (!valid_lock_id(lock_id))
    return;

  if (cpu >= MAX_CPUS || !task_cpu_allowed(p, cpu))
    cpu = pick_allowed_cpu(p, bpf_get_smp_processor_id());

  if (cpu >= MAX_CPUS)
    return;

  task_ctx->admission_cpu = cpu;
  if (!task_ctx->holds_admission)
    grant_admission(p, task_ctx, lock_id, cpu, true);

  task_ctx->must_run_on_admission_cpu = 1;
}

static __always_inline void refresh_running_state(struct task_struct *p) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  __u32 lock_id = UNMANAGED_LOCK_ID;
  bool have_user = false;
  __u32 cpu = bpf_get_smp_processor_id();

  if (stats_only_enabled())
    return;

  if (!registered_threads_active())
    return;

  task_ctx = get_task_ctx(p);
  if (!task_ctx)
    return;

  have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
  if (have_user)
    lock_id = effective_lock_id(user_ctx_word);
  note_cv_admission_state(task_ctx, have_user, user_ctx_word);
  if (should_release_from_user(task_ctx, have_user, user_ctx_word) ||
      should_release_from_cv_sleep(task_ctx, have_user, user_ctx_word)) {
    release_admission(p, task_ctx);
    return;
  }

  note_critical_section_entry(task_ctx, have_user, user_ctx_word);

  if (!task_ctx->holds_admission && have_user &&
      user_slow_path_pending(user_ctx_word) && valid_lock_id(lock_id) &&
      cpu < MAX_CPUS && task_cpu_allowed(p, cpu)) {
    if (!class_dispatch_eligible(lock_id))
      task_ctx->force_inactive_wait = 1;
    else if (grant_admission(p, task_ctx, lock_id, cpu,
                             user_in_critical_section(user_ctx_word)))
      bump_debug_counter(&running_pending_grant_success);
    else
      bump_debug_counter(&running_pending_grant_failure);
  }

  if (task_ctx->must_run_on_admission_cpu && task_ctx->admission_cpu == cpu)
    task_ctx->must_run_on_admission_cpu = 0;
}

s32 BPF_STRUCT_OPS(accordin_select_cpu, struct task_struct *p, s32 prev_cpu,
                   u64 wake_flags) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  bool is_idle = false;
  bool have_user = false;
  bool wants_slow_path = false;
  bool debug_counters;
  s32 cpu;

  if (stats_only_enabled()) {
    cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);
    if (!valid_cpu(cpu) || !task_cpu_allowed(p, (__u32)cpu)) {
      is_idle = false;
      cpu = pick_allowed_cpu(p, prev_cpu);
      if (cpu >= MAX_CPUS)
        cpu = pick_task_cpu(p, prev_cpu);
    }

    if (is_idle && valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);

    return cpu;
  }

  if (!registered_threads_active()) {
    return select_cpu_without_admission(p, prev_cpu, wake_flags);
  }

  debug_counters = debug_counters_enabled();
  task_ctx = get_task_ctx(p);
  if (task_ctx) {
    have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
    clear_invalid_admission_cpu(p, task_ctx);
  }

  if (task_ctx && task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu))
    return (s32)task_ctx->admission_cpu;

  wants_slow_path =
      task_ctx && slow_path_requested(task_ctx, have_user, user_ctx_word);

  if (wants_slow_path && valid_cpu(prev_cpu) &&
      task_cpu_allowed(p, (__u32)prev_cpu)) {
    task_ctx->admission_cpu = (__u32)prev_cpu;
    return prev_cpu;
  }

  cpu = scx_bpf_select_cpu_dfl(p, prev_cpu, wake_flags, &is_idle);

  if (wants_slow_path && (!valid_cpu(cpu) || !task_cpu_allowed(p, (__u32)cpu))) {
    is_idle = false;
    cpu = pick_allowed_cpu(p, prev_cpu);
    if (cpu >= MAX_CPUS)
      cpu = pick_task_cpu(p, prev_cpu);
  } else if (!wants_slow_path &&
             (!valid_cpu(cpu) || !task_cpumask_allows(p, (__u32)cpu))) {
    is_idle = false;
    cpu = pick_task_cpu(p, prev_cpu);
  }

  if (wants_slow_path && valid_cpu(cpu) && task_cpu_allowed(p, (__u32)cpu))
    task_ctx->admission_cpu = (__u32)cpu;

  /* A cond re-acquire is excluded from the shortcut even though the CPU is
   * idle: taking it would skip the enqueue callback, which is where the wake is
   * granted admission or parked. The CPU chosen here is still the one it lands
   * on, since the grant is asked for on the task's current CPU. */
  if (is_idle && valid_cpu(cpu) && task_cpumask_allows(p, (__u32)cpu) &&
      !wants_slow_path && !inactive_dispatch_needed() &&
      !normal_dispatch_needed() && !cvready_dispatch_needed() &&
      !(task_ctx &&
        cv_reacquire_requested(task_ctx, have_user, user_ctx_word))) {
    /* A hinted task always wants the slow path, so this shortcut never carries
     * a wake_consumed_* class. */
    record_enqueue_state(task_ctx, SCX_DSQ_LOCAL,
                         have_user ? effective_lock_id(user_ctx_word)
                                   : UNMANAGED_LOCK_ID,
                         ENQ_PATH_SELECT_LOCAL_DIRECT, (__u32)cpu,
                         user_ctx_word, false);
    if (debug_counters)
      bump_counter(&select_local_direct);
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL, SCX_SLICE_DFL, 0);
  }

  return cpu;
}

void BPF_STRUCT_OPS(accordin_enqueue, struct task_struct *p, u64 enq_flags) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  __u32 lock_id = UNMANAGED_LOCK_ID;
  bool have_user = false;
  bool debug_counters;
  __u32 cpu;

  if (stats_only_enabled()) {
    if (p->nr_cpus_allowed == 1) {
      cpu = pick_allowed_cpu(p, -1);
      if (valid_cpu(cpu) && task_cpu_allowed(p, cpu)) {
        scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL,
                           enq_flags);
        return;
      }
    }

    scx_bpf_dsq_insert(p, SCX_DSQ_GLOBAL, SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (!registered_threads_active()) {
    if (enqueue_normal_local_fast(p, enq_flags))
      return;

    enqueue_normal_dsq(p, enq_flags);
    return;
  }

  task_ctx = get_task_ctx(p);
  if (!task_ctx) {
    if (enqueue_normal_local_fast(p, enq_flags))
      return;

    enqueue_normal_dsq(p, enq_flags);
    return;
  }

  have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
  if (have_user)
    lock_id = effective_lock_id(user_ctx_word);

  debug_counters = debug_counters_enabled();
  if (debug_counters) {
    bool wake = enq_flags & SCX_ENQ_WAKEUP;

    /* Only a task that already published a user word can fail the read; tasks
     * that never registered are not part of the protocol. */
    if (wake && !have_user && task_ctx->user_ctx_ptr)
      bump_counter(&wake_read_fail);
    /* Failed reads of a published pointer while routing is on. The cond branch
     * cannot run without the word, so this bounds the wakes routing could have
     * claimed; it does not separate them from the tasks that were never going
     * to take that branch. */
    if (cv_route_active() && !have_user && task_ctx->user_ctx_ptr)
      bump_counter(&cv_word_read_fail);
    /* Every wakeup counted here reaches exactly one wake_consumed_* class in
     * record_enqueue_state below. */
    if (wake_hinted(enq_flags, user_ctx_word))
      bump_counter(&wake_consumed_seen);
  }

  clear_invalid_admission_cpu(p, task_ctx);
  note_cv_admission_state(task_ctx, have_user, user_ctx_word);

  if (consumed_token_reuse_requested(task_ctx, have_user, user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    release_admission(p, task_ctx);
    enqueue_force_inactive(p, task_ctx, lock_id, cpu, enq_flags, user_ctx_word);
    return;
  }

  /* Both releases run before the cond branch below, which is what makes the
   * order has-token, then cond sleep, then pending: a task that still carried a
   * token from before its wait has given it back by the time the cond branch
   * asks. */
  if (should_release_from_user(task_ctx, have_user, user_ctx_word) ||
      should_release_from_cv_sleep(task_ctx, have_user, user_ctx_word))
    release_admission(p, task_ctx);

  if (cv_reacquire_requested(task_ctx, have_user, user_ctx_word) &&
      valid_lock_id(lock_id)) {
    if (debug_counters)
      bump_counter(&cv_wake_enq);

    cpu = requested_cpu(p, task_ctx, -1);
    if (cpu >= MAX_CPUS) {
      enqueue_normal_dsq_recorded(p, task_ctx, enq_flags, lock_id,
                                  user_ctx_word);
      return;
    }
    task_ctx->admission_cpu = cpu;

    /* The cond sleeper is a waiter for the lock it named, never its holder, so
     * the grant takes a width slot like any other waiter's. */
    if (class_dispatch_eligible(lock_id) &&
        grant_admission(p, task_ctx, lock_id, cpu, false)) {
      /* The word still names the sleep this wake is returning from, and stays
       * that way until the thread runs and retires it. */
      task_ctx->cv_admitted = 1;
      record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | cpu, lock_id,
                           ENQ_PATH_CV_GRANTED_LOCAL, cpu, user_ctx_word,
                           false);
      if (debug_counters)
        bump_counter(&cv_grant_at_enq);
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
      return;
    }

    record_enqueue_state(task_ctx, cvready_dsq_id(lock_id), lock_id,
                         ENQ_PATH_CV_PARKED, cpu, user_ctx_word, false);
    scx_bpf_dsq_insert(p, cvready_dsq_id(lock_id), SCX_SLICE_DFL, enq_flags);
    record_cvready_enqueue();
    /* The class queues are drained through a probe window narrower than the
     * class space, so a parked waiter needs a CPU that reaches its class
     * without waiting for a cursor to arrive at it. The CPU picked for the
     * waiter is one its cpumask allows, so naming the class in that CPU's
     * affinity slot and kicking it makes the first attempt after the park the
     * one that looks straight at this queue. */
    record_cvready_dispatch_lock(cpu, lock_id);
    scx_bpf_kick_cpu(cpu, 0);
    if (debug_counters)
      bump_counter(&cv_parked);
    return;
  }

  if (task_ctx->force_inactive_wait && have_user &&
      user_slow_path_pending(user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    enqueue_force_inactive(p, task_ctx, lock_id, cpu, enq_flags, user_ctx_word);
    return;
  }

  if (task_ctx->admission_cpu < MAX_CPUS &&
      (task_ctx->holds_admission || task_ctx->must_run_on_admission_cpu)) {
    record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | task_ctx->admission_cpu,
                         lock_id, ENQ_PATH_ADMISSION_LOCAL,
                         task_ctx->admission_cpu, user_ctx_word,
                         wake_hinted(enq_flags, user_ctx_word));
    scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | task_ctx->admission_cpu,
                       SCX_SLICE_DFL, enq_flags);
    return;
  }

  if (slow_path_requested(task_ctx, have_user, user_ctx_word)) {
    cpu = requested_cpu(p, task_ctx, -1);
    if (cpu >= MAX_CPUS) {
      enqueue_normal_dsq_recorded(p, task_ctx, enq_flags, lock_id,
                                  user_ctx_word);
      return;
    }
    task_ctx->admission_cpu = cpu;

    if (class_dispatch_eligible(lock_id) &&
        grant_admission(p, task_ctx, lock_id, cpu,
                        user_in_critical_section(user_ctx_word))) {
      record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | cpu, lock_id,
                           ENQ_PATH_SLOW_GRANTED_LOCAL, cpu, user_ctx_word,
                           wake_hinted(enq_flags, user_ctx_word));
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL, enq_flags);
      return;
    }

    record_enqueue_state(task_ctx, inactive_dsq_id(lock_id), lock_id,
                         ENQ_PATH_SLOW_INACTIVE, cpu, user_ctx_word,
                         wake_hinted(enq_flags, user_ctx_word));
    scx_bpf_dsq_insert(p, inactive_dsq_id(lock_id), SCX_SLICE_DFL, enq_flags);
    record_inactive_enqueue();
    if (width_control_enabled)
      account_inactive_depth_enqueue(lock_id);
    return;
  }

  if (enqueue_normal_local_fast_recorded(p, task_ctx, enq_flags, lock_id,
                                         user_ctx_word))
    return;

  if (p->nr_cpus_allowed == 1) {
    cpu = pick_task_cpu(p, -1);
    if (valid_cpu(cpu) && task_cpumask_allows(p, cpu)) {
      record_enqueue_state(task_ctx, SCX_DSQ_LOCAL_ON | cpu, lock_id,
                           ENQ_PATH_NORMAL_LOCAL_FAST, cpu, user_ctx_word,
                           wake_hinted(enq_flags, user_ctx_word));
      scx_bpf_dsq_insert(p, SCX_DSQ_LOCAL_ON | cpu, SCX_SLICE_DFL,
                         enq_flags);
      return;
    }
  }

  enqueue_normal_dsq_recorded(p, task_ctx, enq_flags, lock_id, user_ctx_word);
}

/* One drain attempt against the inactive queues. Reports whether a task moved;
 * a BLOCKED scan deliberately leaves the scan-empty sequence gate unarmed. */
static __always_inline bool try_dispatch_inactive(__u32 cpu, __u32 inactive_seq,
                                                  bool debug_counters,
                                                  bool force) {
  enum inactive_move_outcome outcome;

  if (debug_counters)
    dispatch_inactive_attempts += 1;

  outcome = move_selected_inactive_to_local(cpu, force);
  if (outcome == INACTIVE_MOVE_MOVED) {
    record_inactive_dispatch(cpu);
    if (debug_counters)
      dispatch_inactive_success += 1;
    return true;
  }

  if (debug_counters)
    dispatch_inactive_empty += 1;
  if (outcome != INACTIVE_MOVE_BLOCKED)
    record_inactive_scan_empty(inactive_seq);

  return false;
}

/* One drain attempt against the cvready queues. Unlike the inactive one it
 * reports nothing back to the dispatch gate: the selection reaches a window of
 * the class space rather than all of it, so an empty verdict here would close
 * the gate over a class the window never probed. Occupancy is counted at the
 * park and the drain instead, which the gate reads directly. */
static __always_inline bool try_dispatch_cvready(__u32 cpu, bool debug_counters,
                                                 bool force) {
  if (!move_selected_cvready_to_local(cpu, force))
    return false;

  if (debug_counters) {
    bump_counter(&cv_dispatch);
    if (force)
      bump_counter(&cv_dispatch_forced);
  }
  return true;
}

/* Whether this CPU should look at the cvready queues at all. The occupancy
 * counters answer that for the machine; the kernel's queue count for the class
 * this CPU was last pointed at is kept as a cross-check, because that class is
 * either the one the CPU last drained or the one a park chose this CPU for. */
static __always_inline bool cvready_dispatch_pending(__u32 cpu) {
  volatile __u32 *last_lock;
  __u32 lock_id;

  if (!cv_route_active())
    return false;

  if (cvready_queues_occupied())
    return true;

  last_lock = lookup_cpu_last_cvready_lock(cpu);
  if (!last_lock)
    return false;

  lock_id = *last_lock;
  if (!valid_lock_id(lock_id))
    return false;

  return scx_bpf_dsq_nr_queued(cvready_dsq_id(lock_id)) > 0;
}

/* Interval between inactive scans that ignore the sequence gate, the admission
 * slot and width control. The sched_ext watchdog ejects the scheduler after
 * about 30 s of a runnable task never running, so a period in milliseconds
 * bounds the extra parking a bookkeeping or width-controller mistake can impose
 * on a queued waiter with three orders of magnitude to spare, while the leak is
 * at most a hundred width-bypassing dispatches a second across the machine. */
#define INACTIVE_FORCE_DRAIN_PERIOD_NS 10000000ULL

void BPF_STRUCT_OPS(accordin_dispatch, s32 cpu, struct task_struct *prev) {
  __u32 inactive_seq;
  __u32 normal_seq;
  __u64 now;
  bool force_inactive;
  bool inactive_cpu_available;
  bool cvready_pending;
  bool cvready_attempted = false;
  bool debug_counters;

  (void)prev;

  if (!valid_cpu(cpu))
    return;

  debug_counters = debug_counters_enabled();
  if (debug_counters)
    dispatch_calls += 1;

  if (stats_only_enabled()) {
    scx_bpf_dsq_move_to_local(SCX_DSQ_GLOBAL);
    return;
  }

  inactive_seq = inactive_enqueue_seq;
  normal_seq = normal_enqueue_seq;
  cvready_pending = cvready_dispatch_pending((__u32)cpu);

  /* A scan the period is due for runs whatever the gates say, so a waiter left
   * in a class queue by stale bookkeeping or by a width the controller never
   * reopens waits milliseconds rather than until something else happens to
   * enqueue. A task moved this way reaches the running hook like any other and
   * takes its admission grant there.
   *
   * The deadline is read before the normal queue is served: every return below
   * it belongs to a CPU that found normal work, and sustained normal traffic
   * would otherwise keep the period from ever being noticed. */
  now = bpf_ktime_get_ns();
  force_inactive = now >= inactive_force_drain_at;
  if (force_inactive) {
    inactive_force_drain_at = now + INACTIVE_FORCE_DRAIN_PERIOD_NS;
    if (debug_counters)
      dispatch_inactive_forced += 1;
  }

  /* The sequence gate is only advisory here, because it can close over a queued
   * task: an insert issued from the enqueue callback lands on the DSQ after the
   * callback returns, and a move_to_local on another CPU lifts a task off the
   * list and puts it back on failure, both without an enqueue to reopen the
   * gate. The kernel's queue count decides, so a gate closed by either race
   * heals at the next dispatch on any CPU instead of stranding the task until
   * some later enqueue arrives. */
  if (normal_dispatch_needed_for(normal_seq) ||
      scx_bpf_dsq_nr_queued(NORMAL_DSQ_ID) > 0) {
    if (debug_counters)
      dispatch_normal_attempts += 1;
    if (scx_bpf_dsq_move_to_local(NORMAL_DSQ_ID)) {
      reset_inactive_dispatch_budget((__u32)cpu);
      reset_cvready_streak((__u32)cpu);
      if (debug_counters)
        dispatch_normal_success += 1;
      /* Returning unconditionally lets a steady normal stream starve the
       * inactive queues on exactly the CPUs able to drain them; the CPUs that
       * do reach the scan may be excluded by the waiters' cpumask. */
      if (!force_inactive && !inactive_dispatch_needed_for(inactive_seq) &&
          !cvready_pending)
        return;
    } else {
      if (debug_counters)
        dispatch_normal_empty += 1;
      if (scx_bpf_dsq_nr_queued(NORMAL_DSQ_ID) == 0)
        record_normal_scan_empty(normal_seq);
    }
  } else if (debug_counters) {
    dispatch_normal_skip_seq += 1;
  }

  if (!force_inactive && !inactive_dispatch_needed_for(inactive_seq) &&
      !cvready_pending)
    return;

  inactive_cpu_available = inactive_dispatch_cpu_available((__u32)cpu);
  if (debug_counters) {
    if (!inactive_cpu_available)
      dispatch_inactive_unavailable += 1;
    else if (!inactive_dispatch_budget_available((__u32)cpu))
      dispatch_inactive_budget_blocked += 1;
  }

  /* A thread returning from a cond wait is one the next holder of the lock is
   * already waiting on, so it goes before the queue of threads that only asked
   * for the lock. The streak bounds that preference; the forced pass ignores
   * both the streak and the admission slot, and falls through afterwards so the
   * same period still forces the class queues. */
  if (cv_route_active() && (force_inactive || cvready_pending)) {
    bool cvready_priority =
        cvready_priority_available((__u32)cpu) && inactive_cpu_available;

    if (force_inactive || cvready_priority) {
      cvready_attempted = true;
      if (try_dispatch_cvready((__u32)cpu, debug_counters, force_inactive) &&
          !force_inactive) {
        record_cvready_priority_dispatch((__u32)cpu);
        return;
      }
    }
  }

  if (inactive_cpu_available && inactive_dispatch_budget_available((__u32)cpu) &&
      try_dispatch_inactive((__u32)cpu, inactive_seq, debug_counters,
                            force_inactive)) {
    reset_cvready_streak((__u32)cpu);
    return;
  }

  if ((force_inactive || inactive_cpu_available) &&
      try_dispatch_inactive((__u32)cpu, inactive_seq, debug_counters,
                            force_inactive)) {
    reset_cvready_streak((__u32)cpu);
    return;
  }

  /* Same-priority attempt: the class queues had nothing for this CPU, so a
   * cvready task takes the slot without spending streak budget. This is the
   * only path that drains cvready when the priority is turned off. */
  if (cvready_pending && !cvready_attempted && inactive_cpu_available &&
      try_dispatch_cvready((__u32)cpu, debug_counters, false))
    return;
}

void BPF_STRUCT_OPS(accordin_running, struct task_struct *p) {
  refresh_running_state(p);
}

void BPF_STRUCT_OPS(accordin_tick, struct task_struct *p) {
  refresh_running_state(p);
}

void BPF_STRUCT_OPS(accordin_stopping, struct task_struct *p, bool runnable) {
  struct task_scx_ctx *task_ctx;
  __u32 user_ctx_word = 0;
  __u32 lock_id = UNMANAGED_LOCK_ID;
  bool have_user = false;

  if (!registered_threads_active())
    return;

  task_ctx = lookup_task_ctx(p);
  if (!task_ctx)
    return;

  if (stats_only_enabled())
    return;

  have_user = read_user_ctx(p, task_ctx, &user_ctx_word);
  if (have_user)
    lock_id = effective_lock_id(user_ctx_word);
  note_cv_admission_state(task_ctx, have_user, user_ctx_word);

  /* The cond-sleep release is asked for here as well, so a thread stopping with
   * that bit set gives its token back whether it is about to block on the futex
   * or was merely preempted on the way there. Leaving it to the !runnable
   * branch below would keep a stale token across the preemption. */
  if (should_release_from_user(task_ctx, have_user, user_ctx_word) ||
      should_release_from_cv_sleep(task_ctx, have_user, user_ctx_word)) {
    release_admission(p, task_ctx);
    return;
  }

  note_critical_section_entry(task_ctx, have_user, user_ctx_word);

  if (!runnable) {
    /* A block invalidates any routing decision made for a runnable re-enqueue,
     * so the wakeup is classified from scratch. */
    task_ctx->force_inactive_wait = 0;
    task_ctx->must_run_on_admission_cpu = 0;

    if (!task_ctx->holds_admission)
      return;

    /* Only a confirmed critical section justifies pinning the slot while the
     * owner sleeps; the lock it protects cannot progress without it. */
    if (task_in_critical_section(task_ctx, have_user, user_ctx_word)) {
      task_ctx->must_run_on_admission_cpu = 1;
      return;
    }

    /* Any other block gives the slot back and re-enters admission at wakeup.
     * An unreadable user word is treated as not-in-critical-section: a real
     * owner resumes without a slot and protect_critical_section re-pins it at
     * its next runnable stop. */
    if (!have_user)
      bump_debug_counter(&block_release_read_fail);

    release_admission(p, task_ctx);
    return;
  }

  if (consumed_token_reuse_requested(task_ctx, have_user, user_ctx_word)) {
    release_admission(p, task_ctx);
    task_ctx->force_inactive_wait = 1;
    return;
  }

  if (task_in_critical_section(task_ctx, have_user, user_ctx_word)) {
    protect_critical_section(p, task_ctx, lock_id);
    return;
  }

  if (!task_ctx->holds_admission)
    return;

  task_ctx->must_run_on_admission_cpu = 1;
}

void BPF_STRUCT_OPS(accordin_dump, struct scx_dump_ctx *dump_ctx) {
  __u32 lock_id;

  (void)dump_ctx;

  scx_bpf_dump(
      "accordin_global debug=%u reg_threads=%u normal_seq=%u normal_empty=%u "
      "normal_q=%d inactive_seq=%u inactive_empty=%u\n",
      debug_counters_mode, registered_thread_count, normal_enqueue_seq,
      normal_empty_seq, scx_bpf_dsq_nr_queued(NORMAL_DSQ_ID),
      inactive_enqueue_seq, inactive_empty_seq);
  scx_bpf_dump("accordin_cv route=%u streak_limit=%u cvready_seq=%u "
               "cvready_drained=%u\n",
               cv_route_enabled, cv_priority_streak_limit, cvready_enqueue_seq,
               cvready_drained_count);
  scx_bpf_dump("accordin_width width_ctl=%u underflow=%llu\n",
               width_control_enabled, class_active_underflow_events);
  if (debug_counters_enabled()) {
    scx_bpf_dump(
        "accordin_dispatch calls=%llu normal_skip_seq=%llu normal_attempts=%llu "
        "normal_success=%llu normal_empty=%llu inactive_unavail=%llu "
        "inactive_budget_blocked=%llu inactive_attempts=%llu "
        "inactive_success=%llu inactive_empty=%llu inactive_forced=%llu\n",
        dispatch_calls, dispatch_normal_skip_seq, dispatch_normal_attempts,
        dispatch_normal_success, dispatch_normal_empty,
        dispatch_inactive_unavailable, dispatch_inactive_budget_blocked,
        dispatch_inactive_attempts, dispatch_inactive_success,
        dispatch_inactive_empty, dispatch_inactive_forced);
    scx_bpf_dump(
        "accordin_routing select_local_direct=%llu wake_consumed_seen=%llu "
        "wake_consumed_granted=%llu wake_consumed_inactive=%llu "
        "wake_consumed_normal=%llu wake_read_fail=%llu "
        "running_pending_grant_success=%llu "
        "running_pending_grant_failure=%llu block_release_read_fail=%llu\n",
        select_local_direct, wake_consumed_seen, wake_consumed_granted,
        wake_consumed_inactive, wake_consumed_normal, wake_read_fail,
        running_pending_grant_success, running_pending_grant_failure,
        block_release_read_fail);
    scx_bpf_dump("accordin_cv_routing cv_wake_enq=%llu cv_grant_at_enq=%llu "
                 "cv_parked=%llu cv_dispatch=%llu cv_dispatch_forced=%llu "
                 "cv_word_read_fail=%llu\n",
                 cv_wake_enq, cv_grant_at_enq, cv_parked, cv_dispatch,
                 cv_dispatch_forced, cv_word_read_fail);
  }

#pragma unroll
  for (lock_id = 0; lock_id < MAX_LOCK_CLASSES; lock_id++) {
    __u64 dsq_id = inactive_dsq_id(lock_id);
    s32 queued = scx_bpf_dsq_nr_queued(dsq_id);
    __u32 width = class_width[lock_id];
    s32 active = class_active[lock_id];
    __u32 peak = class_active_peak[lock_id];
    __u32 depth = class_inactive_depth[lock_id];

    if (queued)
      scx_bpf_dump("accordin_inactive_q lock=%u dsq=0x%llx queued=%d\n",
                   lock_id, dsq_id, queued);
    if (width || active || peak || depth)
      scx_bpf_dump(
          "accordin_class lock=%u width=%u active=%d peak=%u depth=%u\n",
          lock_id, width, active, peak, depth);
  }

  /* Kept out of the loop above so the routing feature adds nothing to the dump
   * program when it is off, and so it can be dropped on its own if the dump
   * ever runs short of budget. */
  if (!cv_route_active())
    return;

#pragma unroll
  for (lock_id = 0; lock_id < MAX_LOCK_CLASSES; lock_id++) {
    s32 queued = scx_bpf_dsq_nr_queued(cvready_dsq_id(lock_id));

    if (queued)
      scx_bpf_dump("accordin_cvready_q lock=%u queued=%d\n", lock_id, queued);
  }
}

void BPF_STRUCT_OPS(accordin_dump_task, struct scx_dump_ctx *dump_ctx,
                    struct task_struct *p) {
  struct task_scx_ctx *task_ctx;

  (void)dump_ctx;

  task_ctx = bpf_task_storage_get(&task_ctx_map, p, 0, 0);
  if (!task_ctx)
    return;

  scx_bpf_dump("accordin_task pid=%d tgid=%d holds=%u slot=%u adm_cpu=%u "
               "must=%u force=%u\n",
               p->pid, p->tgid, task_ctx->holds_admission,
               task_ctx->width_slot_held, task_ctx->admission_cpu,
               task_ctx->must_run_on_admission_cpu,
               task_ctx->force_inactive_wait);
  if (debug_counters_enabled())
    scx_bpf_dump(
        "accordin_task_enqueue pid=%d last_path=%u last_lock=%u last_cpu=%u "
        "last_dsq=0x%llx user_word=0x%x\n",
        p->pid, task_ctx->last_enqueue_path, task_ctx->last_enqueue_lock_id,
        task_ctx->last_enqueue_cpu, task_ctx->last_enqueue_dsq,
        task_ctx->last_user_ctx_word);
}

void BPF_STRUCT_OPS(accordin_exit_task, struct task_struct *p,
                    struct scx_exit_task_args *args) {
  struct task_scx_ctx *task_ctx;
  __u32 pid = p->pid;

  (void)args;

  task_ctx = lookup_task_ctx(p);
  if (task_ctx && task_ctx->holds_admission)
    release_admission(p, task_ctx);

  bpf_map_delete_elem(&thread_ctx_addr_map, &pid);
  bpf_task_storage_delete(&task_ctx_map, p);
}

s32 BPF_STRUCT_OPS_SLEEPABLE(accordin_init) {
  __u32 lock_id;
  s32 ret;

  ret = scx_bpf_create_dsq(NORMAL_DSQ_ID, -1);
  if (ret)
    return ret;

#pragma unroll
  for (lock_id = 0; lock_id < MAX_LOCK_CLASSES; lock_id++) {
    ret = scx_bpf_create_dsq(inactive_dsq_id(lock_id), -1);
    if (ret)
      return ret;

    /* Created whatever the routing switch says: an empty DSQ costs nothing,
     * and every cvready queue existing means no path has to prove the switch
     * was on before it may name one. */
    ret = scx_bpf_create_dsq(cvready_dsq_id(lock_id), -1);
    if (ret)
      return ret;
  }

  return 0;
}

void BPF_STRUCT_OPS(accordin_exit, struct scx_exit_info *ei) {
  UEI_RECORD(uei, ei);
}

SCX_OPS_DEFINE(accordin_ops, .select_cpu = (void *)accordin_select_cpu,
               .enqueue = (void *)accordin_enqueue,
               .dispatch = (void *)accordin_dispatch,
               .running = (void *)accordin_running,
               .tick = (void *)accordin_tick,
               .stopping = (void *)accordin_stopping,
               .dump = (void *)accordin_dump,
               .dump_task = (void *)accordin_dump_task,
               .exit_task = (void *)accordin_exit_task,
               .init = (void *)accordin_init, .exit = (void *)accordin_exit,
               .name = "accordin");
