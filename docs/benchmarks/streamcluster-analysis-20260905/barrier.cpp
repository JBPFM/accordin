#include <cstdio>
#include <cstdlib>
#include <vector>
#include <time.h>
#include "parsec_barrier.hpp"
static parsec_barrier_t barrier;
static int iterations;
static void *worker(void *) {
    for (int i=0; i<iterations; ++i) {
        int r=parsec_barrier_wait(&barrier);
        if (r!=0 && r!=PARSEC_BARRIER_SERIAL_THREAD) abort();
    }
    return nullptr;
}
static double now() { struct timespec t;clock_gettime(CLOCK_MONOTONIC,&t);return t.tv_sec+t.tv_nsec*1e-9; }
int main(int argc,char **argv) {
    if (argc!=3) return 2;
    int n=atoi(argv[1]);iterations=atoi(argv[2]);
    if (parsec_barrier_init(&barrier,nullptr,n)) return 3;
    std::vector<pthread_t> threads(n);
    double start=now();
    for (auto &t:threads) if(pthread_create(&t,nullptr,worker,nullptr)) abort();
    for (auto t:threads) pthread_join(t,nullptr);
    printf("BARRIER threads=%d rounds=%d seconds=%.9f\n",n,iterations,now()-start);
    return parsec_barrier_destroy(&barrier);
}
