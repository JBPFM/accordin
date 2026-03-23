use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering, fence};

use crate::arch::{pause, wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns};
use crate::timeslice_extension;

// we remove sample cause in some high contention env wait can occupy full timeslice
// or we can reserve a fast path in low contention
// // Keep in sync with WAIT_TIME_SAMPLE_STRIDE in src/bpf/intf.h.
// const WAIT_TIME_SAMPLE_STRIDE: u32 = 8;
// const WAIT_TIME_SAMPLE_MASK: u32 = WAIT_TIME_SAMPLE_STRIDE - 1;

#[repr(align(64))]
struct CacheAligned<T>(T);

#[repr(C, align(64))]
pub struct Node {
    next: AtomicPtr<Node>,
    prev: AtomicPtr<Node>,
    waiting: AtomicBool,
}

impl Node {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            prev: AtomicPtr::new(ptr::null_mut()),
            waiting: AtomicBool::new(false),
        }
    }
}

/// Per-thread lock scheduling context, read by BPF via bpf_probe_read_user.
#[repr(C)]
pub struct LockSchedThreadCtx {
    pub wait_ns_total: u64,
    pub wait_start_ns: u64,
    pub wait_end_ns: u64,
}

impl LockSchedThreadCtx {
    const fn new() -> Self {
        Self {
            wait_ns_total: 0,
            wait_start_ns: 0,
            wait_end_ns: 0,
        }
    }
}

thread_local! {
    static THREAD_NODE: UnsafeCell<Node> = const { UnsafeCell::new(Node::new()) };
    static THREAD_CTX: UnsafeCell<LockSchedThreadCtx> = const { UnsafeCell::new(LockSchedThreadCtx::new()) };
    static TIMESLICE_REQUESTED: Cell<bool> = const { Cell::new(false) };
    // static WAIT_SAMPLE_COUNTER: Cell<u32> = const { Cell::new(0) };
}

/// Returns a pointer to the current thread's LockSchedThreadCtx.
pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
}

