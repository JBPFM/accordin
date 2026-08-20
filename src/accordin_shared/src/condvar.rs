// SPDX-License-Identifier: GPL-2.0-only

//! Futex condition variable used by the hooked `pthread_cond_t` and by the
//! direct lock API.
//!
//! The module owns the wait protocol: the waiter accounting, the sleep loop,
//! the sleep bracket the class stats read, and the admission state a waiter
//! publishes while its mutex is released. The caller owns the mutex itself and
//! supplies the two operations the protocol brackets its sleep with, an unlock
//! and a re-acquisition whose admission mode the protocol decides.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::admission;
use crate::lock_stats::{record_cv_sleep_end, record_cv_sleep_start};
use crate::mutex_hook::{record_cv_hint_published, record_cv_route_relock};

const DISABLE_CV_ADMISSION_HINT_ENV: &str = "ACCORDIN_DISABLE_CV_ADMISSION_HINT";
const CV_ROUTE_ENV: &str = "ACCORDIN_CV_ROUTE";

const FUTEX_WAIT_PRIVATE: libc::c_int = 128;
const FUTEX_WAKE_PRIVATE: libc::c_int = 129;
const FUTEX_WAIT_BITSET_PRIVATE_REALTIME: libc::c_int = 9 | 128 | 256;
const FUTEX_BITSET_MATCH_ANY: libc::c_uint = libc::c_uint::MAX;

#[cfg(test)]
thread_local! {
    static ROUTE_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[doc(hidden)]
pub fn cv_admission_hint_enabled() -> bool {
    static CV_ADMISSION_HINT_ENABLED: OnceLock<bool> = OnceLock::new();
    *CV_ADMISSION_HINT_ENABLED.get_or_init(|| !crate::env::env_flag(DISABLE_CV_ADMISSION_HINT_ENV))
}

fn cv_route_env_enabled() -> bool {
    static CV_ROUTE: OnceLock<bool> = OnceLock::new();
    *CV_ROUTE.get_or_init(|| crate::env::env_flag(CV_ROUTE_ENV))
}

/// Whether a waiter hands its wakeup to the scheduler through the cv-sleep
/// state instead of the cond-reacquire hint. The routing is forced per thread
/// in tests so that a test never changes what concurrently running tests see.
#[cfg(test)]
#[inline(always)]
fn cv_route_enabled() -> bool {
    ROUTE_FORCED
        .with(|forced| forced.get())
        .unwrap_or_else(cv_route_env_enabled)
}

#[cfg(not(test))]
#[inline(always)]
fn cv_route_enabled() -> bool {
    cv_route_env_enabled()
}

#[cfg(test)]
fn force_cv_route_for_test(forced: Option<bool>) {
    ROUTE_FORCED.with(|route| route.set(forced));
}

/// The futex state behind one condition variable: `seq` is the word waiters
/// block on, and `waiters` counts the sleepers a signal may hand a wakeup to.
pub struct CondState {
    seq: AtomicU32,
    waiters: AtomicU32,
}

impl Default for CondState {
    fn default() -> Self {
        Self::new()
    }
}

impl CondState {
    pub const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            waiters: AtomicU32::new(0),
        }
    }

    /// Wakes one waiter, if any is registered.
    pub fn signal(&self) {
        if self.take_waiter() {
            unsafe { futex_wake(&self.seq, 1) };
        }
    }

    /// Wakes every registered waiter with a single sequence bump.
    pub fn broadcast(&self) {
        if self.waiters.swap(0, Ordering::AcqRel) != 0 {
            self.seq.fetch_add(1, Ordering::Release);
            unsafe { futex_wake(&self.seq, libc::c_int::MAX) };
        }
    }

    #[inline(always)]
    fn take_waiter(&self) -> bool {
        let mut current = self.waiters.load(Ordering::Acquire);
        while current != 0 {
            match self.waiters.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.seq.fetch_add(1, Ordering::Release);
                    return true;
                }
                Err(next) => current = next,
            }
        }
        false
    }

    #[inline(always)]
    fn cancel_waiter(&self) -> bool {
        let mut current = self.waiters.load(Ordering::Acquire);
        while current != 0 {
            match self.waiters.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => current = next,
            }
        }
        false
    }

    /// Withdraws the registration of a waiter whose deadline expired, and
    /// reports what the wait returns.
    ///
    /// A registration names no particular waiter, so the withdrawal is only
    /// this waiter's while no signal ran between the deadline and it: one that
    /// did consumed this waiter's registration, and the count now stands for a
    /// waiter that registered afterwards. Retiring that one would leave it
    /// asleep with nothing left to wake it, so the sequence is re-read and a
    /// wakeup handed on instead. The wait then reports success, which POSIX
    /// permits: a timed wait may always return early as a spurious wake, and
    /// the caller re-checks its predicate under the re-acquired mutex.
    #[inline(always)]
    fn retire_expired_waiter(&self, seq: u32) -> libc::c_int {
        if !self.cancel_waiter() {
            // A signal or a broadcast already took the registration, so the
            // wait ends as the wakeup it was given rather than as a timeout.
            return 0;
        }

        if self.seq.load(Ordering::Acquire) == seq {
            return libc::ETIMEDOUT;
        }

        self.seq.fetch_add(1, Ordering::Release);
        unsafe { futex_wake(&self.seq, 1) };
        0
    }
}

