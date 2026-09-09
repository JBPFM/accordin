#pragma once

/*
 * Ticket lock with a waiting array (TWA) from the mutex microbenchmark.
 *
 * The algorithm keeps no per-thread queue node: the only per-acquisition state
 * is the ticket, which lives in a per (thread, lock) context supplied by the
 * caller. The waiting array is a member of the lock, as in the original.
 */

#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#if defined(__x86_64__) || defined(__i386__)
#include <immintrin.h>
#else
#include <thread>
#endif

struct MbTwaLock {
  static constexpr std::size_t kWaitingArraySize = 4096;
  static_assert((kWaitingArraySize & (kWaitingArraySize - 1)) == 0,
                "kWaitingArraySize must be a power of two");
  static constexpr std::uint64_t kLongTermThreshold = 1;

  struct alignas(64) WaitSlot {
    std::atomic<std::uint32_t> sequence{0};
  };

  alignas(64) std::atomic<std::uint64_t> next_ticket{0};
  alignas(64) std::atomic<std::uint64_t> grant{0};
  alignas(64) std::array<WaitSlot, kWaitingArraySize> waiting_array{};

  struct LockState {
    std::uint64_t ticket;
  };

  struct Context {
    LockState state;
  };

  [[nodiscard]] static inline std::size_t HashTicket(std::uint64_t ticket) {
    ticket ^= ticket >> 33;
    ticket *= 0xff51afd7ed558ccdULL;
    ticket ^= ticket >> 33;
    ticket *= 0xc4ceb9fe1a85ec53ULL;
    ticket ^= ticket >> 33;
    return static_cast<std::size_t>(ticket) & (kWaitingArraySize - 1);
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

  inline void lock(Context &context) {
    const std::uint64_t my_ticket =
        next_ticket.fetch_add(1, std::memory_order_relaxed);

    std::uint64_t observed_grant = grant.load(std::memory_order_acquire);
    if (observed_grant != my_ticket) {
      const std::size_t slot_index = HashTicket(my_ticket);
      std::uint32_t observed_sequence =
          waiting_array[slot_index].sequence.load(std::memory_order_relaxed);

      while ((my_ticket - observed_grant) > kLongTermThreshold) {
        while (waiting_array[slot_index].sequence.load(
                   std::memory_order_acquire) == observed_sequence) {
          Pause();
          observed_grant = grant.load(std::memory_order_acquire);
          if ((my_ticket - observed_grant) <= kLongTermThreshold) {
            break;
          }
        }
        observed_sequence =
            waiting_array[slot_index].sequence.load(std::memory_order_relaxed);
        observed_grant = grant.load(std::memory_order_acquire);
      }

      while (observed_grant != my_ticket) {
        Pause();
        observed_grant = grant.load(std::memory_order_acquire);
      }
    }

    context.state = LockState{my_ticket};
  }

  inline void unlock(Context &context) {
    const std::uint64_t next_ticket_to_grant = context.state.ticket + 1;
    grant.store(next_ticket_to_grant, std::memory_order_release);

    const std::uint64_t wakeup_ticket =
        next_ticket_to_grant + kLongTermThreshold;
    const std::size_t slot_index = HashTicket(wakeup_ticket);
    waiting_array[slot_index].sequence.fetch_add(1, std::memory_order_relaxed);
  }
};
