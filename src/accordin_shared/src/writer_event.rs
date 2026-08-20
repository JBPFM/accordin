// SPDX-License-Identifier: GPL-2.0-only

//! Per-waiter wake event for leader/follower queues.
//!
//! A leader/follower queue wakes a waiter for one of two reasons, and a
//! condition variable conflates them: either the leader has already done the
//! waiter's work, in which case the waiter only has to read the result the
//! leader published and leave, or the waiter is the new queue head and has to
//! re-acquire the mutex to do work of its own. This module separates them into
//! a reason code the waiter blocks on directly.
//!
//! The separation is what the admission state hangs off. A waiter released with
//! work still to do keeps the cv-sleep state published across its wake, so the
//! scheduler routes it through the same grant path an ordinary cond waiter
//! takes; a waiter released with nothing left to do has that state taken back
//! by its waker, because a grant made for a re-acquisition it never performs
//! would be spent on nothing.
//!
//! The event is the futex word, the reason code and the handle a waker needs to
//! reach the waiter's admission word, in one caller-allocated object: the
//! waiters this exists for are created per operation, and a heap allocation per
//! operation is the cost the design is trying to remove.

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::admission::{self, CvSleepArm};
use crate::condvar::{futex_wait, futex_wake};
use crate::mutex_hook::{
    record_writer_event_arm, record_writer_event_completed_misroute,
    record_writer_event_completed_post, record_writer_event_leader_post,
    record_writer_event_relock_admitted, record_writer_event_relock_normal,
    record_writer_event_route_takeback, record_writer_event_route_takeback_lost,
    record_writer_event_route_takeback_unpublished, record_writer_event_spurious_wake,
};

/// The waiter has not been released yet. Doubles as the value the futex wait
/// blocks on, so the reason code is also the sleep condition.
const WAITING: u32 = 0;
/// The waker did this waiter's work and published the result.
const COMPLETED: u32 = 1;
/// The waiter is the new queue head and owns the work from here.
const LEADER: u32 = 2;

/// Why a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeReason {
    /// The work is done and its result is published. The waiter returns without
    /// re-acquiring the mutex, which is the whole point of the reason code.
    Completed,
    /// The waiter is the queue head and must re-acquire the mutex.
    BecomeLeader,
}

/// What a waker found when it went to take a waiter's routed state back.
///
/// The two ways of not taking it are kept apart because they say different
/// things: nothing published means the waiter had not reached its sleep yet,
/// while a lost exchange means it had already left one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RouteTakeback {
    Taken,
    Unpublished,
    Lost,
}

/// How the new leader should re-acquire the mutex it released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventRelock {
    /// Ordinary contended acquisition: it asks admission for a slot itself.
    Normal,
    /// The wakeup was routed by the scheduler, which granted the admission
    /// token as it enqueued the waiter.
    AlreadyAdmitted,
}

/// The state one waiter blocks on.
///
/// `#[repr(C)]` with a fixed layout because the object is embedded in a caller
/// allocation reached through the C ABI, which asks for the size and then
/// initializes the storage in place.
#[repr(C, align(8))]
pub struct WriterEvent {
    /// The futex word and the reason code in one: a waiter sleeps while it
    /// reads `WAITING` and the value that ends the sleep says why.
    state: AtomicU32,
    /// The value the waiter published in its admission word, which a waker
    /// exchanges against to take the routed state back.
    armed_value: AtomicU32,
    /// The address of the waiter's admission word, zero while nothing is
    /// published. Written last and read first, so a non-zero address always
    /// comes with the value that belongs to it.
    armed_word: AtomicUsize,
    /// The re-acquisition mode the wait decided, read only by the waiter.
    relock: AtomicU32,
    _reserved: u32,
}

impl Default for WriterEvent {
    fn default() -> Self {
        Self::new()
    }
}

impl WriterEvent {
    pub const fn new() -> Self {
        Self {
            state: AtomicU32::new(WAITING),
            armed_value: AtomicU32::new(0),
            armed_word: AtomicUsize::new(0),
            relock: AtomicU32::new(EventRelock::Normal as u32),
            _reserved: 0,
        }
    }

