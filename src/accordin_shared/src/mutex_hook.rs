// SPDX-License-Identifier: GPL-2.0-only

use std::collections::HashSet;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use libbpf_rs::{MapCore, MapFlags, MapHandle};

use crate::admission::LockAdmissionScope;
use crate::arch::CacheAligned;
use crate::condvar::{CondRelock, CondStagingRef};
use crate::lock_backend::LockBackend;
use crate::lock_stats::{record_lock_acquired_for_scope, record_wait_end, record_wait_start};

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

/// Whether an acquire bracket still has to ask admission for a slot, or whether
/// the caller already holds the routing decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcquireMode {
    Normal,
    AlreadyAdmitted,
}

impl AcquireMode {
    /// The mode a writer-event re-acquisition enters in.
    ///
    /// The wait decided it outright, so no hint is consulted and no cond
    /// counter moves: the event path publishes no hint and accounts itself
    /// against counters of its own.
    #[inline(always)]
    pub fn for_event_relock(relock: CondRelock) -> Self {
        match relock {
            CondRelock::AlreadyAdmitted => Self::AlreadyAdmitted,
            _ => Self::Normal,
        }
    }
}

/// The mode a cond re-acquisition enters in.
///
/// A routed wakeup already carries the admission decision, and a published hint
/// carries it in the user word, so both enter the lock without re-requesting
/// admission. A hint that is no longer there leaves the re-acquisition an
/// ordinary contender.
#[inline(always)]
pub fn acquire_mode_for_cond_relock(relock: CondRelock, scope: LockAdmissionScope) -> AcquireMode {
    match relock {
        CondRelock::AlreadyAdmitted => AcquireMode::AlreadyAdmitted,
        CondRelock::TakeHint if crate::admission::take_cond_reacquire_pending_for_scope(scope) => {
            record_cv_specialized_relock();
            AcquireMode::AlreadyAdmitted
        }
        _ => {
            record_cv_fallback_relock();
            AcquireMode::Normal
        }
    }
}

/// Takes the backend lock inside an already-opened admission scope and accounts
/// the acquisition.
///
/// Only a `Normal` acquisition asks admission for a slot; a caller that already
/// holds the decision skips the uncontended attempt and the pending
/// notification, both of which exist to obtain one.
#[inline(always)]
pub fn lock_scope_with_stats<B>(
    lock: &B::LockState,
    staging: CondStagingRef,
    scope: LockAdmissionScope,
    mode: AcquireMode,
) where
    B: MutexHookBackend,
{
    let normal = mode == AcquireMode::Normal;

    if normal && !crate::admission::token_consumed_for_scope(scope) && B::try_lock(lock) {
        record_lock_acquired_for_scope(scope);
        staging.publish_hold();
        return;
    }

    let wait_start = record_wait_start();
    if normal && crate::admission::mark_slow_path_pending_for_scope(scope) {
        crate::admission::wait_for_slow_path_admission_for_scope(scope);
    }
    B::lock(lock);
    record_wait_end(wait_start);
    record_lock_acquired_for_scope(scope);
    staging.publish_hold();
}

static THREAD_CTX_MAP: OnceLock<MapHandle> = OnceLock::new();
static REGISTERED_THREAD_COUNT: AtomicUsize = AtomicUsize::new(0);
static REGISTERED_THREAD_COUNT_BPF: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
const HOOK_SCOPE_ENV: &str = "ACCORDIN_HOOK_SCOPE";

static CV_COUNTERS_ENABLED: AtomicBool = AtomicBool::new(false);

// Each counter family is one array indexed by its own enum, and the matching
// name array carries the field order the stats dump prints the family in.
enum CvAdmissionCounter {
    HintsPublished,
    SpecializedRelocks,
    FallbackRelocks,
    RouteRelocks,
}

const CV_ADMISSION_FIELD_COUNT: usize = 4;

pub(crate) const CV_ADMISSION_FIELD_NAMES: [&str; CV_ADMISSION_FIELD_COUNT] = [
    "hints_published",
    "specialized_relocks",
    "fallback_relocks",
    "route_relocks",
];

static CV_ADMISSION_COUNTERS: [AtomicU64; CV_ADMISSION_FIELD_COUNT] =
    [const { AtomicU64::new(0) }; CV_ADMISSION_FIELD_COUNT];

