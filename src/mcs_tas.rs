use anyhow::{Context, anyhow, ensure};
use std::cell::{Cell, UnsafeCell};
use std::os::fd::{AsFd, AsRawFd};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering, fence};
use std::sync::{Mutex, OnceLock};

use crate::arch::{pause, wait_time_elapsed_ns_between, wait_time_start, wait_time_to_ns};
use crate::bpf_intf;
use crate::timeslice_extension;
use libbpf_rs::{MapCore, MapHandle};

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

const OWNER_SLOT_NONE: u32 = bpf_intf::OWNER_SLOT_NONE;
const OWNER_STATE_NONE: u32 = bpf_intf::OWNER_STATE_NONE;
const OWNER_STATE_RUNNING: u32 = bpf_intf::OWNER_STATE_RUNNING;
const OWNER_STATE_PREEMPTED: u32 = bpf_intf::OWNER_STATE_PREEMPTED;

struct MmapOwnerStateBacking {
    _map: MapHandle,
    ptr: *mut libc::c_void,
    len: usize,
}

impl Drop for MmapOwnerStateBacking {
    fn drop(&mut self) {
        if self.len == 0 {
            return;
        }

        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

enum OwnerStateBacking {
    Mmap {
        _mmap: MmapOwnerStateBacking,
    },
    #[cfg(test)]
    Boxed {
        _boxed: Box<[u32]>,
    },
}

struct OwnerStateRegistry {
    _backing: OwnerStateBacking,
    base: NonNull<u32>,
    max_entries: u32,
    next_slot: AtomicU32,
    free_slots: Mutex<Vec<u32>>,
}

// SAFETY: the registry exposes shared map-backed state through atomics and
// protects slot recycling with a mutex.
unsafe impl Send for OwnerStateRegistry {}
unsafe impl Sync for OwnerStateRegistry {}

struct SharedOwnerState {
    registry: &'static OwnerStateRegistry,
    slot: u32,
    ptr: NonNull<u32>,
}

enum OwnerStateStorage {
    Shared(SharedOwnerState),
    Local(Box<AtomicU32>),
}

static OWNER_STATE_REGISTRY: OnceLock<OwnerStateRegistry> = OnceLock::new();

fn round_up(value: usize, align: usize) -> anyhow::Result<usize> {
    ensure!(align != 0, "alignment must be non-zero");
    let remainder = value % align;
    if remainder == 0 {
        return Ok(value);
    }

    value
        .checked_add(align - remainder)
        .ok_or_else(|| anyhow!("value {value} overflows when rounding to {align}"))
}

fn owner_state_map_mmap_len(map: &MapHandle) -> anyhow::Result<usize> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) };
    ensure!(page_size > 0, "failed to determine page size");

    let value_size = round_up(map.value_size() as usize, 8)?;
    let values_len = value_size
        .checked_mul(map.max_entries() as usize)
        .ok_or_else(|| anyhow!("owner_state_map size overflows"))?;
    round_up(values_len, page_size as usize)
}

impl OwnerStateRegistry {
    fn from_map(map: MapHandle) -> anyhow::Result<Self> {
        ensure!(
            map.value_size() as usize == std::mem::size_of::<u32>(),
            "owner_state_map value_size must be 4 bytes, got {}",
            map.value_size(),
        );
        ensure!(
            map.max_entries() > OWNER_SLOT_NONE + 1,
            "owner_state_map must expose at least one usable slot",
        );

        let max_entries = map.max_entries();
        let mmap_len = owner_state_map_mmap_len(&map)?;
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mmap_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                map.as_fd().as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error()).context("failed to mmap owner_state_map");
        }

        let base = NonNull::new(ptr.cast::<u32>())
            .ok_or_else(|| anyhow!("owner_state_map mmap returned a null pointer"))?;

        Ok(Self {
            _backing: OwnerStateBacking::Mmap {
                _mmap: MmapOwnerStateBacking {
                    _map: map,
                    ptr,
                    len: mmap_len,
                },
            },
            base,
            max_entries,
            next_slot: AtomicU32::new(OWNER_SLOT_NONE + 1),
            free_slots: Mutex::new(Vec::new()),
        })
    }

    #[cfg(test)]
    fn new_for_tests(max_entries: u32) -> Self {
        let mut backing = vec![0u32; max_entries as usize].into_boxed_slice();
        let base = NonNull::new(backing.as_mut_ptr()).expect("test owner-state backing");

        Self {
            _backing: OwnerStateBacking::Boxed { _boxed: backing },
            base,
            max_entries,
            next_slot: AtomicU32::new(OWNER_SLOT_NONE + 1),
            free_slots: Mutex::new(Vec::new()),
        }
    }

    fn slot_ptr(&self, slot: u32) -> NonNull<u32> {
        debug_assert!(slot < self.max_entries);
        let ptr = unsafe { self.base.as_ptr().add(slot as usize) };
        NonNull::new(ptr).expect("owner-state slot pointer")
    }

    fn alloc_slot(&'static self) -> Option<SharedOwnerState> {
        let slot = self
            .free_slots
            .lock()
            .expect("owner-state free list poisoned")
            .pop()
            .unwrap_or_else(|| self.next_slot.fetch_add(1, Ordering::AcqRel));

        if slot == OWNER_SLOT_NONE || slot >= self.max_entries {
            return None;
        }

        let state = SharedOwnerState {
            registry: self,
            slot,
            ptr: self.slot_ptr(slot),
        };
        state.store(OWNER_STATE_NONE, Ordering::Release);
        Some(state)
    }

    fn release_slot(&self, slot: u32) {
        if slot == OWNER_SLOT_NONE || slot >= self.max_entries {
            return;
        }

        let ptr = self.slot_ptr(slot);
        unsafe {
            AtomicU32::from_ptr(ptr.as_ptr()).store(OWNER_STATE_NONE, Ordering::Release);
        }
        self.free_slots
            .lock()
            .expect("owner-state free list poisoned")
            .push(slot);
    }
}

