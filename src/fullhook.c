/* SPDX-License-Identifier: GPL-2.0-only */
/* LD_PRELOAD interposer that routes pthread mutexes and condition variables of
 * an unmodified program through the admission runtime. Locks live inside the
 * caller's pthread_mutex_t / pthread_cond_t, so the fast path allocates nothing
 * and never calls back into the C library's own pthread implementation. */
#include <errno.h>
#include <limits.h>
#include <pthread.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <linux/futex.h>
#include <sys/syscall.h>
#include "env_flag.h"
#include "lock_ops.h"

#define EXPORT __attribute__((visibility("default")))
#define TRACED __attribute__((noinline, cold))

/* glibc keeps __data.__kind at offset 16 of pthread_mutex_t. Putting the
 * overlay's kind word there makes PTHREAD_RECURSIVE_MUTEX_INITIALIZER_NP
 * recursive without an init call, while a zero-filled object
 * (PTHREAD_MUTEX_INITIALIZER) is an unlocked normal mutex. */
struct hook_mutex {
    union {
        struct raw_lock raw;
        char slot[16];
    };
    uint32_t kind;
    uint32_t owner; /* recursive: tid of the holder, zero when free */
    uint32_t count; /* recursive: hold depth */
};

_Static_assert(sizeof(struct raw_lock) <= 16, "raw lock exceeds the overlay slot");
_Static_assert(sizeof(struct hook_mutex) <= sizeof(pthread_mutex_t),
               "mutex overlay exceeds pthread_mutex_t");
_Static_assert(_Alignof(struct hook_mutex) <= _Alignof(pthread_mutex_t),
               "mutex overlay is more aligned than pthread_mutex_t");
_Static_assert(offsetof(struct hook_mutex, kind) == 16,
               "kind must alias glibc's __kind field");

/* A futex sequence counter plus a census of sleeping waiters is the whole
 * condition variable; a zero-filled pthread_cond_t is therefore ready to use.
 * A waiter that spins is deliberately absent from the census, so releasing it
 * costs no syscall. */
struct hook_cond {
    _Atomic uint32_t seq;
    _Atomic uint32_t parked;
};

_Static_assert(sizeof(struct hook_cond) <= sizeof(pthread_cond_t),
               "cond overlay exceeds pthread_cond_t");
_Static_assert(_Alignof(struct hook_cond) <= _Alignof(pthread_cond_t),
               "cond overlay is more aligned than pthread_cond_t");

/* Per-mutex wait attribution, enabled by ACCORDIN_HOOK_STATS. Every operation
 * tests one process-wide flag that never changes after startup; with the flag
 * clear the acquisition path is the one the direct library runs. */
enum {
    STATS_ENTRIES = 64,  /* distinct addresses tracked per thread */
    STATS_OVERFLOW = STATS_ENTRIES,
};

struct stats_entry {
    uintptr_t key; /* mutex address, zero in the shared overflow row */
    uint64_t acquisitions;
    uint64_t fast;
    uint64_t slow;
    uint64_t admitted_waits;
    uint64_t yields;
    uint64_t admission_ns;
    uint64_t spin_ns;
    uint64_t hold_ns;
    uint64_t max_hold_ns;
    uint64_t cond_waits;
};

struct stats_table {
    unsigned int used;
    struct stats_entry entries[STATS_ENTRIES + 1];
};

/* Held mutexes of one thread, so unlock finds its acquisition timestamp
 * without rescanning the table. */
struct stats_frame {
    struct stats_entry *entry;
    uintptr_t key;
    uint64_t start_ns;
};

/* Microseconds a condition-variable waiter may spin before parking. */
#define CV_SPIN_US_DEFAULT 1000

static uint64_t cv_spin_ns;
static bool stats_enabled;
static pthread_key_t stats_key;
static _Thread_local struct stats_table *thread_stats;
static _Thread_local struct stats_frame stats_stack[MCS_POOL_SIZE];
static _Thread_local unsigned int stats_depth;
static struct stats_table global_stats;
static atomic_flag global_gate = ATOMIC_FLAG_INIT;

