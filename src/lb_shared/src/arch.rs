#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
use std::sync::OnceLock;

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
const NS_PER_SEC: u128 = 1_000_000_000;

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
struct TscCalibration {
    base_cycles: u64,
    base_ns: u64,
    freq_hz: u64,
}

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
#[inline(always)]
fn scale_cycles_to_ns(delta_cycles: u64, freq_hz: u64) -> u64 {
    ((delta_cycles as u128) * NS_PER_SEC / (freq_hz.max(1) as u128)) as u64
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline(always)]
fn convert_sample_to_ns(calibration: &TscCalibration, sample_cycles: u64) -> u64 {
    let delta_cycles = sample_cycles.saturating_sub(calibration.base_cycles);
    calibration
        .base_ns
        .saturating_add(scale_cycles_to_ns(delta_cycles, calibration.freq_hz))
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline(always)]
fn elapsed_sample_ns(calibration: &TscCalibration, start_cycles: u64, end_cycles: u64) -> u64 {
    scale_cycles_to_ns(end_cycles.saturating_sub(start_cycles), calibration.freq_hz)
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
            let freq_hz = ((delta_cycles as u128) * NS_PER_SEC / (delta_ns.max(1) as u128)).max(1);
            return freq_hz as u64;
        }
        std::hint::spin_loop();
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
fn capture_tsc_anchor() -> (u64, u64) {
    let start_cycles = rdtsc();
    let anchor_ns = monotonic_ns();
    let end_cycles = rdtsc();
    let anchor_cycles = start_cycles.saturating_add(end_cycles.saturating_sub(start_cycles) / 2);
    (anchor_cycles, anchor_ns)
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
fn tsc_calibration() -> &'static TscCalibration {
    static TSC_CALIBRATION: OnceLock<TscCalibration> = OnceLock::new();

    TSC_CALIBRATION.get_or_init(|| {
        let freq_hz = detect_tsc_frequency_hz();
        let (base_cycles, base_ns) = capture_tsc_anchor();

        TscCalibration {
            base_cycles,
            base_ns,
            freq_hz,
        }
    })
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline(always)]
pub fn wait_time_start() -> u64 {
    tsc_calibration();
    rdtsc()
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
#[inline(always)]
pub fn wait_time_start() -> u64 {
    monotonic_ns()
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline(always)]
pub fn wait_time_to_ns(sample_cycles: u64) -> u64 {
    convert_sample_to_ns(tsc_calibration(), sample_cycles)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
#[inline(always)]
pub fn wait_time_to_ns(sample_ns: u64) -> u64 {
    sample_ns
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[inline(always)]
pub fn wait_time_elapsed_ns_between(start_cycles: u64, end_cycles: u64) -> u64 {
    elapsed_sample_ns(tsc_calibration(), start_cycles, end_cycles)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
#[inline(always)]
pub fn wait_time_elapsed_ns_between(start_ns: u64, end_ns: u64) -> u64 {
    end_ns.saturating_sub(start_ns)
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
#[cfg(test)]
mod tests {
    use super::{TscCalibration, convert_sample_to_ns, elapsed_sample_ns, scale_cycles_to_ns};

    fn test_calibration() -> TscCalibration {
        TscCalibration {
            base_cycles: 1_000,
            base_ns: 5_000,
            freq_hz: 2_000_000_000,
        }
    }

    #[test]
    fn cycles_to_ns_uses_calibrated_anchor_and_frequency() {
        let calibration = test_calibration();

        assert_eq!(scale_cycles_to_ns(600, calibration.freq_hz), 300);
        assert_eq!(convert_sample_to_ns(&calibration, 1_600), 5_300);
    }

    #[test]
    fn elapsed_between_samples_scales_by_frequency() {
        let calibration = test_calibration();

        assert_eq!(elapsed_sample_ns(&calibration, 2_000, 3_000), 500);
    }

    #[test]
    fn converting_sample_before_anchor_saturates_to_anchor_ns() {
        let calibration = test_calibration();

        assert_eq!(convert_sample_to_ns(&calibration, 900), 5_000);
    }
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

#[inline(always)]
pub fn compiler_barrier() {
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}
