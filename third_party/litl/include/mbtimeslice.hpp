#pragma once

/*
 * rseq time-slice extension used by the TSE lock variants of the mutex
 * microbenchmark. A thread asks the kernel for a short extension of its time
 * slice while it owns the lock, and gives the extension back when it releases
 * the lock.
 *
 * The feature is a prctl/rseq facility: it needs neither sched_ext nor a BPF
 * scheduler. The microbenchmark selects it with a mode that aborts the process
 * when the kernel does not advertise the extension. Aborting is unacceptable
 * for a library injected with LD_PRELOAD, so this port keeps only automatic
 * semantics: the extension is used when the kernel advertises it, and every
 * entry point is a silent no-op otherwise.
 */

#include <atomic>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <sys/prctl.h>
#include <sys/rseq.h>
#include <sys/syscall.h>
#include <unistd.h>

namespace mblocks {

namespace detail {

#ifndef PR_RSEQ_SLICE_EXTENSION
#define PR_RSEQ_SLICE_EXTENSION 79
#define PR_RSEQ_SLICE_EXTENSION_GET 1
#define PR_RSEQ_SLICE_EXTENSION_SET 2
#define PR_RSEQ_SLICE_EXT_ENABLE 0x01
#endif

#ifndef RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE
#define RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE (1U << 4)
#define RSEQ_CS_FLAG_SLICE_EXT_ENABLED (1U << 5)
#endif

#ifndef ENOTSUPP
#define ENOTSUPP 524
#endif

#if defined(SYS_rseq_slice_yield)
inline constexpr long kRseqSliceYieldSyscallNumber = SYS_rseq_slice_yield;
inline constexpr bool kHasRseqSliceYieldSyscallNumber = true;
#elif defined(__NR_rseq_slice_yield)
inline constexpr long kRseqSliceYieldSyscallNumber = __NR_rseq_slice_yield;
inline constexpr bool kHasRseqSliceYieldSyscallNumber = true;
#elif defined(__x86_64__)
inline constexpr long kRseqSliceYieldSyscallNumber = 471;
inline constexpr bool kHasRseqSliceYieldSyscallNumber = true;
#else
inline constexpr long kRseqSliceYieldSyscallNumber = -1;
inline constexpr bool kHasRseqSliceYieldSyscallNumber = false;
#endif

/* Layout of the slice control word appended to the glibc rseq area. */
union RseqSliceCtrl {
  std::uint32_t all;
  struct {
    std::uint8_t request;
    std::uint8_t granted;
    std::uint16_t reserved;
  } fields;
};

struct RseqWithSliceCtrl {
  std::uint32_t cpu_id_start;
  std::uint32_t cpu_id;
  std::uint64_t rseq_cs;
  std::uint32_t flags;
  std::uint32_t node_id;
  std::uint32_t mm_cid;
  RseqSliceCtrl slice_ctrl;
};

static_assert(offsetof(RseqWithSliceCtrl, slice_ctrl) == 28);

inline constexpr std::size_t kRseqSliceCtrlEnd =
    offsetof(RseqWithSliceCtrl, slice_ctrl) + sizeof(RseqSliceCtrl);

inline void CompilerBarrier() noexcept {
  std::atomic_signal_fence(std::memory_order_seq_cst);
}

inline bool IsUnsupportedErrno(int err) noexcept {
  return err == EOPNOTSUPP || err == ENOTSUPP || err == ENOSYS || err == EINVAL;
}

struct TimesliceThreadState {
  bool initialized;
  bool enabled;
  RseqWithSliceCtrl *rseq;
};

inline TimesliceThreadState &CurrentThreadTimesliceState() noexcept {
  static constinit thread_local TimesliceThreadState state{false, false,
                                                           nullptr};
  return state;
}

inline RseqWithSliceCtrl *CurrentThreadRseqWithSliceCtrl() noexcept {
  if (__rseq_size < kRseqSliceCtrlEnd) {
    return nullptr;
  }

  char *thread_pointer = static_cast<char *>(__builtin_thread_pointer());
  return reinterpret_cast<RseqWithSliceCtrl *>(thread_pointer + __rseq_offset);
}

inline TimesliceThreadState InitTimesliceThreadState() noexcept {
  TimesliceThreadState state{true, false, nullptr};

  if (!kHasRseqSliceYieldSyscallNumber) {
    return state;
  }

  /* glibc did not register an rseq area for this thread. */
  if (__rseq_size == 0) {
    return state;
  }

  state.rseq = CurrentThreadRseqWithSliceCtrl();
  if (state.rseq == nullptr) {
    return state;
  }

  /* The kernel does not advertise the slice extension. */
  if ((state.rseq->flags & RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE) == 0) {
    state.rseq = nullptr;
    return state;
  }

  errno = 0;
  const int current =
      prctl(PR_RSEQ_SLICE_EXTENSION, PR_RSEQ_SLICE_EXTENSION_GET, 0, 0, 0);
  if (current == -1) {
    state.rseq = nullptr;
    return state;
  }

  if ((current & PR_RSEQ_SLICE_EXT_ENABLE) == 0) {
    errno = 0;
    if (prctl(PR_RSEQ_SLICE_EXTENSION, PR_RSEQ_SLICE_EXTENSION_SET,
              PR_RSEQ_SLICE_EXT_ENABLE, 0, 0) == -1) {
      state.rseq = nullptr;
      return state;
    }
  }

  state.rseq->slice_ctrl.all = 0;
  CompilerBarrier();
  state.enabled = true;
  return state;
}

inline const TimesliceThreadState &EnsureTimesliceThreadState() noexcept {
  TimesliceThreadState &state = CurrentThreadTimesliceState();
  if (!state.initialized) {
    state = InitTimesliceThreadState();
  }
  return state;
}

} // namespace detail

/*
 * Brackets a critical section with a slice extension request. The object is
 * stateless: everything it touches is either per-thread rseq storage or the
 * kernel.
 */
class CriticalSectionTimesliceExtension {
public:
  void on_critical_section_enter() const noexcept {
    const auto &state = detail::EnsureTimesliceThreadState();
    if (!state.enabled) {
      return;
    }

    state.rseq->slice_ctrl.fields.request = 1;
    detail::CompilerBarrier();
  }

  void on_critical_section_exit() const noexcept {
    auto &state = detail::CurrentThreadTimesliceState();
    if (!state.initialized || !state.enabled) {
      return;
    }

    detail::CompilerBarrier();
    state.rseq->slice_ctrl.fields.request = 0;

    if (state.rseq->slice_ctrl.fields.granted == 0) {
      return;
    }

    errno = 0;
    if (syscall(detail::kRseqSliceYieldSyscallNumber) == 0) {
      return;
    }

    /* The yield syscall is unavailable, so stop using the extension. */
    if (detail::IsUnsupportedErrno(errno)) {
      state.enabled = false;
    }
  }
};

} // namespace mblocks