/* A private test-and-set gate keeps the merge off the interposed pthread API. */
static void global_lock(void)
{
    while (atomic_flag_test_and_set_explicit(&global_gate, memory_order_acquire))
        spin_pause();
}

static void global_unlock(void)
{
    atomic_flag_clear_explicit(&global_gate, memory_order_release);
}

static struct stats_entry *table_entry(struct stats_table *table, uintptr_t key)
{
    for (unsigned int i = 0; i < table->used; i++)
        if (table->entries[i].key == key)
            return &table->entries[i];
    if (table->used == STATS_ENTRIES)
        return &table->entries[STATS_OVERFLOW];
    struct stats_entry *entry = &table->entries[table->used++];
    entry->key = key;
    return entry;
}

static struct stats_entry *stats_entry(const void *mutex)
{
    struct stats_table *table = thread_stats;

    if (!table) {
        table = calloc(1, sizeof(*table));
        if (!table)
            return NULL;
        thread_stats = table;
        pthread_setspecific(stats_key, table);
    }
    return table_entry(table, (uintptr_t)mutex);
}

static void merge_entry(struct stats_entry *into, const struct stats_entry *from)
{
    into->acquisitions += from->acquisitions;
    into->fast += from->fast;
    into->slow += from->slow;
    into->admitted_waits += from->admitted_waits;
    into->yields += from->yields;
    into->admission_ns += from->admission_ns;
    into->spin_ns += from->spin_ns;
    into->hold_ns += from->hold_ns;
    into->cond_waits += from->cond_waits;
    if (from->max_hold_ns > into->max_hold_ns)
        into->max_hold_ns = from->max_hold_ns;
}

static void merge_table(void *value)
{
    struct stats_table *table = value;

    global_lock();
    for (unsigned int i = 0; i < table->used; i++)
        merge_entry(table_entry(&global_stats, table->entries[i].key),
                    &table->entries[i]);
    merge_entry(&global_stats.entries[STATS_OVERFLOW], &table->entries[STATS_OVERFLOW]);
    global_unlock();
    if (table == thread_stats)
        thread_stats = NULL;
    free(table);
}

static uint64_t waited_ns(const struct stats_entry *entry)
{
    return entry->admission_ns + entry->spin_ns;
}

static void print_row(const char *label, const struct stats_entry *entry)
{
    fprintf(stderr,
            "[hook_stats] %s acq=%llu fast=%llu slow=%llu waits=%llu yields=%llu "
            "admission_ms=%.3f spin_ms=%.3f hold_ms=%.3f max_hold_us=%.3f cond_waits=%llu\n",
            label, (unsigned long long)entry->acquisitions,
            (unsigned long long)entry->fast, (unsigned long long)entry->slow,
            (unsigned long long)entry->admitted_waits,
            (unsigned long long)entry->yields, entry->admission_ns / 1e6,
            entry->spin_ns / 1e6, entry->hold_ns / 1e6, entry->max_hold_ns / 1e3,
            (unsigned long long)entry->cond_waits);
}

static void report_stats(void)
{
    struct stats_entry totals = {0};
    unsigned int count = global_stats.used;
    char label[32];

    /* Insertion sort by waiting time; the table holds at most 64 rows. */
    for (unsigned int i = 1; i < count; i++) {
        struct stats_entry row = global_stats.entries[i];
        unsigned int j = i;
        while (j && waited_ns(&global_stats.entries[j - 1]) < waited_ns(&row)) {
            global_stats.entries[j] = global_stats.entries[j - 1];
            j--;
        }
        global_stats.entries[j] = row;
    }
    fprintf(stderr, "[hook_stats] mutexes=%u sorted by admission+spin time\n", count);
    for (unsigned int i = 0; i < count; i++) {
        const struct stats_entry *entry = &global_stats.entries[i];
        merge_entry(&totals, entry);
        snprintf(label, sizeof(label), "addr=0x%llx", (unsigned long long)entry->key);
        print_row(label, entry);
    }
    if (global_stats.entries[STATS_OVERFLOW].acquisitions) {
        merge_entry(&totals, &global_stats.entries[STATS_OVERFLOW]);
        print_row("addr=overflow", &global_stats.entries[STATS_OVERFLOW]);
    }
    print_row("addr=total", &totals);
}