enum CvRequeueCounter {
    Requeued,
    Fallbacks,
    DrainWakes,
    StrandingWakes,
    StagedHighWater,
    BindingConflicts,
}

const CV_REQUEUE_FIELD_COUNT: usize = 6;

pub(crate) const CV_REQUEUE_FIELD_NAMES: [&str; CV_REQUEUE_FIELD_COUNT] = [
    "requeued",
    "fallbacks",
    "drain_wakes",
    "stranding_wakes",
    "staged_high_water",
    "binding_conflicts",
];

static CV_REQUEUE_COUNTERS: [AtomicU64; CV_REQUEUE_FIELD_COUNT] =
    [const { AtomicU64::new(0) }; CV_REQUEUE_FIELD_COUNT];

enum WriterEventCounter {
    Arms,
    SpuriousWakes,
    LeaderPosts,
    RouteTakebacks,
    RouteTakebackUnpublished,
    RouteTakebackLost,
    CompletedMisroutes,
    RelocksAdmitted,
    RelocksNormal,
}

const WRITER_EVENT_COUNTER_COUNT: usize = 9;

static WRITER_EVENT_COUNTERS: [AtomicU64; WRITER_EVENT_COUNTER_COUNT] =
    [const { AtomicU64::new(0) }; WRITER_EVENT_COUNTER_COUNT];

/// The writer-event family reports one field it does not store:
/// `completed_posts`, because every completed post leaves exactly one of the
/// three takeback outcomes behind and is therefore their sum.
const WRITER_EVENT_FIELD_COUNT: usize = WRITER_EVENT_COUNTER_COUNT + 1;

pub(crate) const WRITER_EVENT_FIELD_NAMES: [&str; WRITER_EVENT_FIELD_COUNT] = [
    "arms",
    "spurious_wakes",
    "completed_posts",
    "leader_posts",
    "route_takebacks",
    "route_takeback_unpublished",
    "route_takeback_lost",
    "completed_misroutes",
    "relocks_admitted",
    "relocks_normal",
];

enum AdmissionMarkCounter {
    SameClass,
    CrossClass,
    CrossClassTokenKept,
}

const ADMISSION_MARK_FIELD_COUNT: usize = 3;

pub(crate) const ADMISSION_MARK_FIELD_NAMES: [&str; ADMISSION_MARK_FIELD_COUNT] = [
    "pending_same_class",
    "pending_cross_class",
    "pending_cross_class_token_kept",
];

/// This family sits alone on its cache line: a mark is taken on every
/// contended acquisition, which is orders of magnitude more often than any
/// other family here counts, and sharing a line with them would let this one
/// dominate their cost the moment debug counters are on.
static ADMISSION_MARK_COUNTERS: CacheAligned<[AtomicU64; ADMISSION_MARK_FIELD_COUNT]> =
    CacheAligned([const { AtomicU64::new(0) }; ADMISSION_MARK_FIELD_COUNT]);

enum AdmissionGateCounter {
    WaitLoops,
    Timeouts,
    BypassUngrantable,
}

const ADMISSION_GATE_FIELD_COUNT: usize = 3;

pub(crate) const ADMISSION_GATE_FIELD_NAMES: [&str; ADMISSION_GATE_FIELD_COUNT] = [
    "gate_wait_loops",
    "gate_timeouts",
    "gate_bypass_ungrantable",
];

/// Kept on a line of its own for the same reason the marks above are: the loop
/// field moves once per spin of a gated wait, which is the densest write any
/// family here makes.
static ADMISSION_GATE_COUNTERS: CacheAligned<[AtomicU64; ADMISSION_GATE_FIELD_COUNT]> =
    CacheAligned([const { AtomicU64::new(0) }; ADMISSION_GATE_FIELD_COUNT]);

fn load_counters<const N: usize>(counters: &[AtomicU64; N]) -> [u64; N] {
    std::array::from_fn(|index| counters[index].load(Ordering::Relaxed))
}

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
    let values = cv_admission_field_values();
    CvAdmissionCounters {
        hints_published: values[CvAdmissionCounter::HintsPublished as usize],
        specialized_relocks: values[CvAdmissionCounter::SpecializedRelocks as usize],
        fallback_relocks: values[CvAdmissionCounter::FallbackRelocks as usize],
        route_relocks: values[CvAdmissionCounter::RouteRelocks as usize],
    }
}

