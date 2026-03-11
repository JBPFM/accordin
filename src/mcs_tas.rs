use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::arch::pause;

#[repr(align(64))]
struct CacheAligned<T>(T);

#[repr(C, align(64))]
pub struct Node {
    next: AtomicPtr<Node>,
    waiting: AtomicBool,
}

impl Node {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            waiting: AtomicBool::new(false),
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
    pub fn lock(&self) {
        if !self.locked.0.swap(true, Ordering::Acquire) {
            return;
        }

        let my_node = Self::thread_node();
        unsafe {
            (*my_node).next.store(ptr::null_mut(), Ordering::Relaxed);
            (*my_node).waiting.store(false, Ordering::Relaxed);
        }

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
