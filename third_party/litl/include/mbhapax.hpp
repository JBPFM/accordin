#pragma once

/*
 * Hapax lock with visible waiters, from the mutex microbenchmark.
 *
 * The algorithm keeps no per-thread queue node: the only per-acquisition state
 * is the hapax value, which lives in a per (thread, lock) context supplied by
 * the caller. The hapax allocator and the waiting array are process wide by
 * construction and stay as they are: hapax values must be unique across the
 * whole process, and the waiting array is shared by every lock and indexed by
 * a hash salted with the lock address.
 */

#include <atomic>
#include <cassert>
#include <cstdint>
#include <thread>
#if defined(__x86_64__) || defined(__i386__)
#include <immintrin.h>
#endif

struct MbHapaxVw {
  struct alignas(64) Slot {
    std::atomic<std::uint64_t> VisibleWaiter{0};
  };

  static constexpr std::uint32_t kWaitingArraySize = 4096;
  static_assert(kWaitingArraySize > 0 &&
                    (kWaitingArraySize & (kWaitingArraySize - 1)) == 0,
                "kWaitingArraySize must be a power of two");

  alignas(64) std::atomic<std::uint64_t> Arrive{0}; // ingress
  alignas(64) std::atomic<std::uint64_t> Depart{0}; // egress

  [[nodiscard]] inline Slot *ToSlot(std::uint64_t hapax) {
    alignas(4096) static Slot waiting_array[kWaitingArraySize]{};
    const auto salt =
        static_cast<std::uint32_t>(reinterpret_cast<std::uintptr_t>(this));
    const std::uint32_t ix =
        ((salt + static_cast<std::uint32_t>(hapax >> 16)) * 17u) &
        (kWaitingArraySize - 1u);
    return waiting_array + ix;
  }

  [[nodiscard]] static inline std::uint64_t NextHapax() {
    static constinit thread_local std::uint64_t PrivateHapax = 0;
    alignas(128) static constinit std::atomic<std::uint64_t> HapaxAllocator{0};

    std::uint64_t hapax = PrivateHapax++;
    if ((hapax & 0xFFFFu) == 0) [[unlikely]] {
      hapax = HapaxAllocator.fetch_add(1, std::memory_order_relaxed) + 1;
      assert(hapax != 0);
      hapax <<= 16;
      assert(hapax + 1 >= PrivateHapax);
      PrivateHapax = hapax + 1;
    }

    assert(hapax != 0);
    return hapax;
  }

  static inline void Pause(std::uint32_t spin_count) {
#if defined(__x86_64__) || defined(__i386__)
    (void)spin_count;
    _mm_pause();
#elif defined(__aarch64__) || defined(__arm__)
    (void)spin_count;
    asm volatile("yield" ::: "memory");
#else
    if ((spin_count & 0xFFu) == 0) {
      std::this_thread::yield();
    }
#endif
  }

  // Store-to-load fence that closes the architectural race window against a
  // tardy waiter in the unlock slow path. Listing 5 of the paper states that a
  // store-load fence, or an equivalent, is needed to avoid that race.
  static inline void StoreLoadFence() noexcept {
    std::atomic_thread_fence(std::memory_order_acq_rel);
  }

  struct LockState {
    std::uint64_t hapax;
  };

  struct Context {
    LockState state;
  };

  inline void lock(Context &context) {
    const std::uint64_t hapax = NextHapax();
    const std::uint64_t pred =
        Arrive.exchange(hapax, std::memory_order_acq_rel);
    assert(pred != hapax);

    if (Depart.load(std::memory_order_acquire) != pred) {
      Slot *slot = ToSlot(pred);
      std::uint64_t expected = 0;

      // The paper registers the visible waiter with CAS(0 -> pred); this must
      // not be relaxed into a TTAS that loads before the CAS.
      if (!slot->VisibleWaiter.compare_exchange_strong(
              expected, pred, std::memory_order_acq_rel,
              std::memory_order_acquire)) {
        // Collision: fall back to spinning globally on Depart, the fallback of
        // Listing 5 of the paper.
        std::uint32_t spin_count = 0;
        while (Depart.load(std::memory_order_acquire) != pred) {
          Pause(++spin_count);
        }
      } else if (Depart.load(std::memory_order_acquire) == pred) {
        // Ratify: this races with unlock(), so the slot has to be cleared with
        // CAS(pred -> 0) rather than a plain store of 0.
        expected = pred;
        (void)slot->VisibleWaiter.compare_exchange_strong(
            expected, 0, std::memory_order_acq_rel, std::memory_order_acquire);
      } else {
        // Common case: wait for the slot to change, cleared by the CAS of the
        // predecessor.
        std::uint32_t spin_count = 0;
        while (slot->VisibleWaiter.load(std::memory_order_acquire) == pred) {
          Pause(++spin_count);
        }
      }
    }

    context.state = LockState{hapax};
  }

  inline void unlock(Context &context) {
    const std::uint64_t hapax = context.state.hapax;
    assert(hapax != 0);

    Slot *slot = ToSlot(hapax);

    // The CAS below publishes the critical section and hands the lock over, so
    // release ordering is enough on success; a waiter observes the change of
    // the slot with an acquire load on the lock side, which establishes the
    // synchronization.
    std::uint64_t expected = hapax;
    if (slot->VisibleWaiter.compare_exchange_strong(
            expected, 0,
            std::memory_order_release,  // success: publish CS + handover
            std::memory_order_relaxed)) // fail: just a probe
    {
      return; // assured positive handover: the store to Depart is skipped
    }

    // Slow path: Depart has to be stored.
    Depart.store(hapax, std::memory_order_release);

    // The paper points out a potential architectural race here, which a
    // store-to-load fence closes.
    StoreLoadFence(); // store Depart; fence; then CAS or load the slot

    // A second CAS(hapax -> 0) closes the tardy waiter race window, as in
    // lines 156-166 of Listing 5 of the paper.
    expected = hapax;
    (void)slot->VisibleWaiter.compare_exchange_strong(
        expected, 0, std::memory_order_release, std::memory_order_relaxed);
  }
};