pub(crate) fn cv_admission_field_values() -> [u64; CV_ADMISSION_FIELD_COUNT] {
    load_counters(&CV_ADMISSION_COUNTERS)
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
    let values = cv_requeue_field_values();
    CvRequeueCounters {
        requeued: values[CvRequeueCounter::Requeued as usize],
        fallbacks: values[CvRequeueCounter::Fallbacks as usize],
        drain_wakes: values[CvRequeueCounter::DrainWakes as usize],
        stranding_wakes: values[CvRequeueCounter::StrandingWakes as usize],
        staged_high_water: values[CvRequeueCounter::StagedHighWater as usize],
        binding_conflicts: values[CvRequeueCounter::BindingConflicts as usize],
    }
}

pub(crate) fn cv_requeue_field_values() -> [u64; CV_REQUEUE_FIELD_COUNT] {
    load_counters(&CV_REQUEUE_COUNTERS)
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
    let values = load_counters(&WRITER_EVENT_COUNTERS);
    WriterEventCounters {
        arms: values[WriterEventCounter::Arms as usize],
        spurious_wakes: values[WriterEventCounter::SpuriousWakes as usize],
        leader_posts: values[WriterEventCounter::LeaderPosts as usize],
        route_takebacks: values[WriterEventCounter::RouteTakebacks as usize],
        route_takeback_unpublished: values[WriterEventCounter::RouteTakebackUnpublished as usize],
        route_takeback_lost: values[WriterEventCounter::RouteTakebackLost as usize],
        completed_misroutes: values[WriterEventCounter::CompletedMisroutes as usize],
        relocks_admitted: values[WriterEventCounter::RelocksAdmitted as usize],
        relocks_normal: values[WriterEventCounter::RelocksNormal as usize],
    }
}

pub(crate) fn writer_event_field_values() -> [u64; WRITER_EVENT_FIELD_COUNT] {
    let counters = writer_event_counters();
    let completed_posts = counters
        .route_takebacks
        .saturating_add(counters.route_takeback_unpublished)
        .saturating_add(counters.route_takeback_lost);
    [
        counters.arms,
        counters.spurious_wakes,
        completed_posts,
        counters.leader_posts,
        counters.route_takebacks,
        counters.route_takeback_unpublished,
        counters.route_takeback_lost,
        counters.completed_misroutes,
        counters.relocks_admitted,
        counters.relocks_normal,
    ]
}

/// Accounts how a slow-path mark related to the class the admission word
/// already named, which is what decides whether the mark keeps a consumed
/// token or releases it.
///
/// Only marks that reached the word are counted: a run with admission disabled
/// rewrites the word without deciding anything about a token, and leaves every
/// field here at zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionMarkCounters {
    /// Marks for the class the word already named, which always keep whatever
    /// token the word carried.
    pub pending_same_class: u64,
    /// Marks that changed the class the word names.
    pub pending_cross_class: u64,
    /// Class-changing marks that carried a consumed token over, which only
    /// happens under `ACCORDIN_PRESERVE_CROSS_CLASS_TOKEN` and only when the
    /// word actually held one.
    pub pending_cross_class_token_kept: u64,
}

pub fn admission_mark_counters() -> AdmissionMarkCounters {
    let values = admission_mark_field_values();
    AdmissionMarkCounters {
        pending_same_class: values[AdmissionMarkCounter::SameClass as usize],
        pending_cross_class: values[AdmissionMarkCounter::CrossClass as usize],
        pending_cross_class_token_kept: values[AdmissionMarkCounter::CrossClassTokenKept as usize],
    }
}

pub(crate) fn admission_mark_field_values() -> [u64; ADMISSION_MARK_FIELD_COUNT] {
    load_counters(&ADMISSION_MARK_COUNTERS.0)
}

/// Accounts what the admission gate does to a managed slow path between its
/// pending mark and the acquisition that publishes its queue node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionGateCounters {
    /// Iterations a gated thread spent waiting for its grant, over and above
    /// the yield every slow path takes.
    pub gate_wait_loops: u64,
    /// Waits that gave up and published unadmitted, which is the liveness
    /// escape rather than an expected outcome.
    pub gate_timeouts: u64,
    /// Waits that found nothing able to grant them, so the gate let the caller
    /// through unchanged: no slots published, a CPU outside the published
    /// range, or a thread the scheduler never took into its map. None of the
    /// three can produce a grant, so waiting for one would never end.
    pub gate_bypass_ungrantable: u64,
}

