/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef ACCORDIN_RSEQ_SLOT_H
#define ACCORDIN_RSEQ_SLOT_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

/* Outcome of one attempt to put a value into an admission entry: the entry now
 * carries the value, the entry carried something other than the expected value,
 * the thread left the CPU the entry belongs to and the value was taken back
 * again, or the attempt was restarted before it could commit. Only a
 * restartable section reports the last. */
#define SLOT_COMMITTED 0
#define SLOT_MISMATCH 1
#define SLOT_MIGRATED 2
#define SLOT_RESTARTED 3

#if (defined(__x86_64__) || defined(__aarch64__)) && defined(__has_include)
#if __has_include(<sys/rseq.h>)
#include <sys/rseq.h>
#define ACCORDIN_RSEQ_SLOT 1
#endif
#endif

#ifdef ACCORDIN_RSEQ_SLOT

/* The word the kernel requires in the four bytes ahead of an abort handler, so
 * that a jump into one can only come from the restart it was written for. Both
 * encodings trap when executed. */
#if defined(__x86_64__)
#define ACCORDIN_RSEQ_SIG 0x53053053
#else
#define ACCORDIN_RSEQ_SIG 0xd428bc00
#endif

/* The descriptor the kernel reads to bound one section: a version and a flags
 * word, then the entry address, the length and the abort handler. It is
 * link-time data of its own section and reads the same whatever instructions
 * the section is made of, so both sequences below share this one block. */
#define ACCORDIN_RSEQ_DESCRIPTOR                                              \
    ".pushsection __rseq_cs, \"aw\"\n\t"                                      \
    ".balign 32\n\t"                                                          \
    "3:\n\t"                                                                  \
    ".long 0, 0\n\t"                                                          \
    ".quad 1f, 2f - 1f, 4f\n\t"                                               \
    ".popsection\n\t"

/* The CPU the kernel last placed this thread on, or a negative value while the
 * area carries no id. The value is only a hint: the section below compares it
 * against the same field once more and restarts when they differ. */
static inline int rseq_cpu(void *area)
{
    return (int)__atomic_load_n(&((struct rseq *)area)->cpu_id,
                                __ATOMIC_RELAXED);
}

/* Put newv into *ptr if *ptr still reads expect, as a plain load, compare and
 * store inside a restartable section bound to cpu. The kernel restarts the
 * section, from the handler named by the descriptor, whenever the thread is
 * preempted, migrated or signalled between its first instruction and the store,
 * so a store that executes at all executes while the thread still runs on cpu.
 * The handler carries the signature word immediately ahead of its entry. */
static inline int rseq_cmpeqv_storev(void *thread_area,
                                     unsigned long long *ptr,
                                     unsigned long long expect,
                                     unsigned long long newv, int cpu)
{
    struct rseq *area = (struct rseq *)thread_area;

#if defined(__x86_64__)
    __asm__ __volatile__ goto(
        ACCORDIN_RSEQ_DESCRIPTOR
        "leaq 3b(%%rip), %%rax\n\t"
        "movq %%rax, %[rseq_cs]\n\t"
        "1:\n\t"
        "cmpl %[cpu], %[cpu_id]\n\t"
        "jnz 4f\n\t"
        "cmpq %[expect], %[entry]\n\t"
        "jnz %l[mismatch]\n\t"
        "movq %[newv], %[entry]\n\t"
        "2:\n\t"
        ".pushsection __rseq_failure, \"ax\"\n\t"
        ".byte 0x0f, 0xb9, 0x3d\n\t"
        ".long %c[sig]\n\t"
        "4:\n\t"
        "jmp %l[restart]\n\t"
        ".popsection\n\t"
        :
        : [rseq_cs] "m"(area->rseq_cs), [cpu_id] "m"(area->cpu_id),
          [cpu] "r"(cpu), [entry] "m"(*ptr), [expect] "r"(expect),
          [newv] "r"(newv), [sig] "i"(ACCORDIN_RSEQ_SIG)
        : "memory", "cc", "rax"
        : mismatch, restart);
#else
    __asm__ __volatile__ goto(
        ACCORDIN_RSEQ_DESCRIPTOR
        "adrp x15, 3b\n\t"
        "add x15, x15, :lo12:3b\n\t"
        "str x15, %[rseq_cs]\n\t"
        "1:\n\t"
        "ldr w15, %[cpu_id]\n\t"
        "sub w15, w15, %w[cpu]\n\t"
        "cbnz w15, 4f\n\t"
        "ldr x15, %[entry]\n\t"
        "sub x15, x15, %[expect]\n\t"
        "cbnz x15, %l[mismatch]\n\t"
        "str %[newv], %[entry]\n\t"
        "2:\n\t"
        "b 5f\n\t"
        ".inst %c[sig]\n\t"
        "4:\n\t"
        "b %l[restart]\n\t"
        "5:\n\t"
        :
        : [rseq_cs] "Qo"(area->rseq_cs), [cpu_id] "Qo"(area->cpu_id),
          [cpu] "r"(cpu), [entry] "Qo"(*ptr), [expect] "r"(expect),
          [newv] "r"(newv), [sig] "i"(ACCORDIN_RSEQ_SIG)
        : "memory", "cc", "x15"
        : mismatch, restart);
#endif
    /* The descriptor belongs to a library that may be unloaded while the thread
     * lives on, so no exit leaves the area pointing at it. A restart has the
     * kernel clear the field already; clearing it again costs one store. */
    __atomic_store_n(&area->rseq_cs, 0, __ATOMIC_RELAXED);
    return SLOT_COMMITTED;
mismatch:
    __atomic_store_n(&area->rseq_cs, 0, __ATOMIC_RELAXED);
    return SLOT_MISMATCH;
restart:
    __atomic_store_n(&area->rseq_cs, 0, __ATOMIC_RELAXED);
    return SLOT_RESTARTED;
}

#endif /* ACCORDIN_RSEQ_SLOT */

/* Whether the sequences above can run at all. The C library registers one area
 * per thread and publishes its size; a zero size means no thread has one. */
static inline bool rseq_area_available(void)
{
#ifdef ACCORDIN_RSEQ_SLOT
    return __rseq_size != 0;
#else
    return false;
#endif
}

/* The calling thread's area, found through the thread pointer, or null where
 * none was registered. Each thread resolves this once and keeps it, so the
 * sequences reach the area without a symbol lookup of their own. */
static inline void *rseq_thread_area(void)
{
#ifdef ACCORDIN_RSEQ_SLOT
    if (rseq_area_available())
        return (char *)__builtin_thread_pointer() + __rseq_offset;
#endif
    return NULL;
}

#endif
