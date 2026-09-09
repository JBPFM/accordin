// Report whether this kernel gives a thread the rseq time-slice extension that
// the mbmcstse algorithm applies to its critical sections. The library treats a
// missing extension as a no-op, so the experiment driver needs this separate
// answer to record mcs-tse as unsupported rather than measure a plain MCS.
#include <cstdio>

#include "mbtimeslice.hpp"

namespace {

const char *unsupported_reason() {
    if (!mblocks::detail::kHasRseqSliceYieldSyscallNumber)
        return "no rseq_slice_yield syscall number for this architecture";
    if (__rseq_size == 0)
        return "the C library registered no rseq area";
    const mblocks::detail::RseqWithSliceCtrl *rseq =
        mblocks::detail::CurrentThreadRseqWithSliceCtrl();
    if (rseq == nullptr)
        return "the rseq area is smaller than the slice control word";
    if ((rseq->flags & RSEQ_CS_FLAG_SLICE_EXT_AVAILABLE) == 0)
        return "the kernel does not advertise the slice extension";
    return "the kernel refused to enable the slice extension";
}

}  // namespace

int main() {
    if (mblocks::detail::EnsureTimesliceThreadState().enabled) {
        std::printf("rseq slice extension enabled\n");
        return 0;
    }
    std::printf("rseq slice extension unavailable: %s\n", unsupported_reason());
    return 77;
}