/* A value that is not a plain number keeps the default; zero disables spinning. */
static uint64_t spin_budget_ns(void)
{
    const char *value = getenv("ACCORDIN_CV_SPIN_US");
    unsigned long long budget;
    char *end;

    if (!value || !*value)
        return CV_SPIN_US_DEFAULT * 1000ULL;
    budget = strtoull(value, &end, 10);
    while (*end == ' ' || *end == '\t')
        end++;
    if (*end)
        return CV_SPIN_US_DEFAULT * 1000ULL;
    return budget * 1000ULL;
}

__attribute__((constructor)) static void hook_start(void)
{
    cv_spin_ns = spin_budget_ns();
    if (!env_flag("ACCORDIN_HOOK_STATS"))
        return;
    if (pthread_key_create(&stats_key, merge_table))
        return;
    stats_enabled = true;
}

__attribute__((destructor)) static void stats_stop(void)
{
    if (!stats_enabled)
        return;
    /* The exiting thread's own table has no key destructor to run. */
    if (thread_stats)
        merge_table(thread_stats);
    report_stats();
}

static void stats_push(struct stats_entry *entry, uintptr_t key)
{
    if (stats_depth >= MCS_POOL_SIZE)
        return;
    stats_stack[stats_depth++] = (struct stats_frame){entry, key, lock_ops_now_ns()};
}

/* Out-of-order unlocks are legal, so the most recent matching hold is closed. */
static void stats_pop(uintptr_t key)
{
    for (unsigned int i = stats_depth; i--;) {
        struct stats_frame *frame = &stats_stack[i];
        if (frame->key != key)
            continue;
        uint64_t held = lock_ops_now_ns() - frame->start_ns;
        frame->entry->hold_ns += held;
        if (held > frame->entry->max_hold_ns)
            frame->entry->max_hold_ns = held;
        memmove(frame, frame + 1, (--stats_depth - i) * sizeof(*frame));
        return;
    }
}

static TRACED void traced_acquire(struct hook_mutex *state)
{
    struct stats_entry *entry = stats_entry(state);
    struct lock_trace trace;

    lock_ops_lock(&state->raw, &trace);
    if (!entry)
        return;
    entry->acquisitions++;
    entry->fast += trace.fast;
    entry->slow += !trace.fast;
    entry->yields += trace.yields;
    entry->admission_ns += trace.admission_ns;
    entry->spin_ns += trace.spin_ns;
    /* admission_wait always yields at least once, so a non-zero count marks
     * exactly the slow paths that asked the scheduler for a slot. */
    entry->admitted_waits += trace.yields != 0;
    stats_push(entry, (uintptr_t)state);
}

static TRACED bool traced_try_acquire(struct hook_mutex *state)
{
    struct stats_entry *entry;

    if (!lock_ops_trylock(&state->raw))
        return false;
    entry = stats_entry(state);
    if (entry) {
        entry->acquisitions++;
        stats_push(entry, (uintptr_t)state);
    }
    return true;
}

static TRACED void traced_release(struct hook_mutex *state)
{
    stats_pop((uintptr_t)state);
    lock_ops_unlock(&state->raw);
}

static inline void acquire(struct hook_mutex *state)
{
    if (stats_enabled)
        traced_acquire(state);
    else
        lock_ops_lock(&state->raw, NULL);
}

static inline bool try_acquire(struct hook_mutex *state)
{
    return stats_enabled ? traced_try_acquire(state) : lock_ops_trylock(&state->raw);
}

static inline void release(struct hook_mutex *state)
{
    if (stats_enabled)
        traced_release(state);
    else
        lock_ops_unlock(&state->raw);
}

static inline struct hook_mutex *mutex_overlay(pthread_mutex_t *mutex)
{
    return (struct hook_mutex *)mutex;
}

static inline struct hook_cond *cond_overlay(pthread_cond_t *cond)
{
    return (struct hook_cond *)cond;
}