/// The mutex a cond wait releases across its sleep: the lock class it belongs
/// to, and whether the backend runs that class through admission at all.
#[derive(Clone, Copy)]
pub struct CondMutex {
    lock_id: u32,
    admission_scoped: bool,
}

impl CondMutex {
    pub const fn new(lock_id: u32, admission_scoped: bool) -> Self {
        Self {
            lock_id,
            admission_scoped,
        }
    }
}

/// How the waiter should re-acquire the cond mutex once its wait ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CondRelock {
    /// Ordinary contended acquisition: the re-acquisition asks admission for a
    /// slot itself.
    Normal,
    /// A cond-reacquire hint may be waiting in the user word. Taking it enters
    /// the lock without asking admission again; otherwise the acquisition falls
    /// back to `Normal`.
    TakeHint,
    /// The wakeup was routed by the scheduler, which granted the admission
    /// token as it enqueued the waiter, so the re-acquisition carries the
    /// decision already.
    AlreadyAdmitted,
}

/// What the waiter published before blocking, which decides how it re-acquires
/// the cond mutex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SleepPublication {
    /// Nothing was published: the class stays outside admission, or the state
    /// belongs to a lock this thread still holds.
    None,
    /// The user word carries the cond-reacquire hint.
    Hint,
    /// The user word carries the cv-sleep state, so the scheduler routes the
    /// wakeup itself.
    Routed,
}

/// Releases `mutex`, blocks until the condition variable is signalled, and
/// re-acquires it through `relock` in the mode the wait protocol decided.
#[inline]
pub fn wait<U, R>(cond: &CondState, mutex: CondMutex, unlock: U, relock: R)
where
    U: FnOnce(),
    R: FnOnce(CondRelock),
{
    let seq = cond.seq.load(Ordering::Acquire);
    cond.waiters.fetch_add(1, Ordering::AcqRel);
    unlock();
    let publication = publish_sleep_state(mutex);
    // The mutex is released across the sleep, so the sleep lands in the
    // released class's unlock-to-acquire gap. Bracketing it here covers both
    // relock modes, which are chosen only after the sleep ends.
    let cv_sleep_start = record_cv_sleep_start();
    let mut slept = false;
    while cond.seq.load(Ordering::Acquire) == seq {
        let rc = unsafe { futex_wait(&cond.seq, seq) };
        slept |= rc == 0 || rc == libc::EINTR;
    }
    retire_sleep_state(publication, slept);
    record_cv_sleep_end(cv_sleep_start);
    relock(relock_mode(publication, slept));
}

