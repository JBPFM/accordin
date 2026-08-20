// SPDX-License-Identifier: GPL-2.0-only

//! Helpers the concurrency tests of this crate and of the backend crates share.
//!
//! The module is part of ordinary builds rather than of `cfg(test)` ones: the
//! tests that use it live in other crates, where a test-only item of this crate
//! is not visible. Nothing here is part of the supported surface.

use std::sync::atomic::{AtomicU32, Ordering};
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
