#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
use std::sync::OnceLock;

#[inline(always)]
fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[inline(always)]
pub fn wait_time_now_ns() -> u64 {
    monotonic_ns()
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn rdtsc() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(target_arch = "x86")]
#[inline(always)]
fn rdtsc() -> u64 {
    unsafe { core::arch::x86::_rdtsc() as u64 }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
fn read_freq_khz(path: &str) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
fn detect_tsc_frequency_hz() -> u64 {
    const KHZ_TO_HZ: u64 = 1_000;
    const CALIBRATION_WINDOW_NS: u64 = 20_000_000;

    for path in [
        "/sys/devices/system/cpu/cpu0/tsc_freq_khz",
        "/sys/devices/system/cpu/cpu0/cpufreq/base_frequency",
        "/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq",
    ] {
        if let Some(freq_khz) = read_freq_khz(path) {
            return freq_khz.saturating_mul(KHZ_TO_HZ).max(1);
        }
    }

    // Fall back to a one-time wall-clock calibration when sysfs doesn't expose TSC frequency.
    let start_ns = monotonic_ns();
    let start_cycles = rdtsc();
    loop {
        let now_ns = monotonic_ns();
        let delta_ns = now_ns.saturating_sub(start_ns);
        if delta_ns >= CALIBRATION_WINDOW_NS {
            let delta_cycles = rdtsc().wrapping_sub(start_cycles);
            let freq_hz =
                ((delta_cycles as u128) * 1_000_000_000u128 / (delta_ns.max(1) as u128)).max(1);
            return freq_hz as u64;
        }
        std::hint::spin_loop();
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
fn tsc_frequency_hz() -> u64 {
    static TSC_FREQUENCY_HZ: OnceLock<u64> = OnceLock::new();
    *TSC_FREQUENCY_HZ.get_or_init(detect_tsc_frequency_hz)
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline(always)]
pub fn wait_time_start() -> u64 {
    rdtsc()
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline(always)]
pub fn wait_time_elapsed_ns(start_cycles: u64) -> u64 {
    let delta_cycles = rdtsc().wrapping_sub(start_cycles);
    let freq_hz = tsc_frequency_hz().max(1);
    ((delta_cycles as u128) * 1_000_000_000u128 / (freq_hz as u128)) as u64
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
#[inline(always)]
pub fn wait_time_start() -> u64 {
    monotonic_ns()
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
#[inline(always)]
pub fn wait_time_elapsed_ns(start_ns: u64) -> u64 {
    monotonic_ns().wrapping_sub(start_ns)
}

/// Compiler-only fence (no hardware barrier), equivalent to C++ `atomic_signal_fence(seq_cst)`.
#[inline(always)]
pub fn compiler_barrier() {
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[inline(always)]
pub fn pause() {
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    {
        std::hint::spin_loop();
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    {
        std::thread::yield_now();
    }
}
