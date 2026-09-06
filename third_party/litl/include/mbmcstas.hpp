#pragma once

/*
 * MCS lock with a test-and-set fast path, from the mutex microbenchmark.
 *
 * The queue node lives in a per (thread, lock) context supplied by the caller
 * instead of a function-local thread_local, so a thread may hold several of
 * these locks at once. lock() returns no state, so the context holds only the
 * node.
 */

#include <atomic>
#if defined(__x86_64__) || defined(__i386__)
#include <immintrin.h>
#else
#include <thread>
#endif

struct MbMcsTasLock {
  struct alignas(64) Node {
    std::atomic<Node *> next{nullptr};
    std::atomic<bool> waiting{false};
  };

  struct Context {
    Node node;
  };

  inline void lock(Context &context) {
    // Fast path: single TAS probe.
    if (!locked_.exchange(true, std::memory_order_acquire)) {
      return;
    }

    // Slow path: MCS queue to serialize contenders.
    Node &my_node = context.node;
    my_node.next.store(nullptr, std::memory_order_relaxed);
    my_node.waiting.store(false, std::memory_order_relaxed);

    Node *pred = tail_.exchange(&my_node, std::memory_order_acq_rel);
    if (pred != nullptr) {
      my_node.waiting.store(true, std::memory_order_relaxed);
      pred->next.store(&my_node, std::memory_order_release);
      while (my_node.waiting.load(std::memory_order_acquire)) {
        Pause();
      }
    }

    while (locked_.exchange(true, std::memory_order_acquire)) {
      Pause();
    }

    // Wake the next queued waiter, if any, so only one queued thread at a
    // time spins on TAS.
    Node *succ = my_node.next.load(std::memory_order_acquire);
    if (succ == nullptr) {
      Node *expected = &my_node;
      if (!tail_.compare_exchange_strong(expected, nullptr,
                                         std::memory_order_acq_rel,
                                         std::memory_order_acquire)) {
        while ((succ = my_node.next.load(std::memory_order_acquire)) ==
               nullptr) {
          Pause();
        }
      }
    }
    if (succ != nullptr) {
      succ->waiting.store(false, std::memory_order_release);
    }
  }

  inline void unlock(Context &) {
    locked_.store(false, std::memory_order_release);
  }

private:
  static inline void Pause() {
#if defined(__x86_64__) || defined(__i386__)
    _mm_pause();
#elif defined(__aarch64__) || defined(__arm__)
    asm volatile("yield" ::: "memory");
#else
    std::this_thread::yield();
#endif
  }

  alignas(64) std::atomic<Node *> tail_{nullptr};
  alignas(64) std::atomic<bool> locked_{false};
};
