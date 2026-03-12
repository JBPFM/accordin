use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::arch::{clock_gettime_ns, pause};

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

/// Per-thread lock scheduling context, read by BPF via bpf_probe_read_user.
#[repr(C)]
pub struct LockSchedThreadCtx {
    pub wait_ns_total: u64,
    pub state: u32,
    pub seq: u32,
}

impl LockSchedThreadCtx {
    const fn new() -> Self {
        Self {
            wait_ns_total: 0,
            state: 0, // ROLE_NONE
            seq: 0,
        }
    }

    #[inline(always)]
    fn seq_begin(&mut self) {
        self.seq = self.seq.wrapping_add(1); // odd = writing
        std::sync::atomic::fence(Ordering::Release);
    }

    #[inline(always)]
    fn seq_end(&mut self) {
        std::sync::atomic::fence(Ordering::Release);
        self.seq = self.seq.wrapping_add(1); // even = committed
    }

    #[inline(always)]
    fn set_role_owner(&mut self) {
        // self.seq_begin();
        self.state = 1; // ROLE_OWNER
        // self.seq_end();
    }

    #[inline(always)]
    fn set_role_none(&mut self) {
        // self.seq_begin();
        self.state = 0; // ROLE_NONE
        // self.seq_end();
    }
}

thread_local! {
    static THREAD_NODE: UnsafeCell<Node> = const { UnsafeCell::new(Node::new()) };
    static THREAD_CTX: UnsafeCell<LockSchedThreadCtx> = const { UnsafeCell::new(LockSchedThreadCtx::new()) };
}

/// Returns a pointer to the current thread's LockSchedThreadCtx.
pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
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
        // Fast path: TAS
        if !self.locked.0.swap(true, Ordering::Acquire) {
            // Acquired immediately — set ROLE_OWNER
            let ctx = thread_ctx();
            unsafe { (*ctx).set_role_owner() };
            return;
        }

        // Slow path: MCS queue + TAS
        let wait_start = clock_gettime_ns();

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

        // Acquired after contention — accumulate wait time, set ROLE_OWNER
        let wait_end = clock_gettime_ns();
        let ctx = thread_ctx();
        unsafe {
            (*ctx).wait_ns_total += wait_end - wait_start;
            (*ctx).set_role_owner();
        }
    }

    /// Returns true if the lock was acquired, false if it was already held.
    #[inline(always)]
    pub fn try_lock(&self) -> bool {
        // CAS instead of swap to avoid unnecessary cache-line invalidation
        if self
            .locked
            .0
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            let ctx = thread_ctx();
            unsafe { (*ctx).set_role_owner() };
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn unlock(&self) {
        // Release the lock first
        self.locked.0.store(false, Ordering::Release);
        // Then clear role — brief "dual OWNER" is a conservative protective bias
        let ctx = thread_ctx();
        unsafe { (*ctx).set_role_none() };
    }
}
