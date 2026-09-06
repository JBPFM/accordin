/*
 * File: scheduling.c
 * Author: Victor Laforet <victor.laforet@inria.fr>
 *
 * Description:
 *      Benchmark varying thread count.
 *
 * The MIT License (MIT)
 *
 * Copyright (c) 2023 Victor Laforet
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

#define DEFAULT_BASE_THREADS 1
#define DEFAULT_MAX_THREADS 10
#define DEFAULT_STEP_DURATION_MS 1000
#define DEFAULT_CONTENTION 100
#define DEFAULT_DUMMY_ARRAY_SIZE 1
#define DEFAULT_THREAD_STEP 1
#define DEFAULT_INCREASING_ONLY 1
#define DEFAULT_IS_LATENCY 0
#define DEFAULT_MULTI_LOCKS 1

#define XSTR(s) STR(s)
#define STR(s) #s

/* ################################################################### *
 * GLOBALS
 * ################################################################### */

int contention = DEFAULT_CONTENTION;
int dummy_array_size = DEFAULT_DUMMY_ARRAY_SIZE;
uint8_t is_latency = DEFAULT_IS_LATENCY;
int multi_locks = DEFAULT_MULTI_LOCKS;

libslock_t *the_locks;

typedef struct dummy_array_t
{
    union
    {
        struct
        {
            int counter;
            struct dummy_array_t *next;
        };
        uint8_t padding[CACHE_LINE_SIZE];
    };
} dummy_array_t;
__attribute__((aligned(CACHE_LINE_SIZE))) dummy_array_t *dummy_array;

/* ################################################################### *
 * STRESS TEST
 * ################################################################### */

typedef struct thread_data
{
    union
    {
        struct
        {
            int id;
            double cs_time;
            ticks last_measurement_at;
            unsigned long op_count;
            uint8_t reset;
            uint8_t started;
            uint8_t stop;
        };
        uint8_t padding[CACHE_LINE_SIZE];
    };
} thread_data_t;

void *test(void *data)
{
    long long unsigned int t1 = 0, t2 = 0;
    int cs_count = 0, i = 0;
    int lock_id;
    dummy_array_t *arr = &dummy_array[rand() % dummy_array_size];

    thread_data_t *d = (thread_data_t *)data;
    if (!is_latency)
        d->last_measurement_at = getticks();
    d->started = 1;

    while (!d->stop)
    {
        lock_id = rand() % multi_locks;
        if (is_latency)
            t1 = getticks();

        libslock_lock(&the_locks[lock_id]);

        for (i = 0; i < dummy_array_size; i++)
        {
            arr->counter++;
            arr = arr->next;
        }

        libslock_unlock(&the_locks[lock_id]);

        if (is_latency)
        {
            t2 = getticks();

            if (d->reset)
            {
                cs_count = 0;
                d->cs_time = 0;
                d->reset = 0;
            }
            cs_count++;

            d->cs_time = (d->cs_time * (cs_count - 1) + (t2 - t1)) / cs_count;
        }
        else
            d->op_count++;

        if (!d->stop)
            cpause(contention);
    }

    return NULL;
}

/* ################################################################### *
 * MEASUREMENT
 * ################################################################### */

void measurement(thread_data_t *data, int len)
{
    static int thread_count;
    static double tmp;
    static double sum;
    static int id = 0;
    static ticks now, last_measurement_at;

    sum = 0;
    thread_count = 0;

    // Always go through all threads as they won't stop in any particular order.
    for (int i = 0; i < len; i++)
    {
        if (data[i].stop || !data[i].started)
            continue;

        if (is_latency)
        {
            if (data[i].reset)
                continue;

            tmp = data[i].cs_time;
            data[i].reset = 1;
            sum += tmp;
        }
        else
        {
            tmp = data[i].op_count;
            now = getticks();
            data[i].op_count = 0;

            last_measurement_at = data[i].last_measurement_at;
            data[i].last_measurement_at = now;

            sum += tmp / (now - last_measurement_at);
        }
        thread_count++;
    }

    if (is_latency)
        tmp = !thread_count ? .0 : sum / thread_count / get_tsc_frequency();
    else
        tmp = sum * get_tsc_frequency();
    printf("%d, %d, %f\n", id++, thread_count, tmp);
}