    /// Puts a fresh event into caller-owned storage.
    ///
    /// # Safety
    ///
    /// `ptr` must point at writable storage of at least `size_of::<WriterEvent>()`
    /// bytes, aligned for this type, that no other thread is using.
    pub unsafe fn init_in_place(ptr: *mut WriterEvent) {
        unsafe { ptr.write(Self::new()) };
    }

    /// Declares the waiter unreleased before it gives the mutex up.
    ///
    /// This has to run while the mutex is still held: past the release a waker
    /// may publish a reason at any moment, and one arriving before the waiter
    /// declared itself would be overwritten here.
    #[inline]
    pub fn arm(&self) {
        self.state.store(WAITING, Ordering::Release);
    }

    /// Blocks until a waker publishes a reason, and reports it.
    ///
    /// `lock_id` names the mutex the waiter released and will re-acquire if it
    /// is made leader; `admission_scoped` is the backend's own answer to whether
    /// that mutex runs through admission at all.
    ///
    /// Must be called with the mutex released: the cv-sleep state describes a
    /// thread that holds nothing, and publishing it under a held scope would
    /// hand the scheduler the wrong picture of that scope.
    pub fn wait(&self, lock_id: u32, admission_scoped: bool) -> WakeReason {
        // A reason that landed before the waiter got here needs no sleep state
        // at all: publishing it would describe a running thread as sleeping,
        // and the retirement below would take it straight back.
        if self.state.load(Ordering::Acquire) != WAITING {
            return self.finish(false, false);
        }

        let routed = self.publish_sleep_state(lock_id, admission_scoped);
        if routed {
            record_writer_event_arm();
        }
        let mut slept = false;

        while self.state.load(Ordering::Acquire) == WAITING {
            let rc = unsafe { futex_wait(&self.state, WAITING) };
            let woke = rc == 0 || rc == libc::EINTR;
            slept |= woke;
            // A wake that leaves the reason unset is not the release this wait
            // is for, and nothing is republished before blocking again. The
            // word is this thread's own and only two things write it: this
            // wait, which has not run since it published, and a waker taking
            // the state back ahead of a completed reason. Re-publishing would
            // store what is already there in the first case and undo the waker
            // in the second, putting a thread that is about to return without
            // the mutex back on the routed path.
            if woke && self.state.load(Ordering::Acquire) == WAITING {
                record_writer_event_spurious_wake();
            }
        }

        self.finish(routed, slept)
    }

    /// Reads the reason the wait ended on, retires whatever sleep state is
    /// still published, and records the re-acquisition mode.
    fn finish(&self, routed: bool, slept: bool) -> WakeReason {
        if self.state.load(Ordering::Acquire) == COMPLETED {
            // Nothing is re-acquired, so the state is retired into a plain
            // release rather than into the pending state a contender publishes.
            // Finding one still published means this wake was routed for a
            // re-acquisition that never happens, which is what the waker's
            // takeback exists to prevent and what the count measures.
            if admission::retire_cv_sleep_to_released() {
                record_writer_event_completed_misroute();
            }
            self.relock
                .store(EventRelock::Normal as u32, Ordering::Relaxed);
            return WakeReason::Completed;
        }

        if routed {
            admission::retire_cv_sleep_to_pending(slept);
        }
        // A wait that never blocked was never routed, so the re-acquisition
        // carries no decision and asks admission for one like any contender.
        let relock = if routed && slept {
            EventRelock::AlreadyAdmitted
        } else {
            EventRelock::Normal
        };
        self.relock.store(relock as u32, Ordering::Relaxed);
        WakeReason::BecomeLeader
    }

    /// The mode the finished wait left for the re-acquisition.
    ///
    /// Accounted against counters of its own rather than the cond path's: those
    /// reconcile hints published against hints answered, and an event relock
    /// publishes no hint, so folding the two together would leave that
    /// reconciliation reading a surplus of answers.
    #[inline]
    pub fn take_relock_mode(&self) -> EventRelock {
        let mode = self.relock_mode();
        match mode {
            EventRelock::AlreadyAdmitted => record_writer_event_relock_admitted(),
            EventRelock::Normal => record_writer_event_relock_normal(),
        }
        mode
    }

