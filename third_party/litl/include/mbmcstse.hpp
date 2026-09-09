#pragma once

/*
 * MCS lock with an rseq time-slice extension held across the critical section.
 *
 * lock() requests the extension once the queue has handed the lock over, and
 * unlock() returns it, so the request covers the span in which a thread owns
 * the lock. Every acquire requests and every release returns; there is no
 * nesting counter, so a thread holding two of these locks returns the
 * extension when it releases the inner one.
 *
 * The queue node and the state returned by lock() live in a per (thread, lock)
 * context supplied by the caller, so a thread may hold several of these locks
 * at once. The slice extension itself is per-thread kernel state and stays
 * shared by every lock in the process.
 */

#include <atomic>
#include <thread>
#if defined(__x86_64__) || defined(__i386__)
#include <immintrin.h>
#endif

#include "mbtimeslice.hpp"

struct MbMcsTseLock {
  struct alignas(64) Node {
    std::atomic<Node *> next{nullptr};
    std::atomic<bool> locked{false};
  };

  struct LockState {
    Node *node;
  };

  struct Context {
    Node node;
    LockState state;
  };

  std::atomic<Node *> tail{nullptr};

  inline void lock(Context &context) {
    Node &my_node = context.node;
    my_node.next.store(nullptr, std::memory_order_relaxed);
    my_node.locked.store(true, std::memory_order_relaxed);

    Node *prev = tail.exchange(&my_node, std::memory_order_acq_rel);
    if (prev != nullptr) {
      prev->next.store(&my_node, std::memory_order_release);
      while (my_node.locked.load(std::memory_order_acquire)) {
        Pause();
      }
    }
    SliceExtension().on_critical_section_enter();
    context.state = LockState{&my_node};
  }

  inline void unlock(Context &context) {
    Node *node = context.state.node;
    Node *succ = node->next.load(std::memory_order_acquire);
    if (succ == nullptr) {
      Node *expected = node;
      if (tail.compare_exchange_strong(expected, nullptr,
                                       std::memory_order_acq_rel,
                                       std::memory_order_acquire)) {
        SliceExtension().on_critical_section_exit();
        return;
      }
      // A new waiter linked in; wait for it to set our next pointer.
      // Spin tightly: this window is very short and we want
      // to hand off the lock as quickly as possible.
      while ((succ = node->next.load(std::memory_order_acquire)) == nullptr) {
        Pause();
      }
    }
    succ->locked.store(false, std::memory_order_release);
    SliceExtension().on_critical_section_exit();
  }

private:
  [[nodiscard]] static inline const mblocks::CriticalSectionTimesliceExtension &
  SliceExtension() {
    static constexpr mblocks::CriticalSectionTimesliceExtension extension{};
    return extension;
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
};
