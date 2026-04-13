use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::arch::pause;
use crate::lock_backend::LockBackend;
use crate::lock_stats::{
    ADMISSION_CPU_NONE, clear_admission_state, grant_slow_path_admission,
    mark_critical_section_entered, mark_slow_path_pending, thread_has_admission,
};

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
    // static WAIT_SAMPLE_COUNTER: Cell<u32> = const { Cell::new(0) };
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
    fn current_cpu() -> u32 {
        let cpu = unsafe { libc::sched_getcpu() };
        assert!(cpu >= 0, "sched_getcpu failed while caching slow-path admission");
        let cpu = cpu as u32;
        assert!(
            cpu != ADMISSION_CPU_NONE,
            "current CPU collided with admission sentinel"
        );
        cpu
    }

    #[inline(always)]
    fn ensure_slow_path_admission(&self) {
        mark_slow_path_pending();
        if thread_has_admission() {
            return;
        }

        std::thread::yield_now();
        grant_slow_path_admission(Self::current_cpu());
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn lock_slow(&self) {
        self.ensure_slow_path_admission();

        // prepare node and set pred
        let my_node = Self::thread_node();
        unsafe {
            (*my_node).next.store(ptr::null_mut(), Ordering::Relaxed);
            (*my_node).waiting.store(false, Ordering::Relaxed);
        }

        let pred = self.tail.0.swap(my_node, Ordering::AcqRel);
        if !pred.is_null() {
            // mcs spin loop
            unsafe {
                (*my_node).waiting.store(true, Ordering::Relaxed);
                (*pred).next.store(my_node, Ordering::Release);
                while (*my_node).waiting.load(Ordering::Acquire) {
                    pause();
                }
            }
        }

        // front spinning
        while self.locked.0.swap(true, Ordering::Acquire) {
            pause();
        }

        // set next node
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

        mark_critical_section_entered();
    }

    /// Returns true if the lock was acquired, false if it was already held.
    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn try_lock_fast(&self) -> bool {
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

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock_fast(&self) {
        self.locked.0.store(false, Ordering::Release);
        clear_admission_state();
    }
}

impl LockBackend for McsTasLockRaw {
    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn lock(&self) {
        self.lock_slow();
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn try_lock(&self) -> bool {
        self.try_lock_fast()
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock(&self) {
        self.unlock_fast();
    }
}

#[cfg(test)]
mod tests {
    use super::Node;
    use crate::bpf_intf;
    use crate::lock_stats::{ADMISSION_CPU_NONE, admission_state_snapshot, clear_admission_state};

    #[test]
    fn node_layout_stays_cache_aligned() {
        assert_eq!(std::mem::size_of::<Node>(), 64);
        assert_eq!(std::mem::align_of::<Node>(), 64);
    }

    #[test]
    fn thread_ctx_layout_matches_bindgen_contract() {
        assert_eq!(
            std::mem::size_of::<crate::lock_stats::LockSchedThreadCtx>(),
            std::mem::size_of::<bpf_intf::lock_sched_thread_ctx>(),
        );
        assert_eq!(
            std::mem::align_of::<crate::lock_stats::LockSchedThreadCtx>(),
            std::mem::align_of::<bpf_intf::lock_sched_thread_ctx>(),
        );
    }

    #[test]
    fn clearing_admission_restores_sentinel_cpu() {
        clear_admission_state();
        let snapshot = admission_state_snapshot();
        assert!(!snapshot.admission_owned);
        assert_eq!(snapshot.admission_cpu, ADMISSION_CPU_NONE);
        assert!(!snapshot.in_critical_section);
        assert!(!snapshot.slow_path_pending);
    }
}