pub fn admission_gate_counters() -> AdmissionGateCounters {
    let values = admission_gate_field_values();
    AdmissionGateCounters {
        gate_wait_loops: values[AdmissionGateCounter::WaitLoops as usize],
        gate_timeouts: values[AdmissionGateCounter::Timeouts as usize],
        gate_bypass_ungrantable: values[AdmissionGateCounter::BypassUngrantable as usize],
    }
}

pub(crate) fn admission_gate_field_values() -> [u64; ADMISSION_GATE_FIELD_COUNT] {
    load_counters(&ADMISSION_GATE_COUNTERS.0)
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
fn add_cv_counter(counter: &AtomicU64, amount: u64) {
    if !CV_COUNTERS_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    counter.fetch_add(amount, Ordering::Relaxed);
}

#[inline(always)]
fn bump_cv_counter(counter: &AtomicU64) {
    add_cv_counter(counter, 1);
}

#[inline(always)]
fn raise_cv_counter(counter: &AtomicU64, value: u64) {
    if !CV_COUNTERS_ENABLED.load(Ordering::Relaxed) {
        return;
    }

    counter.fetch_max(value, Ordering::Relaxed);
}

#[inline(always)]
fn bump_cv_admission(counter: CvAdmissionCounter) {
    bump_cv_counter(&CV_ADMISSION_COUNTERS[counter as usize]);
}

#[inline(always)]
fn bump_cv_requeue(counter: CvRequeueCounter) {
    bump_cv_counter(&CV_REQUEUE_COUNTERS[counter as usize]);
}

#[inline(always)]
fn bump_writer_event(counter: WriterEventCounter) {
    bump_cv_counter(&WRITER_EVENT_COUNTERS[counter as usize]);
}

#[inline(always)]
fn bump_admission_mark(counter: AdmissionMarkCounter) {
    bump_cv_counter(&ADMISSION_MARK_COUNTERS.0[counter as usize]);
}

#[inline(always)]
fn bump_admission_gate(counter: AdmissionGateCounter) {
    bump_cv_counter(&ADMISSION_GATE_COUNTERS.0[counter as usize]);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_admission_gate_wait_loop() {
    bump_admission_gate(AdmissionGateCounter::WaitLoops);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_admission_gate_timeout() {
    bump_admission_gate(AdmissionGateCounter::Timeouts);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_admission_gate_bypass() {
    bump_admission_gate(AdmissionGateCounter::BypassUngrantable);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_admission_mark_same_class() {
    bump_admission_mark(AdmissionMarkCounter::SameClass);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_admission_mark_cross_class() {
    bump_admission_mark(AdmissionMarkCounter::CrossClass);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_admission_mark_cross_class_token_kept() {
    bump_admission_mark(AdmissionMarkCounter::CrossClassTokenKept);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_hint_published() {
    bump_cv_admission(CvAdmissionCounter::HintsPublished);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_specialized_relock() {
    bump_cv_admission(CvAdmissionCounter::SpecializedRelocks);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_fallback_relock() {
    bump_cv_admission(CvAdmissionCounter::FallbackRelocks);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_route_relock() {
    bump_cv_admission(CvAdmissionCounter::RouteRelocks);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_requeued(moved: u64) {
    add_cv_counter(
        &CV_REQUEUE_COUNTERS[CvRequeueCounter::Requeued as usize],
        moved,
    );
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_requeue_fallback() {
    bump_cv_requeue(CvRequeueCounter::Fallbacks);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_drain_wake() {
    bump_cv_requeue(CvRequeueCounter::DrainWakes);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_stranding_wake() {
    bump_cv_requeue(CvRequeueCounter::StrandingWakes);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_binding_conflict() {
    bump_cv_requeue(CvRequeueCounter::BindingConflicts);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_arm() {
    bump_writer_event(WriterEventCounter::Arms);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_spurious_wake() {
    bump_writer_event(WriterEventCounter::SpuriousWakes);
}

/// Completed posts carry no counter of their own: each one leaves exactly one
/// of the three takeback outcomes behind, and the reported count is their sum.
#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_completed_post() {}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_leader_post() {
    bump_writer_event(WriterEventCounter::LeaderPosts);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_route_takeback() {
    bump_writer_event(WriterEventCounter::RouteTakebacks);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_route_takeback_unpublished() {
    bump_writer_event(WriterEventCounter::RouteTakebackUnpublished);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_route_takeback_lost() {
    bump_writer_event(WriterEventCounter::RouteTakebackLost);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_completed_misroute() {
    bump_writer_event(WriterEventCounter::CompletedMisroutes);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_relock_admitted() {
    bump_writer_event(WriterEventCounter::RelocksAdmitted);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_writer_event_relock_normal() {
    bump_writer_event(WriterEventCounter::RelocksNormal);
}

#[doc(hidden)]
#[inline(always)]
pub fn record_cv_staged_high_water(staged: u64) {
    raise_cv_counter(
        &CV_REQUEUE_COUNTERS[CvRequeueCounter::StagedHighWater as usize],
        staged,
    );
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
    // Published from here rather than from the callers: every hook discards the
    // result, and a thread the scheduler cannot read must not be held waiting
    // for a grant it will never be given.
    crate::admission::set_scheduler_registered(registered);
    if registered {
        let count = REGISTERED_THREAD_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        sync_registered_thread_count_to_bpf(count);
    }
    registered
}

#[doc(hidden)]
pub fn unregister_current_thread() {
    crate::admission::set_scheduler_registered(false);
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
                record_post_unlock, record_thread_start, record_wait_end, record_wait_start,
            };
            use $crate::mutex_hook::{
                AcquireMode, MutexHookBackend, acquire_mode_for_cond_relock, lock_scope_with_stats,
            };
            use $crate::writer_event::WriterEvent;

            type Backend = $backend;

            struct HookState {
                lock: <Backend as MutexHookBackend>::LockState,
                /// The managed class this lock is admitted through, claimed
                /// when the state is installed. Installation happens on the
                /// first acquisition, so the managed classes go to the locks a
                /// run takes first rather than to the ones it creates first.
                lock_id: u32,
                /// Waiters a cond signal parked on this lock, released one per
                /// unlock. Most hooked mutexes never carry a cond, so the
                /// staging stays empty here and only a first cond wait gives it
                /// a cache line of its own.
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

                /// The mutex as a cond wait sees it, which is the one path that
                /// gives the lock a staging block.
                #[inline(always)]
                fn cond_mutex(&self) -> CondMutex {
                    CondMutex::new(
                        self.lock_id,
                        Backend::USES_ADMISSION_SCOPE,
                        self.staging.binding_handle(),
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

            #[inline(always)]
            fn lock_with_stats(state: &HookState) {
                if Backend::USES_ADMISSION_SCOPE {
                    let scope = $crate::admission::begin_lock_scope(state.lock_id);
                    acquire_in_scope(state, scope, AcquireMode::Normal);
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

            /// Names the hook state's parts for the shared acquire bracket.
            #[inline(always)]
            fn acquire_in_scope(
                state: &HookState,
                scope: $crate::admission::LockAdmissionScope,
                mode: AcquireMode,
            ) {
                lock_scope_with_stats::<Backend>(&state.lock, state.staging.handle(), scope, mode);
            }

            /// Reacquires the cond mutex after a wait, in the mode the wait
            /// protocol decided. A backend outside admission has no scope to
            /// carry a decision, so its re-acquisition is an ordinary one.
            #[inline(always)]
            fn relock_after_cond_wait(state: &HookState, relock: CondRelock) {
                if !Backend::USES_ADMISSION_SCOPE {
                    lock_with_stats(state);
                    return;
                }

                let scope = $crate::admission::begin_lock_scope(state.lock_id);
                acquire_in_scope(state, scope, acquire_mode_for_cond_relock(relock, scope));
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

            /// The value a word holds before its state is installed, which is
            /// what a zeroed mutex, a `PTHREAD_MUTEX_INITIALIZER` static and a
            /// destroyed mutex all read as. Anything else in a mutex word is
            /// the state pointer: a mutex publishes its state in one exchange
            /// and is never seen part-way through an install.
            const NO_STATE: usize = 0;

            /// The value a cond word holds while its state is being installed.
            /// Only the cond word ever takes it, and reading a word against it
            /// separates an installed state from both an empty word and an
            /// install in progress.
            const COND_INSTALLING: usize = 1;

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

            /// Leaves the mutex in the state a `PTHREAD_MUTEX_INITIALIZER`
            /// static is already in, so that both kinds of mutex reach their
            /// first acquisition the same way and that acquisition installs the
            /// hook state.
            ///
            /// A process may create far more mutexes than it ever locks, and one
            /// that is only ever created then costs its own storage and nothing
            /// else.
            unsafe fn clear_hook_state(mutex: *mut libc::pthread_mutex_t) -> libc::c_int {
                unsafe {
                    if mutex.is_null() {
                        return libc::EINVAL;
                    }

                    std::ptr::write_bytes(
                        mutex as *mut u8,
                        0,
                        std::mem::size_of::<libc::pthread_mutex_t>(),
                    );
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
                if likely(val != NO_STATE) {
                    return Ok(val as *mut HookState);
                }
                unsafe { ensure_state_slow(mutex) }
            }

            /// Installs the state of a mutex that has none, which every mutex
            /// reaches once: a static one because it was never created through
            /// the hook, and a created one because creating it only zeroed it.
            ///
            /// Every contender builds a state of its own and races to publish
            /// it, and the losers free theirs and take the one that landed.
            /// Nothing here waits on another thread's allocation, so a first
            /// touch never blocks and `pthread_mutex_trylock` keeps its promise
            /// of returning either way. A loser gives its managed class back for
            /// the next lock this thread creates, so racing to create a lock
            /// costs no more classes than creating it alone would.
            #[cold]
            #[inline(never)]
            unsafe fn ensure_state_slow(
                mutex: *mut libc::pthread_mutex_t,
            ) -> Result<*mut HookState, libc::c_int> {
                let atomic = unsafe { state_atomic(mutex) };
                let candidate = Box::into_raw(Box::new(HookState::new()));
                match atomic.compare_exchange(
                    NO_STATE,
                    candidate as usize,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => Ok(candidate),
                    Err(published) => {
                        let candidate = unsafe { Box::from_raw(candidate) };
                        $crate::admission::release_unused_lock_class(candidate.lock_id);
                        drop(candidate);
                        Ok(published as *mut HookState)
                    }
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
                if likely(val > COND_INSTALLING) {
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
                    if val > COND_INSTALLING {
                        return Ok(val as *mut CondState);
                    }

                    if val == COND_INSTALLING {
                        spin_loop();
                        val = atomic.load(Ordering::Acquire);
                        continue;
                    }

                    if atomic
                        .compare_exchange(0, COND_INSTALLING, Ordering::AcqRel, Ordering::Acquire)
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
                    clear_hook_state(mutex)
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
                    if state_atomic(mutex).load(Ordering::Acquire) != NO_STATE {
                        if $crate::mutex_hook::registered_hook_scope_enabled() {
                            $crate::mutex_hook::register_hooked_mutex_addr(mutex);
                        }
                        return 0;
                    }

                    if $crate::mutex_hook::registered_hook_scope_enabled() {
                        let ret = real_pthread_mutex_destroy(mutex);
                        if ret != 0 {
                            return ret;
                        }
                    }

                    let ret = clear_hook_state(mutex);
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
                    if val != NO_STATE {
                        drop(Box::from_raw(val as *mut HookState));
                        atomic.store(NO_STATE, Ordering::Release);
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
                    if val != NO_STATE {
                        let ptr = val as *mut HookState;
                        drop(Box::from_raw(ptr));
                        atomic.store(NO_STATE, Ordering::Release);
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
                    if val != NO_STATE {
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
                    if val > COND_INSTALLING {
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

            /// Re-acquires the mutex for a waiter a writer-event wait made
            /// leader.
            ///
            /// A backend outside admission has no scope to carry a decision, so
            /// its re-acquisition is an ordinary one and the wait's decision is
            /// never read.
            #[inline(always)]
            fn relock_writer_event(state: &HookState, event: &WriterEvent) {
                if !Backend::USES_ADMISSION_SCOPE {
                    lock_with_stats(state);
                    return;
                }

                let scope = $crate::admission::begin_lock_scope(state.lock_id);
                let mode = AcquireMode::for_event_relock(event.take_relock_mode());
                acquire_in_scope(state, scope, mode);
            }

            $crate::export_writer_event_abi! {
                /// The writer event as the C ABI hands it out: the wake protocol
                /// plus the binding of the hooked mutex it belongs to.
                ///
                /// The whole object lives in caller-owned storage. The waiters
                /// this exists for are created per operation, and an allocation
                /// per operation is the cost the protocol is there to remove;
                /// for the same reason the binding is resolved once per mutex
                /// rather than once per waiter, so initializing one is a plain
                /// struct write.
                struct HookWriterEvent { binding: HookState }
                pointer = libc::c_void,
                admission_scoped = <Backend as MutexHookBackend>::USES_ADMISSION_SCOPE,
                relock = relock_writer_event,
                size = accordin_writer_event_size,
                align = accordin_writer_event_align,
                init = accordin_writer_event_init,
                arm = accordin_writer_event_arm,
                wait = accordin_writer_event_wait,
                relock_entry = accordin_writer_event_relock,
                post_completed = accordin_writer_event_post_completed,
                post_leader = accordin_writer_event_post_leader,
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
        }
    };
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use libbpf_rs::MapFlags;

    use super::{
        AcquireMode, ThreadCtxMapOps, acquire_mode_for_cond_relock, cv_admission_counters,
        cv_admission_counters_enabled, cv_requeue_counters, hook_scope_value_is_registered,
        hooked_cond_registered, hooked_mutex_registered, record_cv_drain_wake,
        record_cv_fallback_relock, record_cv_hint_published, record_cv_requeue_fallback,
        record_cv_requeued, record_cv_route_relock, record_cv_specialized_relock,
        record_cv_staged_high_water, record_cv_stranding_wake, register_hooked_cond_addr,
        register_hooked_mutex_addr, register_thread_ctx_with_map, unregister_hooked_cond_addr,
        unregister_hooked_mutex_addr, unregister_thread_ctx_with_map,
    };
    use crate::admission;
    use crate::condvar::CondRelock;

    /// A managed class of its own, so the scope this opens is the outermost one
    /// and the relock mapping sees the word it would see in a real wait.
    const RELOCK_LOCK_ID: u32 = 5;

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
        let measurement = crate::test_support::measure_debug_counters();
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

        measurement.enable();
        record_cv_hint_published();
        record_cv_specialized_relock();
        record_cv_fallback_relock();
        record_cv_route_relock();
        record_cv_requeued(2);
        record_cv_requeue_fallback();
        record_cv_drain_wake();
        record_cv_stranding_wake();
        record_cv_staged_high_water(requeue_baseline.staged_high_water + 3);
        drop(measurement);

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

    /// The wait protocol is shared, and the hooks are supposed to delegate the
    /// whole of it rather than keep a copy of the futex handshake. Nothing but
    /// the source itself can show that a copy is absent, so this reads it.
    fn hook_implementation_source() -> &'static str {
        include_str!("mutex_hook.rs")
            .split_once("#[cfg(test)]")
            .map(|(implementation, _)| implementation)
            .expect("mutex_hook.rs should contain a test module")
    }

    #[test]
    fn the_wait_protocol_stays_in_the_shared_condvar_module() {
        assert!(
            !hook_implementation_source().contains("SYS_futex"),
            "the futex protocol belongs to the shared condvar module"
        );
    }

    /// A routed wakeup was admitted as it was enqueued, so its re-acquisition
    /// enters on that decision without consulting the user word at all: a hint
    /// left there belongs to some other re-acquisition and has to survive.
    #[test]
    fn a_routed_cond_relock_enters_without_re_requesting_admission() {
        admission::reset_thread_depth_for_test();
        let scope = admission::begin_lock_scope(RELOCK_LOCK_ID);
        let before = admission::word_for_test();

        assert_eq!(
            acquire_mode_for_cond_relock(CondRelock::AlreadyAdmitted, scope),
            AcquireMode::AlreadyAdmitted
        );
        assert_eq!(
            admission::word_for_test(),
            before,
            "an already-admitted relock leaves the user word alone"
        );

        admission::finish_lock_scope(RELOCK_LOCK_ID);
        admission::reset_thread_depth_for_test();
    }
}