/* ################################################################### *
 * SETUP
 * ################################################################### */

int main(int argc, char **argv)
{
    int i, c;

    int base_threads = DEFAULT_BASE_THREADS;
    int max_threads = DEFAULT_MAX_THREADS;
    int step_duration = DEFAULT_STEP_DURATION_MS;
    int thread_step = DEFAULT_THREAD_STEP;
    int increasing_only = DEFAULT_INCREASING_ONLY;

    struct option long_options[] = {
        {"help", no_argument, NULL, 'h'},
        {"base-threads", required_argument, NULL, 'b'},
        {"contention", required_argument, NULL, 'c'},
        {"step-duration", required_argument, NULL, 'd'},
        {"num-threads", required_argument, NULL, 'n'},
        {"cache-lines", required_argument, NULL, 't'},
        {"thread-step", required_argument, NULL, 's'},
        {"increasing-only", required_argument, NULL, 'i'},
        {"latency", required_argument, NULL, 'l'},
        {"multi-locks", required_argument, NULL, 'm'},
        {NULL, 0, NULL, 0}};

    while (1)
    {
        i = 0;
        c = getopt_long(argc, argv, "hb:c:d:n:t:s:i:l:m:", long_options, &i);

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
            printf("scheduling -- lock stress test\n");
            printf("\n");
            printf("Usage:\n");
            printf("  scheduling [options...]\n");
            printf("\n");
            printf("Options:\n");
            printf("  -h, --help\n");
            printf("        Print this message\n");
            printf("  -b, --base-threads <int>\n");
            printf("        Base number of threads (default=" XSTR(DEFAULT_BASE_THREADS) ")\n");
            printf("  -c, --contention <int>\n");
            printf("        Compute delay between critical sections, in cycles (default=" XSTR(DEFAULT_CONTENTION) ")\n");
            printf("  -d, --step-duration <int>\n");
            printf("        Duration of a step (measurement of a thread count) (default=" XSTR(DEFAULT_STEP_DURATION_MS) ")\n");
            printf("  -n, --num-threads <int>\n");
            printf("        Maximum number of threads (default=" XSTR(DEFAULT_MAX_THREADS) ")\n");
            printf("  -t, --cache-lines <int>\n");
            printf("        Number of cache lines touched in each CS (default=" XSTR(DEFAULT_DUMMY_ARRAY_SIZE) ")\n");
            printf("  -s, --thread-step <int>\n");
            printf("        A measurement will be taken every x thread step (default=" XSTR(DEFAULT_THREAD_STEP) ")\n");
            printf("  -i, --increasing-only <int>\n");
            printf("        Whether to increase then decrease or only increase thread count (default=" XSTR(DEFAULT_INCREASING_ONLY) ")\n");
            printf("  -l, --latency <int>\n");
            printf("        If true, measure cs latency else measure total throughput (default=" XSTR(DEFAULT_IS_LATENCY) ")\n");
            printf("  -m, --multi-locks <int>\n");
            printf("        How many locks to use (default=" XSTR(DEFAULT_MULTI_LOCKS) ")\n");
            exit(0);
        case 'b':
            base_threads = atoi(optarg);
            break;
        case 'c':
            contention = atoi(optarg);
            break;
        case 'd':
            step_duration = atoi(optarg);
            break;
        case 'n':
            max_threads = atoi(optarg);
            break;
        case 't':
            dummy_array_size = atoi(optarg);
            break;
        case 's':
            thread_step = atoi(optarg);
            break;
        case 'i':
            increasing_only = atoi(optarg);
            break;
        case 'l':
            is_latency = atoi(optarg);
            break;
        case 'm':
            multi_locks = atoi(optarg);
            break;
        case '?':
            printf("Use -h or --help for help\n");
            exit(0);
        default:
            exit(1);
        }
    }

    printf("Base nb threads: %d\n", base_threads);
    printf("Max nb threads: %d\n", max_threads);
    printf("Step duration: %d\n", step_duration);
    printf("Contention: %d\n", contention);
    printf("Cache lines: %d\n", dummy_array_size);
    printf("Thread step: %d\n", thread_step);
    printf("Measure: %s\n", is_latency ? "latency" : "throughput");
    printf("Multi locks: %d\n", multi_locks);
    printf("TSC frequency: %ld\n", get_tsc_frequency());

    thread_data_t *data;
    pthread_t *threads;

    if ((data = (thread_data_t *)malloc(max_threads * sizeof(thread_data_t))) == NULL)
    {
        perror("malloc data");
        exit(1);
    }
    if ((threads = (pthread_t *)malloc(max_threads * sizeof(pthread_t))) == NULL)
    {
        perror("malloc threads");
        exit(1);
    }
    if ((dummy_array = (dummy_array_t *)calloc(dummy_array_size, sizeof(dummy_array_t))) == NULL)
    {
        perror("malloc dummy_array");
        exit(1);
    }
    if ((the_locks = (libslock_t *)malloc(multi_locks * sizeof(libslock_t))) == NULL)
    {
        perror("malloc the_locks");
        exit(1);
    }

    // Fill dummy arrays
    srand(1);
    i = 0;
    dummy_array_t *arr = dummy_array;
    while (i < dummy_array_size - 1)
    {
        c = rand() % dummy_array_size;
        if (&dummy_array[c] != arr && dummy_array[c].next == NULL)
        {
            arr->counter = 0;
            arr = arr->next = &dummy_array[c];
            i++;
        }
    }
    arr->next = dummy_array;

    /* Init locks */
    DPRINT("Initializing locks\n");
    for (i = 0; i < multi_locks; i++)
        libslock_init(&the_locks[i]);

    for (i = 0; i < max_threads; i++)
    {
        data[i].id = i;
        data[i].cs_time = 0;
        data[i].op_count = 0;
        data[i].last_measurement_at = 0;
        data[i].reset = 0;
        data[i].stop = 0;
        data[i].started = 0;
    }

    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_JOINABLE);

    struct timespec step_timeout;
    step_timeout.tv_sec = step_duration / 1000;
    step_timeout.tv_nsec = (step_duration % 1000) * 1000000;

    for (i = 0; i < 2 * max_threads + 10; i++)
    {
        if (i < max_threads)
        {
            DPRINT("Creating thread %d\n", i);
            if (pthread_create(&threads[i], &attr, test, (void *)(&data[i])) != 0)
            {
                fprintf(stderr, "Error creating thread\n");
                exit(1);
            }
        }
        else if (i >= max_threads + 10)
        {
            do
            {
                c = rand() % max_threads;
            } while (data[c].stop == 1);
            DPRINT("Stopping thread %d\n", c);
            data[c].stop = 1;
        }

        if (i >= base_threads - 1 && i < 2 * max_threads + 10 - base_threads &&
            (!increasing_only || i <= max_threads) && (i - base_threads + 1) % thread_step == 0)
        {
            nanosleep(&step_timeout, NULL);
            measurement(data, max_threads);
        }
    }
    pthread_attr_destroy(&attr);

    DPRINT("Joining threads\n");
    /* Wait for thread completion */
    for (i = 0; i < max_threads; i++)
    {
        if (pthread_join(threads[i], NULL) != 0)
        {
            fprintf(stderr, "Error waiting for thread completion\n");
            exit(1);
        }
    }

    for (i = 0; i < multi_locks; i++)
        libslock_destroy(&the_locks[i]);
    return EXIT_SUCCESS;
}
