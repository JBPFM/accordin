/* SPDX-License-Identifier: GPL-2.0-only */
#include <ctype.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <bpf/bpf.h>
#include <bpf/libbpf.h>
#include <scx/common.h>
#include "runtime.h"
#include "accordin.skel.h"

#ifdef MCS_TAS
#define PREFIX "MCS_TAS_ACCORDIN_DIRECT"
#else
#define PREFIX "MCS_ACCORDIN_DIRECT"
#endif

_Thread_local struct thread_state thread_state;
struct admission_state *scheduler_admission;
bool admission_enabled;
bool auto_admission;
static struct accordin *skel;
static struct bpf_link *scheduler_link;
static int thread_map_fd = -1;
/* True until the constructor has decided whether a registry exists. A lock
 * taken before that decision, by an allocator or a library the load itself
 * calls into, has nowhere to publish its admission word. */
static bool registry_opening = true;
static pthread_key_t registration_key;
static libbpf_print_fn_t previous_log;
static int cv_flush_prog_fd = -1;
static int cv_flush_running;
static unsigned int cv_flush_requests;
static unsigned int cv_flush_width, cv_flush_flags;
static bool cv_custody_on, cv_counters_on;
/* Set once the scheduler is attached and cleared before it goes away, so a
 * waiter never reaches the mapped admission state through a stale pointer. */
static bool cv_custody_live;

static bool env_flag(const char *name)
{
    const char *value = getenv(name);
    if (!value)
        return false;
    while (isspace((unsigned char)*value))
        value++;
    size_t len = strlen(value);
    while (len && isspace((unsigned char)value[len - 1]))
        len--;
    return (len == 1 && *value == '1') ||
           (len == 4 && !strncasecmp(value, "true", len)) ||
           (len == 3 && !strncasecmp(value, "yes", len)) ||
           (len == 2 && !strncasecmp(value, "on", len));
}

/* Enabled unless explicitly denied, the opposite default of env_flag. */
static bool env_allowed(const char *name)
{
    const char *value = getenv(name);
    if (!value)
        return true;
    while (isspace((unsigned char)*value))
        value++;
    size_t len = strlen(value);
    while (len && isspace((unsigned char)value[len - 1]))
        len--;
    return !((len == 1 && *value == '0') ||
             (len == 5 && !strncasecmp(value, "false", len)) ||
             (len == 2 && !strncasecmp(value, "no", len)) ||
             (len == 3 && !strncasecmp(value, "off", len)));
}

static uint64_t clamp_u64(uint64_t value, uint64_t low, uint64_t high)
{
    return value < low ? low : value > high ? high : value;
}

static unsigned int env_u32(const char *name, unsigned int fallback)
{
    const char *value = getenv(name);
    char *end;
    unsigned long parsed;

    if (!value || !*value)
        return fallback;
    parsed = strtoul(value, &end, 0);
    while (isspace((unsigned char)*end))
        end++;
    if (*end || parsed > UINT32_MAX) {
        fprintf(stderr, "[accordin] ignoring malformed %s=%s\n", name, value);
        return fallback;
    }
    return (unsigned int)parsed;
}

/* Group geometry defaults: eight CPUs to a group cover a slice of one node's
 * last-level cache. The slack is the head-age margin the own queue keeps over
 * the group, wide enough to cover one handover and narrow enough that a queue
 * left behind is served next. Which queue is served follows from the head ages,
 * so the count bound over the own queue is left off. */
#define GROUP_SIZE_DEFAULT 8
#define OWN_LIMIT_DEFAULT 0
#define OWN_SLACK_US_DEFAULT 100

static bool read_sysfs(const char *path, char *text, size_t size)
{
    FILE *file = fopen(path, "re");
    size_t got;

    if (!file)
        return false;
    got = fread(text, 1, size - 1, file);
    fclose(file);
    text[got] = '\0';
    return true;
}

