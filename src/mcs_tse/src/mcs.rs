accordin_shared::define_mcs_lock!(8);

#[cfg(test)]
mod tests {
    use std::cell::UnsafeCell;
    use std::sync::Arc;

    use super::{MAX_THREAD_NODES, McsLockRaw, Node};
    use crate::lock_backend::LockBackend;

    #[test]
    fn node_layout_stays_cache_aligned() {
        assert_eq!(std::mem::size_of::<Node>(), 64);
        assert_eq!(std::mem::align_of::<Node>(), 64);
    }

    #[test]
    fn lock_state_stays_compact() {
        assert_eq!(std::mem::size_of::<McsLockRaw>(), 64);
        assert_eq!(std::mem::align_of::<McsLockRaw>(), 64);
    }

    #[test]
    fn try_lock_requires_unlock_before_reacquire() {
        let lock = McsLockRaw::new();

        assert!(lock.try_lock());
        assert!(!lock.try_lock());
        lock.unlock();
        assert!(lock.try_lock());
        lock.unlock();
    }

    #[test]
    fn failed_try_lock_releases_thread_node() {
        let lock = McsLockRaw::new();

        assert!(lock.try_lock());
        for _ in 0..(MAX_THREAD_NODES * 4) {
            assert!(!lock.try_lock());
        }
        lock.unlock();

        assert!(lock.try_lock());
        lock.unlock();
    }

    struct SharedCounter {
        lock: McsLockRaw,
        value: UnsafeCell<usize>,
    }

    unsafe impl Sync for SharedCounter {}

    #[test]
    fn lock_serializes_counter_updates() {
        let shared = Arc::new(SharedCounter {
            lock: McsLockRaw::new(),
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