    #[inline]
    fn relock_mode(&self) -> EventRelock {
        if self.relock.load(Ordering::Relaxed) == EventRelock::AlreadyAdmitted as u32 {
            EventRelock::AlreadyAdmitted
        } else {
            EventRelock::Normal
        }
    }

    /// Releases a waiter whose work the caller has already done and whose
    /// result the caller has already published.
    ///
    /// The routed state is taken back first: the woken waiter returns without
    /// touching the mutex, so an admission grant made for a re-acquisition it
    /// never performs would be spent on nothing. Losing that exchange costs one
    /// misrouted wake and no correctness, because the waiter finds the reason
    /// either way and retires the state itself.
    ///
    /// The futex wake is the last access this object may ever see: the waiter it
    /// releases may return and destroy the event the instant the reason lands,
    /// so nothing may touch it afterwards. The wake itself is safe against that
    /// because it only names an address to the kernel, and the storage the
    /// waiter frees is its own stack, which stays mapped.
    pub fn post_completed(&self) {
        match self.take_route_back() {
            RouteTakeback::Taken => record_writer_event_route_takeback(),
            RouteTakeback::Unpublished => record_writer_event_route_takeback_unpublished(),
            RouteTakeback::Lost => record_writer_event_route_takeback_lost(),
        }
        record_writer_event_completed_post();
        self.state.store(COMPLETED, Ordering::Release);
        unsafe { futex_wake(&self.state, 1) };
    }

    /// Hands the queue head to a waiter that still has work to do.
    ///
    /// The routed state stays published on purpose: the wake enqueues a thread
    /// whose admission word still names the mutex it is about to re-acquire, so
    /// the scheduler grants it a slot as it makes it runnable instead of making
    /// it contend for one after the fact.
    ///
    /// As with a completed post, the wake is the last access to this object.
    pub fn post_leader(&self) {
        record_writer_event_leader_post();
        self.state.store(LEADER, Ordering::Release);
        unsafe { futex_wake(&self.state, 1) };
    }

    /// Publishes the cv-sleep state and the handle a waker retires it through.
    ///
    /// The address is stored last with release ordering and read first with
    /// acquire ordering, so a waker either sees no handle at all or sees one
    /// whose value belongs to it.
    fn publish_sleep_state(&self, lock_id: u32, admission_scoped: bool) -> bool {
        if !admission_scoped {
            return false;
        }

        let arm = admission::arm_cv_sleep_for_lock(lock_id);
        if !arm.is_armed() {
            return false;
        }

        self.armed_value.store(arm.armed_value(), Ordering::Relaxed);
        self.armed_word.store(arm.word_addr(), Ordering::Release);
        true
    }

