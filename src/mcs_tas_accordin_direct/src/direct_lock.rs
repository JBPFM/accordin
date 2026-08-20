// SPDX-License-Identifier: GPL-2.0-only

use std::cell::{Cell, UnsafeCell};

use accordin_shared::condvar::{CondMutex, CondRelock, CondStaging, CondState};
use accordin_shared::mutex_hook::{
    AcquireMode, LockBackendAdapter, MutexHookBackend, acquire_mode_for_cond_relock,
    lock_scope_with_stats,
};
use accordin_shared::writer_event::{WriterEvent, checked_ref};

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
    /// Waiters a cond broadcast parked on this mutex, released one per unlock.
    /// The staging is an owned handle to a shared block, so a cond still bound
    /// to this mutex after it is destroyed reads a retired block rather than
    /// freed memory.
    staging: CondStaging,
}

impl mcs_tas_accordin_direct_mutex {
    fn new() -> Self {
        Self {
            lock: DirectBackend::create_state(),
            lock_id: accordin_shared::admission::allocate_lock_class(),
            staging: CondStaging::new(),
        }
    }

    #[inline(always)]
    fn cond_mutex(&self) -> CondMutex {
        CondMutex::new(
            self.lock_id,
            <DirectBackend as MutexHookBackend>::USES_ADMISSION_SCOPE,
            self.staging.handle(),
        )
    }
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

#[inline(always)]
fn lock_with_stats(mutex: &mcs_tas_accordin_direct_mutex) {
    let scope = accordin_shared::admission::begin_lock_scope(mutex.lock_id);
    acquire_in_scope(mutex, scope, AcquireMode::Normal);
}

/// Names the mutex's parts for the shared acquire bracket.
#[inline(always)]
fn acquire_in_scope(
    mutex: &mcs_tas_accordin_direct_mutex,
    scope: accordin_shared::admission::LockAdmissionScope,
    mode: AcquireMode,
) {
    lock_scope_with_stats::<DirectBackend>(&mutex.lock, mutex.staging.handle(), scope, mode);
}

/// Reacquires the cond mutex after a wait, in the mode the wait protocol
/// decided.
#[inline(always)]
fn relock_after_cond_wait(mutex: &mcs_tas_accordin_direct_mutex, relock: CondRelock) {
    let scope = accordin_shared::admission::begin_lock_scope(mutex.lock_id);
    acquire_in_scope(mutex, scope, acquire_mode_for_cond_relock(relock, scope));
}

/// Releases the lock and then hands the next staged cond waiter to the lock's
/// own service rate: the release comes first so the waiter finds the lock free,
/// and the phase bookkeeping comes before it so the drain never runs inside the
/// critical section.
///
/// The staging handle is read while the lock is still held. Past the release
/// the mutex may already have been destroyed by the thread that took it next,
/// and the drain then reads a word of a freed block rather than following a
/// pointer out of freed state.
#[inline(always)]
fn unlock_with_stats(mutex: &mcs_tas_accordin_direct_mutex) {
    let hold_end = accordin_shared::lock_stats::record_hold_end_sample();
    let staging = mutex.staging.handle();
    let lock_id = mutex.lock_id;
    staging.clear_hold();
    DirectBackend::unlock(&mutex.lock);
    accordin_shared::admission::finish_lock_scope(lock_id);
    accordin_shared::lock_stats::record_post_unlock(hold_end, lock_id);
    staging.drain_one();
}

#[unsafe(no_mangle)]
pub extern "C" fn mcs_tas_accordin_direct_mutex_create() -> *mut mcs_tas_accordin_direct_mutex {
    Box::into_raw(Box::new(mcs_tas_accordin_direct_mutex::new()))
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
    let mutex = match unsafe { checked_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    lock_with_stats(mutex);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_trylock(
    mutex: *mut mcs_tas_accordin_direct_mutex,
) -> libc::c_int {
    let mutex = match unsafe { checked_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    if DirectBackend::try_lock(&mutex.lock) {
        accordin_shared::lock_stats::record_lock_acquired_for_lock(mutex.lock_id);
        mutex.staging.handle().publish_hold();
        0
    } else {
        libc::EBUSY
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_mutex_unlock(
    mutex: *mut mcs_tas_accordin_direct_mutex,
) -> libc::c_int {
    let mutex = match unsafe { checked_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    unlock_with_stats(mutex);
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
    let cond = match unsafe { checked_ref(cond) } {
        Ok(cond) => cond,
        Err(ret) => return ret,
    };
    let mutex = match unsafe { checked_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    accordin_shared::condvar::wait(
        &cond.state,
        mutex.cond_mutex(),
        || unlock_with_stats(mutex),
        |relock| relock_after_cond_wait(mutex, relock),
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

    let cond = match unsafe { checked_ref(cond) } {
        Ok(cond) => cond,
        Err(ret) => return ret,
    };
    let mutex = match unsafe { checked_ref(mutex) } {
        Ok(mutex) => mutex,
        Err(ret) => return ret,
    };

    ensure_registered();
    accordin_shared::condvar::timedwait(
        &cond.state,
        mutex.cond_mutex(),
        unsafe { &*abstime },
        || unlock_with_stats(mutex),
        |relock| relock_after_cond_wait(mutex, relock),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_cond_signal(
    cond: *mut mcs_tas_accordin_direct_cond,
) -> libc::c_int {
    let cond = match unsafe { checked_ref(cond) } {
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
    let cond = match unsafe { checked_ref(cond) } {
        Ok(cond) => cond,
        Err(ret) => return ret,
    };

    cond.state.broadcast();
    0
}

/// Re-acquires the mutex for a waiter a writer-event wait made leader.
#[inline(always)]
fn relock_writer_event(mutex: &mcs_tas_accordin_direct_mutex, event: &WriterEvent) {
    let scope = accordin_shared::admission::begin_lock_scope(mutex.lock_id);
    let mode = AcquireMode::for_event_relock(event.take_relock_mode());
    acquire_in_scope(mutex, scope, mode);
}

accordin_shared::export_writer_event_abi! {
    /// The writer event as the C API hands it out: the wake protocol plus the
    /// binding of the mutex it belongs to.
    ///
    /// The whole object lives in caller-owned storage. The waiters this exists
    /// for are created per operation, and an allocation per operation is the
    /// cost the protocol is there to remove; for the same reason the binding is
    /// resolved once per mutex rather than once per waiter, so initializing one
    /// is a plain struct write.
    #[allow(non_camel_case_types)]
    pub struct mcs_tas_accordin_direct_writer_event {
        binding: mcs_tas_accordin_direct_mutex
    }
    pointer = mcs_tas_accordin_direct_writer_event,
    admission_scoped = <DirectBackend as MutexHookBackend>::USES_ADMISSION_SCOPE,
    relock = relock_writer_event,
    size = mcs_tas_accordin_direct_writer_event_size,
    align = mcs_tas_accordin_direct_writer_event_align,
    init = mcs_tas_accordin_direct_writer_event_init,
    arm = mcs_tas_accordin_direct_writer_event_arm,
    wait = mcs_tas_accordin_direct_writer_event_wait,
    relock_entry = mcs_tas_accordin_direct_writer_event_relock,
    post_completed = mcs_tas_accordin_direct_writer_event_post_completed,
    post_leader = mcs_tas_accordin_direct_writer_event_post_leader,
}

/// Resolves a mutex to the binding its events are initialized from, which is
/// the whole of what an event needs to know about it.
///
/// The answer never changes for a given mutex, so it is asked once and the
/// caller keeps it for the mutex's lifetime; the binding stays valid for
/// exactly that long.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mcs_tas_accordin_direct_writer_event_bind(
    mutex: *mut mcs_tas_accordin_direct_mutex,
    out_binding: *mut *mut libc::c_void,
) -> libc::c_int {
    if mutex.is_null() || out_binding.is_null() {
        return libc::EINVAL;
    }

    ensure_registered();
    unsafe { out_binding.write(mutex.cast::<libc::c_void>()) };
    0
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use accordin_shared::test_support::{await_progress, deadline_in, waking_thread};

    use super::{
        mcs_tas_accordin_direct_cond, mcs_tas_accordin_direct_cond_broadcast,
        mcs_tas_accordin_direct_cond_create, mcs_tas_accordin_direct_cond_destroy,
        mcs_tas_accordin_direct_cond_signal, mcs_tas_accordin_direct_cond_timedwait,
        mcs_tas_accordin_direct_cond_wait, mcs_tas_accordin_direct_mutex,
        mcs_tas_accordin_direct_mutex_create, mcs_tas_accordin_direct_mutex_destroy,
        mcs_tas_accordin_direct_mutex_lock, mcs_tas_accordin_direct_mutex_trylock,
        mcs_tas_accordin_direct_mutex_unlock, mcs_tas_accordin_direct_writer_event,
        mcs_tas_accordin_direct_writer_event_arm, mcs_tas_accordin_direct_writer_event_bind,
        mcs_tas_accordin_direct_writer_event_init,
        mcs_tas_accordin_direct_writer_event_post_completed,
        mcs_tas_accordin_direct_writer_event_post_leader,
        mcs_tas_accordin_direct_writer_event_relock, mcs_tas_accordin_direct_writer_event_size,
        mcs_tas_accordin_direct_writer_event_wait,
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

    /// A broadcast has to release every waiter it finds, whether the waking
    /// thread moves them onto the mutex's stage or wakes them where they sleep.
    ///
    /// Requeueing is what makes the release indirect: the waiters end up on a
    /// lock they are then drained off one unlock at a time, so the count is
    /// raised and the broadcaster settles longer before it fires to give every
    /// waiter time to reach its sleep.
    fn broadcast_releases_every_waiter(requeue: bool, waiters: u32, settle: Duration) {
        let pair = CondPair::create();
        let payload = Arc::new(AtomicU32::new(0));
        let released = Arc::new(AtomicU32::new(0));

        let threads = (0..waiters)
            .map(|_| {
                let payload = Arc::clone(&payload);
                let released = Arc::clone(&released);
                std::thread::spawn(move || unsafe {
                    let pair = pair;
                    assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
                    while payload.load(Ordering::Acquire) == 0 {
                        assert_eq!(mcs_tas_accordin_direct_cond_wait(pair.cond, pair.mutex), 0);
                    }
                    released.fetch_add(1, Ordering::Release);
                    assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
                })
            })
            .collect::<Vec<_>>();

        let _guard = waking_thread(requeue);
        std::thread::sleep(settle);
        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
            payload.store(1, Ordering::Release);
            assert_eq!(mcs_tas_accordin_direct_cond_broadcast(pair.cond), 0);
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
        }

        await_progress(&released, waiters, "the broadcast");
        for thread in threads {
            thread.join().expect("every waiter should finish");
        }
        pair.destroy();
    }

    #[test]
    fn direct_cond_broadcast_releases_every_waiter() {
        broadcast_releases_every_waiter(false, 4, Duration::from_millis(50));
    }

    #[test]
    fn direct_cond_broadcast_releases_every_waiter_with_requeue() {
        broadcast_releases_every_waiter(true, 8, Duration::from_millis(100));
    }

    /// The hand-off is a wake-one, which the staging switch leaves on the plain
    /// wake, so the rounds have to keep coming with the switch on.
    #[test]
    fn direct_cond_ping_pong_makes_progress_with_the_switch_on() {
        const ROUNDS: u32 = 100;

        let pair = CondPair::create();
        let payload = Arc::new(AtomicU32::new(0));
        let rounds = Arc::new(AtomicU32::new(0));

        let consumer = {
            let payload = Arc::clone(&payload);
            let rounds = Arc::clone(&rounds);
            std::thread::spawn(move || unsafe {
                let pair = pair;
                let _guard = waking_thread(true);

                for _ in 0..ROUNDS {
                    assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
                    while payload.load(Ordering::Acquire) == 0 {
                        assert_eq!(mcs_tas_accordin_direct_cond_wait(pair.cond, pair.mutex), 0);
                    }
                    payload.store(0, Ordering::Release);
                    rounds.fetch_add(1, Ordering::Release);
                    assert_eq!(mcs_tas_accordin_direct_cond_signal(pair.cond), 0);
                    assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
                }
            })
        };

        let _guard = waking_thread(true);
        for round in 0..ROUNDS {
            unsafe {
                assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
                while payload.load(Ordering::Acquire) != 0 {
                    assert_eq!(mcs_tas_accordin_direct_cond_wait(pair.cond, pair.mutex), 0);
                }
                payload.store(1, Ordering::Release);
                assert_eq!(mcs_tas_accordin_direct_cond_signal(pair.cond), 0);
                assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
            }
            await_progress(&rounds, round + 1, "the ping-pong");
        }

        consumer.join().expect("the consumer should finish");
        assert_eq!(rounds.load(Ordering::Acquire), ROUNDS);
        pair.destroy();
    }

    /// A broadcaster that holds nothing has no unlock coming to release the
    /// waiter it staged, so the broadcast itself has to start the chain.
    #[test]
    fn direct_cond_broadcast_without_the_mutex_releases_a_staged_waiter() {
        let pair = CondPair::create();
        let payload = Arc::new(AtomicU32::new(0));
        let released = Arc::new(AtomicU32::new(0));

        let waiter = {
            let payload = Arc::clone(&payload);
            let released = Arc::clone(&released);
            std::thread::spawn(move || unsafe {
                let pair = pair;
                assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
                while payload.load(Ordering::Acquire) == 0 {
                    assert_eq!(mcs_tas_accordin_direct_cond_wait(pair.cond, pair.mutex), 0);
                }
                assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
                released.fetch_add(1, Ordering::Release);
            })
        };

        let _guard = waking_thread(true);
        std::thread::sleep(std::time::Duration::from_millis(100));
        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
            payload.store(1, Ordering::Release);
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_cond_broadcast(pair.cond), 0);
        }

        await_progress(&released, 1, "the stranded waiter");
        waiter.join().expect("the waiter should finish");
        pair.destroy();
    }

    /// POSIX lets the mutex be destroyed before the cond, and lets a cond with
    /// no waiter be woken. The cond keeps its staging alive across the destroy,
    /// so the wakeup reads a retired block rather than the memory the mutex was
    /// freed from.
    #[test]
    fn direct_cond_wakeup_after_the_mutex_is_destroyed_is_safe() {
        let pair = CondPair::create();
        let payload = Arc::new(AtomicU32::new(0));

        let waiter = {
            let payload = Arc::clone(&payload);
            std::thread::spawn(move || unsafe {
                let pair = pair;
                let _guard = waking_thread(true);
                assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
                while payload.load(Ordering::Acquire) == 0 {
                    assert_eq!(mcs_tas_accordin_direct_cond_wait(pair.cond, pair.mutex), 0);
                }
                assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
            })
        };

        let _guard = waking_thread(true);
        std::thread::sleep(std::time::Duration::from_millis(50));
        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(pair.mutex), 0);
            payload.store(1, Ordering::Release);
            // A broadcast, so the staging carries a waiter before the destroy.
            assert_eq!(mcs_tas_accordin_direct_cond_broadcast(pair.cond), 0);
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(pair.mutex), 0);
        }
        waiter.join().expect("the waiter should finish");

        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_destroy(pair.mutex), 0);
            assert_eq!(mcs_tas_accordin_direct_cond_signal(pair.cond), 0);
            assert_eq!(mcs_tas_accordin_direct_cond_broadcast(pair.cond), 0);
            assert_eq!(mcs_tas_accordin_direct_cond_destroy(pair.cond), 0);
        }
    }

    /// The two reasons a queue wakes a waiter for, driven through the C API the
    /// way a leader/follower write queue drives them: one follower is released
    /// with its work done and returns without the mutex, the other is made the
    /// new head and comes back holding it.
    #[test]
    fn direct_writer_event_separates_the_completed_and_leader_wakes() {
        #[derive(Clone, Copy)]
        struct EventArgs {
            mutex: *mut mcs_tas_accordin_direct_mutex,
            event: *mut mcs_tas_accordin_direct_writer_event,
        }

        unsafe impl Send for EventArgs {}

        fn wait_for_a_reason(args: EventArgs) -> libc::c_int {
            unsafe {
                assert_eq!(mcs_tas_accordin_direct_mutex_lock(args.mutex), 0);
                assert_eq!(mcs_tas_accordin_direct_writer_event_arm(args.event), 0);
                assert_eq!(mcs_tas_accordin_direct_mutex_unlock(args.mutex), 0);
                let reason = mcs_tas_accordin_direct_writer_event_wait(args.event);
                if reason == 1 {
                    assert_eq!(mcs_tas_accordin_direct_writer_event_relock(args.event), 0);
                    assert_eq!(mcs_tas_accordin_direct_mutex_unlock(args.mutex), 0);
                }
                reason
            }
        }

        let mutex = mcs_tas_accordin_direct_mutex_create();
        assert!(!mutex.is_null());
        assert!(mcs_tas_accordin_direct_writer_event_size() >= std::mem::size_of::<usize>());

        let mut binding = std::ptr::null_mut();
        unsafe {
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_bind(mutex, &mut binding),
                0
            );
        }
        assert!(!binding.is_null());

        let mut completed = std::mem::MaybeUninit::<mcs_tas_accordin_direct_writer_event>::uninit();
        let mut leader = std::mem::MaybeUninit::<mcs_tas_accordin_direct_writer_event>::uninit();
        let completed_args = EventArgs {
            mutex,
            event: completed.as_mut_ptr(),
        };
        let leader_args = EventArgs {
            mutex,
            event: leader.as_mut_ptr(),
        };

        unsafe {
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_init(completed_args.event, binding),
                0
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_init(leader_args.event, binding),
                0
            );
        }

        let waiters = [completed_args, leader_args]
            .map(|args| std::thread::spawn(move || wait_for_a_reason(args)));

        std::thread::sleep(std::time::Duration::from_millis(100));
        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_lock(mutex), 0);
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_post_completed(completed_args.event),
                0
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_post_leader(leader_args.event),
                0
            );
            assert_eq!(mcs_tas_accordin_direct_mutex_unlock(mutex), 0);
        }

        let [completed_reason, leader_reason] =
            waiters.map(|waiter| waiter.join().expect("every waiter should finish"));
        assert_eq!(completed_reason, 0);
        assert_eq!(leader_reason, 1);

        unsafe {
            assert_eq!(mcs_tas_accordin_direct_mutex_destroy(mutex), 0);
        }
    }

    #[test]
    fn direct_writer_event_rejects_null_arguments() {
        let mutex = mcs_tas_accordin_direct_mutex_create();
        let mut event = std::mem::MaybeUninit::<mcs_tas_accordin_direct_writer_event>::uninit();
        let mut binding = std::ptr::null_mut();

        unsafe {
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_bind(std::ptr::null_mut(), &mut binding),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_bind(mutex, std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_bind(mutex, &mut binding),
                0
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_init(std::ptr::null_mut(), binding),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_init(event.as_mut_ptr(), std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_arm(std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_wait(std::ptr::null_mut()),
                -1
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_post_completed(std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(
                mcs_tas_accordin_direct_writer_event_post_leader(std::ptr::null_mut()),
                libc::EINVAL
            );
            assert_eq!(mcs_tas_accordin_direct_mutex_destroy(mutex), 0);
        }
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
