// SPDX-License-Identifier: GPL-2.0-only

use std::cell::{Cell, UnsafeCell};

use accordin_shared::condvar::{CondMutex, CondRelock, CondState};
use accordin_shared::mutex_hook::{LockBackendAdapter, MutexHookBackend};

type DirectBackend = LockBackendAdapter<crate::mcs_tas::McsTasLockRaw>;

/// Every acquire path here opens an admission scope unconditionally, and the
/// cond protocol is told the backend's own answer rather than a literal. The two
/// have to agree: a backend outside admission would take the scope-less relock
/// branch the hook macro has, which this file does not carry.
const _: () = assert!(<DirectBackend as MutexHookBackend>::USES_ADMISSION_SCOPE);

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct mcs_tas_accordin_direct_mutex {
    lock: <DirectBackend as MutexHookBackend>::LockState,
    lock_id: u32,
}

/// The condition variable the C API hands out. The wait protocol lives in the
/// shared futex implementation; this type only owns the state it blocks on.
#[allow(non_camel_case_types)]
pub struct mcs_tas_accordin_direct_cond {
    state: CondState,
}

struct ThreadCtxGuard;

impl Drop for ThreadCtxGuard {
    fn drop(&mut self) {
        accordin_shared::lock_stats::flush_current_thread_stats();
        accordin_shared::mutex_hook::unregister_current_thread();
    }
}

thread_local! {
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
    static _GUARD: UnsafeCell<Option<ThreadCtxGuard>> = const { UnsafeCell::new(None) };
}

#[inline(always)]
fn ensure_registered() {
    accordin_shared::cpu_affinity::ensure_current_thread_affinity();

    REGISTERED.with(|registered| {
        if registered.get() {
            return;
        }

        accordin_shared::lock_stats::record_thread_start();
        let _ = accordin_shared::mutex_hook::register_current_thread();
        _GUARD.with(|guard| unsafe { *guard.get() = Some(ThreadCtxGuard) });
        registered.set(true);
    });
}

/// Whether the acquire bracket still has to ask admission for a slot, or
/// whether the caller already holds the routing decision.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AcquireMode {
    Normal,
    AlreadyAdmitted,
}

#[inline(always)]
fn lock_with_stats(lock: &<DirectBackend as MutexHookBackend>::LockState, lock_id: u32) {
    let scope = accordin_shared::admission::begin_lock_scope(lock_id);
    lock_scope_with_stats(lock, scope, AcquireMode::Normal);
}

#[inline(always)]
fn lock_scope_with_stats(
    lock: &<DirectBackend as MutexHookBackend>::LockState,
    scope: accordin_shared::admission::LockAdmissionScope,
    mode: AcquireMode,
) {
    let normal = mode == AcquireMode::Normal;

    if normal
        && !accordin_shared::admission::token_consumed_for_scope(scope)
        && DirectBackend::try_lock(lock)
    {
        accordin_shared::lock_stats::record_lock_acquired_for_scope(scope);
        return;
    }

    let wait_start = accordin_shared::lock_stats::record_wait_start();
    if normal && accordin_shared::admission::mark_slow_path_pending_for_scope(scope) {
        std::thread::yield_now();
        accordin_shared::admission::clear_token_consumed_for_scope(scope);
    }
    DirectBackend::lock(lock);
    accordin_shared::lock_stats::record_wait_end(wait_start);
    accordin_shared::lock_stats::record_lock_acquired_for_scope(scope);
}

/// Reacquires the cond mutex after a wait, in the mode the wait protocol
/// decided: a routed wakeup already carries the admission decision, and a
/// published hint carries it in the user word, so both enter the lock without
/// re-requesting admission.
#[inline(always)]
fn relock_after_cond_wait(
    lock: &<DirectBackend as MutexHookBackend>::LockState,
    lock_id: u32,
    relock: CondRelock,
) {
    let scope = accordin_shared::admission::begin_lock_scope(lock_id);
    let mode = match relock {
        CondRelock::AlreadyAdmitted => AcquireMode::AlreadyAdmitted,
        CondRelock::TakeHint
            if accordin_shared::admission::take_cond_reacquire_pending_for_scope(scope) =>
        {
            accordin_shared::mutex_hook::record_cv_specialized_relock();
            AcquireMode::AlreadyAdmitted
        }
        _ => {
            accordin_shared::mutex_hook::record_cv_fallback_relock();
            AcquireMode::Normal
        }
    };
    lock_scope_with_stats(lock, scope, mode);
}

#[inline(always)]
fn unlock_with_stats(lock: &<DirectBackend as MutexHookBackend>::LockState, lock_id: u32) {
    let hold_end = accordin_shared::lock_stats::record_hold_end_sample();
    DirectBackend::unlock(lock);
    accordin_shared::admission::finish_lock_scope(lock_id);
    accordin_shared::lock_stats::record_post_unlock(hold_end, lock_id);
}

unsafe fn mutex_ref<'a>(
    mutex: *mut mcs_tas_accordin_direct_mutex,
) -> Result<&'a mcs_tas_accordin_direct_mutex, libc::c_int> {
    if mutex.is_null() {
        Err(libc::EINVAL)
    } else {
        Ok(unsafe { &*mutex })
    }
}

