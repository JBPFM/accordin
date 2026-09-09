#pragma once

/*
 * CLH lock from the mutex microbenchmark.
 *
 * A thread hands its node to its successor and adopts the node of its
 * predecessor, so nodes are heap allocated and never freed. The node pointer
 * and the state returned by lock() live in a per (thread, lock) context
 * supplied by the caller instead of a function-local thread_local, so a thread
 * may hold several of these locks at once.
 */

#include <atomic>
#include <cassert>
#include <thread>

#if defined(__x86_64__) || defined(__i386__)
#include <immintrin.h>
#endif

struct MbClhLock {
  struct alignas(64) Node {
    std::atomic<bool> locked{false};
  };

  struct LockState {
    Node *pred;
  };

  struct Context {
    Node *node;
    LockState state;
  };

  MbClhLock() : tail_(&sentinel_) {
    sentinel_.locked.store(false, std::memory_order_relaxed);
  }

  inline void lock(Context &context) {
    Node *&my_node = ThreadNodeRef(context);
    my_node->locked.store(true, std::memory_order_relaxed);

    Node *pred = tail_.exchange(my_node, std::memory_order_acq_rel);
    assert(pred != nullptr);
    while (pred->locked.load(std::memory_order_acquire)) {
      Pause();
    }

    context.state = LockState{pred};
  }

  inline void unlock(Context &context) {
    Node *&my_node = ThreadNodeRef(context);
    assert(my_node != nullptr);
    assert(context.state.pred != nullptr);

    my_node->locked.store(false, std::memory_order_release);
    my_node = context.state.pred;
  }

private:
  [[nodiscard]] static inline Node *&ThreadNodeRef(Context &context) {
    if (context.node == nullptr) {
      Node *node = new Node{};
      node->locked.store(false, std::memory_order_relaxed);
      context.node = node;
    }
    return context.node;
  }

  static inline void Pause() {
#if defined(__x86_64__) || defined(__i386__)
    _mm_pause();
#elif defined(__aarch64__) || defined(__arm__)
    asm volatile("yield" ::: "memory");
#else
    std::this_thread::yield();
#endif
  }

  alignas(64) Node sentinel_{};
  std::atomic<Node *> tail_;
};
