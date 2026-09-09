// Use the same required rseq TSE ABI as bench/mutexbench (no silent fallback).
#include "timeslice_extension.hpp"
using namespace locks_bench;
static thread_local CriticalSectionTimesliceExtension extension{
    TimesliceExtensionMode::kRequire};
static thread_local unsigned depth;
extern "C" void experiment_tse_prepare() { extension.prepare_thread(); }
extern "C" void experiment_tse_enter() {
    if (depth++ == 0) extension.on_critical_section_enter();
}
extern "C" void experiment_tse_exit() {
    if (--depth == 0) extension.on_critical_section_exit();
}
#ifdef TSE_PROBE
#include <cstdio>
int main() {
    auto status = CurrentThreadTimesliceExtensionStatus(TimesliceExtensionMode::kRequire);
    std::printf("enabled=%d reason=%s errno=%d\n", status.enabled,
                status.reason ? status.reason : "available", status.error_number);
    return status.enabled ? 0 : 77;
}
#endif
