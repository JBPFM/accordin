//! One admission episode covers all locks currently held by a thread.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering};

// Shared with BPF intf.h; checked by the scheduler loader.
pub const HELD: u32 = 1;
pub const WAITING: u32 = 2;
pub const SPINNING: u32 = 3;
pub const FLAGS: u32 = HELD | WAITING;
pub const DISABLE_ADMISSION_ENV: &str = "ACCORDIN_DISABLE_ADMISSION";
pub const MAX_CPUS: usize = 256;

#[repr(C)]
pub struct SchedulerAdmission {
    pub enabled: AtomicU32,
    pub owners: [AtomicU64; MAX_CPUS],
}

static SCHEDULER: AtomicPtr<SchedulerAdmission> = AtomicPtr::new(std::ptr::null_mut());

/// # Safety
/// The BPF mapping must remain valid until cleared or process exit.
pub unsafe fn set_scheduler(ptr: *mut SchedulerAdmission) {
    SCHEDULER.store(ptr, Ordering::Release);
}

thread_local! {
    static WORD: AtomicU32 = const { AtomicU32::new(0) };
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    static TID: u32 = unsafe { libc::syscall(libc::SYS_gettid) as u32 };
}

pub fn user_word_addr() -> *const u32 {
    WORD.with(AtomicU32::as_ptr)
}

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| !crate::env::env_flag(DISABLE_ADMISSION_ENV))
}

/// Nested acquisitions must keep running so they can release their outer locks.
#[inline(always)]
pub fn begin() -> bool {
    DEPTH.with(|depth| {
        let outer = depth.get() == 0;
        depth.set(depth.get() + 1);
        let managed = outer && enabled();
        if managed {
            // Identify this acquisition even if BPF never observed the unlock.
            WORD.with(|word| {
                let next = (word.load(Ordering::Relaxed) & !FLAGS).wrapping_add(FLAGS + 1);
                word.store(next, Ordering::Relaxed);
            });
        }
        managed
    })
}

/// Ask for a slot for this acquisition, then confirm it before entering the
/// raw lock queue. A yield by itself does not guarantee that dispatch ran.
#[inline(always)]
pub fn wait() {
    let request = WORD.with(|word| word.fetch_or(WAITING, Ordering::Relaxed) & !FLAGS);
    let ptr = SCHEDULER.load(Ordering::Acquire);
    let tid = TID.with(|tid| *tid);
    let ticket = (u64::from(request) << 32) | u64::from(tid);
    loop {
        std::thread::yield_now();
        let Some(scheduler) = (unsafe { ptr.as_ref() }) else {
            break;
        };
        if scheduler.enabled.load(Ordering::Acquire) == 0 {
            break;
        }
        let cpu = unsafe { libc::sched_getcpu() } as usize;
        if cpu < MAX_CPUS && scheduler.owners[cpu].load(Ordering::Relaxed) == ticket {
            break;
        }
    }
    WORD.with(|word| word.store(request | SPINNING, Ordering::Relaxed));
}

#[inline(always)]
pub fn enter(outer: bool) {
    if outer {
        WORD.with(|word| {
            word.store(
                (word.load(Ordering::Relaxed) & !FLAGS) | HELD,
                Ordering::Relaxed,
            )
        });
    }
}

/// Keep HELD until the final unlock, including out-of-order nested unlocks.
#[inline(always)]
pub fn finish() {
    DEPTH.with(|depth| {
        let held = depth.get();
        debug_assert!(held > 0);
        depth.set(held - 1);
        if held == 1 && enabled() {
            WORD.with(|word| {
                word.fetch_and(!FLAGS, Ordering::Relaxed);
            });
        }
    });
}

#[cfg(test)]
pub fn state_for_test() -> u32 {
    WORD.with(|word| word.load(Ordering::Relaxed) & FLAGS)
}

#[cfg(test)]
pub fn reset_for_test() {
    WORD.with(|word| word.store(0, Ordering::Relaxed));
    DEPTH.with(|depth| depth.set(0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_acquisition_cannot_reuse_an_unobserved_old_grant() {
        reset_for_test();
        enter(begin());
        let old_request = WORD.with(|word| word.load(Ordering::Relaxed) & !FLAGS);
        finish();
        enter(begin());
        let new_request = WORD.with(|word| word.load(Ordering::Relaxed) & !FLAGS);
        assert_ne!(old_request, new_request);
        assert_eq!(state_for_test(), HELD);
        finish();
    }
}
