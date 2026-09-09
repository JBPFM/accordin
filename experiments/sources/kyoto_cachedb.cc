// Fixed-work, random-hit benchmark of Kyoto Cabinet's actual in-memory CacheDB.
#include <kccachedb.h>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <thread>
#include <vector>
#include <atomic>
#include <algorithm>

using Clock = std::chrono::steady_clock;
static uint64_t next_random(uint64_t &x) {
    x ^= x << 13; x ^= x >> 7; x ^= x << 17; return x;
}
int main(int argc, char **argv) {
    if (argc != 6) {
        std::fprintf(stderr, "usage: kyoto_cachedb THREADS KEYS TOTAL_OPS VALUE_BYTES WRITE_PERCENT\n");
        return 2;
    }
    int threads = std::stoi(argv[1]), keys = std::stoi(argv[2]);
    uint64_t ops = std::stoull(argv[3]);
    int bytes = std::stoi(argv[4]), writes = std::stoi(argv[5]);
    if (threads < 1 || keys < 1 || ops < unsigned(threads) || bytes < 1 || writes < 0 || writes > 100)
        return 2;
    kyotocabinet::CacheDB db;
    db.tune_buckets(keys * 2);
    if (!db.open("*", kyotocabinet::CacheDB::OWRITER | kyotocabinet::CacheDB::OCREATE))
        return 3;
    std::vector<std::string> keyset;
    std::string value(bytes, 'v');
    for (int i = 0; i < keys; ++i) {
        char key[32]; std::snprintf(key, sizeof key, "%016d", i);
        keyset.emplace_back(key);
        if (!db.set(keyset.back(), value)) return 4;
    }
    // Native barrier leaves the measured mutex replacement identical for all locks.
    pthread_barrier_t gate;
    if (pthread_barrier_init(&gate, nullptr, threads + 1)) return 5;
    std::atomic<uint64_t> errors{0};
    std::vector<std::thread> workers;
    std::vector<Clock::time_point> finish(threads);
    for (int tid = 0; tid < threads; ++tid) workers.emplace_back([&, tid] {
        uint64_t rng = 0x9e3779b97f4a7c15ULL + tid;
        std::string result;
        // Warm every worker's TLS and a bounded part of the resident data before ROI.
        for (int j = 0; j < 256; ++j)
            if (!db.get(keyset[next_random(rng) % keys], &result) || result != value) ++errors;
        pthread_barrier_wait(&gate);
        pthread_barrier_wait(&gate);
        auto count = ops / threads + (uint64_t(tid) < ops % threads);
        uint64_t failures = 0;
        for (uint64_t i = 0; i < count; ++i) {
            const auto &key = keyset[next_random(rng) % keys];
            if (next_random(rng) % 100 < unsigned(writes)) {
                if (!db.set(key, value)) ++failures;
            } else if (!db.get(key, &result) || result != value) ++failures;
        }
        finish[tid] = Clock::now();
        errors.fetch_add(failures, std::memory_order_relaxed);
    });
    pthread_barrier_wait(&gate);
    auto start = Clock::now();
    pthread_barrier_wait(&gate);
    for (auto &worker : workers) worker.join();
    auto end = *std::max_element(finish.begin(), finish.end());
    double seconds = std::chrono::duration<double>(end - start).count();
    // Check the final database, outside the measured region.
    for (const auto &key : keyset) {
        std::string actual;
        if (!db.get(key, &actual) || actual != value) ++errors;
    }
    auto failures = errors.load();
    std::printf("EXP_RESULT name=cachedb ops=%llu seconds=%.9f errors=%llu records=%lld\n",
                (unsigned long long)ops, seconds, (unsigned long long)failures,
                (long long)db.count());
    pthread_barrier_destroy(&gate);
    return failures || db.count() != keys || !db.close() ? 6 : 0;
}
