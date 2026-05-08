/// Emits an MCS lock implementation with a fixed-capacity per-thread node pool.
///
/// The macro expects `crate::admission`, `crate::arch::{CacheAligned, pause}`, and
/// `crate::lock_backend::LockBackend` to be in scope.
///
/// # Parameters
/// - `$max:expr` — maximum supported nesting depth per thread.
#[macro_export]
macro_rules! define_mcs_lock {
    ($max:expr) => {
        use std::cell::UnsafeCell;
        use std::ptr;
        use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

        use $crate::admission::{
            mark_critical_section_entered, mark_critical_section_exit,
            mark_slow_path_pending_for_lock,
        };
        use $crate::arch::{CacheAligned, pause};
        use $crate::lock_backend::LockBackend;

        const MAX_THREAD_NODES: usize = $max;

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
            fn ensure_slow_path_admission(&self) {
                mark_slow_path_pending_for_lock(self as *const Self as u64);
                std::thread::yield_now();
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
                        mark_critical_section_exit();
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
                mark_critical_section_exit();
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
    };
}