// #[inline(always)]
// fn should_sample_wait() -> bool {
//     WAIT_SAMPLE_COUNTER.with(|counter| {
//         let next = counter.get().wrapping_add(1);
//         counter.set(next);
//         (next & WAIT_TIME_SAMPLE_MASK) == 0
//     })
// }
//

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
    fn prepare_mcs_node(my_node: *mut Node) {
        unsafe {
            (*my_node).next.store(ptr::null_mut(), Ordering::Relaxed);
            (*my_node).prev.store(ptr::null_mut(), Ordering::Relaxed);
            (*my_node).waiting.store(false, Ordering::Relaxed);
        }
    }

    #[inline(always)]
    fn mcs_wait_next(&self, my_node: *mut Node, old_tail: *mut Node) -> *mut Node {
        loop {
            if self.tail.0.load(Ordering::Acquire) == my_node
                && self
                    .tail
                    .0
                    .compare_exchange(my_node, old_tail, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                return ptr::null_mut();
            }

            let next = unsafe { (*my_node).next.swap(ptr::null_mut(), Ordering::AcqRel) };
            if !next.is_null() {
                return next;
            }
            pause();
        }
    }

    #[inline(always)]
    fn mcs_unqueue(&self, my_node: *mut Node) -> bool {
        let mut prev = unsafe { (*my_node).prev.load(Ordering::Acquire) };
        debug_assert!(!prev.is_null());

        loop {
            let prev_next = unsafe { (*prev).next.load(Ordering::Acquire) };
            if prev_next == my_node
                && unsafe {
                    (*prev).next.compare_exchange(
                        my_node,
                        ptr::null_mut(),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                }
                .is_ok()
            {
                break;
            }

            if !unsafe { (*my_node).waiting.load(Ordering::Acquire) } {
                return true;
            }

            pause();
            prev = unsafe { (*my_node).prev.load(Ordering::Acquire) };
            debug_assert!(!prev.is_null());
        }

        let next = self.mcs_wait_next(my_node, prev);
        if next.is_null() {
            return false;
        }

        unsafe {
            (*next).prev.store(prev, Ordering::Release);
            (*prev).next.store(next, Ordering::Release);
        }
        false
    }

    #[inline(always)]
    fn mcs_exit(&self, my_node: *mut Node) {
        if self
            .tail
            .0
            .compare_exchange(
                my_node,
                ptr::null_mut(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            return;
        }

        let succ = unsafe { (*my_node).next.swap(ptr::null_mut(), Ordering::AcqRel) };
        if !succ.is_null() {
            unsafe {
                (*succ).waiting.store(false, Ordering::Release);
            }
            return;
        }

        let succ = self.mcs_wait_next(my_node, ptr::null_mut());
        if !succ.is_null() {
            unsafe {
                (*succ).waiting.store(false, Ordering::Release);
            }
        }
    }

    #[inline(always)]
    pub fn lock(&self) {
        // Fast path: TAS
        if !self.locked.0.swap(true, Ordering::Acquire) {
            return;
        }

        // Slow path: MCS queue + TAS
        let wait_start = wait_time_start();
        let ctx = thread_ctx();
        unsafe {
            (*ctx).wait_start_ns = wait_time_to_ns(wait_start);
        }

        let my_node = Self::thread_node();
        'slow_path: loop {
            Self::prepare_mcs_node(my_node);

            let pred = self.tail.0.swap(my_node, Ordering::AcqRel);
            let timeslice_requested = timeslice_extension::on_mcs_spin_start();
            if !pred.is_null() {
                unsafe {
                    (*my_node).waiting.store(true, Ordering::Relaxed);
                    (*my_node).prev.store(pred, Ordering::Relaxed);
                }
                fence(Ordering::Release);
                unsafe {
                    (*pred).next.store(my_node, Ordering::Release);
                }

                loop {
                    if !unsafe { (*my_node).waiting.load(Ordering::Acquire) } {
                        break;
                    }

                    if timeslice_requested && timeslice_extension::grant_was_cleared_by_kernel() {
                        if self.mcs_unqueue(my_node) {
                            break;
                        }
                        timeslice_extension::on_mcs_spin_preempted();
                        continue 'slow_path;
                    }
                    pause();
                }
            }

            while self.locked.0.swap(true, Ordering::Acquire) {
                if timeslice_requested && timeslice_extension::grant_was_cleared_by_kernel() {
                    self.mcs_exit(my_node);
                    timeslice_extension::on_mcs_spin_preempted();
                    continue 'slow_path;
                }
                pause();
            }

            if timeslice_requested {
                TIMESLICE_REQUESTED.with(|cell| cell.set(true));
            }
            self.mcs_exit(my_node);

            // Acquired after contention — accumulate sampled wait time, set ROLE_OWNER
            let wait_end = wait_time_start();
            unsafe {
                (*ctx).wait_ns_total += wait_time_elapsed_ns_between(wait_start, wait_end);
                (*ctx).wait_end_ns = wait_time_to_ns(wait_end);
            }
            return;
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
        self.locked.0.store(false, Ordering::Release);
        TIMESLICE_REQUESTED.with(|cell| {
            if cell.replace(false) {
                timeslice_extension::on_critical_section_exit();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcs_exit_clears_tail_without_successor() {
        let lock = McsTasLockRaw::new();
        let node = Box::into_raw(Box::new(Node::new()));

        lock.tail.0.store(node, Ordering::Release);
        lock.mcs_exit(node);

        assert!(lock.tail.0.load(Ordering::Acquire).is_null());

        unsafe {
            drop(Box::from_raw(node));
        }
    }

    #[test]
    fn mcs_exit_wakes_successor_when_present() {
        let lock = McsTasLockRaw::new();
        let node = Box::into_raw(Box::new(Node::new()));
        let succ = Box::into_raw(Box::new(Node::new()));

        unsafe {
            (*succ).waiting.store(true, Ordering::Relaxed);
            (*node).next.store(succ, Ordering::Release);
        }

        lock.mcs_exit(node);

        assert!(!unsafe { (*succ).waiting.load(Ordering::Acquire) });

        unsafe {
            drop(Box::from_raw(succ));
            drop(Box::from_raw(node));
        }
    }

    #[test]
    fn mcs_unqueue_relinks_neighbors() {
        let lock = McsTasLockRaw::new();
        let prev = Box::into_raw(Box::new(Node::new()));
        let node = Box::into_raw(Box::new(Node::new()));
        let next = Box::into_raw(Box::new(Node::new()));

        unsafe {
            (*prev).next.store(node, Ordering::Release);
            (*node).prev.store(prev, Ordering::Release);
            (*node).next.store(next, Ordering::Release);
            (*node).waiting.store(true, Ordering::Release);
            (*next).prev.store(node, Ordering::Release);
        }
        lock.tail.0.store(next, Ordering::Release);

        assert!(!lock.mcs_unqueue(node));
        assert_eq!(unsafe { (*prev).next.load(Ordering::Acquire) }, next);
        assert_eq!(unsafe { (*next).prev.load(Ordering::Acquire) }, prev);

        unsafe {
            drop(Box::from_raw(next));
            drop(Box::from_raw(node));
            drop(Box::from_raw(prev));
        }
    }

    #[test]
    fn mcs_unqueue_rewinds_tail_when_node_is_last() {
        let lock = McsTasLockRaw::new();
        let prev = Box::into_raw(Box::new(Node::new()));
        let node = Box::into_raw(Box::new(Node::new()));

        unsafe {
            (*prev).next.store(node, Ordering::Release);
            (*node).prev.store(prev, Ordering::Release);
            (*node).waiting.store(true, Ordering::Release);
        }
        lock.tail.0.store(node, Ordering::Release);

        assert!(!lock.mcs_unqueue(node));
        assert!(unsafe { (*prev).next.load(Ordering::Acquire) }.is_null());
        assert_eq!(lock.tail.0.load(Ordering::Acquire), prev);

        unsafe {
            drop(Box::from_raw(node));
            drop(Box::from_raw(prev));
        }
    }
}
