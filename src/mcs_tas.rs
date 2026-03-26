use std::cell::{Cell, UnsafeCell};
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU8, Ordering};

use crate::arch::{pause, wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns};
use crate::bpf_intf;

const CACHE_LINE_SIZE: usize = bpf_intf::CACHE_LINE_SIZE as usize;
const MAX_THREAD_SLOTS: usize = bpf_intf::MAX_NUMBER_THREADS as usize;
const FUTEX_WAIT_PRIVATE: libc::c_int = 128;
const FUTEX_WAKE_PRIVATE: libc::c_int = 129;

#[repr(align(64))]
struct CacheAligned<T>(T);

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct FlexguardQnode {
    waiting: u8,
    _pad0: [u8; 7],
    next: *mut FlexguardQnode,
    cs_counter: u8,
    _rest: [u8; CACHE_LINE_SIZE - 17],
}

impl FlexguardQnode {
    const fn new() -> Self {
        Self {
            waiting: 0,
            _pad0: [0; 7],
            next: ptr::null_mut(),
            cs_counter: FLEXGUARD_CRITICAL_STATE_NONE,
            _rest: [0; CACHE_LINE_SIZE - 17],
        }
    }
}

struct Runtime {
    qnodes: *mut FlexguardQnode,
    num_preempted_cs: *mut i64,
}

// SAFETY: Runtime only contains leaked pointers to process-long-lived storage.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static NEXT_THREAD_ID: AtomicI32 = AtomicI32::new(1);

pub(crate) const FLEXGUARD_CRITICAL_STATE_NONE: u8 = bpf_intf::FLEXGUARD_CRITICAL_STATE_NONE as u8;
pub(crate) const FLEXGUARD_CRITICAL_STATE_HELD: u8 = bpf_intf::FLEXGUARD_CRITICAL_STATE_HELD as u8;
pub(crate) const FLEXGUARD_CRITICAL_STATE_HANDOFF: u8 =
    bpf_intf::FLEXGUARD_CRITICAL_STATE_HANDOFF as u8;

#[repr(C)]
pub struct LockSchedThreadCtx {
    pub wait_ns_total: u64,
    pub wait_start_ns: u64,
    pub wait_end_ns: u64,
    pub lock_state: u32,
}

impl LockSchedThreadCtx {
    const fn new() -> Self {
        Self {
            wait_ns_total: 0,
            wait_start_ns: 0,
            wait_end_ns: 0,
            lock_state: LockSchedState::None as u32,
        }
    }
}

#[repr(u32)]
enum LockSchedState {
    None = 0,
    Spinner = 1,
    Owner = 2,
}

thread_local! {
    static THREAD_ID: Cell<i32> = const { Cell::new(-1) };
    static THREAD_CTX: UnsafeCell<LockSchedThreadCtx> = const { UnsafeCell::new(LockSchedThreadCtx::new()) };
}

fn allocate_local_runtime() -> Runtime {
    let qnodes = Box::leak(vec![FlexguardQnode::new(); MAX_THREAD_SLOTS].into_boxed_slice());
    let counter = Box::leak(Box::new(0_i64));

    Runtime {
        qnodes: qnodes.as_mut_ptr(),
        num_preempted_cs: counter,
    }
}

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(allocate_local_runtime)
}

pub(crate) fn install_bpf_runtime<T>(qnodes: *mut T, num_preempted_cs: *mut i64) {
    let _ = RUNTIME.set(Runtime {
        qnodes: qnodes.cast::<FlexguardQnode>(),
        num_preempted_cs,
    });
}

#[inline(always)]
fn set_thread_lock_state(state: LockSchedState) {
    THREAD_CTX.with(|ctx| unsafe {
        (*ctx.get()).lock_state = state as u32;
    });
}

pub fn thread_ctx() -> *mut LockSchedThreadCtx {
    THREAD_CTX.with(|ctx| ctx.get())
}

#[inline(always)]
fn record_wait_start(wait_start: u64) {
    THREAD_CTX.with(|ctx| unsafe {
        (*ctx.get()).wait_start_ns = wait_time_to_ns(wait_start);
    });
}

#[inline(always)]
fn record_wait_complete(wait_start: u64, wait_end: u64) {
    THREAD_CTX.with(|ctx| unsafe {
        let ctx = &mut *ctx.get();
        ctx.wait_ns_total = ctx
            .wait_ns_total
            .saturating_add(wait_time_elapsed_ns_between(wait_start, wait_end));
        ctx.wait_end_ns = wait_time_to_ns(wait_end);
    });
}

