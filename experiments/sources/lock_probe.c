#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t inner = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t condition = PTHREAD_COND_INITIALIZER;
static unsigned long count, inverse = ~0UL;
static int ready, start;
static void *worker(void *unused) {
    (void)unused;
    pthread_mutex_lock(&mutex);
    ++ready;
    pthread_cond_broadcast(&condition);
    while (!start) pthread_cond_wait(&condition, &mutex);
    pthread_mutex_unlock(&mutex);
    for (unsigned i = 0; i < 10000; ++i) {
        pthread_mutex_lock(&mutex);
        pthread_mutex_lock(&inner);
        if (inverse != ~count) abort();
        inverse = ~++count;
        pthread_mutex_unlock(&inner);
        pthread_mutex_unlock(&mutex);
    }
    return NULL;
}
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    Dl_info info;
    if (!dladdr(dlsym(RTLD_DEFAULT, "pthread_mutex_lock"), &info)) return 3;
    printf("mutex_binding=%s\n", info.dli_fname);
    fflush(stdout);
    if (strcmp(info.dli_fname, argv[1])) return 4;
    usleep(100000);  // Let the external preflight observer inspect the loaded runtime.
    pthread_t threads[8];
    for (int i = 0; i < 8; ++i)
        if (pthread_create(&threads[i], NULL, worker, NULL)) return 5;
    pthread_mutex_lock(&mutex);
    while (ready != 8) pthread_cond_wait(&condition, &mutex);
    start = 1;
    pthread_cond_broadcast(&condition);
    pthread_mutex_unlock(&mutex);
    for (int i = 0; i < 8; ++i) pthread_join(threads[i], NULL);
    printf("counter=%lu expected=80000\n", count);
    return count == 80000 && inverse == ~count ? 0 : 6;
}