/* Mark the CPUs named by a sysfs list such as "0-3,8,10-11". */
static void mark_cpu_list(const char *text, bool *marked)
{
    while (*text) {
        unsigned long first, last, cpu;
        char *end;

        if (!isdigit((unsigned char)*text)) {
            text++;
            continue;
        }
        first = last = strtoul(text, &end, 10);
        if (*end == '-')
            last = strtoul(end + 1, &end, 10);
        for (cpu = first; cpu <= last && cpu < MAX_CPUS; cpu++)
            marked[cpu] = true;
        text = end;
    }
}

/* Cut the online CPUs of each NUMA node, in ascending order, into groups of
 * equal size, so that the members of one group share one memory node, in slices
 * small enough to stay cache-friendly. A node whose CPU count is not a multiple
 * of the size ends in a shorter group, and a CPU no node claims stands alone. */
static void publish_groups(void)
{
    bool online[MAX_CPUS] = {0}, placed[MAX_CPUS] = {0}, nodes[MAX_CPUS] = {0};
    unsigned int size = env_u32("ACCORDIN_GROUP_SIZE", GROUP_SIZE_DEFAULT);
    unsigned int own_limit = env_u32("ACCORDIN_OWN_LIMIT", OWN_LIMIT_DEFAULT);
    unsigned int own_slack_us =
        env_u32("ACCORDIN_OWN_SLACK_US", OWN_SLACK_US_DEFAULT);
    unsigned int count = 0, cpu, node;
    char text[4096];

    if (size > MAX_GROUP_SIZE) {
        fprintf(stderr, "[accordin] group size %u exceeds %u, using %u\n", size,
                MAX_GROUP_SIZE, MAX_GROUP_SIZE);
        size = MAX_GROUP_SIZE;
    }
    if (!size)
        size = 1;
    for (cpu = 0; cpu < MAX_CPUS; cpu++)
        skel->bss->cpu_group[cpu] = CPU_NO_GROUP;
    if (read_sysfs("/sys/devices/system/cpu/online", text, sizeof(text)))
        mark_cpu_list(text, online);
    if (read_sysfs("/sys/devices/system/node/online", text, sizeof(text)))
        mark_cpu_list(text, nodes);
    for (node = 0; node < MAX_CPUS; node++) {
        bool members[MAX_CPUS] = {0};
        /* A full count opens a group at the node's first CPU. */
        unsigned int filled = size;
        char path[128];

        if (!nodes[node])
            continue;
        snprintf(path, sizeof(path), "/sys/devices/system/node/node%u/cpulist",
                 node);
        if (!read_sysfs(path, text, sizeof(text)))
            continue;
        mark_cpu_list(text, members);
        for (cpu = 0; cpu < MAX_CPUS; cpu++) {
            if (!members[cpu] || !online[cpu] || placed[cpu])
                continue;
            if (filled >= size) {
                if (count >= MAX_GROUPS)
                    break;
                count++;
                filled = 0;
            }
            skel->bss->cpu_group[cpu] = count - 1;
            skel->bss->group_member[count - 1][filled++] = cpu;
            skel->bss->group_size[count - 1] = filled;
            placed[cpu] = true;
        }
    }
    for (cpu = 0; cpu < MAX_CPUS && count < MAX_GROUPS; cpu++) {
        if (!online[cpu] || placed[cpu])
            continue;
        skel->bss->cpu_group[cpu] = count;
        skel->bss->group_member[count][0] = cpu;
        skel->bss->group_size[count] = 1;
        placed[cpu] = true;
        count++;
    }
    skel->bss->group_count = count;
    skel->bss->own_limit = own_limit;
    skel->bss->own_slack_ns = (uint64_t)own_slack_us * 1000;
    if (cv_counters_on)
        fprintf(stderr,
                "[accordin_groups] size=%u groups=%u own_limit=%u"
                " own_slack_us=%u\n",
                size, count, own_limit, own_slack_us);
}