#[inline(always)]
pub(crate) fn current_thread_index() -> i32 {
    THREAD_ID.with(|slot| {
        let existing = slot.get();
        if existing >= 0 {
            return existing;
        }

        let thread_id = NEXT_THREAD_ID.fetch_add(1, Ordering::AcqRel);
        assert!(
            thread_id > 0 && thread_id < MAX_THREAD_SLOTS as i32,
            "too many threads for FlexGuard runtime",
        );

        let qnode = unsafe { &mut *runtime().qnodes.add(thread_id as usize) };
        qnode.waiting = 0;
        qnode.next = ptr::null_mut();
        qnode.cs_counter = FLEXGUARD_CRITICAL_STATE_NONE;
        slot.set(thread_id);
        thread_id
    })
}

#[inline(always)]
fn thread_qnode() -> *mut FlexguardQnode {
    let thread_id = current_thread_index() as usize;
    unsafe { runtime().qnodes.add(thread_id) }
}

#[inline(always)]
fn blocking_condition() -> bool {
    unsafe { ptr::read_volatile(runtime().num_preempted_cs) != 0 }
}

#[inline(always)]
unsafe fn qnode_next<'a>(qnode: *mut FlexguardQnode) -> &'a AtomicPtr<FlexguardQnode> {
    unsafe { &*ptr::addr_of!((*qnode).next).cast::<AtomicPtr<FlexguardQnode>>() }
}

#[inline(always)]
unsafe fn qnode_waiting<'a>(qnode: *mut FlexguardQnode) -> &'a AtomicU8 {
    unsafe { &*ptr::addr_of!((*qnode).waiting).cast::<AtomicU8>() }
}

#[inline(always)]
unsafe fn qnode_cs_counter<'a>(qnode: *mut FlexguardQnode) -> &'a AtomicU8 {
    unsafe { &*ptr::addr_of!((*qnode).cs_counter).cast::<AtomicU8>() }
}

#[inline(always)]
fn clear_critical_state(qnode: *mut FlexguardQnode) {
    unsafe {
        qnode_cs_counter(qnode).store(FLEXGUARD_CRITICAL_STATE_NONE, Ordering::Release);
    }
}

#[inline(always)]
fn mark_lock_holder(qnode: *mut FlexguardQnode) {
    unsafe {
        qnode_cs_counter(qnode).store(FLEXGUARD_CRITICAL_STATE_HELD, Ordering::Release);
    }
}

#[inline(always)]
fn mark_handoff_thread(qnode: *mut FlexguardQnode) {
    unsafe {
        qnode_cs_counter(qnode).store(FLEXGUARD_CRITICAL_STATE_HANDOFF, Ordering::Release);
    }
}

#[inline(always)]
fn futex_wait(addr: *mut i32, val: i32) -> libc::c_long {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr,
            FUTEX_WAIT_PRIVATE,
            val,
            ptr::null::<libc::timespec>(),
            ptr::null::<libc::c_void>(),
            0,
        )
    }
}

#[inline(always)]
fn futex_wake(addr: *mut i32, count: i32) -> libc::c_long {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr,
            FUTEX_WAKE_PRIVATE,
            count,
            ptr::null::<libc::timespec>(),
            ptr::null::<libc::c_void>(),
            0,
        )
    }
}

pub struct McsTasLockRaw {
    lock_value: CacheAligned<AtomicI32>,
    queue: CacheAligned<AtomicPtr<FlexguardQnode>>,
}

impl McsTasLockRaw {
    pub const fn new() -> Self {
        Self {
            lock_value: CacheAligned(AtomicI32::new(0)),
            queue: CacheAligned(AtomicPtr::new(ptr::null_mut())),
        }
    }

