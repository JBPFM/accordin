/*
 * File: test_init.c
 * Author: Victor Laforet <victor.laforet@inria.fr>
 *
 * Description:
 *      Test the initialization of a lock.
 *      For example for flexguard:
 *          Deploys the preemption monitor eBPF code to test context switch overhead
 *
 * The MIT License (MIT)
 *
 * Copyright (c) 2024 Victor Laforet
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy of
 * this software and associated documentation files (the "Software"), to deal in
 * the Software without restriction, including without limitation the rights to
 * use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
 * the Software, and to permit persons to whom the Software is furnished to do so,
 * subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
 * FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
 * COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
 * IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
 * CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

#include <getopt.h>
#include <limits.h>
#include <pthread.h>
#include <signal.h>
#include <stdlib.h>
#include <stdio.h>
#include <sys/time.h>
#include <time.h>
#include <malloc.h>
#include "atomic_ops.h"
#include "utils.h"
#include "lock_if.h"

#define XSTR(s) STR(s)
#define STR(s) #s

#define DEFAULT_DURATION 0

/* ################################################################### *
 * GLOBALS
 * ################################################################### */

libslock_t the_lock;
uint32_t done = 0; // uint32_t to use futexes
int duration = DEFAULT_DURATION;

/* ################################################################### *
 * SETUP
 * ################################################################### */

void catcher(int sig)
{
    printf("CAUGHT SIGNAL %d\n", sig);
    done = 1;
    futex_wake(&done, 1);
}

int main(int argc, char **argv)
{
    int i, c;

    struct option long_options[] = {
        {"help", no_argument, NULL, 'h'},
        {"duration", required_argument, NULL, 'd'},
        {NULL, 0, NULL, 0}};

    while (1)
    {
        i = 0;
        c = getopt_long(argc, argv, "hd:", long_options, &i);

        if (c == -1)
            break;

        if (c == 0 && long_options[i].flag == 0)
            c = long_options[i].val;

        switch (c)
        {
        case 0:
            /* Flag is automatically set */
            break;
        case 'h':
            printf("test_init -- Test initialization of a lock\n");
            printf("\n");
            printf("Usage:\n");
            printf("  test_init [options...]\n");
            printf("\n");
            printf("Options:\n");
            printf("  -h, --help\n");
            printf("        Print this message\n");
            printf("  -d, --duration <int>\n");
            printf("        Duration of the test, 0 = infinity (default=" XSTR(DEFAULT_DURATION) ")\n");
            exit(0);
        case 'd':
            duration = atoi(optarg);
            break;
        case '?':
            printf("Use -h or --help for help\n");
            exit(0);
        default:
            exit(1);
        }
    }

    if (signal(SIGHUP, catcher) == SIG_ERR ||
        signal(SIGINT, catcher) == SIG_ERR ||
        signal(SIGTERM, catcher) == SIG_ERR)
    {
        perror("signal");
        exit(1);
    }

    DPRINT("Initializing preemption monitor\n");
    libslock_init(&the_lock);

    struct timespec *timeout = NULL;
    if (duration != 0)
    {
        timeout = malloc(sizeof(struct timespec));
        timeout->tv_sec = duration / 1000;
        timeout->tv_nsec = (duration % 1000) * 1000000;
    }

    long ret = 0;
    while (!done && (ret == 0 || errno != ETIMEDOUT && errno != EINTR))
        ret = futex_wait_timeout(&done, 0, timeout);

    libslock_destroy(&the_lock);
    return EXIT_SUCCESS;
}
