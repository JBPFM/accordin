accordin_shared::define_mcs_lock!(4);

#[cfg(test)]
mod tests {
    use std::cell::UnsafeCell;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{McsLockRaw, Node};
    use crate::lock_backend::LockBackend;

    #[test]
    fn node_layout_stays_cache_aligned() {
        assert_eq!(std::mem::size_of::<Node>(), 64);
        assert_eq!(std::mem::align_of::<Node>(), 64);
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

    #[test]
    fn raw_lock_slow_path_does_not_write_admission_word() {
        crate::admission::reset_state();
        crate::admission::reset_thread_depth_for_test();

        let lock = Arc::new(McsLockRaw::new());
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let holder_lock = Arc::clone(&lock);
        let holder = std::thread::spawn(move || {
            holder_lock.lock();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            holder_lock.unlock();
        });

        locked_rx.recv().unwrap();

        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            release_tx.send(()).unwrap();
        });

        lock.lock();
        assert_eq!(crate::admission::word_for_test(), 0);
        lock.unlock();
        assert_eq!(crate::admission::word_for_test(), 0);

        release.join().unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn supports_four_nested_locks_per_thread() {
        let locks: [McsLockRaw; 4] = std::array::from_fn(|_| McsLockRaw::new());

        for lock in &locks {
            lock.lock();
        }
        for lock in locks.iter().rev() {
            lock.unlock();
        }
    }

    #[test]
    fn try_lock_at_max_nesting_depth_reports_busy() {
        let locks: [McsLockRaw; 4] = std::array::from_fn(|_| McsLockRaw::new());
        let extra = McsLockRaw::new();

        for lock in &locks {
            lock.lock();
        }

        assert!(!extra.try_lock());

        locks[3].unlock();
        assert!(extra.try_lock());
        extra.unlock();

        for lock in locks[..3].iter().rev() {
            lock.unlock();
        }
    }

    #[test]
    #[should_panic(expected = "McsLockRaw nested lock depth exceeded 4")]
    fn fifth_nested_lock_panics() {
        let locks: [McsLockRaw; 5] = std::array::from_fn(|_| McsLockRaw::new());

        for lock in &locks {
            lock.lock();
        }
    }
}
