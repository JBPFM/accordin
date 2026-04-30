use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::arch::pause;
use crate::lock_backend::LockBackend;
use crate::lock_stats::{
    ADMISSION_CPU_NONE, clear_admission_state, grant_slow_path_admission,
    mark_critical_section_entered, mark_slow_path_pending, thread_has_admission,
};

#[repr(align(64))]
struct CacheAligned<T>(T);

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

pub struct McsLockRaw {
    tail: CacheAligned<AtomicPtr<Node>>,
}

const MAX_THREAD_NODES: usize = 4;

struct ThreadNodes {
    nodes: [Node; MAX_THREAD_NODES],
    lock_owners: [*const McsLockRaw; MAX_THREAD_NODES],
    in_use: [bool; MAX_THREAD_NODES],
    depth: usize,
}

impl ThreadNodes {
    const fn new() -> Self {
        Self {
            nodes: [const { Node::new() }; MAX_THREAD_NODES],
            lock_owners: [ptr::null(); MAX_THREAD_NODES],
            in_use: [false; MAX_THREAD_NODES],
            depth: 0,
        }
    }
}

thread_local! {
    static THREAD_NODES: UnsafeCell<ThreadNodes> = const { UnsafeCell::new(ThreadNodes::new()) };
}

impl McsLockRaw {
    pub const fn new() -> Self {
        Self {
            tail: CacheAligned(AtomicPtr::new(ptr::null_mut())),
        }
    }

    #[inline(always)]
    fn acquire_thread_node(&self, locked: bool) -> *mut Node {
        THREAD_NODES.with(|nodes| unsafe {
            let nodes = &mut *nodes.get();
            if nodes.depth == MAX_THREAD_NODES {
                panic!("McsLockRaw nested lock depth exceeded {MAX_THREAD_NODES}");
            }

            let index = nodes
                .in_use
                .iter()
                .position(|in_use| !*in_use)
                .expect("McsLockRaw node accounting lost a free slot");
            nodes.in_use[index] = true;
            nodes.lock_owners[index] = self as *const McsLockRaw;
            nodes.depth += 1;

            let my_node = &mut nodes.nodes[index] as *mut Node;
            (*my_node).next.store(ptr::null_mut(), Ordering::Relaxed);
            (*my_node).locked.store(locked, Ordering::Relaxed);
            my_node
        })
    }

    #[inline(always)]
    fn locked_thread_node(&self) -> *mut Node {
        THREAD_NODES.with(|nodes| unsafe {
            let nodes = &mut *nodes.get();
            let lock_owner = self as *const McsLockRaw;
            let index = nodes
                .lock_owners
                .iter()
                .zip(nodes.in_use.iter())
                .position(|(owner, in_use)| *in_use && *owner == lock_owner)
                .expect("McsLockRaw unlock without a matching thread-local node");
            &mut nodes.nodes[index] as *mut Node
        })
    }

    #[inline(always)]
    fn release_thread_node(&self, my_node: *mut Node) {
        THREAD_NODES.with(|nodes| unsafe {
            let nodes = &mut *nodes.get();
            let lock_owner = self as *const McsLockRaw;
            let index = nodes
                .nodes
                .iter_mut()
                .enumerate()
                .find_map(|(index, node)| {
                    let node_ptr = node as *mut Node;
                    if nodes.in_use[index]
                        && nodes.lock_owners[index] == lock_owner
                        && node_ptr == my_node
                    {
                        Some(index)
                    } else {
                        None
                    }
                })
                .expect("McsLockRaw node release without a matching active node");

            (*my_node).next.store(ptr::null_mut(), Ordering::Relaxed);
            (*my_node).locked.store(false, Ordering::Relaxed);
            nodes.in_use[index] = false;
            nodes.lock_owners[index] = ptr::null();
            nodes.depth -= 1;
        });
    }

    #[inline(always)]
    fn current_cpu() -> u32 {
        let cpu = unsafe { libc::sched_getcpu() };
        assert!(
            cpu >= 0,
            "sched_getcpu failed while caching slow-path admission"
        );
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
    fn lock_queue(&self) {
        self.ensure_slow_path_admission();

        let my_node = self.acquire_thread_node(true);
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
        let my_node = self.acquire_thread_node(false);
        let locked = self
            .tail
            .0
            .compare_exchange(
                ptr::null_mut(),
                my_node,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok();
        if !locked {
            self.release_thread_node(my_node);
        }
        locked
    }

    #[cfg_attr(feature = "perf-symbols", inline(never))]
    #[cfg_attr(not(feature = "perf-symbols"), inline(always))]
    fn unlock_queue(&self) {
        let my_node = self.locked_thread_node();
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
                clear_admission_state();
                self.release_thread_node(my_node);
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
        clear_admission_state();
        self.release_thread_node(my_node);
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
    use crate::bpf_intf;
    use crate::lock_backend::LockBackend;
    use crate::lock_stats::{ADMISSION_CPU_NONE, clear_admission_state, thread_ctx};

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
        unsafe {
            let ctx = &*thread_ctx();
            assert_eq!(ctx.admission_owned, 0);
            assert_eq!(ctx.admission_cpu, ADMISSION_CPU_NONE);
            assert_eq!(ctx.in_critical_section, 0);
            assert_eq!(ctx.slow_path_pending, 0);
        }
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

    #[test]
    fn supports_four_nested_locks_per_thread() {
        let locks: [McsLockRaw; 4] = std::array::from_fn(|_| McsLockRaw::new());

        for lock in &locks {
            lock.lock();
        }
        for lock in locks.iter().rev() {
            lock.unlock();
        }
    }

    #[test]
    #[should_panic(expected = "McsLockRaw nested lock depth exceeded 4")]
    fn fifth_nested_lock_panics() {
        let locks: [McsLockRaw; 5] = std::array::from_fn(|_| McsLockRaw::new());

        for lock in &locks {
            lock.lock();
        }
    }
}
