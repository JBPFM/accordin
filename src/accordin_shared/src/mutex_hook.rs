// SPDX-License-Identifier: GPL-2.0-only

use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use libbpf_rs::{MapCore, MapFlags, MapHandle};

use crate::lock_backend::LockBackend;

pub trait MutexHookBackend {
    type LockState;
    const USES_ADMISSION_SCOPE: bool = false;

    fn create_state() -> Self::LockState;
    fn lock(state: &Self::LockState);
    fn try_lock(state: &Self::LockState) -> bool;
    fn unlock(state: &Self::LockState);
}

/// Adapts a raw lock implementation to the pthread hook backend interface.
///
/// Use this when the lock file should only contain the lock algorithm. The
/// adapter keeps the shared admission lifecycle outside the raw lock: slow-path
/// acquisition marks the thread as waiting, and unlock clears the critical
/// section after the raw lock releases. Stats are still handled by
/// `export_mutex_hooks!`.
pub struct LockBackendAdapter<L>(PhantomData<fn() -> L>);

impl<L> MutexHookBackend for LockBackendAdapter<L>
where
    L: LockBackend + Default,
{
    type LockState = L;
    const USES_ADMISSION_SCOPE: bool = true;

    fn create_state() -> Self::LockState {
        L::default()
    }

    fn lock(state: &Self::LockState) {
        LockBackend::lock(state);
    }

    fn try_lock(state: &Self::LockState) -> bool {
        LockBackend::try_lock(state)
    }

    fn unlock(state: &Self::LockState) {
        LockBackend::unlock(state);
    }
}

static THREAD_CTX_MAP: OnceLock<MapHandle> = OnceLock::new();
static REGISTERED_THREAD_COUNT: AtomicUsize = AtomicUsize::new(0);
static REGISTERED_THREAD_COUNT_BPF: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
const HOOK_SCOPE_ENV: &str = "ACCORDIN_HOOK_SCOPE";
const DISABLE_CV_ADMISSION_HINT_ENV: &str = "ACCORDIN_DISABLE_CV_ADMISSION_HINT";

static CV_COUNTERS_ENABLED: AtomicBool = AtomicBool::new(false);
static CV_HINTS_PUBLISHED: AtomicU64 = AtomicU64::new(0);
static CV_SPECIALIZED_RELOCKS: AtomicU64 = AtomicU64::new(0);
static CV_FALLBACK_RELOCKS: AtomicU64 = AtomicU64::new(0);

/// Reconciles the userspace side of the cond-reacquire protocol against the
/// scheduler's wake-routing counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CvAdmissionCounters {
    pub hints_published: u64,
    pub specialized_relocks: u64,
    pub fallback_relocks: u64,
}

pub fn cv_admission_counters() -> CvAdmissionCounters {
    CvAdmissionCounters {
        hints_published: CV_HINTS_PUBLISHED.load(Ordering::Relaxed),
        specialized_relocks: CV_SPECIALIZED_RELOCKS.load(Ordering::Relaxed),
        fallback_relocks: CV_FALLBACK_RELOCKS.load(Ordering::Relaxed),
    }
}

/// The counters share cache lines across every hooked thread, so they stay off
/// unless the run explicitly asks for debug counters.
#[inline]
pub fn cv_admission_counters_enabled() -> bool {
    CV_COUNTERS_ENABLED.load(Ordering::Relaxed)
}

#[doc(hidden)]
pub fn set_cv_admission_counters_enabled(enabled: bool) {
    CV_COUNTERS_ENABLED.store(enabled, Ordering::Relaxed);
}

#[inline(always)]
fn bump_cv_counter(counter: &AtomicU64) {
    if !CV_COUNTERS_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    counter.fetch_add(1, Ordering::Relaxed);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_hint_published() {
    bump_cv_counter(&CV_HINTS_PUBLISHED);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_specialized_relock() {
    bump_cv_counter(&CV_SPECIALIZED_RELOCKS);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_fallback_relock() {
    bump_cv_counter(&CV_FALLBACK_RELOCKS);
}

#[inline(always)]
pub fn current_tid() -> u32 {
    unsafe { libc::syscall(libc::SYS_gettid) as u32 }
}

pub fn set_thread_ctx_map(map: MapHandle) {
    let _ = THREAD_CTX_MAP.set(map);
}

#[doc(hidden)]
pub fn set_registered_thread_count_ptr(ptr: *mut u32) {
    REGISTERED_THREAD_COUNT_BPF.store(ptr, Ordering::Release);
    sync_registered_thread_count_to_bpf(registered_thread_count());
}

fn sync_registered_thread_count_to_bpf(count: usize) {
    let ptr = REGISTERED_THREAD_COUNT_BPF.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }

    let count = count.min(u32::MAX as usize) as u32;
    unsafe {
        ptr.write_volatile(count);
    }
}

fn hook_scope_value_is_registered(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case("registered"))
}

pub fn registered_hook_scope_enabled() -> bool {
    static REGISTERED_SCOPE: OnceLock<bool> = OnceLock::new();
    *REGISTERED_SCOPE.get_or_init(|| {
        hook_scope_value_is_registered(std::env::var(HOOK_SCOPE_ENV).ok().as_deref())
    })
}

#[doc(hidden)]
pub fn cv_admission_hint_enabled() -> bool {
    static CV_ADMISSION_HINT_ENABLED: OnceLock<bool> = OnceLock::new();
    *CV_ADMISSION_HINT_ENABLED.get_or_init(|| !crate::env::env_flag(DISABLE_CV_ADMISSION_HINT_ENV))
}

fn hooked_mutexes() -> &'static Mutex<HashSet<usize>> {
    static HOOKED_MUTEXES: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    HOOKED_MUTEXES.get_or_init(|| Mutex::new(HashSet::new()))
}

