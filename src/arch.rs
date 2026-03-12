#[inline(always)]
pub fn clock_gettime_ns() -> u64 {
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