/// Like `wait`, but gives up at `abstime` and then reports `ETIMEDOUT`.
#[inline]
pub fn timedwait<U, R>(
    cond: &CondState,
    mutex: CondMutex,
    abstime: &libc::timespec,
    unlock: U,
    relock: R,
) -> libc::c_int
where
    U: FnOnce(),
    R: FnOnce(CondRelock),
{
    let seq = cond.seq.load(Ordering::Acquire);
    let mut ret = 0;
    cond.waiters.fetch_add(1, Ordering::AcqRel);
    unlock();
    let publication = publish_sleep_state(mutex);
    let cv_sleep_start = record_cv_sleep_start();
    let mut slept = false;
    while cond.seq.load(Ordering::Acquire) == seq {
        let rc = unsafe { futex_wait_until_realtime(&cond.seq, seq, abstime) };
        slept |= rc == 0 || rc == libc::EINTR || rc == libc::ETIMEDOUT;
        if rc == libc::ETIMEDOUT {
            if cond.seq.load(Ordering::Acquire) == seq {
                ret = cond.retire_expired_waiter(seq);
            }
            break;
        }
    }
    retire_sleep_state(publication, slept);
    record_cv_sleep_end(cv_sleep_start);
    relock(relock_mode(publication, slept));
    ret
}

/// Publishes the state the waiter sleeps under. With routing on, the user word
/// names the lock to re-acquire and carries the cv-sleep flag, which is what
/// the scheduler routes the wakeup from; the cond-reacquire hint is published
/// instead whenever that state cannot be taken, so a waiter that still holds a
/// managed lock keeps the behaviour it has with routing off.
#[inline]
fn publish_sleep_state(mutex: CondMutex) -> SleepPublication {
    if !mutex.admission_scoped {
        return SleepPublication::None;
    }

    if cv_route_enabled() && admission::set_cv_sleep_for_lock(mutex.lock_id) {
        return SleepPublication::Routed;
    }

    if cv_admission_hint_enabled()
        && admission::mark_cond_reacquire_pending_for_cond_mutex(mutex.lock_id)
    {
        record_cv_hint_published();
        return SleepPublication::Hint;
    }

    SleepPublication::None
}

/// Retires the cv-sleep state as soon as the wait stops blocking: from here on
/// the thread is runnable and the scheduler must no longer read it as sleeping.
///
/// What replaces it is the pending state of the same class, so the re-acquisition
/// that follows is described as the contention it is for its whole duration; the
/// scheduler reads a bare class as a thread that has finished with the lock.
#[inline]
fn retire_sleep_state(publication: SleepPublication, slept: bool) {
    if publication == SleepPublication::Routed {
        admission::retire_cv_sleep_to_pending(slept);
    }
}

/// A waiter that never blocked was never routed and never had a hint answered,
/// so it re-acquires as an ordinary contender.
#[inline]
fn relock_mode(publication: SleepPublication, slept: bool) -> CondRelock {
    if !slept {
        return CondRelock::Normal;
    }

    match publication {
        SleepPublication::Routed => {
            record_cv_route_relock();
            CondRelock::AlreadyAdmitted
        }
        SleepPublication::Hint | SleepPublication::None => CondRelock::TakeHint,
    }
}

/// Returns 0 when the wait completed, otherwise the errno reported by the
/// syscall, so callers classify the outcome without reading thread-local errno
/// themselves.
#[inline(always)]
unsafe fn futex_wait(addr: *const AtomicU32, expected: u32) -> libc::c_int {
    unsafe {
        let ret = libc::syscall(
            libc::SYS_futex,
            addr as *const u32,
            FUTEX_WAIT_PRIVATE,
            expected,
            std::ptr::null::<libc::timespec>(),
        );
        if ret == 0 {
            0
        } else {
            *libc::__errno_location()
        }
    }
}

#[inline(always)]
unsafe fn futex_wait_until_realtime(
    addr: *const AtomicU32,
    expected: u32,
    abstime: *const libc::timespec,
) -> libc::c_int {
    unsafe {
        let ret = libc::syscall(
            libc::SYS_futex,
            addr as *const u32,
            FUTEX_WAIT_BITSET_PRIVATE_REALTIME,
            expected,
            abstime,
            std::ptr::null::<libc::c_void>(),
            FUTEX_BITSET_MATCH_ANY,
        );
        if ret == 0 {
            0
        } else {
            *libc::__errno_location()
        }
    }
}

