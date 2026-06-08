#include <algorithm>
#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <iomanip>
#include <iostream>
#include <string>
#include <thread>
#include <vector>

#include <sys/syscall.h>
#include <unistd.h>

#if defined(__x86_64__) || defined(__i386__)
#include <immintrin.h>
#endif

using Clock = std::chrono::steady_clock;

extern "C" __attribute__((noinline, used)) void
trace_mcs_worker_start(uint64_t worker_id) {
  asm volatile("" : : "r"(worker_id) : "memory");
}

extern "C" __attribute__((noinline, used)) void
trace_mcs_worker_end(uint64_t worker_id) {
  asm volatile("" : : "r"(worker_id) : "memory");
}

extern "C" __attribute__((noinline, used)) void
trace_mcs_wait_begin(uint64_t lock_id, uint64_t worker_id,
                     uint64_t queue_seq) {
  asm volatile("" : : "r"(lock_id), "r"(worker_id), "r"(queue_seq) : "memory");
}

extern "C" __attribute__((noinline, used)) void
trace_mcs_wait_end(uint64_t lock_id, uint64_t worker_id, uint64_t queue_seq) {
  asm volatile("" : : "r"(lock_id), "r"(worker_id), "r"(queue_seq) : "memory");
}

extern "C" __attribute__((noinline, used)) void
trace_mcs_release(uint64_t lock_id, uint64_t sample_id,
                  uint64_t successor_tid, uint64_t successor_seq) {
  asm volatile("" : : "r"(lock_id), "r"(sample_id), "r"(successor_tid),
               "r"(successor_seq)
               : "memory");
}

extern "C" __attribute__((noinline, used)) void
trace_mcs_acquire(uint64_t lock_id, uint64_t worker_id, uint64_t queue_seq,
                  uint64_t sample_id) {
  asm volatile("" : : "r"(lock_id), "r"(worker_id), "r"(queue_seq),
               "r"(sample_id)
               : "memory");
}

namespace {

uint64_t GetTid() {
  return static_cast<uint64_t>(syscall(SYS_gettid));
}

void SpinPause() {
#if defined(__x86_64__) || defined(__i386__)
  _mm_pause();
#else
  std::this_thread::yield();
#endif
}

void BurnIters(uint64_t requested_ns) {
  const uint64_t raw_iters = std::max<uint64_t>(1, (requested_ns * 9 + 16) / 32);
  volatile uint64_t x = 0;
  for (uint64_t i = 0; i < raw_iters; ++i) {
    x = (x * 1664525u) + 1013904223u + i;
  }
}

struct Config {
  int threads = 40;
  uint64_t duration_ms = 1000;
  uint64_t warmup_ms = 250;
  uint64_t startup_delay_ms = 2000;
  uint64_t critical_ns = 300;
  uint64_t outside_ns = 3000;
  uint64_t trace_stride = 256;
};

uint64_t ParseU64(const char *value, const char *flag) {
  char *end = nullptr;
  const unsigned long long parsed = std::strtoull(value, &end, 10);
  if (end == value || *end != '\0') {
    std::cerr << "invalid " << flag << ": " << value << "\n";
    std::exit(2);
  }
  return static_cast<uint64_t>(parsed);
}

void Usage(const char *argv0) {
  std::cerr << "Usage: " << argv0
            << " [--threads N] [--duration-ms N] [--warmup-ms N]"
            << " [--startup-delay-ms N] [--critical-ns N] [--outside-ns N]"
            << " [--trace-stride N]\n";
}

Config ParseArgs(int argc, char **argv) {
  Config cfg;
  for (int i = 1; i < argc; ++i) {
    auto need = [&](const char *flag) -> const char * {
      if (i + 1 >= argc) {
        Usage(argv[0]);
        std::exit(2);
      }
      return argv[++i];
    };
    const std::string arg = argv[i];
    if (arg == "--threads") {
      cfg.threads = static_cast<int>(ParseU64(need("--threads"), "--threads"));
    } else if (arg == "--duration-ms") {
      cfg.duration_ms = ParseU64(need("--duration-ms"), "--duration-ms");
    } else if (arg == "--warmup-ms") {
      cfg.warmup_ms = ParseU64(need("--warmup-ms"), "--warmup-ms");
    } else if (arg == "--startup-delay-ms") {
      cfg.startup_delay_ms =
          ParseU64(need("--startup-delay-ms"), "--startup-delay-ms");
    } else if (arg == "--critical-ns") {
      cfg.critical_ns = ParseU64(need("--critical-ns"), "--critical-ns");
    } else if (arg == "--outside-ns") {
      cfg.outside_ns = ParseU64(need("--outside-ns"), "--outside-ns");
    } else if (arg == "--trace-stride") {
      cfg.trace_stride = ParseU64(need("--trace-stride"), "--trace-stride");
    } else if (arg == "--help" || arg == "-h") {
      Usage(argv[0]);
      std::exit(0);
    } else {
      std::cerr << "unknown argument: " << arg << "\n";
      Usage(argv[0]);
      std::exit(2);
    }
  }
  if (cfg.threads <= 0 || cfg.duration_ms == 0 || cfg.trace_stride == 0) {
    Usage(argv[0]);
    std::exit(2);
  }
  return cfg;
}

class TraceMcsLock {
public:
  struct alignas(64) Node {
    std::atomic<Node *> next{nullptr};
    std::atomic<bool> locked{false};
    uint64_t tid = 0;
    uint64_t worker_id = 0;
    uint64_t queue_seq = 0;
    uint64_t sample_id = 0;
  };