bool accordin_cv_custody_ready(void)
{
    struct admission_state *state;

    if (!admission_active() || !cv_custody_on || cv_flush_prog_fd < 0 ||
        !__atomic_load_n(&cv_custody_live, __ATOMIC_ACQUIRE))
        return false;
    state = scheduler_admission;
    return state && __atomic_load_n(&state->enabled, __ATOMIC_ACQUIRE);
}

/* A child of fork() inherits the transfer state of a process that may have been
 * mid-flush in another thread, and no thread to finish it. */
static void forget_flush(void)
{
    __atomic_store_n(&cv_flush_running, 0, __ATOMIC_RELAXED);
    __atomic_store_n(&cv_flush_requests, 0, __ATOMIC_RELAXED);
}

static bool claim_flush(void)
{
    int idle = 0;

    return __atomic_compare_exchange_n(&cv_flush_running, &idle, 1, false,
                                       __ATOMIC_ACQUIRE, __ATOMIC_RELAXED);
}

/* A flushing thread also serves the requests that arrive while it runs, but
 * only for a bounded number of passes: a steady stream of notifications must
 * not pin one thread to the transfer. */
#define CV_FLUSH_MAX_PASSES 3

/* Returns the waits released, or a negative value when this call did not carry
 * the caller's batch through: another thread owns the transfer, the pass cap
 * was reached with work left, or the program failed. The caller keeps its
 * pending mark and retries at its next release point; custody expiry remains
 * the backstop. */
int accordin_cv_flush_now(unsigned int width, unsigned int flags)
{
    struct cv_flush_ctx ctx = {
        .width = width ? width : cv_flush_width,
        .flags = flags ? flags : cv_flush_flags,
    };
    /* A syscall program rejects ctx_out and writes its results back through
     * ctx_in instead. */
    LIBBPF_OPTS(bpf_test_run_opts, opts, .ctx_in = &ctx, .ctx_size_in = sizeof(ctx));
    unsigned int passes = 0;
    int moved = 0;

    if (cv_flush_prog_fd < 0 || !admission_active())
        return 0;
    /* Counted after the notification it belongs to is published, so a pass
     * that begins after reading this count carries that notification. */
    __atomic_fetch_add(&cv_flush_requests, 1, __ATOMIC_SEQ_CST);
    /* One transfer at a time. */
    if (!claim_flush())
        return -1;
    for (;;) {
        unsigned int seen = __atomic_load_n(&cv_flush_requests, __ATOMIC_SEQ_CST);
        int result = bpf_prog_test_run_opts(cv_flush_prog_fd, &opts);
        bool complete;

        passes++;
        /* A width-bounded pass leaves a tail parked, and a notification that
         * landed during the pass may not have been covered by it. */
        complete = !result && !ctx.pending;
        __atomic_store_n(&cv_flush_running, 0, __ATOMIC_SEQ_CST);
        if (result)
            return -1;
        moved += (int)ctx.moved;
        if (complete &&
            __atomic_load_n(&cv_flush_requests, __ATOMIC_SEQ_CST) == seen)
            return moved;
        if (passes >= CV_FLUSH_MAX_PASSES || !claim_flush())
            return -1;
    }
}

static void unregister_thread(void *value)
{
    uint32_t tid = (uintptr_t)value;
    bpf_map_delete_elem(thread_map_fd, &tid);
}

void register_thread(void)
{
    if (!thread_state.tid)
        thread_state.tid = syscall(SYS_gettid);
    /* An unpublished word reads as an idle thread, which is never admitted, so
     * a thread that locks while the registry is still opening stays
     * unregistered and publishes at its next lock instead. Where no registry is
     * coming there is nothing to publish, ever. */
    if (thread_map_fd < 0 &&
        __atomic_load_n(&registry_opening, __ATOMIC_ACQUIRE))
        return;
    if (thread_map_fd >= 0) {
        uint64_t address = (uintptr_t)&thread_state.word;
        SCX_BUG_ON(bpf_map_update_elem(thread_map_fd, &thread_state.tid, &address, BPF_ANY),
                   "Failed to register admission word");
        SCX_BUG_ON(pthread_setspecific(registration_key, (void *)(uintptr_t)thread_state.tid),
                   "Failed to register thread cleanup");
    }
    thread_state.registered = true;
}