static inline bool recursive(const struct hook_mutex *state)
{
    return state->kind == PTHREAD_MUTEX_RECURSIVE;
}

/* One raw acquisition per outermost entry keeps admission on a single episode
 * for the whole recursive hold. */
static int recursive_acquire(struct hook_mutex *state, bool blocking)
{
    ensure_registered();
    if (__atomic_load_n(&state->owner, __ATOMIC_RELAXED) == thread_state.tid) {
        state->count++;
        return 0;
    }
    if (blocking)
        acquire(state);
    else if (!try_acquire(state))
        return EBUSY;
    __atomic_store_n(&state->owner, thread_state.tid, __ATOMIC_RELAXED);
    state->count = 1;
    return 0;
}

static void hook_lock(pthread_mutex_t *mutex)
{
    struct hook_mutex *state = mutex_overlay(mutex);

    if (recursive(state))
        recursive_acquire(state, true);
    else
        acquire(state);
}

static int hook_trylock(pthread_mutex_t *mutex)
{
    struct hook_mutex *state = mutex_overlay(mutex);

    if (recursive(state))
        return recursive_acquire(state, false);
    return try_acquire(state) ? 0 : EBUSY;
}

static void hook_unlock(pthread_mutex_t *mutex)
{
    struct hook_mutex *state = mutex_overlay(mutex);

    if (recursive(state)) {
        if (--state->count)
            return;
        __atomic_store_n(&state->owner, 0, __ATOMIC_RELAXED);
    }
    release(state);
}

EXPORT int pthread_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr)
{
    struct hook_mutex *state = mutex_overlay(mutex);
    int type = PTHREAD_MUTEX_DEFAULT;

    if (attr)
        pthread_mutexattr_gettype(attr, &type);
    memset(mutex, 0, sizeof(*mutex));
    /* Errorcheck, adaptive, robust and protocol attributes behave as normal. */
    state->kind = type == PTHREAD_MUTEX_RECURSIVE ? PTHREAD_MUTEX_RECURSIVE
                                                  : PTHREAD_MUTEX_NORMAL;
    return 0;
}

EXPORT int pthread_mutex_destroy(pthread_mutex_t *mutex)
{
    memset(mutex, 0, sizeof(*mutex));
    return 0;
}

EXPORT int pthread_mutex_lock(pthread_mutex_t *mutex)
{
    hook_lock(mutex);
    return 0;
}

EXPORT int pthread_mutex_trylock(pthread_mutex_t *mutex)
{
    return hook_trylock(mutex);
}

EXPORT int pthread_mutex_unlock(pthread_mutex_t *mutex)
{
    hook_unlock(mutex);
    return 0;
}

EXPORT int pthread_mutex_timedlock(pthread_mutex_t *mutex, const struct timespec *abstime)
{
    (void)mutex;
    (void)abstime;
    fputs("accordin fullhook: pthread_mutex_timedlock is not supported\n", stderr);
    abort();
}

static long futex_call(_Atomic uint32_t *word, int op, uint32_t value,
                       const struct timespec *abstime, uint32_t mask)
{
    return syscall(SYS_futex, (uint32_t *)word, op, value, abstime, NULL, mask);
}

/* The sequence bump and the waiter census form a sequentially consistent pair
 * with cond_wait's census bump and sequence snapshot, so a waiter that has
 * published itself either is woken here or observes the new sequence and does
 * not sleep. Waking costs a syscall only while somebody waits. */
static void cond_wake(pthread_cond_t *cond, int waiters)
{
    struct hook_cond *state = cond_overlay(cond);

    atomic_fetch_add_explicit(&state->seq, 1, memory_order_seq_cst);
    if (atomic_load_explicit(&state->parked, memory_order_seq_cst))
        futex_call(&state->seq, FUTEX_WAKE_PRIVATE, (uint32_t)waiters, NULL, 0);
}

/* The spin phase runs only with a slot, so the waiter never burns a CPU that
 * the scheduler owes to somebody else. */
static bool cv_spin_allowed(void)
{
    if (!cv_spin_ns || thread_state.depth || !admission_enabled ||
        !scheduler_admission)
        return false;
    ensure_registered();
    return thread_state.registered;
}

