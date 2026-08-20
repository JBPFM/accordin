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

static CV_COUNTERS_ENABLED: AtomicBool = AtomicBool::new(false);
static CV_HINTS_PUBLISHED: AtomicU64 = AtomicU64::new(0);
static CV_SPECIALIZED_RELOCKS: AtomicU64 = AtomicU64::new(0);
static CV_FALLBACK_RELOCKS: AtomicU64 = AtomicU64::new(0);
static CV_ROUTE_RELOCKS: AtomicU64 = AtomicU64::new(0);
static CV_REQUEUED: AtomicU64 = AtomicU64::new(0);
static CV_REQUEUE_FALLBACKS: AtomicU64 = AtomicU64::new(0);
static CV_DRAIN_WAKES: AtomicU64 = AtomicU64::new(0);
static CV_STRANDING_WAKES: AtomicU64 = AtomicU64::new(0);
static CV_STAGED_HIGH_WATER: AtomicU64 = AtomicU64::new(0);
static CV_BINDING_CONFLICTS: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_ARMS: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_SPURIOUS_WAKES: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_COMPLETED_POSTS: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_LEADER_POSTS: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_ROUTE_TAKEBACKS: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_ROUTE_TAKEBACK_UNPUBLISHED: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_ROUTE_TAKEBACK_LOST: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_COMPLETED_MISROUTES: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_RELOCKS_ADMITTED: AtomicU64 = AtomicU64::new(0);
static WRITER_EVENT_RELOCKS_NORMAL: AtomicU64 = AtomicU64::new(0);

/// Reconciles the userspace side of the cond-reacquire protocol against the
/// scheduler's wake-routing counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CvAdmissionCounters {
    pub hints_published: u64,
    pub specialized_relocks: u64,
    pub fallback_relocks: u64,
    /// Re-acquisitions that took the admission decision from a scheduler-routed
    /// wakeup rather than from a hint in the user word.
    pub route_relocks: u64,
}

pub fn cv_admission_counters() -> CvAdmissionCounters {
    CvAdmissionCounters {
        hints_published: CV_HINTS_PUBLISHED.load(Ordering::Relaxed),
        specialized_relocks: CV_SPECIALIZED_RELOCKS.load(Ordering::Relaxed),
        fallback_relocks: CV_FALLBACK_RELOCKS.load(Ordering::Relaxed),
        route_relocks: CV_ROUTE_RELOCKS.load(Ordering::Relaxed),
    }
}

/// Accounts the staging a cond broadcast parks its waiters on against the
/// unlocks that release them. Only broadcasts stage, so every count here is
/// about a wake-all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CvRequeueCounters {
    /// Waiters the kernel moved from the cond sequence word to a lock's stage.
    pub requeued: u64,
    /// Broadcasts that gave up on the requeue and woke their waiters directly,
    /// so the wakeup kept its correctness and lost its pacing.
    pub fallbacks: u64,
    /// Staged waiters released by an unlock.
    pub drain_wakes: u64,
    /// Staged waiters released by a broadcaster that found the lock free, which
    /// leaves no unlock to start the drain chain.
    pub stranding_wakes: u64,
    /// Most waiters ever staged on one lock at once.
    pub staged_high_water: u64,
    /// Conds that were waited on with a second mutex and gave their binding
    /// up over it, keeping their wakeups correct and losing the pacing. A
    /// cond re-associated with another mutex after its first association ended
    /// is counted here too: the two are indistinguishable from a binding.
    pub binding_conflicts: u64,
}

pub fn cv_requeue_counters() -> CvRequeueCounters {
    CvRequeueCounters {
        requeued: CV_REQUEUED.load(Ordering::Relaxed),
        fallbacks: CV_REQUEUE_FALLBACKS.load(Ordering::Relaxed),
        drain_wakes: CV_DRAIN_WAKES.load(Ordering::Relaxed),
        stranding_wakes: CV_STRANDING_WAKES.load(Ordering::Relaxed),
        staged_high_water: CV_STAGED_HIGH_WATER.load(Ordering::Relaxed),
        binding_conflicts: CV_BINDING_CONFLICTS.load(Ordering::Relaxed),
    }
}