fn hooked_conds() -> &'static Mutex<HashSet<usize>> {
    static HOOKED_CONDS: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
    HOOKED_CONDS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn register_addr(registry: &Mutex<HashSet<usize>>, addr: usize) -> bool {
    if addr == 0 {
        return false;
    }
    registry
        .lock()
        .is_ok_and(|mut entries| entries.insert(addr))
}

fn unregister_addr(registry: &Mutex<HashSet<usize>>, addr: usize) -> bool {
    if addr == 0 {
        return false;
    }
    registry
        .lock()
        .is_ok_and(|mut entries| entries.remove(&addr))
}

fn contains_addr(registry: &Mutex<HashSet<usize>>, addr: usize) -> bool {
    addr != 0 && registry.lock().is_ok_and(|entries| entries.contains(&addr))
}

pub fn register_hooked_mutex_addr(mutex: *mut libc::pthread_mutex_t) -> bool {
    register_addr(hooked_mutexes(), mutex as usize)
}

pub fn unregister_hooked_mutex_addr(mutex: *mut libc::pthread_mutex_t) -> bool {
    unregister_addr(hooked_mutexes(), mutex as usize)
}

pub fn hooked_mutex_registered(mutex: *mut libc::pthread_mutex_t) -> bool {
    contains_addr(hooked_mutexes(), mutex as usize)
}

pub fn register_hooked_cond_addr(cond: *mut libc::pthread_cond_t) -> bool {
    register_addr(hooked_conds(), cond as usize)
}

pub fn unregister_hooked_cond_addr(cond: *mut libc::pthread_cond_t) -> bool {
    unregister_addr(hooked_conds(), cond as usize)
}

pub fn hooked_cond_registered(cond: *mut libc::pthread_cond_t) -> bool {
    contains_addr(hooked_conds(), cond as usize)
}

trait ThreadCtxMapOps {
    fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool;
    fn delete_entry(&self, key: &[u8]) -> bool;
}

impl ThreadCtxMapOps for MapHandle {
    fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool {
        self.update(key, value, flags).is_ok()
    }

    fn delete_entry(&self, key: &[u8]) -> bool {
        self.delete(key).is_ok()
    }
}

fn register_thread_ctx_with_map<M>(map: &M, tid: u32, admission_word_ptr: u64) -> bool
where
    M: ThreadCtxMapOps + ?Sized,
{
    map.update_entry(
        &tid.to_ne_bytes(),
        &admission_word_ptr.to_ne_bytes(),
        MapFlags::ANY,
    )
}

fn unregister_thread_ctx_with_map<M>(map: &M, tid: u32) -> bool
where
    M: ThreadCtxMapOps + ?Sized,
{
    map.delete_entry(&tid.to_ne_bytes())
}

#[doc(hidden)]
pub fn register_current_thread() -> bool {
    let Some(map) = THREAD_CTX_MAP.get() else {
        return false;
    };

    let tid = current_tid();
    let admission_word_ptr = crate::admission::user_word_addr() as u64;
    let registered = register_thread_ctx_with_map(map, tid, admission_word_ptr);
    if registered {
        let count = REGISTERED_THREAD_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        sync_registered_thread_count_to_bpf(count);
    }
    registered
}

#[doc(hidden)]
pub fn unregister_current_thread() {
    let Some(map) = THREAD_CTX_MAP.get() else {
        return;
    };

    if unregister_thread_ctx_with_map(map, current_tid()) {
        let count = REGISTERED_THREAD_COUNT.fetch_sub(1, Ordering::Relaxed) - 1;
        sync_registered_thread_count_to_bpf(count);
    }
}

pub fn registered_thread_count() -> usize {
    REGISTERED_THREAD_COUNT.load(Ordering::Relaxed)
}

