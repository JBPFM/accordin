/* SPDX-License-Identifier: GPL-2.0-only */
#include <ctype.h>
#include <pthread.h>
#include <stdio.h>
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
/* Keep MCS on parking by default: its measured write gain was not stable
 * enough to justify the streamcluster cost. Both backends accept an override. */
#ifdef MCS_TAS
uint64_t cv_spin_ns = 50000;
#else
uint64_t cv_spin_ns;
#endif
static struct accordin *skel;
static struct bpf_link *scheduler_link;
static int thread_map_fd = -1;
static pthread_key_t registration_key;
static libbpf_print_fn_t previous_log;

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

__attribute__((constructor)) static void scheduler_start(void)
{
    const char *spin = getenv("ACCORDIN_CV_SPIN_US");
    if (spin && *spin >= '0' && *spin <= '9') {
        char *end;
        unsigned long us = strtoul(spin, &end, 10);
        if (!*end && us <= 1000000)
            cv_spin_ns = (uint64_t)us * 1000;
    }
    admission_enabled = !env_flag("ACCORDIN_DISABLE_ADMISSION");
    if (env_flag(PREFIX "_DISABLE_BPF"))
        return;
    previous_log = libbpf_set_print(libbpf_log);
    SCX_BUG_ON(libbpf_num_possible_cpus() > (int)MAX_CPUS,
               "Admission supports at most %u CPUs", MAX_CPUS);
    SCX_BUG_ON(pthread_key_create(&registration_key, unregister_thread),
               "Failed to create thread cleanup key");
    skel = SCX_OPS_OPEN(accordin_ops, accordin);
    skel->bss->stats_only_mode = env_flag(PREFIX "_STATS_ONLY");
    SCX_OPS_LOAD(skel, accordin_ops, accordin, uei);
    thread_map_fd = bpf_map__fd(skel->maps.thread_ctx_addr_map);
    scheduler_link = SCX_OPS_ATTACH(skel, accordin_ops, accordin);
    scheduler_admission = &skel->bss->admission;
    fprintf(stderr, "[%s] eBPF scheduler loaded successfully\n", PREFIX);
}

__attribute__((destructor)) static void scheduler_stop(void)
{
    if (!skel)
        return;
    if (UEI_EXITED(skel, uei))
        UEI_REPORT(skel, uei);
    scheduler_admission = NULL;
    bpf_link__destroy(scheduler_link);
    pthread_key_delete(registration_key);
    thread_map_fd = -1;
    accordin__destroy(skel);
    btf__free(__COMPAT_vmlinux_btf);
    libbpf_set_print(previous_log);
}