    #[inline(always)]
    fn try_acquire_fast_path(&self) -> bool {
        self.lock_value
            .0
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    #[inline(always)]
    unsafe fn mcs_exit(&self, qnode: *mut FlexguardQnode) {
        let next = unsafe { qnode_next(qnode) };
        if next.load(Ordering::Acquire).is_null() {
            if self
                .queue
                .0
                .compare_exchange(qnode, ptr::null_mut(), Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return;
            }

            while next.load(Ordering::Acquire).is_null() {
                pause();
            }
        }

        let successor = next.load(Ordering::Acquire);
        if !successor.is_null() {
            unsafe {
                qnode_waiting(successor).store(0, Ordering::Release);
            }
        }
    }

    #[inline(always)]
    pub fn try_lock(&self) -> bool {
        let qnode = thread_qnode();
        if self.try_acquire_fast_path() {
            set_thread_lock_state(LockSchedState::Owner);
            mark_lock_holder(qnode);
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn lock(&self) {
        let qnode = thread_qnode();

        if self.try_acquire_fast_path() {
            set_thread_lock_state(LockSchedState::Owner);
            mark_lock_holder(qnode);
            return;
        }

        let wait_start = wait_time_start();
        record_wait_start(wait_start);

        'slow_path: loop {
            let mut enqueued = false;

            if !blocking_condition() {
                enqueued = true;
                unsafe {
                    qnode_next(qnode).store(ptr::null_mut(), Ordering::Relaxed);
                    qnode_waiting(qnode).store(1, Ordering::Relaxed);
                }

                let pred = self.queue.0.swap(qnode, Ordering::SeqCst);
                if pred.is_null() {
                    mark_handoff_thread(qnode);
                } else {
                    unsafe {
                        qnode_next(pred).store(qnode, Ordering::Release);
                    }
                    while unsafe { qnode_waiting(qnode).load(Ordering::Acquire) } != 0
                        && !blocking_condition()
                    {
                        pause();
                    }

                    if unsafe { qnode_waiting(qnode).load(Ordering::Acquire) } == 0 {
                        mark_handoff_thread(qnode);
                    }
                }
            }

            set_thread_lock_state(LockSchedState::Spinner);
            mark_handoff_thread(qnode);

            let mut state = self.lock_value.0.load(Ordering::Acquire);
            if state == 0 {
                state = self
                    .lock_value
                    .0
                    .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                    .unwrap_or_else(|current| current);
            }

            while state != 0 {
                if blocking_condition() {
                    if enqueued {
                        unsafe {
                            self.mcs_exit(qnode);
                        }
                        enqueued = false;
                    }

                    if self.lock_value.0.load(Ordering::Acquire) != 2 {
                        state = self.lock_value.0.swap(2, Ordering::SeqCst);
                    }

                    if state != 0 {
                        let addr = ptr::addr_of!(self.lock_value.0) as *mut AtomicI32 as *mut i32;
                        futex_wait(addr, 2);
                        state = self.lock_value.0.swap(2, Ordering::SeqCst);
                        if state != 0 && !blocking_condition() {
                            continue 'slow_path;
                        }
                    }
                } else {
                    pause();
                    if self.lock_value.0.load(Ordering::Acquire) == 0 {
                        state = self
                            .lock_value
                            .0
                            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                            .unwrap_or_else(|current| current);
                    }
                }
            }

            if enqueued {
                unsafe {
                    self.mcs_exit(qnode);
                }
            }

            let wait_end = wait_time_start();
            record_wait_complete(wait_start, wait_end);
            set_thread_lock_state(LockSchedState::Owner);
            mark_lock_holder(qnode);
            return;
        }
    }

    #[inline(always)]
    pub fn unlock(&self) {
        let qnode = thread_qnode();
        let addr = ptr::addr_of!(self.lock_value.0) as *mut AtomicI32 as *mut i32;

        if self.lock_value.0.swap(0, Ordering::SeqCst) != 1 {
            futex_wake(addr, 1);
        }

        set_thread_lock_state(LockSchedState::None);
        clear_critical_state(qnode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    #[test]
    fn flexguard_qnode_layout_matches_bindgen_contract() {
        assert_eq!(
            std::mem::size_of::<FlexguardQnode>(),
            std::mem::size_of::<bpf_intf::flexguard_qnode_t>(),
        );
        assert_eq!(
            std::mem::align_of::<FlexguardQnode>(),
            std::mem::align_of::<bpf_intf::flexguard_qnode_t>(),
        );
        assert_eq!(offset_of!(FlexguardQnode, waiting), 0);
        assert_eq!(offset_of!(FlexguardQnode, next), 8);
        assert_eq!(offset_of!(FlexguardQnode, cs_counter), 16);
    }

    #[test]
    fn critical_state_markers_update_qnode_state() {
        let mut qnode = FlexguardQnode::new();
        let ptr = ptr::addr_of_mut!(qnode);

        mark_handoff_thread(ptr);
        assert_eq!(qnode.cs_counter, FLEXGUARD_CRITICAL_STATE_HANDOFF);

        mark_lock_holder(ptr);
        assert_eq!(qnode.cs_counter, FLEXGUARD_CRITICAL_STATE_HELD);

        clear_critical_state(ptr);
        assert_eq!(qnode.cs_counter, FLEXGUARD_CRITICAL_STATE_NONE);
    }

    #[test]
    fn new_lock_starts_unlocked_and_without_queue() {
        let lock = McsTasLockRaw::new();

        assert_eq!(lock.lock_value.0.load(Ordering::Relaxed), 0);
        assert!(lock.queue.0.load(Ordering::Relaxed).is_null());
    }

    #[test]
    fn thread_ctx_layout_matches_bindgen_contract() {
        assert_eq!(
            std::mem::size_of::<LockSchedThreadCtx>(),
            std::mem::size_of::<bpf_intf::lock_sched_thread_ctx>(),
        );
        assert_eq!(
            std::mem::align_of::<LockSchedThreadCtx>(),
            std::mem::align_of::<bpf_intf::lock_sched_thread_ctx>(),
        );
        assert_eq!(offset_of!(LockSchedThreadCtx, wait_ns_total), 0);
        assert_eq!(offset_of!(LockSchedThreadCtx, wait_start_ns), 8);
        assert_eq!(offset_of!(LockSchedThreadCtx, wait_end_ns), 16);
        assert_eq!(offset_of!(LockSchedThreadCtx, lock_state), 24);
    }
}