unsafe fn cond_ref<'a>(
    cond: *mut mcs_tas_accordin_direct_cond,
) -> Result<&'a mcs_tas_accordin_direct_cond, libc::c_int> {
    if cond.is_null() {
        Err(libc::EINVAL)
    } else {
        Ok(unsafe { &*cond })
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn mcs_tas_accordin_direct_mutex_create() -> *mut mcs_tas_accordin_direct_mutex {
    Box::into_raw(Box::new(mcs_tas_accordin_direct_mutex {
        lock: DirectBackend::create_state(),
        lock_id: accordin_shared::admission::allocate_lock_class(),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_destroy(
    mutex: *mut mcs_tas_accordin_direct_mutex,
) -> libc::c_int {
    if mutex.is_null() {
        return libc::EINVAL;
    }

    drop(unsafe { Box::from_raw(mutex) });
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_lock(
    mutex: *mut mcs_tas_accordin_direct_mutex,
) -> libc::c_int {
    let mutex = match unsafe { mutex_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    lock_with_stats(&mutex.lock, mutex.lock_id);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_trylock(
    mutex: *mut mcs_tas_accordin_direct_mutex,
) -> libc::c_int {
    let mutex = match unsafe { mutex_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    if DirectBackend::try_lock(&mutex.lock) {
        accordin_shared::lock_stats::record_lock_acquired_for_lock(mutex.lock_id);
        0
    } else {
        libc::EBUSY
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_unlock(
    mutex: *mut mcs_tas_accordin_direct_mutex,
) -> libc::c_int {
    let mutex = match unsafe { mutex_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    unlock_with_stats(&mutex.lock, mutex.lock_id);
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn mcs_tas_accordin_direct_cond_create() -> *mut mcs_tas_accordin_direct_cond {
    Box::into_raw(Box::new(mcs_tas_accordin_direct_cond {
        state: CondState::new(),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_cond_destroy(
    cond: *mut mcs_tas_accordin_direct_cond,
) -> libc::c_int {
    if cond.is_null() {
        return libc::EINVAL;
    }

    drop(unsafe { Box::from_raw(cond) });
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_cond_wait(
    cond: *mut mcs_tas_accordin_direct_cond,
    mutex: *mut mcs_tas_accordin_direct_mutex,
) -> libc::c_int {
    let cond = match unsafe { cond_ref(cond) } {
        Ok(cond) => cond,
        Err(ret) => return ret,
    };
    let mutex = match unsafe { mutex_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    accordin_shared::condvar::wait(
        &cond.state,
        CondMutex::new(
            mutex.lock_id,
            <DirectBackend as MutexHookBackend>::USES_ADMISSION_SCOPE,
        ),
        || unlock_with_stats(&mutex.lock, mutex.lock_id),
        |relock| relock_after_cond_wait(&mutex.lock, mutex.lock_id, relock),
    );
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_cond_timedwait(
    cond: *mut mcs_tas_accordin_direct_cond,
    mutex: *mut mcs_tas_accordin_direct_mutex,
    abstime: *const libc::timespec,
) -> libc::c_int {
    if abstime.is_null() {
        return libc::EINVAL;
    }

    let cond = match unsafe { cond_ref(cond) } {
        Ok(cond) => cond,
        Err(ret) => return ret,
    };
    let mutex = match unsafe { mutex_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    accordin_shared::condvar::timedwait(
        &cond.state,
        CondMutex::new(
            mutex.lock_id,
            <DirectBackend as MutexHookBackend>::USES_ADMISSION_SCOPE,
        ),
        unsafe { &*abstime },
        || unlock_with_stats(&mutex.lock, mutex.lock_id),
        |relock| relock_after_cond_wait(&mutex.lock, mutex.lock_id, relock),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_cond_signal(
    cond: *mut mcs_tas_accordin_direct_cond,
) -> libc::c_int {
    let cond = match unsafe { cond_ref(cond) } {
        Ok(cond) => cond,
        Err(ret) => return ret,
    };

    cond.state.signal();
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_cond_broadcast(
    cond: *mut mcs_tas_accordin_direct_cond,
) -> libc::c_int {
    let cond = match unsafe { cond_ref(cond) } {
        Ok(cond) => cond,
        Err(ret) => return ret,
    };

    cond.state.broadcast();
    0
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{
        mcs_tas_accordin_direct_cond, mcs_tas_accordin_direct_cond_broadcast,
        mcs_tas_accordin_direct_cond_create, mcs_tas_accordin_direct_cond_destroy,
        mcs_tas_accordin_direct_cond_signal, mcs_tas_accordin_direct_cond_timedwait,
        mcs_tas_accordin_direct_cond_wait, mcs_tas_accordin_direct_mutex,
        mcs_tas_accordin_direct_mutex_create, mcs_tas_accordin_direct_mutex_destroy,
        mcs_tas_accordin_direct_mutex_lock, mcs_tas_accordin_direct_mutex_trylock,
        mcs_tas_accordin_direct_mutex_unlock,
    };

    /// The C API hands out raw pointers to shared state, which the test moves
    /// into the waiter threads the same way a C caller would.
    #[derive(Clone, Copy)]
    struct CondPair {
        cond: *mut mcs_tas_accordin_direct_cond,
        mutex: *mut mcs_tas_accordin_direct_mutex,
    }

    unsafe impl Send for CondPair {}
    unsafe impl Sync for CondPair {}

    impl CondPair {
        fn create() -> Self {
            let pair = Self {
                cond: mcs_tas_accordin_direct_cond_create(),
                mutex: mcs_tas_accordin_direct_mutex_create(),
            };
            assert!(!pair.cond.is_null());
            assert!(!pair.mutex.is_null());
            pair
        }

        fn destroy(self) {
            unsafe {
                assert_eq!(mcs_tas_accordin_direct_cond_destroy(self.cond), 0);
                assert_eq!(mcs_tas_accordin_direct_mutex_destroy(self.mutex), 0);
            }
        }
    }

    fn deadline_in(nanos: i64) -> libc::timespec {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
        let deadline_nsec = now.tv_nsec + nanos;
        libc::timespec {
            tv_sec: now.tv_sec + deadline_nsec / 1_000_000_000,
            tv_nsec: deadline_nsec % 1_000_000_000,
        }
    }

    #[test]
    fn direct_mutex_trylock_requires_unlock_before_reacquire() {
        let mutex = mcs_tas_accordin_direct_mutex_create();
        assert!(!mutex.is_null());

        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_trylock(mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_mutex_trylock(mutex), libc::EBUSY);
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_mutex_destroy(mutex), 0);
        }
    }

    #[test]
    fn direct_cond_create_and_destroy_round_trip() {
        let cond = mcs_tas_accordin_direct_cond_create();
        assert!(!cond.is_null());
        unsafe {
            assert_eq!(mcs_tas_accordin_direct_cond_signal(cond), 0);
            assert_eq!(mcs_tas_accordin_direct_cond_broadcast(cond), 0);
            assert_eq!(mcs_tas_accordin_direct_cond_destroy(cond), 0);
        }
    }

    #[test]
    fn direct_cond_rejects_null_arguments() {
        let pair = CondPair::create();
        let abstime = deadline_in(1_000_000);

        unsafe {
            assert_eq!(
                mcs_tas_accordin_direct_cond_destroy(std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_cond_signal(std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_cond_broadcast(std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_cond_wait(std::ptr::null_mut(), pair.mutex),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_cond_wait(pair.cond, std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_cond_timedwait(pair.cond, pair.mutex, std::ptr::null()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_cond_timedwait(std::ptr::null_mut(), pair.mutex, &abstime),
                libc::EINVAL
            );
        }

        pair.destroy();
    }

    #[test]
    fn direct_cond_wait_returns_to_a_signalled_predicate() {
        let pair = CondPair::create();
        let payload = Arc::new(AtomicU32::new(0));

        let waiter = {
            let payload = Arc::clone(&payload);
            std::thread::spawn(move || unsafe {
                let pair = pair;
                assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
                while payload.load(Ordering::Acquire) == 0 {
                    assert_eq!(mcs_tas_accordin_direct_cond_wait(pair.cond, pair.mutex), 0);
                }
                let observed = payload.load(Ordering::Acquire);
                assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
                observed
            })
        };

        std::thread::sleep(std::time::Duration::from_millis(50));
        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
            payload.store(9, Ordering::Release);
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_cond_signal(pair.cond), 0);
        }

        assert_eq!(waiter.join().expect("the waiter should finish"), 9);
        pair.destroy();
    }

    #[test]
    fn direct_cond_broadcast_releases_every_waiter() {
        let pair = CondPair::create();
        let payload = Arc::new(AtomicU32::new(0));

        let waiters = (0..4)
            .map(|_| {
                let payload = Arc::clone(&payload);
                std::thread::spawn(move || unsafe {
                    let pair = pair;
                    assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
                    while payload.load(Ordering::Acquire) == 0 {
                        assert_eq!(mcs_tas_accordin_direct_cond_wait(pair.cond, pair.mutex), 0);
                    }
                    assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
                })
            })
            .collect::<Vec<_>>();

        std::thread::sleep(std::time::Duration::from_millis(50));
        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
            payload.store(1, Ordering::Release);
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_cond_broadcast(pair.cond), 0);
        }

        for waiter in waiters {
            waiter.join().expect("every waiter should finish");
        }
        pair.destroy();
    }

    #[test]
    fn direct_cond_timedwait_reports_the_timeout_and_holds_the_mutex_again() {
        let pair = CondPair::create();
        let abstime = deadline_in(1_000_000);

        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
            assert_eq!(
                mcs_tas_accordin_direct_cond_timedwait(pair.cond, pair.mutex, &abstime),
                libc::ETIMEDOUT
            );
            assert_eq!(
                mcs_tas_accordin_direct_mutex_trylock(pair.mutex),
                libc::EBUSY
            );
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
        }

        pair.destroy();
    }
}