  struct LockState {
    Node *node = nullptr;
  };

  explicit TraceMcsLock(uint64_t trace_stride) : trace_stride_(trace_stride) {}

  LockState lock(uint64_t worker_id) {
    static thread_local Node my_node;
    my_node.next.store(nullptr, std::memory_order_relaxed);
    my_node.locked.store(true, std::memory_order_relaxed);
    my_node.tid = GetTid();
    my_node.worker_id = worker_id;
    my_node.queue_seq = queue_seq_.fetch_add(1, std::memory_order_relaxed) + 1;
    my_node.sample_id = 0;

    Node *prev = tail_.exchange(&my_node, std::memory_order_acq_rel);
    if (prev != nullptr) {
      trace_mcs_wait_begin(kLockId, worker_id, my_node.queue_seq);
      prev->next.store(&my_node, std::memory_order_release);
      while (my_node.locked.load(std::memory_order_acquire)) {
        SpinPause();
      }
      trace_mcs_wait_end(kLockId, worker_id, my_node.queue_seq);
      if (my_node.sample_id != 0) {
        trace_mcs_acquire(kLockId, worker_id, my_node.queue_seq,
                          my_node.sample_id);
      }
    }
    return {&my_node};
  }

  void unlock(LockState state) {
    Node *node = state.node;
    Node *succ = node->next.load(std::memory_order_acquire);
    if (succ == nullptr) {
      Node *expected = node;
      if (tail_.compare_exchange_strong(expected, nullptr,
                                        std::memory_order_acq_rel,
                                        std::memory_order_acquire)) {
        return;
      }
      while ((succ = node->next.load(std::memory_order_acquire)) == nullptr) {
        SpinPause();
      }
    }

    const uint64_t handoff_id =
        handoff_seq_.fetch_add(1, std::memory_order_relaxed) + 1;
    const bool sampled = (handoff_id % trace_stride_) == 0;
    succ->sample_id = sampled ? handoff_id : 0;
    if (sampled) {
      trace_mcs_release(kLockId, handoff_id, succ->tid, succ->queue_seq);
    }
    succ->locked.store(false, std::memory_order_release);
  }

private:
  static constexpr uint64_t kLockId = 1;
  const uint64_t trace_stride_;
  std::atomic<Node *> tail_{nullptr};
  std::atomic<uint64_t> queue_seq_{0};
  std::atomic<uint64_t> handoff_seq_{0};
};

int Run(const Config &cfg) {
  TraceMcsLock lock(cfg.trace_stride);
  std::atomic<int> ready{0};
  std::atomic<bool> warmup_start{false};
  std::atomic<bool> warmup_stop{false};
  std::atomic<bool> measure_start{false};
  std::atomic<bool> measure_stop{false};
  std::atomic<uint64_t> total_ops{0};
  std::vector<uint64_t> per_thread_ops(static_cast<size_t>(cfg.threads), 0);
  std::vector<std::thread> workers;
  workers.reserve(static_cast<size_t>(cfg.threads));

  for (int i = 0; i < cfg.threads; ++i) {
    workers.emplace_back([&, worker_id = static_cast<uint64_t>(i)] {
      trace_mcs_worker_start(worker_id);
      ready.fetch_add(1, std::memory_order_release);
      while (!warmup_start.load(std::memory_order_acquire)) {
        SpinPause();
      }
      while (!warmup_stop.load(std::memory_order_acquire)) {
        auto state = lock.lock(worker_id);
        BurnIters(cfg.critical_ns);
        lock.unlock(state);
        BurnIters(cfg.outside_ns);
      }
      while (!measure_start.load(std::memory_order_acquire)) {
        SpinPause();
      }

      uint64_t local_ops = 0;
      while (!measure_stop.load(std::memory_order_acquire)) {
        auto state = lock.lock(worker_id);
        BurnIters(cfg.critical_ns);
        lock.unlock(state);
        BurnIters(cfg.outside_ns);
        ++local_ops;
      }
      per_thread_ops[static_cast<size_t>(worker_id)] = local_ops;
      total_ops.fetch_add(local_ops, std::memory_order_relaxed);
      trace_mcs_worker_end(worker_id);
    });
  }

  while (ready.load(std::memory_order_acquire) < cfg.threads) {
    std::this_thread::sleep_for(std::chrono::milliseconds(1));
  }
  if (cfg.startup_delay_ms > 0) {
    std::this_thread::sleep_for(std::chrono::milliseconds(cfg.startup_delay_ms));
  }

  warmup_start.store(true, std::memory_order_release);
  if (cfg.warmup_ms > 0) {
    std::this_thread::sleep_for(std::chrono::milliseconds(cfg.warmup_ms));
  }
  warmup_stop.store(true, std::memory_order_release);

  const auto start = Clock::now();
  measure_start.store(true, std::memory_order_release);
  std::this_thread::sleep_for(std::chrono::milliseconds(cfg.duration_ms));
  measure_stop.store(true, std::memory_order_release);

  for (auto &worker : workers) {
    worker.join();
  }
  const auto end = Clock::now();
  const double elapsed_s =
      std::chrono::duration_cast<std::chrono::duration<double>>(end - start)
          .count();
  const uint64_t ops = total_ops.load(std::memory_order_relaxed);

  std::cout << "threads: " << cfg.threads << "\n";
  std::cout << "critical_ns: " << cfg.critical_ns << "\n";
  std::cout << "outside_ns: " << cfg.outside_ns << "\n";
  std::cout << "trace_stride: " << cfg.trace_stride << "\n";
  std::cout << "elapsed_seconds: " << std::fixed << std::setprecision(6)
            << elapsed_s << "\n";
  std::cout << "total_operations: " << ops << "\n";
  std::cout << "throughput_ops_per_sec: " << std::fixed << std::setprecision(2)
            << (elapsed_s > 0.0 ? static_cast<double>(ops) / elapsed_s : 0.0)
            << "\n";
  std::cout << "per_thread_operations: ";
  for (size_t i = 0; i < per_thread_ops.size(); ++i) {
    if (i != 0) {
      std::cout << ",";
    }
    std::cout << per_thread_ops[i];
  }
  std::cout << "\n";
  return 0;
}

} // namespace

int main(int argc, char **argv) {
  const Config cfg = ParseArgs(argc, argv);
  return Run(cfg);
}
