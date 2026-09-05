// SPDX-License-Identifier: GPL-2.0-only

use crate::lock_backend::LockBackend;
use crate::mcs::McsLockRaw;
use accordin_shared::mutex_hook::MutexHookBackend;

struct McsBackend;

impl MutexHookBackend for McsBackend {
    type LockState = McsLockRaw;
    const USES_ADMISSION_SCOPE: bool = true;

    fn create_state() -> Self::LockState {
        McsLockRaw::new()
    }

    fn lock(state: &Self::LockState) {
        LockBackend::lock(state);
    }

    fn try_lock(state: &Self::LockState) -> bool {
        LockBackend::try_lock(state)
    }

    fn unlock(state: &Self::LockState) {
        LockBackend::unlock(state);
    }
}

accordin_shared::export_mutex_hooks!(super::McsBackend);

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    use accordin_shared::mutex_hook::MutexHookBackend;

    use super::McsBackend;
    use super::exported_mutex_hooks::{
        pthread_mutex_destroy, pthread_mutex_init, pthread_mutex_lock, pthread_mutex_trylock,
        pthread_mutex_unlock,
    };

    /// The word the hook keeps its state pointer in, which is the first word of
    /// the mutex the caller owns.
    fn state_word(mutex: *mut libc::pthread_mutex_t) -> usize {
        unsafe { (mutex as *const usize).read_volatile() }
    }

    /// Serialises the tests that install hook states, so that the class
    /// accounting one counts its own mutexes and not another test's.
    fn class_accounting() -> std::sync::MutexGuard<'static, ()> {
        static ACCOUNTING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ACCOUNTING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A mutex the caller owns for as long as the test needs it, addressed the
    /// way the C ABI addresses one.
    struct RawMutex(*mut libc::pthread_mutex_t);

    // The hook is the only thing that writes the mutex, and it is what the
    // tests are exercising across threads.
    unsafe impl Send for RawMutex {}
    unsafe impl Sync for RawMutex {}

    impl RawMutex {
        fn created() -> Self {
            let storage = Box::into_raw(Box::new(
                std::mem::MaybeUninit::<libc::pthread_mutex_t>::uninit(),
            ));
            let mutex = storage.cast::<libc::pthread_mutex_t>();
            assert_eq!(unsafe { pthread_mutex_init(mutex, std::ptr::null()) }, 0);
            Self(mutex)
        }

        fn ptr(&self) -> *mut libc::pthread_mutex_t {
            self.0
        }
    }

    impl Drop for RawMutex {
        fn drop(&mut self) {
            assert_eq!(unsafe { pthread_mutex_destroy(self.0) }, 0);
            assert_eq!(state_word(self.0), 0, "a destroy leaves no state behind");
            drop(unsafe {
                Box::from_raw(
                    self.0
                        .cast::<std::mem::MaybeUninit<libc::pthread_mutex_t>>(),
                )
            });
        }
    }

    /// Creating a mutex is a memset and nothing else: the state it needs to be
    /// locked is installed by the acquisition that first needs it, so a mutex a
    /// process creates and never locks costs its own storage alone.
    #[test]
    fn a_mutex_gets_its_hook_state_from_its_first_acquisition() {
        let _accounting = class_accounting();
        let mutex = RawMutex::created();

        assert_eq!(
            state_word(mutex.ptr()),
            0,
            "creating a mutex allocates no state"
        );

        assert_eq!(unsafe { pthread_mutex_lock(mutex.ptr()) }, 0);
        let installed = state_word(mutex.ptr());
        assert!(installed > 1, "the first acquisition installs the state");
        assert_eq!(unsafe { pthread_mutex_unlock(mutex.ptr()) }, 0);

        assert_eq!(unsafe { pthread_mutex_lock(mutex.ptr()) }, 0);
        assert_eq!(
            state_word(mutex.ptr()),
            installed,
            "later acquisitions keep the state the first one installed"
        );
        assert_eq!(unsafe { pthread_mutex_unlock(mutex.ptr()) }, 0);
    }

    /// A first touch through `pthread_mutex_trylock` installs the state and
    /// answers, rather than waiting for another thread to install one: a
    /// trylock that could block would not be one.
    #[test]
    fn a_first_touch_through_trylock_installs_the_state_and_returns() {
        let _accounting = class_accounting();
        let mutex = RawMutex::created();

        assert_eq!(unsafe { pthread_mutex_trylock(mutex.ptr()) }, 0);
        assert!(state_word(mutex.ptr()) > 1);
        assert_eq!(unsafe { pthread_mutex_trylock(mutex.ptr()) }, libc::EBUSY);
        assert_eq!(unsafe { pthread_mutex_unlock(mutex.ptr()) }, 0);
    }

    /// A mutex that is created and destroyed without ever being locked never
    /// had a state, and its destroy has nothing to release.
    #[test]
    fn destroying_a_mutex_that_was_never_locked_releases_nothing() {
        let _accounting = class_accounting();
        let mutex = RawMutex::created();
        assert_eq!(state_word(mutex.ptr()), 0);
    }

    /// Threads that reach an unlocked mutex together race to install its state,
    /// and all but one have to give theirs up: a second state would be a second
    /// lock word, and the two would not exclude each other.
    #[test]
    fn racing_first_acquisitions_install_one_hook_state() {
        const ACQUIRERS: usize = 8;

        let _accounting = class_accounting();
        let mutex = Arc::new(RawMutex::created());
        let barrier = Arc::new(Barrier::new(ACQUIRERS));

        let acquirers = (0..ACQUIRERS)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    assert_eq!(unsafe { pthread_mutex_lock(mutex.ptr()) }, 0);
                    let observed = state_word(mutex.ptr());
                    assert_eq!(unsafe { pthread_mutex_unlock(mutex.ptr()) }, 0);
                    observed
                })
            })
            .collect::<Vec<_>>();

        let observed = acquirers
            .into_iter()
            .map(|acquirer| acquirer.join().expect("every acquirer should finish"))
            .collect::<Vec<_>>();

        let installed = state_word(mutex.ptr());
        assert!(installed > 1, "the race leaves an installed state behind");
        for state in observed {
            assert_eq!(
                state, installed,
                "the losers of the install lock through the state that won"
            );
        }
    }

    /// The managed classes are a pool of a few dozen, and a workload whose
    /// threads meet at a barrier first-touches its shared locks as many ways as
    /// the barrier is wide. A class spent on a state that loses its race and is
    /// freed would come out of that pool for good, so the loser keeps its class
    /// for the next lock it creates and the pool only ever pays for locks that
    /// exist.
    ///
    /// The threads are released off a spin rather than a barrier wake, and the
    /// rounds are repeated, because only an install that two threads reach at
    /// once produces a loser at all. What the accounting owes either way is the
    /// same: one class per mutex. The rule itself is pinned down without a race
    /// by the allocator's own tests.
    #[test]
    fn racing_first_acquisitions_spend_one_class_per_mutex() {
        const RACERS: usize = 8;
        const ROUNDS: usize = 6;

        let _accounting = class_accounting();
        accordin_shared::admission::reset_lock_id_allocator_for_test();
        let before = accordin_shared::admission::allocated_class_count();
        let mut owned = Vec::new();

        for _ in 0..ROUNDS {
            let contested = Arc::new(RawMutex::created());
            let ready = Arc::new(AtomicUsize::new(0));
            let go = Arc::new(AtomicBool::new(false));

            let racers = (0..RACERS)
                .map(|_| {
                    let contested = Arc::clone(&contested);
                    let ready = Arc::clone(&ready);
                    let go = Arc::clone(&go);
                    std::thread::spawn(move || {
                        // Spun up to the release rather than woken at it, so
                        // that the threads enter the install together and all
                        // but one of them lose it and free the state they
                        // built.
                        ready.fetch_add(1, Ordering::Release);
                        while !go.load(Ordering::Acquire) {
                            std::hint::spin_loop();
                        }
                        assert_eq!(unsafe { pthread_mutex_lock(contested.ptr()) }, 0);
                        assert_eq!(unsafe { pthread_mutex_unlock(contested.ptr()) }, 0);

                        // Every thread then creates a lock of its own, which is
                        // what a thread that kept an unused class spends it on.
                        let own = RawMutex::created();
                        assert_eq!(unsafe { pthread_mutex_lock(own.ptr()) }, 0);
                        assert_eq!(unsafe { pthread_mutex_unlock(own.ptr()) }, 0);
                        own
                    })
                })
                .collect::<Vec<_>>();

            while ready.load(Ordering::Acquire) != RACERS {
                std::hint::spin_loop();
            }
            go.store(true, Ordering::Release);

            for racer in racers {
                owned.push(racer.join().expect("every racer should finish"));
            }
            owned.push(Arc::into_inner(contested).expect("the racers are joined"));
        }

        let drawn = accordin_shared::admission::allocated_class_count() - before;
        assert_eq!(
            drawn as usize,
            ROUNDS * (RACERS + 1),
            "one class per mutex that exists, and none for the races that were lost"
        );

        drop(owned);
    }

    #[test]
    fn mcs_backend_uses_per_lock_admission_scope() {
        assert!(<McsBackend as MutexHookBackend>::USES_ADMISSION_SCOPE);
    }

    #[test]
    fn mcs_backend_unlock_does_not_yield() {
        let source = include_str!("mutex_hook.rs");
        let body = source
            .split_once("fn unlock(state: &Self::LockState) {")
            .and_then(|(_, rest)| rest.split_once("\n    }"))
            .map(|(body, _)| body)
            .expect("McsBackend::unlock body should be present");

        assert!(
            !body.contains("yield_now"),
            "McsBackend::unlock should only release the lock"
        );
    }
}