#[macro_export]
macro_rules! export_mutex_hooks {
    ($backend:ty) => {
        mod exported_mutex_hooks {
            use std::cell::{Cell, UnsafeCell};
            use std::hint::spin_loop;
            use std::sync::OnceLock;
            use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

            use $crate::lock_stats::{
                record_hold_end_sample, record_lock_acquired, record_lock_acquired_for_lock,
                record_lock_acquired_for_scope, record_post_unlock, record_thread_start,
                record_wait_end, record_wait_start,
            };
            use $crate::mutex_hook::MutexHookBackend;

            type Backend = $backend;

            struct HookState {
                lock: <Backend as MutexHookBackend>::LockState,
                lock_id: u32,
            }

            unsafe impl Send for HookState {}
            unsafe impl Sync for HookState {}

            struct CondState {
                seq: AtomicU32,
                waiters: AtomicU32,
            }

            unsafe impl Send for CondState {}
            unsafe impl Sync for CondState {}

            struct ThreadCtxGuard;

            impl Drop for ThreadCtxGuard {
                fn drop(&mut self) {
                    $crate::lock_stats::flush_current_thread_stats();
                    $crate::mutex_hook::unregister_current_thread();
                }
            }

            thread_local! {
                static REGISTERED: Cell<bool> = const { Cell::new(false) };
                static _GUARD: UnsafeCell<Option<ThreadCtxGuard>> = const { UnsafeCell::new(None) };
            }

            #[inline(always)]
            fn ensure_registered() {
                $crate::cpu_affinity::ensure_current_thread_affinity();

                REGISTERED.with(|registered| {
                    if !registered.get() {
                        record_thread_start();
                        let _ = $crate::mutex_hook::register_current_thread();
                        _GUARD.with(|guard| unsafe { *guard.get() = Some(ThreadCtxGuard) });
                        registered.set(true);
                    }
                });
            }

            /// Whether the acquire bracket still has to ask admission for a
            /// slot, or whether the caller already holds the routing decision.
            #[derive(Clone, Copy, PartialEq, Eq)]
            enum AcquireMode {
                Normal,
                AlreadyAdmitted,
            }

            #[inline(always)]
            fn lock_with_stats(lock: &<Backend as MutexHookBackend>::LockState, lock_id: u32) {
                if Backend::USES_ADMISSION_SCOPE {
                    let scope = $crate::admission::begin_lock_scope(lock_id);
                    lock_scope_with_stats(lock, scope, AcquireMode::Normal);
                    return;
                }

                if Backend::try_lock(lock) {
                    record_lock_acquired();
                    return;
                }

                let wait_start = record_wait_start();
                Backend::lock(lock);
                record_wait_end(wait_start);
                record_lock_acquired();
            }

            #[inline(always)]
            fn lock_scope_with_stats(
                lock: &<Backend as MutexHookBackend>::LockState,
                scope: $crate::admission::LockAdmissionScope,
                mode: AcquireMode,
            ) {
                let normal = mode == AcquireMode::Normal;

                if normal
                    && !$crate::admission::token_consumed_for_scope(scope)
                    && Backend::try_lock(lock)
                {
                    record_lock_acquired_for_scope(scope);
                    return;
                }

                let wait_start = record_wait_start();
                if normal && $crate::admission::mark_slow_path_pending_for_scope(scope) {
                    std::thread::yield_now();
                    $crate::admission::clear_token_consumed_for_scope(scope);
                }
                Backend::lock(lock);
                record_wait_end(wait_start);
                record_lock_acquired_for_scope(scope);
            }

            #[inline(always)]
            fn publish_cond_reacquire_hint(lock_id: u32) {
                if Backend::USES_ADMISSION_SCOPE
                    && $crate::mutex_hook::cv_admission_hint_enabled()
                    && $crate::admission::mark_cond_reacquire_pending_for_cond_mutex(lock_id)
                {
                    $crate::mutex_hook::record_cv_hint_published();
                }
            }

            /// Reacquires the cond mutex after a wait. A waiter that actually
            /// blocked was already routed through admission at wake time, so
            /// taking the published hint acknowledges the consumed token and the
            /// lock is entered without re-requesting admission.
            #[inline(always)]
            fn relock_after_cond_wait(
                lock: &<Backend as MutexHookBackend>::LockState,
                lock_id: u32,
                slept: bool,
            ) {
                if !Backend::USES_ADMISSION_SCOPE {
                    lock_with_stats(lock, lock_id);
                    return;
                }

                let scope = $crate::admission::begin_lock_scope(lock_id);
                let mode = if slept
                    && $crate::admission::take_cond_reacquire_pending_for_scope(scope)
                {
                    $crate::mutex_hook::record_cv_specialized_relock();
                    AcquireMode::AlreadyAdmitted
                } else {
                    $crate::mutex_hook::record_cv_fallback_relock();
                    AcquireMode::Normal
                };
                lock_scope_with_stats(lock, scope, mode);
            }

            #[inline(always)]
            fn unlock_with_stats(lock: &<Backend as MutexHookBackend>::LockState, lock_id: u32) {
                let hold_end = record_hold_end_sample();
                Backend::unlock(lock);
                if Backend::USES_ADMISSION_SCOPE {
                    $crate::admission::finish_lock_scope(lock_id);
                }
                record_post_unlock(hold_end, lock_id);
            }

            const SENTINEL: usize = 1;
            const FUTEX_WAIT_PRIVATE: libc::c_int = 128;
            const FUTEX_WAKE_PRIVATE: libc::c_int = 129;
            const FUTEX_WAIT_BITSET_PRIVATE_REALTIME: libc::c_int = 9 | 128 | 256;
            const FUTEX_BITSET_MATCH_ANY: libc::c_uint = libc::c_uint::MAX;

            type PthreadMutexInitFn = unsafe extern "C" fn(
                *mut libc::pthread_mutex_t,
                *const libc::pthread_mutexattr_t,
            ) -> libc::c_int;
            type PthreadMutexDestroyFn =
                unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int;
            type PthreadMutexLockFn =
                unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int;
            type PthreadMutexTrylockFn =
                unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int;
            type PthreadMutexUnlockFn =
                unsafe extern "C" fn(*mut libc::pthread_mutex_t) -> libc::c_int;
            type PthreadCondInitFn = unsafe extern "C" fn(
                *mut libc::pthread_cond_t,
                *const libc::pthread_condattr_t,
            ) -> libc::c_int;
            type PthreadCondDestroyFn =
                unsafe extern "C" fn(*mut libc::pthread_cond_t) -> libc::c_int;
            type PthreadCondSignalFn =
                unsafe extern "C" fn(*mut libc::pthread_cond_t) -> libc::c_int;
            type PthreadCondBroadcastFn =
                unsafe extern "C" fn(*mut libc::pthread_cond_t) -> libc::c_int;
            type PthreadCondWaitFn = unsafe extern "C" fn(
                *mut libc::pthread_cond_t,
                *mut libc::pthread_mutex_t,
            ) -> libc::c_int;
            type PthreadCondTimedwaitFn = unsafe extern "C" fn(
                *mut libc::pthread_cond_t,
                *mut libc::pthread_mutex_t,
                *const libc::timespec,
            ) -> libc::c_int;

            unsafe fn resolve_next_symbol(name: &'static [u8]) -> usize {
                unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr().cast()) as usize }
            }

            unsafe fn transmute_symbol<T: Copy>(ptr: usize) -> Option<T> {
                if ptr == 0 {
                    None
                } else {
                    Some(unsafe { std::mem::transmute_copy(&ptr) })
                }
            }

            unsafe fn real_pthread_mutex_init(
                mutex: *mut libc::pthread_mutex_t,
                attr: *const libc::pthread_mutexattr_t,
            ) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) =
                    (unsafe {
                        transmute_symbol::<PthreadMutexInitFn>(*REAL.get_or_init(|| unsafe {
                            resolve_next_symbol(b"pthread_mutex_init\0")
                        }))
                    })
                else {
                    return libc::ENOSYS;
                };
                unsafe { func(mutex, attr) }
            }

            unsafe fn real_pthread_mutex_destroy(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) =
                    (unsafe {
                        transmute_symbol::<PthreadMutexDestroyFn>(*REAL.get_or_init(|| unsafe {
                            resolve_next_symbol(b"pthread_mutex_destroy\0")
                        }))
                    })
                else {
                    return libc::ENOSYS;
                };
                unsafe { func(mutex) }
            }

            unsafe fn real_pthread_mutex_lock(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) =
                    (unsafe {
                        transmute_symbol::<PthreadMutexLockFn>(*REAL.get_or_init(|| unsafe {
                            resolve_next_symbol(b"pthread_mutex_lock\0")
                        }))
                    })
                else {
                    return libc::ENOSYS;
                };
                unsafe { func(mutex) }
            }

            unsafe fn real_pthread_mutex_trylock(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) =
                    (unsafe {
                        transmute_symbol::<PthreadMutexTrylockFn>(*REAL.get_or_init(|| unsafe {
                            resolve_next_symbol(b"pthread_mutex_trylock\0")
                        }))
                    })
                else {
                    return libc::ENOSYS;
                };
                unsafe { func(mutex) }
            }

            unsafe fn real_pthread_mutex_unlock(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) =
                    (unsafe {
                        transmute_symbol::<PthreadMutexUnlockFn>(*REAL.get_or_init(|| unsafe {
                            resolve_next_symbol(b"pthread_mutex_unlock\0")
                        }))
                    })
                else {
                    return libc::ENOSYS;
                };
                unsafe { func(mutex) }
            }

            unsafe fn real_pthread_cond_init(
                cond: *mut libc::pthread_cond_t,
                attr: *const libc::pthread_condattr_t,
            ) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) = (unsafe {
                    transmute_symbol::<PthreadCondInitFn>(
                        *REAL
                            .get_or_init(|| unsafe { resolve_next_symbol(b"pthread_cond_init\0") }),
                    )
                }) else {
                    return libc::ENOSYS;
                };
                unsafe { func(cond, attr) }
            }

            unsafe fn real_pthread_cond_destroy(cond: *mut libc::pthread_cond_t) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) =
                    (unsafe {
                        transmute_symbol::<PthreadCondDestroyFn>(*REAL.get_or_init(|| unsafe {
                            resolve_next_symbol(b"pthread_cond_destroy\0")
                        }))
                    })
                else {
                    return libc::ENOSYS;
                };
                unsafe { func(cond) }
            }

            unsafe fn real_pthread_cond_signal(cond: *mut libc::pthread_cond_t) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) =
                    (unsafe {
                        transmute_symbol::<PthreadCondSignalFn>(*REAL.get_or_init(|| unsafe {
                            resolve_next_symbol(b"pthread_cond_signal\0")
                        }))
                    })
                else {
                    return libc::ENOSYS;
                };
                unsafe { func(cond) }
            }

            unsafe fn real_pthread_cond_broadcast(cond: *mut libc::pthread_cond_t) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) = (unsafe {
                    transmute_symbol::<PthreadCondBroadcastFn>(*REAL.get_or_init(|| unsafe {
                        resolve_next_symbol(b"pthread_cond_broadcast\0")
                    }))
                }) else {
                    return libc::ENOSYS;
                };
                unsafe { func(cond) }
            }

            unsafe fn real_pthread_cond_wait(
                cond: *mut libc::pthread_cond_t,
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) = (unsafe {
                    transmute_symbol::<PthreadCondWaitFn>(
                        *REAL
                            .get_or_init(|| unsafe { resolve_next_symbol(b"pthread_cond_wait\0") }),
                    )
                }) else {
                    return libc::ENOSYS;
                };
                unsafe { func(cond, mutex) }
            }

            unsafe fn real_pthread_cond_timedwait(
                cond: *mut libc::pthread_cond_t,
                mutex: *mut libc::pthread_mutex_t,
                abstime: *const libc::timespec,
            ) -> libc::c_int {
                static REAL: OnceLock<usize> = OnceLock::new();
                let Some(func) = (unsafe {
                    transmute_symbol::<PthreadCondTimedwaitFn>(*REAL.get_or_init(|| unsafe {
                        resolve_next_symbol(b"pthread_cond_timedwait\0")
                    }))
                }) else {
                    return libc::ENOSYS;
                };
                unsafe { func(cond, mutex, abstime) }
            }

            #[inline(always)]
            unsafe fn state_atomic(mutex: *mut libc::pthread_mutex_t) -> &'static AtomicUsize {
                unsafe { &*(mutex as *const AtomicUsize) }
            }

            #[inline(always)]
            unsafe fn cond_atomic(cond: *mut libc::pthread_cond_t) -> &'static AtomicUsize {
                unsafe { &*(cond as *const AtomicUsize) }
            }

            #[inline(always)]
            fn should_hook_mutex(mutex: *mut libc::pthread_mutex_t) -> bool {
                !$crate::mutex_hook::registered_hook_scope_enabled()
                    || $crate::mutex_hook::hooked_mutex_registered(mutex)
            }

            unsafe fn initialize_hook_state(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    let state = Box::new(HookState {
                        lock: Backend::create_state(),
                        lock_id: if Backend::USES_ADMISSION_SCOPE {
                            $crate::admission::allocate_lock_class()
                        } else {
                            $crate::admission::UNMANAGED_LOCK_ID
                        },
                    });
                    let ptr = Box::into_raw(state);

                    std::ptr::write_bytes(
                        mutex as *mut u8,
                        0,
                        std::mem::size_of::<libc::pthread_mutex_t>(),
                    );
                    (*(mutex as *mut usize)) = ptr as usize;
                    0
                }
            }

            unsafe fn initialize_cond_state(cond: *mut libc::pthread_cond_t) -> libc::c_int {
                unsafe {
                    if cond.is_null() {
                        return libc::EINVAL;
                    }

                    let state = Box::new(CondState {
                        seq: AtomicU32::new(0),
                        waiters: AtomicU32::new(0),
                    });
                    let ptr = Box::into_raw(state);
                    std::ptr::write_bytes(
                        cond as *mut u8,
                        0,
                        std::mem::size_of::<libc::pthread_cond_t>(),
                    );
                    (*(cond as *mut usize)) = ptr as usize;
                    $crate::mutex_hook::register_hooked_cond_addr(cond);
                    0
                }
            }

            #[inline(always)]
            unsafe fn ensure_state(
                mutex: *mut libc::pthread_mutex_t,
            ) -> Result<*mut HookState, libc::c_int> {
                if mutex.is_null() {
                    return Err(libc::EINVAL);
                }

                let val = unsafe { state_atomic(mutex) }.load(Ordering::Acquire);
                if likely(val > SENTINEL) {
                    return Ok(val as *mut HookState);
                }
                unsafe { ensure_state_slow(mutex, val) }
            }

            #[cold]
            #[inline(never)]
            unsafe fn ensure_state_slow(
                mutex: *mut libc::pthread_mutex_t,
                initial: usize,
            ) -> Result<*mut HookState, libc::c_int> {
                let atomic = unsafe { state_atomic(mutex) };
                let mut val = initial;
                loop {
                    if val > SENTINEL {
                        return Ok(val as *mut HookState);
                    }

                    if val == SENTINEL {
                        spin_loop();
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    if atomic
                        .compare_exchange(0, SENTINEL, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    let state = Box::new(HookState {
                        lock: Backend::create_state(),
                        lock_id: if Backend::USES_ADMISSION_SCOPE {
                            $crate::admission::allocate_lock_class()
                        } else {
                            $crate::admission::UNMANAGED_LOCK_ID
                        },
                    });
                    let ptr = Box::into_raw(state);

                    atomic.store(ptr as usize, Ordering::Release);
                    return Ok(ptr);
                }
            }

            #[inline(always)]
            unsafe fn ensure_cond_state(
                cond: *mut libc::pthread_cond_t,
            ) -> Result<*mut CondState, libc::c_int> {
                if cond.is_null() {
                    return Err(libc::EINVAL);
                }

                let val = unsafe { cond_atomic(cond) }.load(Ordering::Acquire);
                if likely(val > SENTINEL) {
                    return Ok(val as *mut CondState);
                }
                unsafe { ensure_cond_state_slow(cond, val) }
            }

            #[cold]
            #[inline(never)]
            unsafe fn ensure_cond_state_slow(
                cond: *mut libc::pthread_cond_t,
                initial: usize,
            ) -> Result<*mut CondState, libc::c_int> {
                let atomic = unsafe { cond_atomic(cond) };
                let mut val = initial;
                loop {
                    if val > SENTINEL {
                        return Ok(val as *mut CondState);
                    }

                    if val == SENTINEL {
                        spin_loop();
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    if atomic
                        .compare_exchange(0, SENTINEL, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    let state = Box::new(CondState {
                        seq: AtomicU32::new(0),
                        waiters: AtomicU32::new(0),
                    });
                    let ptr = Box::into_raw(state);
                    atomic.store(ptr as usize, Ordering::Release);
                    $crate::mutex_hook::register_hooked_cond_addr(cond);
                    return Ok(ptr);
                }
            }

            #[inline(always)]
            const fn likely(b: bool) -> bool {
                if !b {
                    cold_path();
                }
                b
            }

            #[cold]
            #[inline(never)]
            const fn cold_path() {}

            /// Returns 0 when the wait completed, otherwise the errno reported
            /// by the syscall, so callers classify the outcome without reading
            /// thread-local errno themselves.
            #[inline(always)]
            unsafe fn futex_wait(addr: *const AtomicU32, expected: u32) -> libc::c_int {
                unsafe {
                    let ret = libc::syscall(
                        libc::SYS_futex,
                        addr as *const u32,
                        FUTEX_WAIT_PRIVATE,
                        expected,
                        std::ptr::null::<libc::timespec>(),
                    );
                    if ret == 0 {
                        0
                    } else {
                        *libc::__errno_location()
                    }
                }
            }

            #[inline(always)]
            unsafe fn futex_wait_until_realtime(
                addr: *const AtomicU32,
                expected: u32,
                abstime: *const libc::timespec,
            ) -> libc::c_int {
                unsafe {
                    let ret = libc::syscall(
                        libc::SYS_futex,
                        addr as *const u32,
                        FUTEX_WAIT_BITSET_PRIVATE_REALTIME,
                        expected,
                        abstime,
                        std::ptr::null::<libc::c_void>(),
                        FUTEX_BITSET_MATCH_ANY,
                    );
                    if ret == 0 {
                        0
                    } else {
                        *libc::__errno_location()
                    }
                }
            }

            #[inline(always)]
            unsafe fn futex_wake(addr: *const AtomicU32, count: libc::c_int) -> libc::c_long {
                unsafe {
                    libc::syscall(
                        libc::SYS_futex,
                        addr as *const u32,
                        FUTEX_WAKE_PRIVATE,
                        count,
                    )
                }
            }

            #[inline(always)]
            fn signal_one_waiter(cond_state: *mut CondState) -> bool {
                unsafe {
                    let waiters = &(*cond_state).waiters;
                    let mut current = waiters.load(Ordering::Acquire);
                    while current != 0 {
                        match waiters.compare_exchange_weak(
                            current,
                            current - 1,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                (*cond_state).seq.fetch_add(1, Ordering::Release);
                                return true;
                            }
                            Err(next) => current = next,
                        }
                    }
                    false
                }
            }

            #[inline(always)]
            fn cancel_one_waiter(cond_state: *mut CondState) {
                unsafe {
                    let waiters = &(*cond_state).waiters;
                    let mut current = waiters.load(Ordering::Acquire);
                    while current != 0 {
                        match waiters.compare_exchange_weak(
                            current,
                            current - 1,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => return,
                            Err(next) => current = next,
                        }
                    }
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_init(
                mutex: *mut libc::pthread_mutex_t,
                attr: *const libc::pthread_mutexattr_t,
            ) -> libc::c_int {
                unsafe {
                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        return real_pthread_mutex_init(mutex, attr);
                    }
                    initialize_hook_state(mutex)
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_register_hooked_mutex(
                mutex: *mut libc::c_void,
            ) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    let mutex = mutex.cast::<libc::pthread_mutex_t>();
                    let val = state_atomic(mutex).load(Ordering::Acquire);
                    if val > SENTINEL {
                        $crate::mutex_hook::register_hooked_mutex_addr(mutex);
                        return 0;
                    }

                    if val == SENTINEL {
                        return libc::EBUSY;
                    }

                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        let ret = real_pthread_mutex_destroy(mutex);
                        if ret != 0 {
                            return ret;
                        }
                    }

                    let ret = initialize_hook_state(mutex);
                    if ret == 0 {
                        $crate::mutex_hook::register_hooked_mutex_addr(mutex);
                    }
                    ret
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_unregister_hooked_mutex(
                mutex: *mut libc::c_void,
            ) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    let mutex = mutex.cast::<libc::pthread_mutex_t>();
                    $crate::mutex_hook::unregister_hooked_mutex_addr(mutex);

                    let atomic = state_atomic(mutex);
                    let val = atomic.load(Ordering::Acquire);
                    if val > SENTINEL {
                        drop(Box::from_raw(val as *mut HookState));
                        atomic.store(0, Ordering::Release);
                    }
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_destroy(
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    if !should_hook_mutex(mutex) {
                        return real_pthread_mutex_destroy(mutex);
                    }

                    $crate::mutex_hook::unregister_hooked_mutex_addr(mutex);

                    let atomic = state_atomic(mutex);
                    let val = atomic.load(Ordering::Acquire);
                    if val > SENTINEL {
                        let ptr = val as *mut HookState;
                        drop(Box::from_raw(ptr));
                        atomic.store(0, Ordering::Release);
                    }
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_lock(
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                unsafe {
                    if !should_hook_mutex(mutex) {
                        return real_pthread_mutex_lock(mutex);
                    }
                    ensure_registered();
                    let state = match ensure_state(mutex) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    lock_with_stats(&(*state).lock, (*state).lock_id);
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_trylock(
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                unsafe {
                    if !should_hook_mutex(mutex) {
                        return real_pthread_mutex_trylock(mutex);
                    }
                    ensure_registered();
                    let state = match ensure_state(mutex) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    if Backend::try_lock(&(*state).lock) {
                        if Backend::USES_ADMISSION_SCOPE {
                            record_lock_acquired_for_lock((*state).lock_id);
                        } else {
                            record_lock_acquired();
                        }
                        0
                    } else {
                        libc::EBUSY
                    }
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_mutex_unlock(
                mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    if !should_hook_mutex(mutex) {
                        return real_pthread_mutex_unlock(mutex);
                    }

                    let atomic = state_atomic(mutex);
                    let val = atomic.load(Ordering::Acquire);
                    if val > SENTINEL {
                        let state = &*(val as *mut HookState);
                        unlock_with_stats(&state.lock, state.lock_id);
                        return 0;
                    }
                    libc::EINVAL
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_init(
                cond: *mut libc::pthread_cond_t,
                attr: *const libc::pthread_condattr_t,
            ) -> libc::c_int {
                unsafe {
                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        return real_pthread_cond_init(cond, attr);
                    }
                    initialize_cond_state(cond)
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_destroy(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                unsafe {
                    if cond.is_null() {
                        return libc::EINVAL;
                    }

                    if $crate::mutex_hook::registered_hook_scope_enabled()
                        && !$crate::mutex_hook::hooked_cond_registered(cond)
                    {
                        return real_pthread_cond_destroy(cond);
                    }

                    $crate::mutex_hook::unregister_hooked_cond_addr(cond);

                    let atomic = cond_atomic(cond);
                    let val = atomic.load(Ordering::Acquire);
                    if val > SENTINEL {
                        drop(Box::from_raw(val as *mut CondState));
                        atomic.store(0, Ordering::Release);
                    }
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_signal(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                unsafe {
                    if $crate::mutex_hook::registered_hook_scope_enabled()
                        && !$crate::mutex_hook::hooked_cond_registered(cond)
                    {
                        return real_pthread_cond_signal(cond);
                    }

                    let cond_state = match ensure_cond_state(cond) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    if signal_one_waiter(cond_state) {
                        futex_wake(&(*cond_state).seq, 1);
                    }
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_broadcast(
                cond: *mut libc::pthread_cond_t,
            ) -> libc::c_int {
                unsafe {
                    if $crate::mutex_hook::registered_hook_scope_enabled()
                        && !$crate::mutex_hook::hooked_cond_registered(cond)
                    {
                        return real_pthread_cond_broadcast(cond);
                    }

                    let cond_state = match ensure_cond_state(cond) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    if (*cond_state).waiters.swap(0, Ordering::AcqRel) != 0 {
                        (*cond_state).seq.fetch_add(1, Ordering::Release);
                        futex_wake(&(*cond_state).seq, libc::c_int::MAX);
                    }
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_wait(
                cond: *mut libc::pthread_cond_t,
                user_mutex: *mut libc::pthread_mutex_t,
            ) -> libc::c_int {
                unsafe {
                    if !should_hook_mutex(user_mutex) {
                        return real_pthread_cond_wait(cond, user_mutex);
                    }

                    let state = match ensure_state(user_mutex) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    let cond_state = match ensure_cond_state(cond) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    let seq = (*cond_state).seq.load(Ordering::Acquire);
                    (*cond_state).waiters.fetch_add(1, Ordering::AcqRel);
                    unlock_with_stats(&(*state).lock, (*state).lock_id);
                    publish_cond_reacquire_hint((*state).lock_id);
                    let mut slept = false;
                    while (*cond_state).seq.load(Ordering::Acquire) == seq {
                        let rc = futex_wait(&(*cond_state).seq, seq);
                        slept |= rc == 0 || rc == libc::EINTR;
                    }
                    relock_after_cond_wait(&(*state).lock, (*state).lock_id, slept);
                    0
                }
            }

            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn pthread_cond_timedwait(
                cond: *mut libc::pthread_cond_t,
                user_mutex: *mut libc::pthread_mutex_t,
                abstime: *const libc::timespec,
            ) -> libc::c_int {
                unsafe {
                    if abstime.is_null() {
                        return libc::EINVAL;
                    }

                    if !should_hook_mutex(user_mutex) {
                        return real_pthread_cond_timedwait(cond, user_mutex, abstime);
                    }

                    let state = match ensure_state(user_mutex) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    let cond_state = match ensure_cond_state(cond) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };
                    let seq = (*cond_state).seq.load(Ordering::Acquire);
                    let mut ret = 0;
                    (*cond_state).waiters.fetch_add(1, Ordering::AcqRel);
                    unlock_with_stats(&(*state).lock, (*state).lock_id);
                    publish_cond_reacquire_hint((*state).lock_id);
                    let mut slept = false;
                    while (*cond_state).seq.load(Ordering::Acquire) == seq {
                        let rc = futex_wait_until_realtime(&(*cond_state).seq, seq, abstime);
                        slept |= rc == 0 || rc == libc::EINTR || rc == libc::ETIMEDOUT;
                        if rc == libc::ETIMEDOUT {
                            if (*cond_state).seq.load(Ordering::Acquire) == seq {
                                cancel_one_waiter(cond_state);
                                ret = libc::ETIMEDOUT;
                            }
                            break;
                        }
                    }
                    relock_after_cond_wait(&(*state).lock, (*state).lock_id, slept);
                    ret
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use libbpf_rs::MapFlags;

    use super::{
        ThreadCtxMapOps, cv_admission_counters, cv_admission_counters_enabled,
        hook_scope_value_is_registered, hooked_cond_registered, hooked_mutex_registered,
        record_cv_fallback_relock, record_cv_hint_published, record_cv_specialized_relock,
        register_hooked_cond_addr, register_hooked_mutex_addr, register_thread_ctx_with_map,
        set_cv_admission_counters_enabled, unregister_hooked_cond_addr,
        unregister_hooked_mutex_addr, unregister_thread_ctx_with_map,
    };

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum MapCall {
        Update {
            key: Vec<u8>,
            value: Vec<u8>,
            flags: MapFlags,
        },
        Delete {
            key: Vec<u8>,
        },
    }

    #[derive(Default)]
    struct RecordingMap {
        calls: RefCell<Vec<MapCall>>,
    }

    impl ThreadCtxMapOps for RecordingMap {
        fn update_entry(&self, key: &[u8], value: &[u8], flags: MapFlags) -> bool {
            self.calls.borrow_mut().push(MapCall::Update {
                key: key.to_vec(),
                value: value.to_vec(),
                flags,
            });
            true
        }

        fn delete_entry(&self, key: &[u8]) -> bool {
            self.calls
                .borrow_mut()
                .push(MapCall::Delete { key: key.to_vec() });
            true
        }
    }

    impl RecordingMap {
        fn calls(&self) -> Vec<MapCall> {
            self.calls.borrow().clone()
        }
    }

    #[test]
    fn register_thread_ctx_uses_map_helper_update() {
        let map = RecordingMap::default();

        assert!(register_thread_ctx_with_map(&map, 7, 0x1122_3344_5566_7788,));

        assert_eq!(
            map.calls(),
            vec![MapCall::Update {
                key: 7u32.to_ne_bytes().to_vec(),
                value: 0x1122_3344_5566_7788u64.to_ne_bytes().to_vec(),
                flags: MapFlags::ANY,
            }]
        );
    }

    #[test]
    fn unregister_thread_ctx_uses_map_helper_delete() {
        let map = RecordingMap::default();

        assert!(unregister_thread_ctx_with_map(&map, 11));

        assert_eq!(
            map.calls(),
            vec![MapCall::Delete {
                key: 11u32.to_ne_bytes().to_vec(),
            }]
        );
    }

    #[test]
    fn hook_scope_registered_value_is_explicit() {
        assert!(hook_scope_value_is_registered(Some("registered")));
        assert!(hook_scope_value_is_registered(Some("REGISTERED")));
        assert!(!hook_scope_value_is_registered(None));
        assert!(!hook_scope_value_is_registered(Some("all")));
        assert!(!hook_scope_value_is_registered(Some("")));
    }

    #[test]
    fn hooked_mutex_registry_tracks_registered_addresses() {
        let mut mutex = std::mem::MaybeUninit::<libc::pthread_mutex_t>::uninit();
        let ptr = mutex.as_mut_ptr();

        unregister_hooked_mutex_addr(ptr);
        assert!(!hooked_mutex_registered(ptr));

        assert!(register_hooked_mutex_addr(ptr));
        assert!(hooked_mutex_registered(ptr));

        assert!(unregister_hooked_mutex_addr(ptr));
        assert!(!hooked_mutex_registered(ptr));
    }

    #[test]
    fn hooked_cond_registry_tracks_registered_addresses() {
        let mut cond = std::mem::MaybeUninit::<libc::pthread_cond_t>::uninit();
        let ptr = cond.as_mut_ptr();

        unregister_hooked_cond_addr(ptr);
        assert!(!hooked_cond_registered(ptr));

        assert!(register_hooked_cond_addr(ptr));
        assert!(hooked_cond_registered(ptr));

        assert!(unregister_hooked_cond_addr(ptr));
        assert!(!hooked_cond_registered(ptr));
    }

    #[test]
    fn cv_admission_counters_only_count_while_enabled() {
        assert!(!cv_admission_counters_enabled());

        let baseline = cv_admission_counters();
        record_cv_hint_published();
        record_cv_specialized_relock();
        record_cv_fallback_relock();
        assert_eq!(cv_admission_counters(), baseline);

        set_cv_admission_counters_enabled(true);
        record_cv_hint_published();
        record_cv_specialized_relock();
        record_cv_fallback_relock();
        set_cv_admission_counters_enabled(false);

        let counters = cv_admission_counters();
        assert_eq!(counters.hints_published, baseline.hints_published + 1);
        assert_eq!(
            counters.specialized_relocks,
            baseline.specialized_relocks + 1
        );
        assert_eq!(counters.fallback_relocks, baseline.fallback_relocks + 1);
    }

    fn hook_implementation_source() -> &'static str {
        include_str!("mutex_hook.rs")
            .split_once("#[cfg(test)]")
            .map(|(implementation, _)| implementation)
            .expect("mutex_hook.rs should contain a test module")
    }

    #[test]
    fn cond_wait_path_marks_admission_hint_before_futex_sleep() {
        let implementation = hook_implementation_source();

        assert!(implementation.contains("const DISABLE_CV_ADMISSION_HINT_ENV"));
        assert!(implementation.contains("fn cv_admission_hint_enabled()"));

        assert_eq!(
            implementation
                .matches("publish_cond_reacquire_hint(")
                .count(),
            3,
            "one definition plus the cond_wait and cond_timedwait call sites"
        );

        let cond_wait = implementation
            .split_once("pub unsafe extern \"C\" fn pthread_cond_wait")
            .map(|(_, body)| body)
            .expect("the macro should define the cond wait hook");
        let hint_pos = cond_wait
            .find("publish_cond_reacquire_hint(")
            .expect("cond wait should publish the lock admission hint");
        let wait_pos = cond_wait
            .find("futex_wait(&(*cond_state).seq, seq)")
            .expect("cond wait should still use futex wait");

        assert!(
            hint_pos < wait_pos,
            "cond wait should publish the admission hint before sleeping"
        );
    }

    #[test]
    fn cond_wait_path_relocks_through_the_specialized_reacquire() {
        let implementation = hook_implementation_source();

        assert_eq!(
            implementation
                .matches("relock_after_cond_wait(&(*state).lock, (*state).lock_id, slept)")
                .count(),
            2,
            "both cond wait paths should relock through the cond-specific reacquire"
        );

        let reacquire = implementation
            .split_once("fn relock_after_cond_wait")
            .and_then(|(_, rest)| rest.split_once("#[inline(always)]"))
            .map(|(body, _)| body)
            .expect("the macro should define the cond-specific reacquire");

        assert!(reacquire.contains("take_cond_reacquire_pending_for_scope"));
        assert!(reacquire.contains("AcquireMode::AlreadyAdmitted"));
        assert!(
            !reacquire.contains("yield_now"),
            "the already-admitted arm should not notify admission again"
        );
    }
}