#[inline(always)]
unsafe fn futex_wake(addr: *const AtomicU32, count: libc::c_int) -> libc::c_long {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr as *const u32,
            FUTEX_WAKE_PRIVATE,
            count,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use super::{
        CondMutex, CondRelock, CondState, SleepPublication, force_cv_route_for_test,
        publish_sleep_state, relock_mode, retire_sleep_state, timedwait, wait,
    };
    use crate::admission;

    const HINT_LOCK_ID: u32 = 4;
    const ROUTE_LOCK_ID: u32 = 5;
    const SMOKE_LOCK_ID: u32 = 6;

    struct RouteGuard;

    impl Drop for RouteGuard {
        fn drop(&mut self) {
            force_cv_route_for_test(None);
            admission::reset_thread_depth_for_test();
            admission::reset_state();
        }
    }

    /// Starts a wait from the state a hooked unlock leaves behind: the word
    /// names the released class and no scope stays held.
    fn released_cond_mutex(lock_id: u32, route: bool) -> RouteGuard {
        force_cv_route_for_test(Some(route));
        admission::reset_thread_depth_for_test();
        admission::reset_state();

        let scope = admission::begin_lock_scope(lock_id);
        admission::mark_critical_section_entered_for_scope(scope);
        admission::finish_lock_scope(lock_id);
        RouteGuard
    }

    fn word_lock_id() -> u32 {
        admission::word_for_test() >> admission::USER_ADMISSION_LOCK_ID_SHIFT
    }

    #[test]
    fn routing_off_publishes_the_cond_reacquire_hint() {
        let _guard = released_cond_mutex(HINT_LOCK_ID, false);

        assert_eq!(
            publish_sleep_state(CondMutex::new(HINT_LOCK_ID, true)),
            SleepPublication::Hint
        );
        assert!(!admission::cv_sleep_set_for_test(HINT_LOCK_ID));

        let scope = admission::begin_lock_scope(HINT_LOCK_ID);
        assert!(admission::take_cond_reacquire_pending_for_scope(scope));
        admission::finish_lock_scope(HINT_LOCK_ID);
    }

    #[test]
    fn routing_on_publishes_cv_sleep_and_retires_it_after_the_wait() {
        let _guard = released_cond_mutex(ROUTE_LOCK_ID, true);

        let publication = publish_sleep_state(CondMutex::new(ROUTE_LOCK_ID, true));
        assert_eq!(publication, SleepPublication::Routed);
        assert!(admission::cv_sleep_set_for_test(ROUTE_LOCK_ID));
        assert_eq!(word_lock_id(), admission::class_of(ROUTE_LOCK_ID));

        let scope = admission::begin_lock_scope(ROUTE_LOCK_ID);
        assert!(
            !admission::take_cond_reacquire_pending_for_scope(scope),
            "the routed state must not read as a cond-reacquire hint"
        );
        admission::finish_lock_scope(ROUTE_LOCK_ID);

        retire_sleep_state(publication, true);
        assert!(!admission::cv_sleep_set_for_test(ROUTE_LOCK_ID));
        assert!(
            admission::slow_path_pending_set_for_test(ROUTE_LOCK_ID),
            "the re-acquisition contends as a pending waiter for the class"
        );
        assert_eq!(word_lock_id(), admission::class_of(ROUTE_LOCK_ID));
    }

    #[test]
    fn a_held_lock_keeps_the_routing_off_publication() {
        let _guard = released_cond_mutex(ROUTE_LOCK_ID, true);

        let outer = admission::begin_lock_scope(ROUTE_LOCK_ID);
        admission::mark_critical_section_entered_for_scope(outer);
        admission::begin_lock_scope(HINT_LOCK_ID);

        assert_eq!(
            publish_sleep_state(CondMutex::new(HINT_LOCK_ID, true)),
            SleepPublication::None
        );
        assert!(!admission::cv_sleep_set_for_test(HINT_LOCK_ID));

        admission::finish_lock_scope(HINT_LOCK_ID);
        admission::finish_lock_scope(ROUTE_LOCK_ID);
    }

    #[test]
    fn an_unscoped_backend_publishes_nothing() {
        let _guard = released_cond_mutex(ROUTE_LOCK_ID, true);

        assert_eq!(
            publish_sleep_state(CondMutex::new(ROUTE_LOCK_ID, false)),
            SleepPublication::None
        );
        assert!(!admission::cv_sleep_set_for_test(ROUTE_LOCK_ID));
    }

    #[test]
    fn only_a_routed_sleep_relocks_as_already_admitted() {
        assert_eq!(
            relock_mode(SleepPublication::Routed, true),
            CondRelock::AlreadyAdmitted
        );
        assert_eq!(
            relock_mode(SleepPublication::Hint, true),
            CondRelock::TakeHint
        );
        assert_eq!(
            relock_mode(SleepPublication::None, true),
            CondRelock::TakeHint
        );

        for publication in [
            SleepPublication::Routed,
            SleepPublication::Hint,
            SleepPublication::None,
        ] {
            assert_eq!(relock_mode(publication, false), CondRelock::Normal);
        }
    }

    /// A signal that lands before the sleep leaves the futex wait immediately,
    /// which is the path that must not claim an admission token. The scheduler
    /// never routed such a wait, so both publications retire into the same
    /// state: the class it will re-acquire, pending, with the token it consumed
    /// before the wait still recorded.
    #[test]
    fn a_wait_that_never_sleeps_relocks_normally_without_a_token() {
        for (route, lock_id) in [(false, HINT_LOCK_ID), (true, ROUTE_LOCK_ID)] {
            let _guard = released_cond_mutex(lock_id, route);

            let cond = CondState::new();
            let mut observed = None;
            wait(
                &cond,
                CondMutex::new(lock_id, true),
                // A signal that lands while the mutex is being released.
                || {
                    cond.seq.fetch_add(1, Ordering::Release);
                },
                |relock| observed = Some(relock),
            );

            assert_eq!(observed, Some(CondRelock::Normal));
            assert!(!admission::cv_sleep_set_for_test(lock_id));
            assert!(admission::slow_path_pending_set_for_test(lock_id));

            let scope = admission::begin_lock_scope(lock_id);
            assert!(
                admission::token_consumed_for_scope(scope),
                "an unrouted relock suppresses its fast path either way"
            );
            admission::finish_lock_scope(lock_id);
        }
    }

    /// A signal landing between the deadline check and the cancel consumes the
    /// timing-out waiter's registration, and the count then stands for a waiter
    /// that registered behind it. The interleaving cannot be produced by timing
    /// here, so the state the two atomics are left in at that instant is built
    /// directly and the cancel is asked with the sequence the check saw.
    #[test]
    fn a_stolen_registration_forwards_the_wakeup_instead_of_timing_out() {
        let cond = CondState::new();
        let expired_seq = cond.seq.load(Ordering::Acquire);

        cond.waiters.fetch_add(1, Ordering::AcqRel);
        assert!(cond.take_waiter(), "the signal takes the expiring waiter");
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        let new_waiter_seq = cond.seq.load(Ordering::Acquire);

        assert_eq!(cond.retire_expired_waiter(expired_seq), 0);
        assert_ne!(
            cond.seq.load(Ordering::Acquire),
            new_waiter_seq,
            "the waiter behind it has to see its sleep condition broken"
        );
    }

    #[test]
    fn an_expired_waiter_with_no_signal_reports_the_deadline() {
        let cond = CondState::new();
        let expired_seq = cond.seq.load(Ordering::Acquire);
        cond.waiters.fetch_add(1, Ordering::AcqRel);

        assert_eq!(cond.retire_expired_waiter(expired_seq), libc::ETIMEDOUT);
        assert_eq!(cond.waiters.load(Ordering::Acquire), 0);
        assert_eq!(cond.seq.load(Ordering::Acquire), expired_seq);
    }

    #[test]
    fn a_registration_a_broadcast_already_took_ends_the_wait_as_a_wakeup() {
        let cond = CondState::new();
        let expired_seq = cond.seq.load(Ordering::Acquire);
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();

        assert_eq!(cond.retire_expired_waiter(expired_seq), 0);
    }

    #[test]
    fn a_timed_wait_that_expires_reports_the_timeout() {
        let _guard = released_cond_mutex(ROUTE_LOCK_ID, true);

        let cond = CondState::new();
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
        let deadline_nsec = now.tv_nsec + 1_000_000;
        let abstime = libc::timespec {
            tv_sec: now.tv_sec + deadline_nsec / 1_000_000_000,
            tv_nsec: deadline_nsec % 1_000_000_000,
        };

        let mut observed = None;
        let ret = timedwait(
            &cond,
            CondMutex::new(ROUTE_LOCK_ID, true),
            &abstime,
            || {},
            |relock| observed = Some(relock),
        );

        assert_eq!(ret, libc::ETIMEDOUT);
        assert_eq!(observed, Some(CondRelock::AlreadyAdmitted));
        assert!(!admission::cv_sleep_set_for_test(ROUTE_LOCK_ID));
    }

    /// The scheduler reads the published state while the waiter is off-CPU, so
    /// the publication has to be in place before the futex wait and gone before
    /// the thread contends for the mutex again.
    #[test]
    fn the_sleep_state_brackets_the_futex_wait() {
        let protocol = include_str!("condvar.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .map(|(implementation, _)| implementation)
            .expect("condvar.rs should contain a test module");

        for (name, entry) in [
            ("wait", "pub fn wait<U, R>"),
            ("timedwait", "pub fn timedwait<U, R>"),
        ] {
            let body = protocol
                .split_once(entry)
                .map(|(_, body)| body)
                .unwrap_or_else(|| panic!("the module should define {name}"));
            let publish_pos = body
                .find("publish_sleep_state(mutex)")
                .unwrap_or_else(|| panic!("{name} should publish the sleep state"));
            let sleep_start_pos = body
                .find("record_cv_sleep_start()")
                .unwrap_or_else(|| panic!("{name} should open the sleep bracket"));
            let futex_pos = body
                .find("futex_wait")
                .unwrap_or_else(|| panic!("{name} should block on the futex"));
            let retire_pos = body
                .find("retire_sleep_state(publication, slept)")
                .unwrap_or_else(|| panic!("{name} should retire the sleep state"));
            let sleep_end_pos = body
                .find("record_cv_sleep_end(cv_sleep_start)")
                .unwrap_or_else(|| panic!("{name} should close the sleep bracket"));
            let relock_pos = body
                .find("relock(relock_mode(publication, slept))")
                .unwrap_or_else(|| panic!("{name} should relock the cond mutex"));

            assert!(
                publish_pos < sleep_start_pos && sleep_start_pos < futex_pos,
                "{name} should publish before it blocks"
            );
            assert!(
                futex_pos < retire_pos && retire_pos < sleep_end_pos,
                "{name} should retire the state as soon as the wait stops blocking"
            );
            assert!(
                sleep_end_pos < relock_pos,
                "{name} should close the bracket before the relock mode is chosen"
            );
        }
    }

    struct SpinMutex {
        locked: AtomicBool,
    }

    impl SpinMutex {
        const fn new() -> Self {
            Self {
                locked: AtomicBool::new(false),
            }
        }

        fn lock(&self) {
            while self.locked.swap(true, Ordering::Acquire) {
                std::hint::spin_loop();
            }
        }

        fn unlock(&self) {
            self.locked.store(false, Ordering::Release);
        }
    }

    struct Shared {
        mutex: SpinMutex,
        cond: CondState,
        payload: AtomicU32,
    }

    fn run_cond_handoff(route: bool) -> CondRelock {
        let shared = Arc::new(Shared {
            mutex: SpinMutex::new(),
            cond: CondState::new(),
            payload: AtomicU32::new(0),
        });

        let waiter = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex(SMOKE_LOCK_ID, route);
                let mutex = CondMutex::new(SMOKE_LOCK_ID, true);
                let mut observed = CondRelock::Normal;

                shared.mutex.lock();
                while shared.payload.load(Ordering::Acquire) == 0 {
                    wait(
                        &shared.cond,
                        mutex,
                        || shared.mutex.unlock(),
                        |relock| {
                            observed = relock;
                            shared.mutex.lock();
                        },
                    );
                }
                let payload = shared.payload.load(Ordering::Acquire);
                shared.mutex.unlock();
                (payload, observed)
            })
        };

        std::thread::sleep(std::time::Duration::from_millis(50));
        shared.mutex.lock();
        shared.payload.store(7, Ordering::Release);
        shared.mutex.unlock();
        shared.cond.signal();

        let (payload, observed) = waiter.join().expect("the waiter should finish");
        assert_eq!(payload, 7);
        observed
    }

    #[test]
    fn routing_off_hands_a_signal_over_without_claiming_a_routed_token() {
        assert_ne!(run_cond_handoff(false), CondRelock::AlreadyAdmitted);
    }

    #[test]
    fn routing_on_hands_a_signal_over_without_consulting_the_hint() {
        assert_ne!(run_cond_handoff(true), CondRelock::TakeHint);
    }

    /// The scheduler grants the wake its admission while the word still names
    /// the cond sleep, and reads that word again while the re-acquisition is
    /// contending. The class has to be published as pending for that whole
    /// window: a class with no flag reads as a thread done with the lock, and
    /// the grant would be taken back under the contention it was made for.
    #[test]
    fn a_routed_relock_contends_with_the_class_published_as_pending() {
        let shared = Arc::new(Shared {
            mutex: SpinMutex::new(),
            cond: CondState::new(),
            payload: AtomicU32::new(0),
        });

        let waiter = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex(SMOKE_LOCK_ID, true);
                let mutex = CondMutex::new(SMOKE_LOCK_ID, true);
                let mut observed = None;

                shared.mutex.lock();
                while shared.payload.load(Ordering::Acquire) == 0 {
                    wait(
                        &shared.cond,
                        mutex,
                        || shared.mutex.unlock(),
                        |relock| {
                            observed = Some((
                                relock,
                                admission::slow_path_pending_set_for_test(SMOKE_LOCK_ID),
                                admission::cv_sleep_set_for_test(SMOKE_LOCK_ID),
                            ));
                            shared.mutex.lock();
                        },
                    );
                }
                shared.mutex.unlock();
                observed.expect("the waiter should have re-acquired the mutex")
            })
        };

        std::thread::sleep(std::time::Duration::from_millis(50));
        shared.mutex.lock();
        shared.payload.store(3, Ordering::Release);
        shared.mutex.unlock();
        shared.cond.signal();

        let (relock, pending, cv_sleep) = waiter.join().expect("the waiter should finish");
        assert_eq!(relock, CondRelock::AlreadyAdmitted);
        assert!(pending, "the relock contends with the class pending");
        assert!(!cv_sleep, "the sleep state is retired before the relock");
    }

    #[test]
    fn a_broadcast_releases_every_waiter() {
        let shared = Arc::new(Shared {
            mutex: SpinMutex::new(),
            cond: CondState::new(),
            payload: AtomicU32::new(0),
        });

        let waiters = (0..4)
            .map(|_| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    let _guard = released_cond_mutex(SMOKE_LOCK_ID, true);
                    let mutex = CondMutex::new(SMOKE_LOCK_ID, true);

                    shared.mutex.lock();
                    while shared.payload.load(Ordering::Acquire) == 0 {
                        wait(
                            &shared.cond,
                            mutex,
                            || shared.mutex.unlock(),
                            |_| shared.mutex.lock(),
                        );
                    }
                    shared.mutex.unlock();
                })
            })
            .collect::<Vec<_>>();

        std::thread::sleep(std::time::Duration::from_millis(50));
        shared.mutex.lock();
        shared.payload.store(1, Ordering::Release);
        shared.mutex.unlock();
        shared.cond.broadcast();

        for waiter in waiters {
            waiter.join().expect("every waiter should finish");
        }
    }
}