/// Accounts the writer-event wake protocol against the routing it asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriterEventCounters {
    /// Waits that blocked and published the cv-sleep state, so the scheduler
    /// could route the wake that ends them. A wait that found its reason
    /// already published never publishes and is not counted here.
    pub arms: u64,
    /// Wakes that left the reason unset, so the wait blocked again.
    pub spurious_wakes: u64,
    /// Waiters released with their work already done, which return without
    /// re-acquiring the mutex.
    pub completed_posts: u64,
    /// Waiters released as the new queue head, whose wake keeps the routed
    /// state so the scheduler admits them as it makes them runnable.
    pub leader_posts: u64,
    /// Completed posts that took the routed state back from the waiter.
    pub route_takebacks: u64,
    /// Completed posts that found no state published, because the waiter had
    /// not reached its sleep yet.
    pub route_takeback_unpublished: u64,
    /// Completed posts that lost the exchange to a waiter which had already
    /// left its sleep and rewritten its own word.
    pub route_takeback_lost: u64,
    /// Completed wakes that still carried the routed state when the reason
    /// landed, so the scheduler admitted a re-acquisition that never happened.
    /// This is the outcome the takeback exists to keep at zero; the two counts
    /// above are the attempts, this is the result.
    pub completed_misroutes: u64,
    /// Re-acquisitions that entered on the decision their wake was granted.
    pub relocks_admitted: u64,
    /// Re-acquisitions that asked admission for a slot themselves, which is
    /// what a wait that never blocked leaves behind.
    pub relocks_normal: u64,
}