static int libbpf_log(enum libbpf_print_level level, const char *fmt, va_list args)
{
    return level == LIBBPF_DEBUG ? 0 : vfprintf(stderr, fmt, args);
}

/* The verifier log arrives as one debug message per program; report only the
 * instruction budget line, tagged with the program it belongs to. */
static int verifier_log(enum libbpf_print_level level, const char *fmt, va_list args)
{
    char *text = NULL, *line, *next;
    char program[64] = "";

    if (level != LIBBPF_DEBUG)
        return vfprintf(stderr, fmt, args);
    if (vasprintf(&text, fmt, args) < 0)
        return 0;
    for (line = text; line && *line; line = next) {
        char *quote = strstr(line, "prog '");
        next = strchr(line, '\n');
        if (next)
            *next++ = '\0';
        if (quote) {
            char *end = strchr(quote + 6, '\'');
            if (end && (size_t)(end - quote - 6) < sizeof(program)) {
                memcpy(program, quote + 6, end - quote - 6);
                program[end - quote - 6] = '\0';
            }
        }
        if (!strncmp(line, "processed ", 10))
            fprintf(stderr, "[accordin_verify] %s: %s\n", program, line);
    }
    free(text);
    return 0;
}

/* Load every program with a verifier log and report its cost, then leave the
 * scheduler unattached so the caller measures without perturbing the machine. */
static void verify_programs(void)
{
    struct bpf_program *program;

    bpf_object__for_each_program(program, skel->obj)
        bpf_program__set_log_level(program, 1);
    libbpf_set_print(verifier_log);
    SCX_BUG_ON(accordin__load(skel), "Failed to load skel");
    libbpf_set_print(libbpf_log);
    accordin__destroy(skel);
    skel = NULL;
}

static void report_counters(void)
{
    fprintf(stderr,
            "[accordin_cv] parked=%llu flushed=%llu expired=%llu drained=%llu "
            "parked_now=%llu flush_calls=%llu flush_misses=%llu\n",
            (unsigned long long)skel->bss->cv_parked,
            (unsigned long long)skel->bss->cv_flushed,
            (unsigned long long)skel->bss->cv_expired,
            (unsigned long long)skel->bss->cv_drained,
            (unsigned long long)skel->bss->cv_parked_now,
            (unsigned long long)skel->bss->cv_flush_calls,
            (unsigned long long)skel->bss->cv_flush_misses);
}

