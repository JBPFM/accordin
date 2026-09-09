#pragma once

/*
 * MCS lock with a test-and-set fast path, an explicit next-in-line hand-off and
 * an rseq time-slice extension held across the critical section, from the mutex
 * microbenchmark.
 *
 * The queue node and the state returned by lock() live in a per (thread, lock)
 * context supplied by the caller instead of a function-local thread_local, so
 * a thread may hold several of these locks at once. The slice extension itself
 * is per-thread kernel state and stays shared by every lock in the process.
 */

#include <atomic>
#if defined(__x86_64__) || defined(__i386__)
#include <immintrin.h>
#else
#include <thread>
#endif

#include "mbtimeslice.hpp"

struct MbMcsTasNextTseLock {
  struct alignas(64) Node {
    std::atomic<Node *> next{nullptr};
    std::atomic<bool> waiting{false};
    std::atomic<bool> is_next{false};
  };

  struct LockState {
    Node *node;
    bool timeslice_requested;
  };

  struct Context {
    Node node;
    LockState state;
  };

  inline void lock(Context &context) {
    // Fast path: single TAS probe.
    if (!locked_.exchange(true, std::memory_order_acquire)) {
      SliceExtension().on_critical_section_enter();
      context.state = LockState{nullptr, true};
      return;
    }

    // Slow path: MCS queue to serialize contenders.
    Node &my_node = context.node;
    my_node.next.store(nullptr, std::memory_order_relaxed);
    my_node.waiting.store(false, std::memory_order_relaxed);
    my_node.is_next.store(false, std::memory_order_relaxed);

    Node *pred = tail_.exchange(&my_node, std::memory_order_acq_rel);
    bool timeslice_requested = false;
    if (pred != nullptr) {
      my_node.waiting.store(true, std::memory_order_relaxed);
      pred->next.store(&my_node, std::memory_order_release);
      while (!my_node.is_next.load(std::memory_order_acquire)) {
        Pause();
      }

      // Request a slice extension once this thread becomes the designated
      // waiter that is next in line to inherit the lock.
      timeslice_requested = RequestTimesliceExtension();
      while (my_node.waiting.load(std::memory_order_acquire)) {
        Pause();
      }
    } else {
      timeslice_requested = RequestTimesliceExtension();
      while (locked_.exchange(true, std::memory_order_acquire)) {
        Pause();
      }
    }

    SignalSuccessorIfPresent(my_node);
    context.state = LockState{&my_node, timeslice_requested};
  }

  inline void unlock(Context &context) {
    LockState &state = context.state;
    Node *node = state.node;
    if (node == nullptr) {
      locked_.store(false, std::memory_order_release);
      FinishTimesliceExtension(state);
      return;
    }

    Node *succ = node->next.load(std::memory_order_acquire);
    if (succ == nullptr) {
      Node *expected = node;
      if (tail_.compare_exchange_strong(expected, nullptr,
                                        std::memory_order_acq_rel,
                                        std::memory_order_acquire)) {
        locked_.store(false, std::memory_order_release);
        FinishTimesliceExtension(state);
        state.node = nullptr;
        return;
      }
      while ((succ = node->next.load(std::memory_order_acquire)) == nullptr) {
        Pause();
      }
    }

    succ->is_next.store(true, std::memory_order_release);
    succ->waiting.store(false, std::memory_order_release);
    FinishTimesliceExtension(state);
    state.node = nullptr;
  }

private:
  [[nodiscard]] inline bool RequestTimesliceExtension() const {
    SliceExtension().on_critical_section_enter();
    return true;
  }

  inline void FinishTimesliceExtension(LockState &state) const {
    if (!state.timeslice_requested) {
      return;
    }
    SliceExtension().on_critical_section_exit();
    state.timeslice_requested = false;
  }

  [[nodiscard]] static inline const mblocks::CriticalSectionTimesliceExtension &
  SliceExtension() {
    static constexpr mblocks::CriticalSectionTimesliceExtension extension{};
    return extension;
  }

  inline void SignalSuccessorIfPresent(Node &node) {
    Node *succ = node.next.load(std::memory_order_acquire);
    if (succ == nullptr && tail_.load(std::memory_order_acquire) != &node) {
      while ((succ = node.next.load(std::memory_order_acquire)) == nullptr) {
        Pause();
      }
    }
    if (succ != nullptr) {
      succ->is_next.store(true, std::memory_order_release);
    }
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

  alignas(64) std::atomic<Node *> tail_{nullptr};
  alignas(64) std::atomic<bool> locked_{false};
};
