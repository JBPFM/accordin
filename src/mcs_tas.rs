use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::arch::{pause, read_tsc};

#[repr(align(64))]
struct CacheAligned<T>(T);

const WAIT_TSC_SAMPLE_STRIDE: u8 = 8;

#[repr(C, align(64))]
pub struct Node {
    next: AtomicPtr<Node>,
    waiting: AtomicBool,
    wait_sample_countdown: u8,
    sampled_wait_tsc_cycles: u64,
    sampled_wait_tsc_samples: u64,
}

impl Node {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            waiting: AtomicBool::new(false),
            wait_sample_countdown: 0,
            sampled_wait_tsc_cycles: 0,
            sampled_wait_tsc_samples: 0,
        }
    }
}

thread_local! {
    static THREAD_NODE: UnsafeCell<Node> = const { UnsafeCell::new(Node::new()) };
}

/// MCS-TAS lock with cache-aligned state variables.
pub struct McsTasLockRaw {
    tail: CacheAligned<AtomicPtr<Node>>,
    locked: CacheAligned<AtomicBool>,
}

impl McsTasLockRaw {
    pub const fn new() -> Self {
        Self {
            tail: CacheAligned(AtomicPtr::new(ptr::null_mut())),
            locked: CacheAligned(AtomicBool::new(false)),
        }
    }

    fn thread_node() -> *mut Node {
        THREAD_NODE.with(|node| node.get())
    }

    #[inline(always)]
    fn should_sample_wait(my_node: *mut Node) -> bool {
        unsafe {
            let countdown = (*my_node).wait_sample_countdown;
            if countdown == 0 {
                (*my_node).wait_sample_countdown = WAIT_TSC_SAMPLE_STRIDE.saturating_sub(1);
                true
            } else {
                (*my_node).wait_sample_countdown = countdown - 1;
                false
            }
        }
    }

    #[inline(always)]
    fn record_sampled_wait(my_node: *mut Node, cycles: u64) {
        unsafe {
            (*my_node).sampled_wait_tsc_cycles += cycles;
            (*my_node).sampled_wait_tsc_samples += 1;
        }
    }

    #[inline(always)]
    pub fn lock(&self) {
        if !self.locked.0.swap(true, Ordering::Acquire) {
            return;
        }

        let my_node = Self::thread_node();
        unsafe {
            (*my_node).next.store(ptr::null_mut(), Ordering::Relaxed);
            (*my_node).waiting.store(false, Ordering::Relaxed);
        }

        let sample_wait = Self::should_sample_wait(my_node);
        let wait_start = if sample_wait { read_tsc() } else { 0 };
        let pred = self.tail.0.swap(my_node, Ordering::AcqRel);
        if !pred.is_null() {
            unsafe {
                (*my_node).waiting.store(true, Ordering::Relaxed);
                (*pred).next.store(my_node, Ordering::Release);
                while (*my_node).waiting.load(Ordering::Acquire) {
                    pause();
                }
            }
        }

        while self.locked.0.swap(true, Ordering::Acquire) {
            pause();
        }
        if sample_wait {
            Self::record_sampled_wait(my_node, read_tsc().saturating_sub(wait_start));
        }

        let mut succ = unsafe { (*my_node).next.load(Ordering::Acquire) };
        if succ.is_null()
            && self
                .tail
                .0
                .compare_exchange(
                    my_node,
                    ptr::null_mut(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
        {
            while {
                succ = unsafe { (*my_node).next.load(Ordering::Acquire) };
                succ.is_null()
            } {
                pause();
            }
        }
        if !succ.is_null() {
            unsafe {
                (*succ).waiting.store(false, Ordering::Release);
            }
        }
    }

    /// Returns true if the lock was acquired, false if it was already held.
    #[inline(always)]
    pub fn try_lock(&self) -> bool {
        !self.locked.0.swap(true, Ordering::Acquire)
    }

    #[inline(always)]
    pub fn unlock(&self) {
        self.locked.0.store(false, Ordering::Release);
    }
}