pub fn writer_event_counters() -> WriterEventCounters {
    WriterEventCounters {
        arms: WRITER_EVENT_ARMS.load(Ordering::Relaxed),
        spurious_wakes: WRITER_EVENT_SPURIOUS_WAKES.load(Ordering::Relaxed),
        completed_posts: WRITER_EVENT_COMPLETED_POSTS.load(Ordering::Relaxed),
        leader_posts: WRITER_EVENT_LEADER_POSTS.load(Ordering::Relaxed),
        route_takebacks: WRITER_EVENT_ROUTE_TAKEBACKS.load(Ordering::Relaxed),
        route_takeback_unpublished: WRITER_EVENT_ROUTE_TAKEBACK_UNPUBLISHED.load(Ordering::Relaxed),
        route_takeback_lost: WRITER_EVENT_ROUTE_TAKEBACK_LOST.load(Ordering::Relaxed),
        completed_misroutes: WRITER_EVENT_COMPLETED_MISROUTES.load(Ordering::Relaxed),
        relocks_admitted: WRITER_EVENT_RELOCKS_ADMITTED.load(Ordering::Relaxed),
        relocks_normal: WRITER_EVENT_RELOCKS_NORMAL.load(Ordering::Relaxed),
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

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_route_relock() {
    bump_cv_counter(&CV_ROUTE_RELOCKS);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_requeued(moved: u64) {
    if !CV_COUNTERS_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    CV_REQUEUED.fetch_add(moved, Ordering::Relaxed);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_requeue_fallback() {
    bump_cv_counter(&CV_REQUEUE_FALLBACKS);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_drain_wake() {
    bump_cv_counter(&CV_DRAIN_WAKES);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_stranding_wake() {
    bump_cv_counter(&CV_STRANDING_WAKES);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_binding_conflict() {
    bump_cv_counter(&CV_BINDING_CONFLICTS);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_arm() {
    bump_cv_counter(&WRITER_EVENT_ARMS);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_spurious_wake() {
    bump_cv_counter(&WRITER_EVENT_SPURIOUS_WAKES);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_completed_post() {
    bump_cv_counter(&WRITER_EVENT_COMPLETED_POSTS);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_leader_post() {
    bump_cv_counter(&WRITER_EVENT_LEADER_POSTS);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_route_takeback() {
    bump_cv_counter(&WRITER_EVENT_ROUTE_TAKEBACKS);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_route_takeback_unpublished() {
    bump_cv_counter(&WRITER_EVENT_ROUTE_TAKEBACK_UNPUBLISHED);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_route_takeback_lost() {
    bump_cv_counter(&WRITER_EVENT_ROUTE_TAKEBACK_LOST);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_completed_misroute() {
    bump_cv_counter(&WRITER_EVENT_COMPLETED_MISROUTES);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_relock_admitted() {
    bump_cv_counter(&WRITER_EVENT_RELOCKS_ADMITTED);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_relock_normal() {
    bump_cv_counter(&WRITER_EVENT_RELOCKS_NORMAL);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_staged_high_water(staged: u64) {
    if !CV_COUNTERS_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    CV_STAGED_HIGH_WATER.fetch_max(staged, Ordering::Relaxed);
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

// Registry membership is only consulted under registered hook scope, so writers are gated on it too.
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
            use std::sync::atomic::{AtomicUsize, Ordering};

            use $crate::condvar::{CondMutex, CondRelock, CondStaging, CondState};
            use $crate::lock_stats::{
                record_hold_end_sample, record_lock_acquired, record_lock_acquired_for_lock,
                record_lock_acquired_for_scope, record_post_unlock, record_thread_start,
                record_wait_end, record_wait_start,
            };
            use $crate::mutex_hook::MutexHookBackend;
            use $crate::writer_event::{EventRelock, WakeReason, WriterEvent};

            type Backend = $backend;

            struct HookState {
                lock: <Backend as MutexHookBackend>::LockState,
                lock_id: u32,
                /// Waiters a cond signal parked on this lock, released one per
                /// unlock. Most hooked mutexes never carry a cond, so the state
                /// stays on its own cache line and the unlock reads it once.
                staging: CondStaging,
            }

            impl HookState {
                fn new() -> Self {
                    Self {
                        lock: Backend::create_state(),
                        lock_id: if Backend::USES_ADMISSION_SCOPE {
                            $crate::admission::allocate_lock_class()
                        } else {
                            $crate::admission::UNMANAGED_LOCK_ID
                        },
                        staging: CondStaging::new(),
                    }
                }

                #[inline(always)]
                fn cond_mutex(&self) -> CondMutex {
                    CondMutex::new(
                        self.lock_id,
                        Backend::USES_ADMISSION_SCOPE,
                        self.staging.handle(),
                    )
                }
            }

            unsafe impl Send for HookState {}
            unsafe impl Sync for HookState {}

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
            fn lock_with_stats(state: &HookState) {
                if Backend::USES_ADMISSION_SCOPE {
                    let scope = $crate::admission::begin_lock_scope(state.lock_id);
                    lock_scope_with_stats(state, scope, AcquireMode::Normal);
                    return;
                }

                if Backend::try_lock(&state.lock) {
                    record_lock_acquired();
                    return;
                }

                let wait_start = record_wait_start();
                Backend::lock(&state.lock);
                record_wait_end(wait_start);
                record_lock_acquired();
            }

            #[inline(always)]
            fn lock_scope_with_stats(
                state: &HookState,
                scope: $crate::admission::LockAdmissionScope,
                mode: AcquireMode,
            ) {
                let normal = mode == AcquireMode::Normal;

                if normal
                    && !$crate::admission::token_consumed_for_scope(scope)
                    && Backend::try_lock(&state.lock)
                {
                    record_lock_acquired_for_scope(scope);
                    state.staging.handle().publish_hold();
                    return;
                }

                let wait_start = record_wait_start();
                if normal && $crate::admission::mark_slow_path_pending_for_scope(scope) {
                    std::thread::yield_now();
                    $crate::admission::clear_token_consumed_for_scope(scope);
                }
                Backend::lock(&state.lock);
                record_wait_end(wait_start);
                record_lock_acquired_for_scope(scope);
                state.staging.handle().publish_hold();
            }

            /// Reacquires the cond mutex after a wait, in the mode the wait
            /// protocol decided: a routed wakeup already carries the admission
            /// decision, and a published hint carries it in the user word, so
            /// both enter the lock without re-requesting admission.
            #[inline(always)]
            fn relock_after_cond_wait(state: &HookState, relock: CondRelock) {
                if !Backend::USES_ADMISSION_SCOPE {
                    lock_with_stats(state);
                    return;
                }

                let scope = $crate::admission::begin_lock_scope(state.lock_id);
                let mode = match relock {
                    CondRelock::AlreadyAdmitted => AcquireMode::AlreadyAdmitted,
                    CondRelock::TakeHint
                        if $crate::admission::take_cond_reacquire_pending_for_scope(scope) =>
                    {
                        $crate::mutex_hook::record_cv_specialized_relock();
                        AcquireMode::AlreadyAdmitted
                    }
                    _ => {
                        $crate::mutex_hook::record_cv_fallback_relock();
                        AcquireMode::Normal
                    }
                };
                lock_scope_with_stats(state, scope, mode);
            }

            /// Releases the lock and then hands the next staged cond waiter to
            /// the lock's own service rate: the release comes first so the
            /// waiter finds the lock free, and the phase bookkeeping comes
            /// before it so the drain never runs inside the critical section.
            ///
            /// The staging handle is read while the lock is still held. Past
            /// the release the mutex may already have been destroyed by the
            /// thread that took it next, and the drain then reads a word of a
            /// freed block rather than following a pointer out of freed state.
            #[inline(always)]
            fn unlock_with_stats(state: &HookState) {
                let hold_end = record_hold_end_sample();
                let staging = state.staging.handle();
                let lock_id = state.lock_id;
                if Backend::USES_ADMISSION_SCOPE {
                    staging.clear_hold();
                }
                Backend::unlock(&state.lock);
                if Backend::USES_ADMISSION_SCOPE {
                    $crate::admission::finish_lock_scope(lock_id);
                }
                record_post_unlock(hold_end, lock_id);
                staging.drain_one();
            }

            const SENTINEL: usize = 1;

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

                    let state = Box::new(HookState::new());
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

                    let state = Box::new(CondState::new());
                    let ptr = Box::into_raw(state);
                    std::ptr::write_bytes(
                        cond as *mut u8,
                        0,
                        std::mem::size_of::<libc::pthread_cond_t>(),
                    );
                    (*(cond as *mut usize)) = ptr as usize;
                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        $crate::mutex_hook::register_hooked_cond_addr(cond);
                    }
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

                    let state = Box::new(HookState::new());
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

                    let state = Box::new(CondState::new());
                    let ptr = Box::into_raw(state);
                    atomic.store(ptr as usize, Ordering::Release);
                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        $crate::mutex_hook::register_hooked_cond_addr(cond);
                    }
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
                        if $crate::mutex_hook::registered_hook_scope_enabled() {
                            $crate::mutex_hook::register_hooked_mutex_addr(mutex);
                        }
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
                    if ret == 0 && $crate::mutex_hook::registered_hook_scope_enabled() {
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
                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        $crate::mutex_hook::unregister_hooked_mutex_addr(mutex);
                    }

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

                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        $crate::mutex_hook::unregister_hooked_mutex_addr(mutex);
                    }

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
                    lock_with_stats(&*state);
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
                            (*state).staging.handle().publish_hold();
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
                        unlock_with_stats(state);
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

                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        $crate::mutex_hook::unregister_hooked_cond_addr(cond);
                    }

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
                    (*cond_state).signal();
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
                    (*cond_state).broadcast();
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
                    $crate::condvar::wait(
                        &*cond_state,
                        (*state).cond_mutex(),
                        || unlock_with_stats(&*state),
                        |relock| relock_after_cond_wait(&*state, relock),
                    );
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
                    $crate::condvar::timedwait(
                        &*cond_state,
                        (*state).cond_mutex(),
                        &*abstime,
                        || unlock_with_stats(&*state),
                        |relock| relock_after_cond_wait(&*state, relock),
                    )
                }
            }

            /// The writer event as the C ABI hands it out: the wake protocol
            /// plus the binding of the hooked mutex it belongs to.
            ///
            /// The whole object lives in caller-owned storage. The waiters this
            /// exists for are created per operation, and an allocation per
            /// operation is the cost the protocol is there to remove; for the
            /// same reason the binding is resolved once per mutex rather than
            /// once per waiter, so initializing one is a plain struct write.
            #[repr(C)]
            struct HookWriterEvent {
                event: WriterEvent,
                state: *mut HookState,
            }

            #[inline(always)]
            unsafe fn writer_event_ref<'a>(
                event: *mut libc::c_void,
            ) -> Result<&'a HookWriterEvent, libc::c_int> {
                if event.is_null() {
                    return Err(libc::EINVAL);
                }

                Ok(unsafe { &*event.cast::<HookWriterEvent>() })
            }

            /// The storage a caller has to provide for one event.
            #[unsafe(no_mangle)]
            pub extern "C" fn accordin_writer_event_size() -> usize {
                std::mem::size_of::<HookWriterEvent>()
            }

            /// The alignment that storage has to satisfy.
            #[unsafe(no_mangle)]
            pub extern "C" fn accordin_writer_event_align() -> usize {
                std::mem::align_of::<HookWriterEvent>()
            }

            /// Resolves a mutex to the binding its events are initialized from,
            /// which is the whole of what an event needs to know about it.
            ///
            /// Answering this costs a registry lookup under a scoped hook, and
            /// the answer never changes for a given mutex, so it is asked once
            /// and the caller keeps it for the mutex's lifetime. The binding
            /// stays valid for exactly that long: it names the hook state, which
            /// only a destroy of the mutex releases.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_writer_event_bind(
                mutex: *mut libc::c_void,
                out_binding: *mut *mut libc::c_void,
            ) -> libc::c_int {
                unsafe {
                    if out_binding.is_null() {
                        return libc::EINVAL;
                    }

                    // The re-acquisition enters the backend lock directly, so a
                    // mutex the hook is not interposing is refused rather than
                    // locked twice under two different lock words.
                    let mutex = mutex.cast::<libc::pthread_mutex_t>();
                    if !should_hook_mutex(mutex) {
                        return libc::ENOTSUP;
                    }

                    ensure_registered();
                    let state = match ensure_state(mutex) {
                        Ok(state) => state,
                        Err(ret) => return ret,
                    };

                    out_binding.write(state.cast::<libc::c_void>());
                    0
                }
            }

            /// Puts a fresh event into caller-owned storage, bound to what
            /// `accordin_writer_event_bind` resolved for its mutex.
            ///
            /// This runs once per waiter, so it does no lookups of its own. The
            /// thread is already registered by then: a waiter arms while holding
            /// the mutex, and taking the mutex is what registers it.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_writer_event_init(
                event: *mut libc::c_void,
                binding: *mut libc::c_void,
            ) -> libc::c_int {
                unsafe {
                    if event.is_null() || binding.is_null() {
                        return libc::EINVAL;
                    }

                    let event = event.cast::<HookWriterEvent>();
                    WriterEvent::init_in_place(std::ptr::addr_of_mut!((*event).event));
                    std::ptr::addr_of_mut!((*event).state).write(binding.cast::<HookState>());
                    0
                }
            }

            /// Declares the waiter unreleased. The caller must still hold the
            /// mutex: past the release a poster may publish a reason at any
            /// moment, and one arriving first would be overwritten here.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_writer_event_arm(
                event: *mut libc::c_void,
            ) -> libc::c_int {
                let event = match unsafe { writer_event_ref(event) } {
                    Ok(event) => event,
                    Err(ret) => return ret,
                };

                event.event.arm();
                0
            }

            /// Blocks until a poster publishes a reason and reports it: 0 for a
            /// waiter whose work is done, 1 for the new queue head. The caller
            /// must have released the mutex first.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_writer_event_wait(
                event: *mut libc::c_void,
            ) -> libc::c_int {
                let event = match unsafe { writer_event_ref(event) } {
                    Ok(event) => event,
                    Err(_) => return -1,
                };

                let lock_id = unsafe { (*event.state).lock_id };
                match event.event.wait(lock_id, Backend::USES_ADMISSION_SCOPE) {
                    WakeReason::Completed => 0,
                    WakeReason::BecomeLeader => 1,
                }
            }

            /// Re-acquires the mutex for a waiter the wait made leader, in the
            /// mode that wait decided: a routed wakeup already carries the
            /// admission decision and enters the lock without asking again.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_writer_event_relock(
                event: *mut libc::c_void,
            ) -> libc::c_int {
                let event = match unsafe { writer_event_ref(event) } {
                    Ok(event) => event,
                    Err(ret) => return ret,
                };
                let state = unsafe { &*event.state };

                if !Backend::USES_ADMISSION_SCOPE {
                    lock_with_stats(state);
                    return 0;
                }

                let scope = $crate::admission::begin_lock_scope(state.lock_id);
                let mode = match event.event.take_relock_mode() {
                    EventRelock::AlreadyAdmitted => AcquireMode::AlreadyAdmitted,
                    EventRelock::Normal => AcquireMode::Normal,
                };
                lock_scope_with_stats(state, scope, mode);
                0
            }

            /// Releases a waiter whose work the caller already did. The caller
            /// must have published the result first, and must never touch the
            /// event again: the waiter may destroy it the instant the reason
            /// lands.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_writer_event_post_completed(
                event: *mut libc::c_void,
            ) -> libc::c_int {
                let event = match unsafe { writer_event_ref(event) } {
                    Ok(event) => event,
                    Err(ret) => return ret,
                };

                event.event.post_completed();
                0
            }

            /// Hands the queue head to a waiter that still has work to do. As
            /// with a completed post, the event must not be touched again.
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn accordin_writer_event_post_leader(
                event: *mut libc::c_void,
            ) -> libc::c_int {
                let event = match unsafe { writer_event_ref(event) } {
                    Ok(event) => event,
                    Err(ret) => return ret,
                };

                event.event.post_leader();
                0
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use libbpf_rs::MapFlags;

    use super::{
        ThreadCtxMapOps, cv_admission_counters, cv_admission_counters_enabled, cv_requeue_counters,
        hook_scope_value_is_registered, hooked_cond_registered, hooked_mutex_registered,
        record_cv_drain_wake, record_cv_fallback_relock, record_cv_hint_published,
        record_cv_requeue_fallback, record_cv_requeued, record_cv_route_relock,
        record_cv_specialized_relock, record_cv_staged_high_water, record_cv_stranding_wake,
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
        let requeue_baseline = cv_requeue_counters();
        record_cv_hint_published();
        record_cv_specialized_relock();
        record_cv_fallback_relock();
        record_cv_requeued(4);
        record_cv_drain_wake();
        assert_eq!(cv_admission_counters(), baseline);
        assert_eq!(cv_requeue_counters(), requeue_baseline);

        set_cv_admission_counters_enabled(true);
        record_cv_hint_published();
        record_cv_specialized_relock();
        record_cv_fallback_relock();
        record_cv_route_relock();
        record_cv_requeued(2);
        record_cv_requeue_fallback();
        record_cv_drain_wake();
        record_cv_stranding_wake();
        record_cv_staged_high_water(requeue_baseline.staged_high_water + 3);
        set_cv_admission_counters_enabled(false);

        let counters = cv_admission_counters();
        assert_eq!(counters.hints_published, baseline.hints_published + 1);
        assert_eq!(
            counters.specialized_relocks,
            baseline.specialized_relocks + 1
        );
        assert_eq!(counters.fallback_relocks, baseline.fallback_relocks + 1);
        assert_eq!(counters.route_relocks, baseline.route_relocks + 1);

        let requeue = cv_requeue_counters();
        assert_eq!(requeue.requeued, requeue_baseline.requeued + 2);
        assert_eq!(requeue.fallbacks, requeue_baseline.fallbacks + 1);
        assert_eq!(requeue.drain_wakes, requeue_baseline.drain_wakes + 1);
        assert_eq!(
            requeue.stranding_wakes,
            requeue_baseline.stranding_wakes + 1
        );
        assert_eq!(
            requeue.staged_high_water,
            requeue_baseline.staged_high_water + 3,
            "the high-water mark keeps the largest stage it ever saw"
        );
    }

    fn hook_implementation_source() -> &'static str {
        include_str!("mutex_hook.rs")
            .split_once("#[cfg(test)]")
            .map(|(implementation, _)| implementation)
            .expect("mutex_hook.rs should contain a test module")
    }

    #[test]
    fn cond_wait_hooks_delegate_the_whole_wait_protocol() {
        let implementation = hook_implementation_source();

        assert!(
            !implementation.contains("SYS_futex"),
            "the futex protocol belongs to the shared condvar module"
        );

        for (name, hook, entry) in [
            (
                "cond wait",
                "pub unsafe extern \"C\" fn pthread_cond_wait",
                "$crate::condvar::wait(",
            ),
            (
                "cond timedwait",
                "pub unsafe extern \"C\" fn pthread_cond_timedwait",
                "$crate::condvar::timedwait(",
            ),
        ] {
            let body = implementation
                .split_once(hook)
                .map(|(_, body)| body)
                .unwrap_or_else(|| panic!("the macro should define the {name} hook"));
            let entry_pos = body
                .find(entry)
                .unwrap_or_else(|| panic!("{name} should run the shared wait protocol"));
            let unlock_pos = body
                .find("|| unlock_with_stats(")
                .unwrap_or_else(|| panic!("{name} should release the mutex through the protocol"));
            let relock_pos = body
                .find("|relock| relock_after_cond_wait(")
                .unwrap_or_else(|| panic!("{name} should relock in the mode the protocol picked"));

            assert!(
                entry_pos < unlock_pos && unlock_pos < relock_pos,
                "{name} should hand the unlock and the relock to the protocol"
            );
        }
    }

    #[test]
    fn cond_relock_enters_without_re_requesting_admission() {
        let implementation = hook_implementation_source();

        let reacquire = implementation
            .split_once("fn relock_after_cond_wait")
            .and_then(|(_, rest)| rest.split_once("#[inline(always)]"))
            .map(|(body, _)| body)
            .expect("the macro should define the cond-specific reacquire");

        assert!(reacquire.contains("CondRelock::AlreadyAdmitted => AcquireMode::AlreadyAdmitted"));
        assert!(reacquire.contains("take_cond_reacquire_pending_for_scope"));
        assert!(
            !reacquire.contains("yield_now"),
            "the already-admitted arm should not notify admission again"
        );
    }
}
