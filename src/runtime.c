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
static struct accordin *skel;
static struct bpf_link *scheduler_link;
static int thread_map_fd = -1;
static pthread_key_t registration_key;
static libbpf_print_fn_t previous_log;
static int cv_flush_prog_fd = -1;
static int cv_flush_running;
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

bool accordin_cv_custody_ready(void)
{
    struct admission_state *state;

    if (!cv_custody_on || cv_flush_prog_fd < 0 ||
        !__atomic_load_n(&cv_custody_live, __ATOMIC_ACQUIRE))
        return false;
    state = scheduler_admission;
    return state && __atomic_load_n(&state->enabled, __ATOMIC_ACQUIRE);
}

int accordin_cv_flush_now(unsigned int width, unsigned int flags)
{
    struct cv_flush_ctx ctx = {
        .width = width ? width : cv_flush_width,
        .flags = flags ? flags : cv_flush_flags,
    };
    /* A syscall program rejects ctx_out and writes its results back through
     * ctx_in instead. */
    LIBBPF_OPTS(bpf_test_run_opts, opts, .ctx_in = &ctx, .ctx_size_in = sizeof(ctx));
    int idle = 0;
    int result;

    if (cv_flush_prog_fd < 0)
        return 0;
    /* One transfer at a time: a concurrent notifier's work is already covered
     * by the flush in flight, or by the next one. */
    if (!__atomic_compare_exchange_n(&cv_flush_running, &idle, 1, false,
                                     __ATOMIC_ACQUIRE, __ATOMIC_RELAXED))
        return 0;
    result = bpf_prog_test_run_opts(cv_flush_prog_fd, &opts);
    __atomic_store_n(&cv_flush_running, 0, __ATOMIC_RELEASE);
    return result ? -1 : (int)ctx.moved;
}

static void unregister_thread(void *value)
{
    uint32_t tid = (uintptr_t)value;
    bpf_map_delete_elem(thread_map_fd, &tid);
}

void register_thread(void)
{
    thread_state.tid = syscall(SYS_gettid);
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

    admission_enabled = !env_flag("ACCORDIN_DISABLE_ADMISSION");
    if (env_flag(PREFIX "_DISABLE_BPF"))
        return;
    previous_log = libbpf_set_print(libbpf_log);
    SCX_BUG_ON(libbpf_num_possible_cpus() > (int)MAX_CPUS,
               "Admission supports at most %u CPUs", MAX_CPUS);
    SCX_BUG_ON(pthread_key_create(&registration_key, unregister_thread),
               "Failed to create thread cleanup key");
    cv_custody_on = env_allowed("ACCORDIN_CV_CUSTODY");
    cv_counters_on = env_flag("ACCORDIN_CV_COUNTERS");
    cv_flush_width = env_u32("ACCORDIN_CV_FLUSH_WIDTH", 0);
    cv_flush_flags = env_u32("ACCORDIN_CV_FLUSH_FLAGS", CV_FLUSH_EXPIRE);
    skel = SCX_OPS_OPEN(accordin_ops, accordin);
    skel->bss->stats_only_mode = env_flag(PREFIX "_STATS_ONLY");
    skel->bss->cv_custody_enabled = cv_custody_on;
    limit_ns = (uint64_t)env_u32("ACCORDIN_CV_CUSTODY_MS", 20) * 1000000ULL;
    skel->bss->cv_custody_limit_ns = limit_ns;
    skel->bss->cv_scan_period_ns = clamp_u64(limit_ns / 2, 1000000ULL, 10000000ULL);
    if (env_flag("ACCORDIN_VERIFY_ONLY")) {
        verify_programs();
        admission_enabled = false;
        pthread_key_delete(registration_key);
        btf__free(__COMPAT_vmlinux_btf);
        __COMPAT_vmlinux_btf = NULL;
        libbpf_set_print(previous_log);
        return;
    }
    SCX_OPS_LOAD(skel, accordin_ops, accordin, uei);
    thread_map_fd = bpf_map__fd(skel->maps.thread_ctx_addr_map);
    cv_flush_prog_fd = bpf_program__fd(skel->progs.accordin_cv_flush);
    scheduler_link = SCX_OPS_ATTACH(skel, accordin_ops, accordin);
    scheduler_admission = &skel->bss->admission;
    __atomic_store_n(&cv_custody_live, true, __ATOMIC_RELEASE);
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
    pthread_key_delete(registration_key);
    thread_map_fd = -1;
    accordin__destroy(skel);
    btf__free(__COMPAT_vmlinux_btf);
    libbpf_set_print(previous_log);
}