impl SharedOwnerState {
    #[inline(always)]
    fn load(&self, order: Ordering) -> u32 {
        unsafe { AtomicU32::from_ptr(self.ptr.as_ptr()).load(order) }
    }

    #[inline(always)]
    fn store(&self, value: u32, order: Ordering) {
        unsafe {
            AtomicU32::from_ptr(self.ptr.as_ptr()).store(value, order);
        }
    }
}

impl OwnerStateStorage {
    fn new() -> std::result::Result<Self, libc::c_int> {
        match owner_state_registry() {
            Some(registry) => registry.alloc_slot().map(Self::Shared).ok_or(libc::ENOMEM),
            None => Ok(Self::Local(Box::new(AtomicU32::new(OWNER_STATE_NONE)))),
        }
    }

    #[inline(always)]
    fn slot(&self) -> u32 {
        match self {
            Self::Shared(state) => state.slot,
            Self::Local(_) => OWNER_SLOT_NONE,
        }
    }

    #[inline(always)]
    fn load(&self, order: Ordering) -> u32 {
        match self {
            Self::Shared(state) => state.load(order),
            Self::Local(state) => state.load(order),
        }
    }

    #[inline(always)]
    fn store(&self, value: u32, order: Ordering) {
        match self {
            Self::Shared(state) => state.store(value, order),
            Self::Local(state) => state.store(value, order),
        }
    }
}

impl Drop for OwnerStateStorage {
    fn drop(&mut self) {
        if let Self::Shared(state) = self {
            state.registry.release_slot(state.slot);
        }
    }
}

#[cfg(not(test))]
fn owner_state_registry() -> Option<&'static OwnerStateRegistry> {
    OWNER_STATE_REGISTRY.get()
}

#[cfg(test)]
fn owner_state_registry() -> Option<&'static OwnerStateRegistry> {
    static TEST_OWNER_STATE_REGISTRY: OnceLock<OwnerStateRegistry> = OnceLock::new();

    OWNER_STATE_REGISTRY.get().or_else(|| {
        Some(TEST_OWNER_STATE_REGISTRY.get_or_init(|| OwnerStateRegistry::new_for_tests(1024)))
    })
}

pub(crate) fn set_owner_state_map(map: MapHandle) -> anyhow::Result<()> {
    if OWNER_STATE_REGISTRY.get().is_some() {
        return Ok(());
    }

    let registry = OwnerStateRegistry::from_map(map)?;
    OWNER_STATE_REGISTRY
        .set(registry)
        .map_err(|_| anyhow!("owner_state_map is already initialized"))?;
    Ok(())
}

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
    pub owner_state_slot: u32,
    pub _pad: u32,
}

impl LockSchedThreadCtx {
    const fn new() -> Self {
        Self {
            wait_ns_total: 0,
            wait_start_ns: 0,
            wait_end_ns: 0,
            owner_state_slot: OWNER_SLOT_NONE,
            _pad: 0,
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
    owner_state: OwnerStateStorage,
    owner_seq: CacheAligned<AtomicU32>,
    owner_sleepers: CacheAligned<AtomicU32>,
    sleep_seq: CacheAligned<AtomicU32>,
    sleepers: CacheAligned<AtomicU32>,
}

impl McsTasLockRaw {
    pub fn new() -> std::result::Result<Self, libc::c_int> {
        Ok(Self {
            tail: CacheAligned(AtomicPtr::new(ptr::null_mut())),
            locked: CacheAligned(AtomicBool::new(false)),
            owner_state: OwnerStateStorage::new()?,
            owner_seq: CacheAligned(AtomicU32::new(0)),
            owner_sleepers: CacheAligned(AtomicU32::new(0)),
            sleep_seq: CacheAligned(AtomicU32::new(0)),
            sleepers: CacheAligned(AtomicU32::new(0)),
        })
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

        unsafe {
            (*ctx).owner_state_slot = self.owner_state.slot();
        }
        self.owner_state
            .store(OWNER_STATE_RUNNING, Ordering::Release);
    }

    #[inline(always)]
    fn clear_owner(&self) {
        let ctx = thread_ctx();

        unsafe {
            (*ctx).owner_state_slot = OWNER_SLOT_NONE;
        }
        self.owner_state.store(OWNER_STATE_NONE, Ordering::Release);
    }

    #[inline(always)]
    fn owner_is_preempted(&self) -> bool {
        self.owner_state.load(Ordering::Acquire) == OWNER_STATE_PREEMPTED
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

            // Acquired after contention — accumulate the measured wait interval.
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
        let lock = McsTasLockRaw::new().expect("test lock");
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
        let lock = McsTasLockRaw::new().expect("test lock");
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
        let lock = McsTasLockRaw::new().expect("test lock");
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
        let lock = McsTasLockRaw::new().expect("test lock");
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
