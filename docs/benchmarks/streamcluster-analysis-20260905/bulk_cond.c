/* Diagnostic only: FlexGuard-style condvar for the barrier's wait/broadcast
 * subset. No cancellation, timeout, signal, or general POSIX conformance claim.
 * The existing mutex backend remains unchanged. */
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <string.h>
#include <limits.h>
#include <errno.h>
#include <linux/futex.h>
#include <sys/syscall.h>
#include <unistd.h>
struct state { uint32_t seq,target; };
int pthread_cond_init(pthread_cond_t *c,const pthread_condattr_t *attr) {
    if(attr) return ENOTSUP;
    memset(c,0,sizeof(*c));return 0;
}
int pthread_cond_destroy(pthread_cond_t *c) { (void)c;return 0; }
int pthread_cond_wait(pthread_cond_t *c,pthread_mutex_t *m) {
    struct state *s=(void*)c;
    uint32_t target=__atomic_add_fetch(&s->target,1,__ATOMIC_RELAXED);
    uint32_t seq=__atomic_load_n(&s->seq,__ATOMIC_ACQUIRE);
    int ret=pthread_mutex_unlock(m);if(ret)return ret;
    while(target>seq) {
        syscall(SYS_futex,&s->seq,FUTEX_WAIT_PRIVATE,seq,NULL,NULL,0);
        seq=__atomic_load_n(&s->seq,__ATOMIC_ACQUIRE);
    }
    return pthread_mutex_lock(m);
}
int pthread_cond_broadcast(pthread_cond_t *c) {
    struct state *s=(void*)c;
    __atomic_store_n(&s->seq,__atomic_load_n(&s->target,__ATOMIC_RELAXED),__ATOMIC_RELEASE);
    syscall(SYS_futex,&s->seq,FUTEX_WAKE_PRIVATE,INT_MAX,NULL,NULL,0);return 0;
}
