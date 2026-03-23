use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering, fence};

use crate::arch::{pause, wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns};
use crate::timeslice_extension;

// we remove sample cause in some high contention env wait can occupy full timeslice
// or we can reserve a fast path in low contention
// // Keep in sync with WAIT_TIME_SAMPLE_STRIDE in src/bpf/intf.h.
// const WAIT_TIME_SAMPLE_STRIDE: u32 = 8;
// const WAIT_TIME_SAMPLE_MASK: u32 = WAIT_TIME_SAMPLE_STRIDE - 1;

#[repr(align(64))]
struct CacheAligned<T>(T);

#[cfg(all(target_os = "linux", target_env = "gnu"))]
const FUTEX_PRIVATE_FLAG: libc::c_int = 128;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const FUTEX_WAIT_PRIVATE: libc::c_int = libc::FUTEX_WAIT | FUTEX_PRIVATE_FLAG;
#[cfg(all(target_os = "linux", target_env = "gnu"))]
const FUTEX_WAKE_PRIVATE: libc::c_int = libc::FUTEX_WAKE | FUTEX_PRIVATE_FLAG;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[inline(always)]
fn futex_wait(word: &AtomicU32, expected: u32) {
    loop {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                FUTEX_WAIT_PRIVATE,
                expected,
                ptr::null::<libc::timespec>(),
            )
        };
        if rc == 0 {
            return;
        }

        let err = unsafe { *libc::__errno_location() };
        if err != libc::EINTR {
            return;
        }
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
#[inline(always)]
fn futex_wait(_word: &AtomicU32, _expected: u32) {
    std::thread::yield_now();
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[inline(always)]
fn futex_wake_one(word: &AtomicU32) {
    let _ = unsafe { libc::syscall(libc::SYS_futex, word.as_ptr(), FUTEX_WAKE_PRIVATE, 1) };
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
#[inline(always)]
fn futex_wake_one(_word: &AtomicU32) {}

const ROLE_NONE: u32 = 0;
const ROLE_OWNER: u32 = 1;

const OWNER_STATE_NONE: u32 = 0;
const OWNER_STATE_RUNNING: u32 = 1;
const OWNER_STATE_PREEMPTED: u32 = 2;

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
    pub role: u32,
    pub _pad: u32,
    pub owner_state_ptr: u64,
}

impl LockSchedThreadCtx {
    const fn new() -> Self {
        Self {
            wait_ns_total: 0,
            wait_start_ns: 0,
            wait_end_ns: 0,
            role: ROLE_NONE,
            _pad: 0,
            owner_state_ptr: 0,
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
    owner_state: CacheAligned<AtomicU32>,
    owner_seq: CacheAligned<AtomicU32>,
    owner_sleepers: CacheAligned<AtomicU32>,
    sleep_seq: CacheAligned<AtomicU32>,
    sleepers: CacheAligned<AtomicU32>,
}

impl McsTasLockRaw {
    pub const fn new() -> Self {
        Self {
            tail: CacheAligned(AtomicPtr::new(ptr::null_mut())),
            locked: CacheAligned(AtomicBool::new(false)),
            owner_state: CacheAligned(AtomicU32::new(OWNER_STATE_NONE)),
            owner_seq: CacheAligned(AtomicU32::new(0)),
            owner_sleepers: CacheAligned(AtomicU32::new(0)),
            sleep_seq: CacheAligned(AtomicU32::new(0)),
            sleepers: CacheAligned(AtomicU32::new(0)),
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
    fn publish_owner(&self) {
        let ctx = thread_ctx();

        self.owner_state.0.store(OWNER_STATE_RUNNING, Ordering::Release);
        unsafe {
            (*ctx).owner_state_ptr = self.owner_state.0.as_ptr() as u64;
            (*ctx).role = ROLE_OWNER;
        }
    }

    #[inline(always)]
    fn clear_owner(&self) {
        let ctx = thread_ctx();

        unsafe {
            (*ctx).role = ROLE_NONE;
            (*ctx).owner_state_ptr = 0;
        }
        self.owner_state.0.store(OWNER_STATE_NONE, Ordering::Release);
    }

    #[inline(always)]
    fn owner_is_preempted(&self) -> bool {
        self.owner_state.0.load(Ordering::Acquire) == OWNER_STATE_PREEMPTED
            && self.locked.0.load(Ordering::Acquire)
    }

    #[inline(always)]
    fn wait_for_preempted_owner(&self) {
        self.owner_sleepers.0.fetch_add(1, Ordering::AcqRel);

        if self.owner_is_preempted() {
            let seq = self.owner_seq.0.load(Ordering::Acquire);
            if self.owner_is_preempted() {
                futex_wait(&self.owner_seq.0, seq);
            }
        }

        self.owner_sleepers.0.fetch_sub(1, Ordering::Release);
    }

    #[inline(always)]
    fn wake_owner_waiters(&self) {
        if self.owner_sleepers.0.load(Ordering::Acquire) == 0 {
            return;
        }

        self.owner_seq.0.fetch_add(1, Ordering::Release);
        futex_wake_one(&self.owner_seq.0);
    }

    #[inline(always)]
    fn wait_for_unlock_change(&self) {
        self.sleepers.0.fetch_add(1, Ordering::AcqRel);

        if self.locked.0.load(Ordering::Acquire) {
            let seq = self.sleep_seq.0.load(Ordering::Acquire);
            if self.locked.0.load(Ordering::Acquire) {
                futex_wait(&self.sleep_seq.0, seq);
            }
        }

        self.sleepers.0.fetch_sub(1, Ordering::Release);
    }

    #[inline(always)]
    fn wake_preempted_waiters(&self) {
        if self.sleepers.0.load(Ordering::Acquire) == 0 {
            return;
        }

        self.sleep_seq.0.fetch_add(1, Ordering::Release);
        futex_wake_one(&self.sleep_seq.0);
    }

    #[inline(always)]
    pub fn lock(&self) {
        // Fast path: TAS
        if !self.locked.0.swap(true, Ordering::Acquire) {
            self.publish_owner();
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

                    if self.owner_is_preempted() {
                        if self.mcs_unqueue(my_node) {
                            break;
                        }
                        if timeslice_requested {
                            timeslice_extension::clear_extension_request();
                        }
                        self.wait_for_preempted_owner();
                        continue 'slow_path;
                    }

                    if timeslice_requested && timeslice_extension::grant_was_cleared_by_kernel() {
                        if self.mcs_unqueue(my_node) {
                            break;
                        }
                        timeslice_extension::clear_extension_request();
                        self.wait_for_unlock_change();
                        continue 'slow_path;
                    }
                    pause();
                }
            }

            loop {
                if self.owner_is_preempted() {
                    self.mcs_exit(my_node);
                    if timeslice_requested {
                        timeslice_extension::clear_extension_request();
                    }
                    self.wait_for_preempted_owner();
                    continue 'slow_path;
                }

                if !self.locked.0.swap(true, Ordering::Acquire) {
                    break;
                }

                if timeslice_requested && timeslice_extension::grant_was_cleared_by_kernel() {
                    self.mcs_exit(my_node);
                    timeslice_extension::on_mcs_spin_preempted();
                    continue 'slow_path;
                }
                pause();
            }

            self.publish_owner();
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
            self.publish_owner();
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn unlock(&self) {
        self.clear_owner();
        self.locked.0.store(false, Ordering::Release);
        self.wake_preempted_waiters();
        self.wake_owner_waiters();
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

    #[test]
    fn publish_owner_updates_lock_and_thread_ctx() {
        let lock = McsTasLockRaw::new();
        let ctx = thread_ctx();

        lock.publish_owner();

        assert_eq!(lock.owner_state.0.load(Ordering::Acquire), OWNER_STATE_RUNNING);
        assert_eq!(unsafe { (*ctx).role }, ROLE_OWNER);
        assert_eq!(
            unsafe { (*ctx).owner_state_ptr },
            lock.owner_state.0.as_ptr() as u64,
        );

        lock.clear_owner();
    }

    #[test]
    fn clear_owner_resets_lock_and_thread_ctx() {
        let lock = McsTasLockRaw::new();
        let ctx = thread_ctx();

        lock.publish_owner();
        lock.clear_owner();

        assert_eq!(lock.owner_state.0.load(Ordering::Acquire), OWNER_STATE_NONE);
        assert_eq!(unsafe { (*ctx).role }, ROLE_NONE);
        assert_eq!(unsafe { (*ctx).owner_state_ptr }, 0);
    }

    #[test]
    fn wake_owner_waiters_advances_owner_seq_when_sleepers_exist() {
        let lock = McsTasLockRaw::new();
        let before = lock.owner_seq.0.load(Ordering::Acquire);

        lock.owner_sleepers.0.store(1, Ordering::Release);
        lock.wake_owner_waiters();

        assert_eq!(lock.owner_seq.0.load(Ordering::Acquire), before + 1);
    }
}