__attribute__((constructor)) static void scheduler_start(void)
{
    uint64_t limit_ns;
    cpu_set_t cpus;

    admission_enabled = !env_flag("ACCORDIN_DISABLE_ADMISSION");
    auto_admission = env_flag("ACCORDIN_AUTO_ADMISSION");
    if (env_flag(PREFIX "_DISABLE_BPF")) {
        __atomic_store_n(&registry_opening, false, __ATOMIC_RELEASE);
        return;
    }
    previous_log = libbpf_set_print(libbpf_log);
    SCX_BUG_ON(libbpf_num_possible_cpus() > (int)MAX_CPUS,
               "Admission supports at most %u CPUs", MAX_CPUS);
    SCX_BUG_ON(pthread_key_create(&registration_key, unregister_thread),
               "Failed to create thread cleanup key");
    cv_custody_on = env_allowed("ACCORDIN_CV_CUSTODY");
    cv_counters_on = env_flag("ACCORDIN_CV_COUNTERS");
    cv_flush_width = env_u32("ACCORDIN_CV_FLUSH_WIDTH", 0);
    /* Notified waits leave custody through the flush; the timer keeps expiry.
     * Where a released wait lands in the admission queue is set by its park
     * stamp, so the walk order shows only under a width cap, and walking the
     * custody queue from its head hands the oldest parks over first. */
    cv_flush_flags = env_u32("ACCORDIN_CV_FLUSH_FLAGS", CV_FLUSH_MOVE);
    SCX_BUG_ON(pthread_atfork(NULL, NULL, forget_flush),
               "Failed to register fork cleanup");
    skel = SCX_OPS_OPEN(accordin_ops, accordin);
    skel->bss->stats_only_mode = env_flag(PREFIX "_STATS_ONLY");
    skel->rodata->auto_admission = auto_admission && admission_enabled &&
                                  !skel->bss->stats_only_mode;
    skel->rodata->auto_tgid = getpid();
    SCX_BUG_ON(sched_getaffinity(0, sizeof(cpus), &cpus), "Failed to read affinity");
    skel->rodata->auto_capacity = CPU_COUNT(&cpus);
    skel->bss->cv_custody_enabled = cv_custody_on;
    limit_ns = (uint64_t)env_u32("ACCORDIN_CV_CUSTODY_MS", 20) * 1000000ULL;
    skel->bss->cv_custody_limit_ns = limit_ns;
    skel->bss->cv_scan_period_ns = clamp_u64(limit_ns / 2, 1000000ULL, 10000000ULL);
    publish_groups();
    if (env_flag("ACCORDIN_VERIFY_ONLY")) {
        verify_programs();
        admission_enabled = false;
        __atomic_store_n(&registry_opening, false, __ATOMIC_RELEASE);
        pthread_key_delete(registration_key);
        btf__free(__COMPAT_vmlinux_btf);
        __COMPAT_vmlinux_btf = NULL;
        libbpf_set_print(previous_log);
        return;
    }
    SCX_OPS_LOAD(skel, accordin_ops, accordin, uei);
    thread_map_fd = bpf_map__fd(skel->maps.thread_ctx_addr_map);
    __atomic_store_n(&registry_opening, false, __ATOMIC_RELEASE);
    cv_flush_prog_fd = bpf_program__fd(skel->progs.accordin_cv_flush);
    scheduler_link = SCX_OPS_ATTACH(skel, accordin_ops, accordin);
    scheduler_admission = &skel->bss->admission;
    __atomic_store_n(&cv_custody_live, true, __ATOMIC_RELEASE);
    /* The head peek is resolved by the loader, so only the attached scheduler
     * can say whether the running kernel offers it. */
    if (cv_counters_on)
        fprintf(stderr, "[accordin_peek] dsq_peek=%s\n",
                skel->bss->dsq_peek_ready ? "yes" : "no");
    fprintf(stderr, "[%s] eBPF scheduler loaded successfully\n", PREFIX);
}

__attribute__((destructor)) static void scheduler_stop(void)
{
    struct admission_state *state = scheduler_admission;

    if (!skel)
        return;
    __atomic_store_n(&cv_custody_live, false, __ATOMIC_RELEASE);
    if (UEI_EXITED(skel, uei))
        UEI_REPORT(skel, uei);
    scheduler_admission = NULL;
    cv_flush_prog_fd = -1;
    bpf_link__destroy(scheduler_link);
    /* The detached scheduler no longer admits anyone; a waiter still reading
     * the mapping must see that rather than a stale grant. */
    if (state)
        __atomic_store_n(&state->enabled, 0, __ATOMIC_RELEASE);
    if (cv_counters_on)
        report_counters();
    if (auto_admission)
        fprintf(stderr, "[accordin_auto] active=%u capacity=%u trigger_runnable=%u at_ns=%llu\n",
                skel->bss->admission.active, skel->rodata->auto_capacity,
                skel->bss->auto_trigger_runnable,
                (unsigned long long)skel->bss->auto_activated_at);
    pthread_key_delete(registration_key);
    thread_map_fd = -1;
    accordin__destroy(skel);
    btf__free(__COMPAT_vmlinux_btf);
    libbpf_set_print(previous_log);
}
