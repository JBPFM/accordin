/* SPDX-License-Identifier: MIT */
/* Include the mutex adapter to reject native shadow mutex calls, including
 * after an actual condvar wait. Relock parking metadata is permitted. */
#define _GNU_SOURCE
#include <stdio.h>
#include "../src/accordin.c"

static int unexpected_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr) {
    abort();
}
static int unexpected_mutex(pthread_mutex_t *mutex) {
    abort();
}
__typeof__(&pthread_mutex_init) REAL(pthread_mutex_init) = unexpected_init;
__typeof__(&pthread_mutex_destroy) REAL(pthread_mutex_destroy) = unexpected_mutex;
__typeof__(&pthread_mutex_lock) REAL(pthread_mutex_lock) = unexpected_mutex;
__typeof__(&pthread_mutex_trylock) REAL(pthread_mutex_trylock) = unexpected_mutex;
__typeof__(&pthread_mutex_unlock) REAL(pthread_mutex_unlock) = unexpected_mutex;

int main(void) {
    pthread_mutex_t mutex;
    pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
    require_success(accordin_mutex_init(&mutex, NULL));
    struct accordin_mutex *impl = get_mutex(&mutex);
    for (int i = 0; i < 2; i++) {
        require_success(i ? accordin_mutex_trylock(&mutex, NULL)
                          : accordin_mutex_lock(&mutex, NULL));
        if (ACCORDIN_DIRECT(trylock)(impl->direct) != EBUSY)
            abort();
        struct timespec expired = {0};
        if (accordin_cond_timedwait(&cond, &mutex, NULL, &expired) != ETIMEDOUT)
            abort();
        if (ACCORDIN_DIRECT(trylock)(impl->direct) != EBUSY)
            abort();
        accordin_mutex_unlock(&mutex, NULL);
    }
    require_success(accordin_cond_destroy(&cond));
    require_success(accordin_mutex_destroy(&mutex));
    puts("PASS no native shadow mutex calls, including after condvar wait (NDEBUG)");
    return 0;
}
