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
