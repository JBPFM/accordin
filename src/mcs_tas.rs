use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::arch::{pause, wait_time_elapsed_ns, wait_time_start};
use crate::timeslice_extension;

// Keep in sync with WAIT_TIME_SAMPLE_STRIDE in src/bpf/intf.h.
const WAIT_TIME_SAMPLE_STRIDE: u32 = 8;
const WAIT_TIME_SAMPLE_MASK: u32 = WAIT_TIME_SAMPLE_STRIDE - 1;

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
}

impl LockSchedThreadCtx {
    const fn new() -> Self {
        Self { wait_ns_total: 0 }
    }
}

thread_local! {
    static THREAD_NODE: UnsafeCell<Node> = const { UnsafeCell::new(Node::new()) };
    static THREAD_CTX: UnsafeCell<LockSchedThreadCtx> = const { UnsafeCell::new(LockSchedThreadCtx::new()) };
    static WAIT_SAMPLE_COUNTER: Cell<u32> = const { Cell::new(0) };
    static TIMESLICE_REQUESTED: Cell<bool> = const { Cell::new(false) };
}

/// Returns a pointer to the current thread's LockSchedThreadCtx.
pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
}

/// Prepare timeslice extension for the current thread. Call once per thread.
pub fn prepare_thread_timeslice() {
    timeslice_extension::prepare_thread();
}

#[inline(always)]
fn should_sample_wait() -> bool {
    WAIT_SAMPLE_COUNTER.with(|counter| {
        let next = counter.get().wrapping_add(1);
        counter.set(next);
        (next & WAIT_TIME_SAMPLE_MASK) == 0
    })
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
            return;
        }

        // Slow path: MCS queue + TAS
        let sample_wait = should_sample_wait();
        let wait_start = if sample_wait { wait_time_start() } else { 0 };

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

        // Request timeslice extension after getting lock.
        let tse_requested = timeslice_extension::on_contended_lock_enter();
        TIMESLICE_REQUESTED.with(|cell| cell.set(tse_requested));

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

        // Acquired after contention — accumulate sampled wait time, set ROLE_OWNER
        let ctx = thread_ctx();
        unsafe {
            if sample_wait {
                (*ctx).wait_ns_total += wait_time_elapsed_ns(wait_start);
            }
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
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn unlock(&self) {
        // Release the lock first
        self.locked.0.store(false, Ordering::Release);
        // Yield extended timeslice if one was granted
        TIMESLICE_REQUESTED.with(|cell| {
            if cell.get() {
                cell.set(false);
                timeslice_extension::on_critical_section_exit();
            }
        });
    }
}
