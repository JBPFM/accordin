use std::cell::Cell;
use std::ptr;
use std::sync::OnceLock;
use std::sync::atomic::{
    AtomicI32, AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};

use crate::arch::pause;
use crate::bpf_intf;

const CACHE_LINE_SIZE: usize = bpf_intf::CACHE_LINE_SIZE as usize;
const MAX_THREAD_SLOTS: usize = bpf_intf::MAX_NUMBER_THREADS as usize;
const FUTEX_WAIT_PRIVATE: libc::c_int = 128;
const FUTEX_WAKE_PRIVATE: libc::c_int = 129;
const FRONT_RUNNER_EMPTY: u64 = 0;
const QNODE_NEXT_NONE: usize = 0;
const QNODE_NEXT_PARKED: usize = 1;
const LOCK_VALUE_UNLOCKED: i32 = 0;
const LOCK_VALUE_LOCKED: i32 = 1;
const LOCK_VALUE_CONTENDED: i32 = 2;

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
    num_preempted_holders: *mut i64,
    preempted_flags: *mut u8,
    qnode_generations: *mut AtomicU32,
}

// SAFETY: Runtime only contains leaked pointers to process-long-lived storage.
unsafe impl Send for Runtime {}
unsafe impl Sync for Runtime {}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static NEXT_THREAD_ID: AtomicI32 = AtomicI32::new(1);

pub(crate) const FLEXGUARD_CRITICAL_STATE_NONE: u8 = bpf_intf::FLEXGUARD_CRITICAL_STATE_NONE as u8;
pub(crate) const FLEXGUARD_CRITICAL_STATE_HELD: u8 = bpf_intf::FLEXGUARD_CRITICAL_STATE_HELD as u8;
pub(crate) const FLEXGUARD_CRITICAL_STATE_FRONT: u8 =
    bpf_intf::FLEXGUARD_CRITICAL_STATE_FRONT as u8;

thread_local! {
    static THREAD_ID: Cell<i32> = const { Cell::new(-1) };
}

fn allocate_local_runtime() -> Runtime {
    let qnodes = Box::leak(vec![FlexguardQnode::new(); MAX_THREAD_SLOTS].into_boxed_slice());
    let counter = Box::leak(Box::new(0_i64));
    let preempted_flags = Box::leak(vec![0_u8; MAX_THREAD_SLOTS].into_boxed_slice());

    Runtime {
        qnodes: qnodes.as_mut_ptr(),
        num_preempted_holders: counter,
        preempted_flags: preempted_flags.as_mut_ptr(),
        qnode_generations: allocate_generation_slots(),
    }
}

fn allocate_generation_slots() -> *mut AtomicU32 {
    let generations = std::iter::repeat_with(|| AtomicU32::new(0))
        .take(MAX_THREAD_SLOTS)
        .collect::<Vec<_>>()
        .into_boxed_slice();
    Box::leak(generations).as_mut_ptr()
}

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(allocate_local_runtime)
}

pub(crate) fn install_bpf_runtime<T>(
    qnodes: *mut T,
    num_preempted_holders: *mut i64,
    preempted_flags: *mut u8,
) {
    let _ = RUNTIME.set(Runtime {
        qnodes: qnodes.cast::<FlexguardQnode>(),
        num_preempted_holders,
        preempted_flags,
        qnode_generations: allocate_generation_slots(),
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

        initialize_qnode_slot(thread_id as usize);
        slot.set(thread_id);
        thread_id
    })
}

#[inline(always)]
fn initialize_qnode_slot(index: usize) -> *mut FlexguardQnode {
    let qnode = unsafe { runtime().qnodes.add(index) };
    unsafe {
        qnode_waiting(qnode).store(0, Ordering::Relaxed);
        qnode_next_word(qnode).store(QNODE_NEXT_NONE, Ordering::Relaxed);
        qnode_cs_counter(qnode).store(FLEXGUARD_CRITICAL_STATE_NONE, Ordering::Relaxed);
        ptr::write_volatile(runtime().preempted_flags.add(index), 0);
        (&*runtime().qnode_generations.add(index)).store(0, Ordering::Relaxed);
    }
    qnode
}

