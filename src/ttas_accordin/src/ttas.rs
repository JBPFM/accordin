use std::sync::atomic::{AtomicBool, Ordering};

use crate::admission::{mark_critical_section_exit, mark_slow_path_pending};
use crate::arch::{CacheAligned, pause};
use crate::lock_backend::LockBackend;

/// Test-and-test-and-set lock with cache-aligned state.
pub struct TtasLockRaw {
    locked: CacheAligned<AtomicBool>,
}

impl TtasLockRaw {
    pub const fn new() -> Self {
        Self {
            locked: CacheAligned(AtomicBool::new(false)),
        }
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn lock_slow(&self) {
        mark_slow_path_pending();
        std::thread::yield_now();
        loop {
            while self.locked.0.load(Ordering::Relaxed) {
                pause();
            }
            if self
                .locked
                .0
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            pause();
        }
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn try_lock_fast(&self) -> bool {
        self.locked
            .0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock_fast(&self) {
        self.locked.0.store(false, Ordering::Release);
        mark_critical_section_exit();
    }
}

impl LockBackend for TtasLockRaw {
    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn lock(&self) {
        self.lock_slow();
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn try_lock(&self) -> bool {
        self.try_lock_fast()
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock(&self) {
        self.unlock_fast();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::UnsafeCell;
    use std::sync::Arc;

    use super::TtasLockRaw;
    use crate::lock_backend::LockBackend;

    #[test]
    fn try_lock_requires_unlock_before_reacquire() {
        let lock = TtasLockRaw::new();

        assert!(lock.try_lock());
        assert!(!lock.try_lock());
        lock.unlock();
        assert!(lock.try_lock());
        lock.unlock();
    }

    struct SharedCounter {
        lock: TtasLockRaw,
        value: UnsafeCell<usize>,
    }

    unsafe impl Sync for SharedCounter {}

    #[test]
    fn lock_serializes_counter_updates() {
        let shared = Arc::new(SharedCounter {
            lock: TtasLockRaw::new(),
            value: UnsafeCell::new(0),
        });
        let mut handles = Vec::new();

        for _ in 0..4 {
            let shared = Arc::clone(&shared);
            handles.push(std::thread::spawn(move || {
                for _ in 0..10_000 {
                    shared.lock.lock();
                    unsafe {
                        let next = (*shared.value.get()).wrapping_add(1);
                        *shared.value.get() = next;
                    }
                    shared.lock.unlock();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(unsafe { *shared.value.get() }, 40_000);
    }
}
