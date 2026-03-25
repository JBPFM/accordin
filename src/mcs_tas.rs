use std::cell::Cell;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU8, Ordering};

use crate::arch::pause;
use crate::bpf_intf;

const CACHE_LINE_SIZE: usize = bpf_intf::CACHE_LINE_SIZE as usize;
const MAX_THREAD_SLOTS: usize = bpf_intf::MAX_NUMBER_THREADS as usize;

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

thread_local! {
    static THREAD_ID: Cell<i32> = const { Cell::new(-1) };
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
fn sched_yield() {
    unsafe {
        libc::sched_yield();
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
            mark_lock_holder(qnode);
            return;
        }

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

                    sched_yield();

                    state = self.lock_value.0.load(Ordering::Acquire);
                    if state == 0 {
                        state = self
                            .lock_value
                            .0
                            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                            .unwrap_or_else(|current| current);
                    }

                    if state != 0 && !blocking_condition() {
                        continue 'slow_path;
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

            mark_lock_holder(qnode);
            return;
        }
    }

    #[inline(always)]
    pub fn unlock(&self) {
        let qnode = thread_qnode();
        self.lock_value.0.store(0, Ordering::SeqCst);
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
}