#[inline(always)]
fn thread_qnode() -> *mut FlexguardQnode {
    let thread_id = current_thread_index() as usize;
    unsafe { runtime().qnodes.add(thread_id) }
}

#[inline(always)]
fn holder_preempted() -> bool {
    unsafe { ptr::read_volatile(runtime().num_preempted_holders) != 0 }
}

unsafe fn qnode_next_word<'a>(qnode: *mut FlexguardQnode) -> &'a AtomicUsize {
    unsafe { &*ptr::addr_of!((*qnode).next).cast::<AtomicUsize>() }
}

#[inline(always)]
unsafe fn qnode_waiting<'a>(qnode: *mut FlexguardQnode) -> &'a AtomicU8 {
    unsafe { &*ptr::addr_of!((*qnode).waiting).cast::<AtomicU8>() }
}

#[inline(always)]
unsafe fn qnode_cs_counter<'a>(qnode: *mut FlexguardQnode) -> &'a AtomicU8 {
    unsafe { &*ptr::addr_of!((*qnode).cs_counter).cast::<AtomicU8>() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QnodeNextState {
    None,
    Parked,
    Successor(*mut FlexguardQnode),
}

#[inline(always)]
fn decode_qnode_next(raw: usize) -> QnodeNextState {
    match raw {
        QNODE_NEXT_NONE => QnodeNextState::None,
        QNODE_NEXT_PARKED => QnodeNextState::Parked,
        successor => QnodeNextState::Successor(successor as *mut FlexguardQnode),
    }
}

#[inline(always)]
fn encode_qnode_next(successor: *mut FlexguardQnode) -> usize {
    debug_assert!(!successor.is_null());
    debug_assert_ne!(successor as usize, QNODE_NEXT_PARKED);
    successor as usize
}

#[inline(always)]
unsafe fn qnode_next_futex_addr(qnode: *mut FlexguardQnode) -> *mut i32 {
    unsafe { ptr::addr_of_mut!((*qnode).next).cast::<i32>() }
}

#[inline(always)]
unsafe fn link_successor(pred: *mut FlexguardQnode, successor: *mut FlexguardQnode) -> bool {
    let previous =
        unsafe { qnode_next_word(pred) }.swap(encode_qnode_next(successor), Ordering::AcqRel);
    debug_assert!(
        previous == QNODE_NEXT_NONE || previous == QNODE_NEXT_PARKED,
        "predecessor next should only be empty or parked before successor link",
    );
    previous == QNODE_NEXT_PARKED
}

#[inline(always)]
fn qnode_index(qnode: *mut FlexguardQnode) -> usize {
    let index = unsafe { qnode.offset_from(runtime().qnodes) };
    debug_assert!(index >= 0 && index < MAX_THREAD_SLOTS as isize);
    index as usize
}

#[inline(always)]
fn qnode_state(qnode: *mut FlexguardQnode) -> u8 {
    unsafe { qnode_cs_counter(qnode).load(Ordering::Acquire) }
}

#[inline(always)]
fn qnode_generation(index: usize) -> u32 {
    unsafe { (&*runtime().qnode_generations.add(index)).load(Ordering::Acquire) }
}

#[inline(always)]
fn advance_qnode_generation(qnode: *mut FlexguardQnode) -> u32 {
    let index = qnode_index(qnode);
    unsafe {
        // Each qnode slot has a single writer: the owning thread that reuses it.
        // Other threads only read the generation to reject stale front-runner tokens.
        let generation = &*runtime().qnode_generations.add(index);
        let next = generation.load(Ordering::Relaxed).wrapping_add(1);
        generation.store(next, Ordering::Release);
        next
    }
}

#[inline(always)]
fn qnode_preempted(qnode: *mut FlexguardQnode) -> bool {
    let index = qnode_index(qnode);
    unsafe { ptr::read_volatile(runtime().preempted_flags.add(index)) != 0 }
}

#[inline(always)]
fn encode_front_runner(front: *mut FlexguardQnode) -> u64 {
    let index = qnode_index(front) as u64;
    debug_assert!(index != 0);
    ((qnode_generation(index as usize) as u64) << 32) | index
}

#[inline(always)]
fn decode_front_runner(token: u64) -> Option<(usize, u32)> {
    if token == FRONT_RUNNER_EMPTY {
        return None;
    }

    let index = (token & u32::MAX as u64) as usize;
    if index == 0 || index >= MAX_THREAD_SLOTS {
        return None;
    }

    Some((index, (token >> 32) as u32))
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
fn mark_front_runner(qnode: *mut FlexguardQnode) {
    unsafe {
        qnode_cs_counter(qnode).store(FLEXGUARD_CRITICAL_STATE_FRONT, Ordering::Release);
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
    front_runner: CacheAligned<AtomicU64>,
}

impl McsTasLockRaw {
    pub const fn new() -> Self {
        Self {
            lock_value: CacheAligned(AtomicI32::new(LOCK_VALUE_UNLOCKED)),
            queue: CacheAligned(AtomicPtr::new(ptr::null_mut())),
            front_runner: CacheAligned(AtomicU64::new(FRONT_RUNNER_EMPTY)),
        }
    }

    #[inline(always)]
    fn try_acquire_fast_path(&self) -> bool {
        self.lock_value
            .0
            .compare_exchange(
                LOCK_VALUE_UNLOCKED,
                LOCK_VALUE_LOCKED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    #[inline(always)]
    fn publish_front_runner(&self, front: *mut FlexguardQnode) {
        self.front_runner
            .0
            .store(encode_front_runner(front), Ordering::Release);
    }

    #[inline(always)]
    fn set_front_runner(&self, front: *mut FlexguardQnode) {
        mark_front_runner(front);
        self.publish_front_runner(front);
    }

    #[inline(always)]
    unsafe fn release_successor(&self, successor: *mut FlexguardQnode) {
        self.set_front_runner(successor);
        unsafe { qnode_waiting(successor).store(0, Ordering::Release) };
    }

    #[inline(always)]
    fn front_runner_blocked(&self) -> bool {
        loop {
            let token = self.front_runner.0.load(Ordering::Acquire);
            let Some((index, generation)) = decode_front_runner(token) else {
                return false;
            };

            if qnode_generation(index) == generation {
                let front = unsafe { runtime().qnodes.add(index) };
                if qnode_state(front) == FLEXGUARD_CRITICAL_STATE_FRONT {
                    return qnode_preempted(front);
                }
            }

            match self.front_runner.0.compare_exchange(
                token,
                FRONT_RUNNER_EMPTY,
                Ordering::SeqCst,
                Ordering::Acquire,
            ) {
                Ok(_) => return false,
                Err(current) if current != token => continue,
                Err(_) => return false,
            }
        }
    }

    #[inline(always)]
    fn should_enqueue_mcs(&self) -> bool {
        if holder_preempted() {
            return false;
        }

        !self.front_runner_blocked()
    }

    #[inline(always)]
    fn phase2_blocking(&self) -> bool {
        holder_preempted() || self.front_runner_blocked()
    }

    #[inline(always)]
    unsafe fn mcs_exit(&self, qnode: *mut FlexguardQnode) {
        let next = unsafe { qnode_next_word(qnode) };
        if matches!(
            decode_qnode_next(next.load(Ordering::Acquire)),
            QnodeNextState::None
        ) {
            if self
                .queue
                .0
                .compare_exchange(qnode, ptr::null_mut(), Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return;
            }

            while matches!(
                decode_qnode_next(next.load(Ordering::Acquire)),
                QnodeNextState::None
            ) {
                pause();
            }
        }

        if let QnodeNextState::Successor(successor) =
            decode_qnode_next(next.load(Ordering::Acquire))
        {
            unsafe { self.release_successor(successor) };
        }
    }

    #[inline(always)]
    unsafe fn mcs_exit_blocking(&self, qnode: *mut FlexguardQnode) {
        let next = unsafe { qnode_next_word(qnode) };

        loop {
            match decode_qnode_next(next.load(Ordering::Acquire)) {
                QnodeNextState::Successor(successor) => {
                    unsafe { self.release_successor(successor) };
                    return;
                }
                QnodeNextState::Parked => {
                    futex_wait(
                        unsafe { qnode_next_futex_addr(qnode) },
                        QNODE_NEXT_PARKED as i32,
                    );
                }
                QnodeNextState::None => {
                    if self
                        .queue
                        .0
                        .compare_exchange(
                            qnode,
                            ptr::null_mut(),
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        return;
                    }

                    match next.compare_exchange(
                        QNODE_NEXT_NONE,
                        QNODE_NEXT_PARKED,
                        Ordering::SeqCst,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            while next.load(Ordering::Acquire) == QNODE_NEXT_PARKED {
                                futex_wait(
                                    unsafe { qnode_next_futex_addr(qnode) },
                                    QNODE_NEXT_PARKED as i32,
                                );
                            }
                        }
                        Err(raw) => {
                            if matches!(decode_qnode_next(raw), QnodeNextState::Successor(_)) {
                                continue;
                            }
                        }
                    }
                }
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

            if self.should_enqueue_mcs() {
                enqueued = true;
                advance_qnode_generation(qnode);
                unsafe {
                    qnode_next_word(qnode).store(QNODE_NEXT_NONE, Ordering::Relaxed);
                    qnode_waiting(qnode).store(1, Ordering::Relaxed);
                }

                let pred = self.queue.0.swap(qnode, Ordering::SeqCst);
                if pred.is_null() {
                    self.set_front_runner(qnode);
                } else {
                    if unsafe { link_successor(pred, qnode) } {
                        futex_wake(unsafe { qnode_next_futex_addr(pred) }, 1);
                    }

                    while unsafe { qnode_waiting(qnode).load(Ordering::Acquire) } != 0 {
                        if self.phase2_blocking() {
                            break;
                        }
                        pause();
                    }

                    if unsafe { qnode_waiting(qnode).load(Ordering::Acquire) } == 0 {
                        self.set_front_runner(qnode);
                    }
                }
            }

            let mut state = self.lock_value.0.load(Ordering::Acquire);
            if state == LOCK_VALUE_UNLOCKED {
                state = self
                    .lock_value
                    .0
                    .compare_exchange(
                        LOCK_VALUE_UNLOCKED,
                        LOCK_VALUE_LOCKED,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .unwrap_or_else(|current| current);
            }

            while state != LOCK_VALUE_UNLOCKED {
                if self.phase2_blocking() {
                    if enqueued {
                        unsafe {
                            self.mcs_exit_blocking(qnode);
                        }
                        enqueued = false;
                    }

                    if self.lock_value.0.load(Ordering::Acquire) != LOCK_VALUE_CONTENDED {
                        state = self
                            .lock_value
                            .0
                            .swap(LOCK_VALUE_CONTENDED, Ordering::SeqCst);
                    }

                    if state != LOCK_VALUE_UNLOCKED {
                        let addr = ptr::addr_of!(self.lock_value.0) as *mut AtomicI32 as *mut i32;
                        futex_wait(addr, LOCK_VALUE_CONTENDED);
                        state = self
                            .lock_value
                            .0
                            .swap(LOCK_VALUE_CONTENDED, Ordering::SeqCst);
                        if state != LOCK_VALUE_UNLOCKED && !self.phase2_blocking() {
                            continue 'slow_path;
                        }
                    }
                } else {
                    pause();
                    if self.lock_value.0.load(Ordering::Acquire) == LOCK_VALUE_UNLOCKED {
                        state = self
                            .lock_value
                            .0
                            .compare_exchange(
                                LOCK_VALUE_UNLOCKED,
                                LOCK_VALUE_LOCKED,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            )
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
        let addr = ptr::addr_of!(self.lock_value.0) as *mut AtomicI32 as *mut i32;

        if self
            .lock_value
            .0
            .swap(LOCK_VALUE_UNLOCKED, Ordering::SeqCst)
            != LOCK_VALUE_LOCKED
        {
            futex_wake(addr, 1);
        }

        clear_critical_state(qnode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;
    use std::sync::{Mutex, MutexGuard};

    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn lock_test_guard() -> MutexGuard<'static, ()> {
        TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset_lock(lock: &McsTasLockRaw) {
        lock.lock_value
            .0
            .store(LOCK_VALUE_UNLOCKED, Ordering::Relaxed);
        lock.queue.0.store(ptr::null_mut(), Ordering::Relaxed);
        lock.front_runner
            .0
            .store(FRONT_RUNNER_EMPTY, Ordering::Relaxed);
        unsafe {
            ptr::write_volatile(runtime().num_preempted_holders, 0);
        }
    }

    fn reset_qnode_slot(index: usize) -> *mut FlexguardQnode {
        initialize_qnode_slot(index)
    }

    #[test]
    fn flexguard_qnode_layout_matches_bindgen_contract() {
        let _guard = lock_test_guard();
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
        let _guard = lock_test_guard();
        let mut qnode = FlexguardQnode::new();
        let ptr = ptr::addr_of_mut!(qnode);

        mark_front_runner(ptr);
        assert_eq!(qnode.cs_counter, FLEXGUARD_CRITICAL_STATE_FRONT);

        mark_lock_holder(ptr);
        assert_eq!(qnode.cs_counter, FLEXGUARD_CRITICAL_STATE_HELD);

        clear_critical_state(ptr);
        assert_eq!(qnode.cs_counter, FLEXGUARD_CRITICAL_STATE_NONE);
    }

    #[test]
    fn new_lock_starts_unlocked_and_without_queue() {
        let _guard = lock_test_guard();
        let lock = McsTasLockRaw::new();
        reset_lock(&lock);

        assert_eq!(
            lock.lock_value.0.load(Ordering::Relaxed),
            LOCK_VALUE_UNLOCKED
        );
        assert!(lock.queue.0.load(Ordering::Relaxed).is_null());
        assert_eq!(
            lock.front_runner.0.load(Ordering::Relaxed),
            FRONT_RUNNER_EMPTY
        );
    }

    #[test]
    fn stale_front_runner_token_does_not_follow_reused_qnode_generation() {
        let _guard = lock_test_guard();
        let lock = McsTasLockRaw::new();
        reset_lock(&lock);
        let pred = reset_qnode_slot(1);

        advance_qnode_generation(pred);
        mark_front_runner(pred);
        unsafe {
            ptr::write_volatile(runtime().preempted_flags.add(1), 1);
        }
        lock.publish_front_runner(pred);

        clear_critical_state(pred);
        unsafe {
            ptr::write_volatile(runtime().preempted_flags.add(1), 0);
        }

        advance_qnode_generation(pred);
        mark_front_runner(pred);
        unsafe {
            ptr::write_volatile(runtime().preempted_flags.add(1), 1);
        }

        assert!(
            !lock.front_runner_blocked(),
            "stale front-runner token should not reactivate when the same qnode slot is reused",
        );
    }

    #[test]
    fn front_runner_blocked_detects_preempted_front_waiter() {
        let _guard = lock_test_guard();
        let lock = McsTasLockRaw::new();
        reset_lock(&lock);
        let front = reset_qnode_slot(1);

        advance_qnode_generation(front);
        mark_front_runner(front);
        lock.publish_front_runner(front);
        unsafe {
            ptr::write_volatile(runtime().preempted_flags.add(1), 1);
        }

        assert!(
            lock.front_runner_blocked(),
            "published front-runner token should report a preempted front waiter directly",
        );
    }

    #[test]
    fn advance_qnode_generation_wraps_at_u32_max() {
        let _guard = lock_test_guard();
        let qnode = unsafe { runtime().qnodes.add(1) };

        unsafe {
            (&*runtime().qnode_generations.add(1)).store(u32::MAX, Ordering::Relaxed);
        }

        assert_eq!(advance_qnode_generation(qnode), 0);
        assert_eq!(qnode_generation(1), 0);
    }

    #[test]
    fn link_successor_detects_parked_predecessor() {
        let _guard = lock_test_guard();
        let mut predecessor = FlexguardQnode::new();
        let mut successor = FlexguardQnode::new();
        let pred = ptr::addr_of_mut!(predecessor);
        let succ = ptr::addr_of_mut!(successor);

        unsafe {
            qnode_next_word(pred).store(QNODE_NEXT_PARKED, Ordering::Relaxed);
        }

        assert!(unsafe { link_successor(pred, succ) });
        assert_eq!(
            unsafe { qnode_next_word(pred).load(Ordering::Acquire) },
            succ as usize
        );
    }

    #[test]
    fn blocking_exit_waits_for_late_successor_and_hands_off() {
        let _guard = lock_test_guard();
        let lock = McsTasLockRaw::new();
        reset_lock(&lock);
        let exiting_ptr = reset_qnode_slot(1);
        let successor_ptr = reset_qnode_slot(2);

        lock.queue.0.store(successor_ptr, Ordering::Relaxed);
        unsafe {
            qnode_waiting(successor_ptr).store(1, Ordering::Relaxed);
        }

        let exiting_addr = exiting_ptr as usize;
        let successor_addr = successor_ptr as usize;
        let linker = std::thread::spawn(move || {
            let exiting_ptr = exiting_addr as *mut FlexguardQnode;
            let successor_ptr = successor_addr as *mut FlexguardQnode;

            std::thread::sleep(std::time::Duration::from_millis(10));
            if unsafe { link_successor(exiting_ptr, successor_ptr) } {
                futex_wake(unsafe { qnode_next_futex_addr(exiting_ptr) }, 1);
            }
        });

        unsafe {
            lock.mcs_exit_blocking(exiting_ptr);
        }
        linker.join().expect("linker thread should finish");

        assert_eq!(
            unsafe { qnode_waiting(successor_ptr).load(Ordering::Acquire) },
            0,
            "blocking exit should eventually hand off to the late successor",
        );
        assert_eq!(
            unsafe { qnode_next_word(exiting_ptr).load(Ordering::Acquire) },
            successor_ptr as usize,
            "late successor should become visible in the exiting qnode next slot",
        );
    }

    #[test]
    fn handoff_publishes_successor_as_front_runner_before_it_runs() {
        let _guard = lock_test_guard();
        let lock = McsTasLockRaw::new();
        reset_lock(&lock);
        let predecessor = reset_qnode_slot(1);
        let successor = reset_qnode_slot(2);
        unsafe {
            qnode_next_word(predecessor).store(successor as usize, Ordering::Relaxed);
            qnode_waiting(successor).store(1, Ordering::Relaxed);
        }
        advance_qnode_generation(successor);

        unsafe {
            lock.mcs_exit(predecessor);
        }

        assert_eq!(
            qnode_state(successor),
            FLEXGUARD_CRITICAL_STATE_FRONT,
            "handoff should publish the successor as the current front-runner before it gets CPU time",
        );

        unsafe {
            ptr::write_volatile(runtime().preempted_flags.add(2), 1);
        }
        assert!(
            lock.front_runner_blocked(),
            "published successor should immediately become observable as a blocked front-runner",
        );
    }

    #[test]
    fn holder_preempted_skips_mcs_enqueue_even_without_front_runner_signal() {
        let _guard = lock_test_guard();
        let lock = McsTasLockRaw::new();
        reset_lock(&lock);

        unsafe {
            ptr::write_volatile(runtime().num_preempted_holders, 1);
        }

        assert!(
            !lock.should_enqueue_mcs(),
            "holder-preempted fallback should skip queue churn before any front-runner signal becomes visible",
        );

        unsafe {
            ptr::write_volatile(runtime().num_preempted_holders, 0);
        }
    }

    #[test]
    fn front_runner_blocked_skips_enqueue_without_direct_successor_relay() {
        let _guard = lock_test_guard();
        let lock = McsTasLockRaw::new();
        reset_lock(&lock);
        let front = reset_qnode_slot(1);

        advance_qnode_generation(front);
        mark_front_runner(front);
        lock.publish_front_runner(front);
        unsafe {
            ptr::write_volatile(runtime().preempted_flags.add(1), 1);
        }

        assert!(
            !lock.should_enqueue_mcs(),
            "new arrivals should skip MCS when the published front waiter is already preempted",
        );
    }
}
