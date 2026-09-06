#pragma once

/*
 * Reciprocating lock from the mutex microbenchmark, following Listing 2 of the
 * paper.
 *
 * The wait element lives in a per (thread, lock) context supplied by the caller
 * instead of a function-local thread_local, so a thread may hold several of
 * these locks at once. The successor pointer stays a member of the lock: it is
 * touched only by the current owner, so it is per-lock state and not
 * per-thread state.
 */

#include <atomic>
#include <cassert>
#include <cstddef>
#include <cstdint>
#include <thread>

#if defined(__x86_64__) || defined(__i386__)
#include <immintrin.h>
#endif

struct MbReciprocatingLock {
  // Listing 2: Element { atomic<int> Gate }
  struct alignas(128) WaitElement {
    std::atomic<int> Gate{0};
    // Sequester/pad to 128B to reduce false sharing (as done in the paper's
    // eval).
    std::byte _pad[128 - sizeof(std::atomic<int>)];
  };
  static_assert(alignof(WaitElement) == 128);
  static_assert(sizeof(WaitElement) == 128);

  struct Context {
    WaitElement element;
  };

  // Listing 2: LOCKEDEMPTY = (Element*)uintptr_t(1)
  static inline WaitElement *const LOCKEDEMPTY =
      reinterpret_cast<WaitElement *>(static_cast<std::uintptr_t>(1));

  // Listing 2: Lock { atomic<Element*> Arv; atomic<Element*> Terminus; Element*
  // Succ; }
  std::atomic<WaitElement *> Arv{nullptr};
  std::atomic<WaitElement *> Terminus{nullptr};
  WaitElement *Succ{nullptr}; // only touched by the current owner

  static inline void Pause() noexcept {
#if defined(__x86_64__) || defined(__i386__)
    _mm_pause();
#elif defined(__aarch64__) || defined(__arm__)
    asm volatile("yield" ::: "memory");
#else
    // Fallback (not ideal, but keeps portability).
    std::this_thread::yield();
#endif
  }

  // cas() helper that returns the previous value (as assumed by the paper).
  static inline WaitElement *CasPrev(std::atomic<WaitElement *> &a,
                                     WaitElement *expected,
                                     WaitElement *desired) noexcept {
    a.compare_exchange_strong(expected, desired, std::memory_order_seq_cst,
                              std::memory_order_seq_cst);
    // On success: expected is the old value; on failure: expected is the
    // current/old value.
    return expected;
  }

  static inline WaitElement *UntagLowBit(WaitElement *p) noexcept {
    return reinterpret_cast<WaitElement *>(reinterpret_cast<std::uintptr_t>(p) &
                                           ~static_cast<std::uintptr_t>(1));
  }

  // Listing 2: Acquire(L)
  inline void lock(Context &context) {
    WaitElement &E = context.element;
    E.Gate.store(0, std::memory_order_relaxed);

    // returns previous tail
    WaitElement *tail = Arv.exchange(&E, std::memory_order_seq_cst);
    assert(tail != &E);

    if (tail == nullptr) {
      // Uncontended acquire -- fast-path return
      Succ = nullptr;
      Terminus.store(&E, std::memory_order_seq_cst);
      return;
    }

    // Contention -- slow-path -- need to wait
    WaitElement *succ = UntagLowBit(tail);
    while (E.Gate.load(std::memory_order_acquire) == 0) {
      Pause();
    }

    assert(Arv.load(std::memory_order_seq_cst) != nullptr);

    WaitElement *eos = Terminus.load(std::memory_order_seq_cst);
    assert(eos != nullptr && eos != &E);

    if (tail == eos) {
      // Detected logical end-of-segment : annul succ
      Terminus.store(LOCKEDEMPTY, std::memory_order_seq_cst);
      succ = nullptr;
    }

    Succ = succ;
  }

  // Listing 2: Release(L)
  inline void unlock() {
    assert(Arv.load(std::memory_order_seq_cst) != nullptr);

    // case : normal succession within entry segment
    WaitElement *succ = Succ;
    if (succ != nullptr) {
      succ->Gate.store(1, std::memory_order_release);
      return;
    }

    // case : try uncontended fast-path
    WaitElement *eos = Terminus.load(std::memory_order_seq_cst);
    assert(eos != nullptr);

    WaitElement *v = CasPrev(Arv, eos, nullptr);
    if (v == eos) {
      return;
    }

    // case : detach arrival segment which becomes next entry segment
    WaitElement *w = Arv.exchange(LOCKEDEMPTY, std::memory_order_seq_cst);
    assert(w != nullptr && w != LOCKEDEMPTY && w != eos);

    w->Gate.store(1, std::memory_order_release);
  }
};