static uint64_t cv_spin_deadline(const struct timespec *abstime)
{
    uint64_t base = lock_ops_now_ns();
    uint64_t deadline = base + cv_spin_ns;
    struct timespec now;
    int64_t remaining;

    if (!abstime)
        return deadline;
    /* The caller's deadline is on the realtime clock the futex wait uses. */
    clock_gettime(CLOCK_REALTIME, &now);
    remaining = ((int64_t)abstime->tv_sec - now.tv_sec) * 1000000000LL +
                ((int64_t)abstime->tv_nsec - now.tv_nsec);
    if (remaining < 0)
        remaining = 0;
    if (base + (uint64_t)remaining < deadline)
        deadline = base + (uint64_t)remaining;
    return deadline;
}

/* Ask for this CPU's admission slot with one yield and, if it is granted,
 * watch the sequence directly instead of sleeping. Returns whether the
 * sequence moved, in which case no futex wait is needed at all. */
static bool cv_spin(struct hook_cond *state, uint32_t seq,
                    const struct timespec *abstime)
{
    bool moved = false;

    admission_begin();
    if (admission_try_once(USER_CV)) {
        uint64_t deadline = cv_spin_deadline(abstime);

        admission_publish_spinning(USER_CV);
        for (unsigned int spins = 0;; spins++) {
            if (atomic_load_explicit(&state->seq, memory_order_acquire) != seq) {
                moved = true;
                break;
            }
            spin_pause();
            /* Reading the clock every iteration would cost more than the spin. */
            if (!(spins & 63) && lock_ops_now_ns() >= deadline)
                break;
        }
    }
    admission_finish();
    return moved;
}

static int cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                     const struct timespec *abstime)
{
    struct hook_cond *state = cond_overlay(cond);
    int op = abstime ? (FUTEX_WAIT_BITSET_PRIVATE | FUTEX_CLOCK_REALTIME)
                     : FUTEX_WAIT_PRIVATE;
    int result = 0;
    uint32_t seq;

    if (stats_enabled) {
        struct stats_entry *entry = stats_entry(mutex_overlay(mutex));
        if (entry)
            entry->cond_waits++;
    }
    seq = atomic_load_explicit(&state->seq, memory_order_seq_cst);
    hook_unlock(mutex);

    if (!(cv_spin_allowed() && cv_spin(state, seq, abstime))) {
        /* Joining the census and re-reading the sequence pairs sequentially
         * with cond_wake's bump and census read, so a signal that misses the
         * census here is one this thread observes before it sleeps. */
        atomic_fetch_add_explicit(&state->parked, 1, memory_order_seq_cst);
        if (atomic_load_explicit(&state->seq, memory_order_seq_cst) == seq) {
            for (;;) {
                if (!futex_call(&state->seq, op, seq, abstime, FUTEX_BITSET_MATCH_ANY))
                    break;
                if (errno == EINTR)
                    continue;
                if (errno == ETIMEDOUT)
                    result = ETIMEDOUT;
                /* EAGAIN means the sequence already moved, which is a wakeup. */
                break;
            }
        }
        atomic_fetch_sub_explicit(&state->parked, 1, memory_order_seq_cst);
    }
    /* Reacquisition is an ordinary outer lock and opens a new episode. */
    hook_lock(mutex);
    return result;
}

EXPORT int pthread_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr)
{
    (void)attr;
    memset(cond, 0, sizeof(*cond));
    return 0;
}

EXPORT int pthread_cond_destroy(pthread_cond_t *cond)
{
    memset(cond, 0, sizeof(*cond));
    return 0;
}

EXPORT int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex)
{
    return cond_wait(cond, mutex, NULL);
}

EXPORT int pthread_cond_timedwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                                  const struct timespec *abstime)
{
    return cond_wait(cond, mutex, abstime);
}

EXPORT int pthread_cond_signal(pthread_cond_t *cond)
{
    cond_wake(cond, 1);
    return 0;
}

EXPORT int pthread_cond_broadcast(pthread_cond_t *cond)
{
    cond_wake(cond, INT_MAX);
    return 0;
}
