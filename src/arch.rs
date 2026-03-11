#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
use std::time::Instant;

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
pub fn read_tsc() -> u64 {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        return core::arch::x86_64::_rdtsc();
    }

    #[cfg(target_arch = "x86")]
    unsafe {
        return core::arch::x86::_rdtsc() as u64;
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
    {
        thread_local! {
            static START: Instant = Instant::now();
        }

        START.with(|start| start.elapsed().as_nanos() as u64)
    }
}
