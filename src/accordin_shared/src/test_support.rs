// SPDX-License-Identifier: GPL-2.0-only

//! Helpers the concurrency tests of this crate and of the backend crates share.
//!
//! The module is part of ordinary builds rather than of `cfg(test)` ones: the
//! tests that use it live in other crates, where a test-only item of this crate
//! is not visible. Nothing here is part of the supported surface.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::condvar::force_cv_requeue_for_thread;

/// How long a test waits for the other side of a hand-off before it calls the
/// wakeup lost. Long enough that a loaded machine never reaches it.
const PROGRESS_TIMEOUT: Duration = Duration::from_secs(20);

/// Spins until `counter` reaches `expected`, and fails the test if it stalls.
///
/// A wakeup that never arrives stops the count where it was, so `what` names
/// the hand-off that was lost rather than leaving the test hung.
#[doc(hidden)]
pub fn await_progress(counter: &AtomicU32, expected: u32, what: &str) {
    let deadline = Instant::now() + PROGRESS_TIMEOUT;
    while counter.load(Ordering::Acquire) < expected {
        assert!(Instant::now() < deadline, "{what} stalled");
        std::thread::yield_now();
    }
}

/// An absolute realtime deadline `nanos` from now, which is the form the timed
/// wait takes.
#[doc(hidden)]
pub fn deadline_in(nanos: i64) -> libc::timespec {
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

#[doc(hidden)]
pub fn deadline_in_millis(millis: i64) -> libc::timespec {
    deadline_in(millis * 1_000_000)
}

/// Serializes the tests that read a delta out of the debug counters. Those
/// counters are one process-wide array behind one process-wide enable flag, so
/// two tests measuring at the same time measure each other as well.
static DEBUG_COUNTER_MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

/// Holds the debug-counter measurement slot, and leaves the counters off when
/// it is dropped whichever way the test left them.
#[doc(hidden)]
pub struct DebugCounterMeasurement {
    _guard: MutexGuard<'static, ()>,
}

impl DebugCounterMeasurement {
    /// Starts counting. Kept apart from taking the slot so that a test can also
    /// observe the counters standing still while they are off.
    pub fn enable(&self) {
        crate::mutex_hook::set_cv_admission_counters_enabled(true);
    }
}

impl Drop for DebugCounterMeasurement {
    fn drop(&mut self) {
        crate::mutex_hook::set_cv_admission_counters_enabled(false);
    }
}

#[doc(hidden)]
pub fn measure_debug_counters() -> DebugCounterMeasurement {
    let guard = DEBUG_COUNTER_MEASUREMENT_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    DebugCounterMeasurement { _guard: guard }
}

/// Restores the requeue switch of the thread that took it.
#[doc(hidden)]
pub struct RequeueGuard;

impl Drop for RequeueGuard {
    fn drop(&mut self) {
        force_cv_requeue_for_thread(None);
    }
}

/// Chooses the requeue mode for the calling thread, which is the waking side of
/// a hand-off: a waiter's release comes from the staged count, which the unlock
/// drains either way.
#[doc(hidden)]
pub fn waking_thread(requeue: bool) -> RequeueGuard {
    force_cv_requeue_for_thread(Some(requeue));
    RequeueGuard
}
