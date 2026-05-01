use std::cell::Cell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use crate::admission::{
    mark_critical_section_entered, mark_critical_section_exit, mark_slow_path_pending,
};
use crate::arch::pause;
use crate::lock_backend::LockBackend;

#[repr(align(64))]
struct CacheAligned<T>(T);

const MAX_THREAD_SLOTS: usize = 1600;
static NEXT_THREAD_ID: AtomicUsize = AtomicUsize::new(0);

#[repr(C, align(64))]
pub struct Node {
    next: AtomicPtr<Node>,
    locked: AtomicBool,
}

impl Node {
    pub const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
            locked: AtomicBool::new(false),
        }
    }
}

thread_local! {
    static THREAD_ID: Cell<usize> = const { Cell::new(usize::MAX) };
}

pub struct McsLockRaw {
    tail: CacheAligned<AtomicPtr<Node>>,
    qnodes: Box<[Node]>,
}

impl McsLockRaw {
    pub fn new() -> Self {
        Self {
            tail: CacheAligned(AtomicPtr::new(ptr::null_mut())),
            qnodes: (0..MAX_THREAD_SLOTS)
                .map(|_| Node::new())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        }
    }

    #[inline(always)]
    fn thread_index() -> usize {
        THREAD_ID.with(|slot| {
            let id = slot.get();
            if id != usize::MAX {
                return id;
            }

            let next = NEXT_THREAD_ID.fetch_add(1, Ordering::AcqRel);
            assert!(next < MAX_THREAD_SLOTS, "too many threads for MCS lock");
            slot.set(next);
            next
        })
    }

    #[inline(always)]
    fn thread_node(&self) -> *mut Node {
        let index = Self::thread_index();
        self.qnodes.as_ptr().wrapping_add(index).cast_mut()
    }

    #[inline(always)]
    fn prepare_node(&self, locked: bool) -> *mut Node {
        let my_node = self.thread_node();
        unsafe {
            (*my_node).next.store(ptr::null_mut(), Ordering::Relaxed);
            (*my_node).locked.store(locked, Ordering::Relaxed);
        }
        my_node
    }

    #[inline(always)]
    fn ensure_slow_path_admission(&self) {
        mark_slow_path_pending();
        std::thread::yield_now();
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn lock_queue(&self) {
        self.ensure_slow_path_admission();

        let my_node = self.prepare_node(true);
        let pred = self.tail.0.swap(my_node, Ordering::AcqRel);
        if !pred.is_null() {
            unsafe {
                (*pred).next.store(my_node, Ordering::Release);
                while (*my_node).locked.load(Ordering::Acquire) {
                    pause();
                }
            }
        }

        mark_critical_section_entered();
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn try_lock_fast(&self) -> bool {
        let my_node = self.prepare_node(false);
        self.tail
            .0
            .compare_exchange(
                ptr::null_mut(),
                my_node,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock_queue(&self) {
        let my_node = self.thread_node();
        let mut succ = unsafe { (*my_node).next.load(Ordering::Acquire) };

        if succ.is_null() {
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
                mark_critical_section_exit();
                return;
            }

            while {
                succ = unsafe { (*my_node).next.load(Ordering::Acquire) };
                succ.is_null()
            } {
                pause();
            }
        }

        unsafe {
            (*succ).locked.store(false, Ordering::Release);
        }
        mark_critical_section_exit();
    }
}

impl LockBackend for McsLockRaw {
    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn lock(&self) {
        self.lock_queue();
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn try_lock(&self) -> bool {
        self.try_lock_fast()
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock(&self) {
        self.unlock_queue();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::UnsafeCell;
    use std::sync::Arc;

    use super::{McsLockRaw, Node};
    use crate::lock_backend::LockBackend;

    #[test]
    fn node_layout_stays_cache_aligned() {
        assert_eq!(std::mem::size_of::<Node>(), 64);
        assert_eq!(std::mem::align_of::<Node>(), 64);
    }

    #[test]
    fn try_lock_requires_unlock_before_reacquire() {
        let lock = McsLockRaw::new();

        assert!(lock.try_lock());
        assert!(!lock.try_lock());
        lock.unlock();
        assert!(lock.try_lock());
        lock.unlock();
    }

    struct SharedCounter {
        lock: McsLockRaw,
        value: UnsafeCell<usize>,
    }

    unsafe impl Sync for SharedCounter {}

    #[test]
    fn lock_serializes_counter_updates() {
        let shared = Arc::new(SharedCounter {
            lock: McsLockRaw::new(),
            value: UnsafeCell::new(0),
        });
        let mut handles = Vec::new();

        for _ in 0..4 {
            let shared = Arc::clone(&shared);
            handles.push(std::thread::spawn(move || {
                for _ in 0..10_000 {
                    shared.lock.lock();
                    unsafe {
                        let next = (*shared.value.get()).wrapping_add(1);
                        *shared.value.get() = next;
                    }
                    shared.lock.unlock();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(unsafe { *shared.value.get() }, 40_000);
    }
}