    /// Retires the waiter's routed state on its behalf.
    fn take_route_back(&self) -> RouteTakeback {
        let word_addr = self.armed_word.load(Ordering::Acquire);
        if word_addr == 0 {
            return RouteTakeback::Unpublished;
        }

        if CvSleepArm::from_parts(word_addr, self.armed_value.load(Ordering::Relaxed)).retire() {
            RouteTakeback::Taken
        } else {
            RouteTakeback::Lost
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{COMPLETED, EventRelock, LEADER, RouteTakeback, WAITING, WakeReason, WriterEvent};
    use crate::admission;
    use crate::condvar::futex_wake;

    const EVENT_LOCK_ID: u32 = 7;

    struct AdmissionGuard;

    impl Drop for AdmissionGuard {
        fn drop(&mut self) {
            admission::reset_thread_depth_for_test();
            admission::reset_state();
        }
    }

    /// Starts from the state an unlock leaves behind: the word names the
    /// released class and no scope stays held.
    fn released_mutex(lock_id: u32) -> AdmissionGuard {
        admission::reset_thread_depth_for_test();
        admission::reset_state();

        let scope = admission::begin_lock_scope(lock_id);
        admission::mark_critical_section_entered_for_scope(scope);
        admission::finish_lock_scope(lock_id);
        AdmissionGuard
    }

    #[test]
    fn an_event_initializes_in_place_as_unreleased() {
        let mut storage = std::mem::MaybeUninit::<WriterEvent>::uninit();
        let event = unsafe {
            WriterEvent::init_in_place(storage.as_mut_ptr());
            storage.assume_init_ref()
        };

        assert_eq!(event.state.load(Ordering::Acquire), WAITING);
        assert_eq!(event.armed_word.load(Ordering::Acquire), 0);
    }

    /// The reason code alone decides the wait, so a post that lands before the
    /// waiter blocks must end the wait without a sleep and without a routed
    /// re-acquisition.
    #[test]
    fn a_reason_published_before_the_wait_ends_it_without_sleeping() {
        let _guard = released_mutex(EVENT_LOCK_ID);

        let event = WriterEvent::new();
        event.arm();
        event.post_leader();

        assert_eq!(event.wait(EVENT_LOCK_ID, true), WakeReason::BecomeLeader);
        assert_eq!(event.relock_mode(), EventRelock::Normal);
        assert!(!admission::cv_sleep_set_for_test(EVENT_LOCK_ID));
    }

    #[test]
    fn a_completed_reason_retires_the_sleep_state_into_a_release() {
        let _guard = released_mutex(EVENT_LOCK_ID);

        let event = WriterEvent::new();
        event.arm();
        event.state.store(COMPLETED, Ordering::Release);

        assert_eq!(event.wait(EVENT_LOCK_ID, true), WakeReason::Completed);
        assert!(!admission::cv_sleep_set_for_test(EVENT_LOCK_ID));
        assert!(
            !admission::slow_path_pending_set_for_test(EVENT_LOCK_ID),
            "a writer that returns without the mutex is not contending for it"
        );
    }

    #[test]
    fn an_unscoped_backend_publishes_no_sleep_state() {
        let _guard = released_mutex(EVENT_LOCK_ID);

        let event = WriterEvent::new();
        event.arm();
        event.state.store(LEADER, Ordering::Release);

        assert_eq!(event.wait(EVENT_LOCK_ID, false), WakeReason::BecomeLeader);
        assert!(!admission::cv_sleep_set_for_test(EVENT_LOCK_ID));
    }

    /// A waiter that actually blocks and is made leader keeps the class in its
    /// word for the whole re-acquisition, and enters it on the decision its
    /// wake carried.
    #[test]
    fn a_blocked_leader_wake_relocks_on_the_routed_decision() {
        let event = Arc::new(WriterEvent::new());
        let observed = Arc::new(AtomicU32::new(u32::MAX));

        let waiter = {
            let event = Arc::clone(&event);
            let observed = Arc::clone(&observed);
            std::thread::spawn(move || {
                let _guard = released_mutex(EVENT_LOCK_ID);
                event.arm();
                let reason = event.wait(EVENT_LOCK_ID, true);
                observed.store(
                    u32::from(
                        reason == WakeReason::BecomeLeader
                            && event.take_relock_mode() == EventRelock::AlreadyAdmitted
                            && admission::slow_path_pending_set_for_test(EVENT_LOCK_ID)
                            && !admission::cv_sleep_set_for_test(EVENT_LOCK_ID),
                    ),
                    Ordering::Release,
                );
            })
        };

        std::thread::sleep(std::time::Duration::from_millis(50));
        event.post_leader();
        waiter.join().expect("the waiter should finish");

        assert_eq!(
            observed.load(Ordering::Acquire),
            1,
            "a routed leader wake relocks as already admitted with the class pending"
        );
    }

    /// The waker takes the routed state back for a completed post, which is
    /// what keeps the scheduler from granting a slot for a re-acquisition that
    /// never happens.
    #[test]
    fn a_completed_post_takes_the_routed_state_back_from_the_waiter() {
        let event = Arc::new(WriterEvent::new());
        let armed = Arc::new(AtomicU32::new(0));
        let observed = Arc::new(AtomicU32::new(u32::MAX));

        let waiter = {
            let event = Arc::clone(&event);
            let armed = Arc::clone(&armed);
            let observed = Arc::clone(&observed);
            std::thread::spawn(move || {
                let _guard = released_mutex(EVENT_LOCK_ID);
                event.arm();
                armed.store(1, Ordering::Release);
                let reason = event.wait(EVENT_LOCK_ID, true);
                observed.store(
                    u32::from(
                        reason == WakeReason::Completed
                            && !admission::cv_sleep_set_for_test(EVENT_LOCK_ID),
                    ),
                    Ordering::Release,
                );
            })
        };

        while armed.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert_ne!(
            event.armed_word.load(Ordering::Acquire),
            0,
            "a sleeping waiter publishes the handle its waker retires through"
        );
        assert_eq!(
            event.take_route_back(),
            RouteTakeback::Taken,
            "the exchange against the exact armed value takes the state back"
        );
        assert_eq!(
            event.take_route_back(),
            RouteTakeback::Lost,
            "a second attempt finds a word that no longer carries the armed value"
        );

        event.post_completed();
        waiter.join().expect("the waiter should finish");
        assert_eq!(observed.load(Ordering::Acquire), 1);
    }

    /// Reads a waiter's admission word through the handle it published, which
    /// is the only way another thread can see it.
    fn published_word(event: &WriterEvent) -> u32 {
        let addr = event.armed_word.load(Ordering::Acquire);
        assert_ne!(addr, 0, "the waiter should have published its handle");
        unsafe { (*(addr as *const AtomicU32)).load(Ordering::Acquire) }
    }

    /// Once a waker has taken the routed state back, nothing may put it back
    /// before the reason lands. A wake that carries no reason sends the waiter
    /// round the loop again, and if that re-published the sleep state the
    /// completed wake behind it would be routed for a re-acquisition that never
    /// happens, with the waker still counting its takeback as a success.
    #[test]
    fn wakes_without_a_reason_do_not_undo_a_takeback() {
        const SPURIOUS_WAKES: u32 = 200;

        let event = Arc::new(WriterEvent::new());
        let armed = Arc::new(AtomicU32::new(0));
        let observed = Arc::new(AtomicU32::new(u32::MAX));

        let waiter = {
            let event = Arc::clone(&event);
            let armed = Arc::clone(&armed);
            let observed = Arc::clone(&observed);
            std::thread::spawn(move || {
                let _guard = released_mutex(EVENT_LOCK_ID);
                event.arm();
                armed.store(1, Ordering::Release);
                let reason = event.wait(EVENT_LOCK_ID, true);
                observed.store(
                    u32::from(reason == WakeReason::Completed),
                    Ordering::Release,
                );
            })
        };

        while armed.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert_eq!(event.take_route_back(), RouteTakeback::Taken);
        let released = published_word(&event);
        assert_eq!(
            released & (admission::USER_ADMISSION_FLAG_MASK),
            0,
            "a retired state leaves the class with no flag"
        );

        for _ in 0..SPURIOUS_WAKES {
            unsafe { futex_wake(&event.state, 1) };
            std::thread::yield_now();
            assert_eq!(
                published_word(&event),
                released,
                "a wake carrying no reason must leave the retired word alone"
            );
        }

        event.post_completed();
        waiter.join().expect("the waiter should finish");
        assert_eq!(observed.load(Ordering::Acquire), 1);
    }

    /// Losing the exchange is defined to be harmless, so a handle naming a
    /// value the word no longer carries must report failure and leave the word
    /// alone rather than overwrite it.
    #[test]
    fn a_stale_armed_value_leaves_the_word_untouched() {
        let _guard = released_mutex(EVENT_LOCK_ID);

        let event = WriterEvent::new();
        assert!(
            event.publish_sleep_state(EVENT_LOCK_ID, true),
            "the released class is managed and no scope is held"
        );
        let armed = event.armed_value.load(Ordering::Relaxed);

        // The owner leaves the sleep early and rewrites its own word.
        admission::retire_cv_sleep_to_pending(true);
        let rewritten = admission::word_for_test();
        assert_ne!(rewritten, armed);

        assert_eq!(
            event.take_route_back(),
            RouteTakeback::Lost,
            "the exchange sees a changed word"
        );
        assert_eq!(
            admission::word_for_test(),
            rewritten,
            "a lost exchange leaves what the owner wrote in place"
        );
    }

    #[test]
    fn a_handle_naming_nothing_retires_nothing() {
        let event = WriterEvent::new();
        assert_eq!(event.take_route_back(), RouteTakeback::Unpublished);

        assert!(!admission::CvSleepArm::none().is_armed());
        assert!(!admission::CvSleepArm::none().retire());
    }
}
