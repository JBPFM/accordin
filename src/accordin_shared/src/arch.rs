/// Wraps a value so it sits on its own cache line (64-byte aligned).
///
/// Use the `.0` field for direct access to the wrapped value.
#[repr(align(64))]
pub struct CacheAligned<T>(pub T);

#[inline(always)]
pub fn pause() {
    #[cfg(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "x86_64",
        target_arch = "x86"
    ))]
    {
        std::hint::spin_loop();
    }

    #[cfg(not(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "x86_64",
        target_arch = "x86"
    )))]
    {
        std::thread::yield_now();
    }
}
