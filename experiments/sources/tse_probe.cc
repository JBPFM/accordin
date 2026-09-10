// Report whether this kernel gives a thread the rseq time-slice extension that
// the mbmcstse algorithm applies to its critical sections. The library treats a
// missing extension as a no-op, so the experiment driver needs this separate
// answer to record mcs-tse as unsupported rather than measure a plain MCS.
//
// The line printed here carries the observables the library's own decision
// rests on, so a run record says why the answer came out the way it did.
#include <cstdio>

#include "mbtimeslice.hpp"

int main() {
    const bool enabled = mblocks::detail::EnsureTimesliceThreadState().enabled;
    const mblocks::detail::RseqWithSliceCtrl *rseq =
        mblocks::detail::CurrentThreadRseqWithSliceCtrl();
    std::printf("rseq slice extension %s: slice_yield_syscall=%d rseq_size=%u "
                "slice_ctrl=%s available_flag=%d\n",
                enabled ? "enabled" : "unavailable",
                mblocks::detail::kHasRseqSliceYieldSyscallNumber ? 1 : 0,
                static_cast<unsigned>(__rseq_size),
                rseq == nullptr ? "absent" : "present",
                rseq != nullptr &&
                    (rseq->flags & RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE) != 0);
    return enabled ? 0 : 77;
}
