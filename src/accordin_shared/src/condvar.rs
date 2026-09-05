// SPDX-License-Identifier: GPL-2.0-only

//! Futex condition variable used by the hooked `pthread_cond_t` and by the
//! direct lock API.
//!
//! The module owns the wait protocol: the waiter accounting, the sleep loop,
//! the sleep bracket the class stats read, and the admission state a waiter
//! publishes while its mutex is released. The caller owns the mutex itself and
//! supplies the two operations the protocol brackets its sleep with, an unlock
//! and a re-acquisition whose admission mode the protocol decides.
//!
//! A broadcast may either wake its waiters or requeue them onto the staging of
//! the mutex they will re-acquire. Requeueing turns a wake-to-run into a park:
//! the waiters stay off-CPU until an unlock of that mutex releases them one at
//! a time, so a broadcast no longer makes every waiter runnable and contending
//! at once. A signal releases a single waiter and always wakes it. Both forms
//! are private futex operations on the same words.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use crate::admission;
use crate::arch::CacheAligned;
use crate::lock_stats::{record_cv_sleep_end, record_cv_sleep_start};
use crate::mutex_hook::{
    record_cv_binding_conflict, record_cv_drain_wake, record_cv_hint_published,
    record_cv_requeue_fallback, record_cv_requeued, record_cv_route_relock,
    record_cv_staged_high_water, record_cv_stranding_wake,
};

const DISABLE_CV_ADMISSION_HINT_ENV: &str = "ACCORDIN_DISABLE_CV_ADMISSION_HINT";
/// Not prefixed by a backend name: the waiter publishes the cv-sleep state under
/// this switch and the scheduler loader reads the same one, whichever backend is
/// preloaded.
#[doc(hidden)]
pub const CV_ROUTE_ENV: &str = "ACCORDIN_CV_ROUTE";
const CV_REQUEUE_ENV: &str = "ACCORDIN_CV_REQUEUE";

const FUTEX_WAIT_PRIVATE: libc::c_int = 128;
const FUTEX_WAKE_PRIVATE: libc::c_int = 129;
const FUTEX_CMP_REQUEUE_PRIVATE: libc::c_int = 4 | 128;
const FUTEX_WAIT_BITSET_PRIVATE_REALTIME: libc::c_int = 9 | 128 | 256;
const FUTEX_BITSET_MATCH_ANY: libc::c_uint = libc::c_uint::MAX;

/// A requeue only fails against a sequence bump from another waker, which
/// cannot repeat indefinitely under any real wakeup rate; the tries are bounded
/// so a pathological one falls back to a wake instead of spinning.
const REQUEUE_ATTEMPTS: u32 = 3;

#[cfg(test)]
thread_local! {
    static ROUTE_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

thread_local! {
    static REQUEUE_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether any thread has ever asked for the per-thread requeue override.
///
/// Only tests do, and they live in other crates, so the override cannot be
/// compiled out. The flag keeps it out of the way instead: a preloaded library
/// never sets it, and every broadcast then decides from the environment alone
/// without reaching for thread-local storage.
static REQUEUE_OVERRIDDEN: AtomicBool = AtomicBool::new(false);

fn cv_admission_hint_enabled() -> bool {
    static CV_ADMISSION_HINT_ENABLED: OnceLock<bool> = OnceLock::new();
    *CV_ADMISSION_HINT_ENABLED.get_or_init(|| !crate::env::env_flag(DISABLE_CV_ADMISSION_HINT_ENV))
}

fn cv_route_env_enabled() -> bool {
    static CV_ROUTE: OnceLock<bool> = OnceLock::new();
    *CV_ROUTE.get_or_init(|| crate::env::env_flag(CV_ROUTE_ENV))
}

/// Whether a waiter hands its wakeup to the scheduler through the cv-sleep
/// state instead of the cond-reacquire hint. The routing is forced per thread
/// in tests so that a test never changes what concurrently running tests see.
#[cfg(test)]
#[inline(always)]
fn cv_route_enabled() -> bool {
    ROUTE_FORCED
        .with(|forced| forced.get())
        .unwrap_or_else(cv_route_env_enabled)
}

#[cfg(not(test))]
#[inline(always)]
fn cv_route_enabled() -> bool {
    cv_route_env_enabled()
}

#[cfg(test)]
fn force_cv_route_for_test(forced: Option<bool>) {
    ROUTE_FORCED.with(|route| route.set(forced));
}

fn cv_requeue_env_enabled() -> bool {
    static CV_REQUEUE: OnceLock<bool> = OnceLock::new();
    *CV_REQUEUE.get_or_init(|| crate::env::env_flag(CV_REQUEUE_ENV))
}

/// Whether a broadcast parks its waiters on the mutex staging instead of waking
/// them. Only the broadcasting side reads this: a drain is driven by the staged
/// count, so a lock keeps releasing waiters that were staged before the switch
/// was turned off.
#[inline]
fn cv_requeue_enabled() -> bool {
    let from_env = cv_requeue_env_enabled();
    if !REQUEUE_OVERRIDDEN.load(Ordering::Relaxed) {
        return from_env;
    }

    REQUEUE_FORCED
        .with(std::cell::Cell::get)
        .unwrap_or(from_env)
}

/// Overrides the requeue switch for the calling thread only, so that a test
/// choosing a mode never changes what threads outside it see.
#[doc(hidden)]
pub fn force_cv_requeue_for_thread(forced: Option<bool>) {
    announce_cv_requeue_override();
    REQUEUE_FORCED.with(|requeue| requeue.set(forced));
}

/// Announces that some thread of this process may turn requeueing on, without
/// deciding for any thread whether it does.
///
/// A test that stages waiters by hand needs the staging to be creatable before
/// it has chosen a mode, and a test that chose one must not have that choice
/// taken back.
#[doc(hidden)]
pub fn announce_cv_requeue_override() {
    REQUEUE_OVERRIDDEN.store(true, Ordering::Relaxed);
}

/// Whether anything in this process can ever park a waiter on a stage, and so
/// whether the lock brackets have to carry the staging bookkeeping at all.
///
/// A staged count only becomes nonzero through a broadcast that requeued, which
/// needs requeueing to have been on at that broadcast. The environment switch is
/// read once through a `OnceLock`, so off there is off for the whole run, and
/// the per-thread override is the only other way to turn it on: it announces
/// itself here before any broadcast can act on it, and never takes the
/// announcement back.
#[inline(always)]
fn cv_requeue_possible() -> bool {
    cv_requeue_env_enabled() || REQUEUE_OVERRIDDEN.load(Ordering::Relaxed)
}

/// Takes one release out of a counter of releases still owed, and reports
/// whether this call is the one that took it.
///
/// Zero is the floor: a release that is not there is never handed out, which is
/// what keeps a wakeup owed to exactly one waiter. Both the registered waiters
/// of a cond and the waiters parked on a stage are counted this way.
#[inline(always)]
fn claim_one(counter: &AtomicU32) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    while current != 0 {
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(next) => current = next,
        }
    }
    false
}

/// The words a broadcast parks waiters on, one block per lock.
///
/// `stage` is a wait target and nothing else: its value is never read or
/// compared, and waiters only ever arrive on it by being requeued off a cond
/// sequence word. `staged` counts the waiters parked there that no unlock has
/// released yet.
///
/// The block is shared: the lock owns it, and every cond whose waits release
/// that lock holds a reference of its own. A cond may outlive its mutex, and a
/// broadcast on it then still reads live memory rather than the freed lock;
/// what it finds is a retired block, which parks nothing.
///
/// The words sit alone on their cache line: every unlock of the lock reads
/// `staged`, and that read must not pull in the contended tail the lock hands
/// between its own waiters.
pub struct CondStagingBlock {
    words: CacheAligned<StagingWords>,
}

struct StagingWords {
    stage: AtomicU32,
    staged: AtomicU32,
    /// Cleared when the lock is destroyed. A retired block has no unlock left
    /// to release anything parked on it, so broadcasts stop parking there.
    alive: AtomicBool,
}

impl CondStagingBlock {
    const fn new() -> Self {
        Self::in_state(true)
    }

    /// The block every destroyed lock leaves in its own word, dead from the
    /// start. It parks nothing and owes nothing, so it needs no storage of its
    /// own and every retired lock shares the one.
    const fn retired() -> Self {
        Self::in_state(false)
    }

    const fn in_state(alive: bool) -> Self {
        Self {
            words: CacheAligned(StagingWords {
                stage: AtomicU32::new(0),
                staged: AtomicU32::new(0),
                alive: AtomicBool::new(alive),
            }),
        }
    }

    /// Whether the lock that owns the block still exists.
    ///
    /// Read and written with sequential consistency against the staged count:
    /// a destroy releases what it finds parked, a broadcast re-reads this after
    /// parking, and the two orders together leave no waiter parked on a block
    /// whose destroy has already run.
    #[inline]
    fn alive(&self) -> bool {
        self.words.0.alive.load(Ordering::SeqCst)
    }

    /// Marks the block dead and releases whatever is parked on it, so a mutex
    /// destroyed under a live cond leaves nobody waiting for an unlock that is
    /// never coming.
    fn retire(&self) {
        self.words.0.alive.store(false, Ordering::SeqCst);
        self.release_all();
    }

    /// Releases one staged waiter, and is the whole cost the staging adds to an
    /// unlock: a lock that never carried a cond wakeup pays one relaxed load of
    /// a line nothing else writes.
    #[inline(always)]
    fn drain_one(&self) {
        if self.words.0.staged.load(Ordering::Relaxed) == 0 {
            return;
        }

        self.drain_one_cold();
    }

    #[cold]
    #[inline(never)]
    fn drain_one_cold(&self) {
        if self.release_staged_waiter() {
            record_cv_drain_wake();
        }
    }

    /// Claims one staged waiter and wakes it. The claim is what paces the
    /// release: each caller takes at most one, so the waiters leave the stage
    /// at the rate the lock is being unlocked at.
    fn release_staged_waiter(&self) -> bool {
        let words = &self.words.0;
        if !claim_one(&words.staged) {
            return false;
        }

        unsafe { futex_wake(&words.stage, 1) };
        true
    }

    /// Gives up the pacing and releases everything at once, which is what a
    /// retired block does with the waiters it still carries.
    fn release_all(&self) {
        let words = &self.words.0;
        if words.staged.swap(0, Ordering::SeqCst) != 0 {
            unsafe { futex_wake(&words.stage, libc::c_int::MAX) };
        }
    }

    /// Accounts waiters the kernel moved onto the stage.
    ///
    /// The count only ever runs too high: a waiter that leaves the stage on a
    /// timeout or a spurious wake takes no claim with it. Too high costs one
    /// empty wake per stranded count and nothing else, while too low would
    /// leave a parked waiter with no release coming, so nothing but a claimed
    /// wake is allowed to decrement it.
    fn stage_waiters(&self, moved: u32) {
        if moved == 0 {
            return;
        }

        let previous = self.words.0.staged.fetch_add(moved, Ordering::SeqCst);
        record_cv_requeued(u64::from(moved));
        record_cv_staged_high_water(u64::from(previous) + u64::from(moved));
    }

    fn staged_count(&self) -> u32 {
        self.words.0.staged.load(Ordering::Acquire)
    }
}

/// The staging a lock owns.
///
/// The block is only reachable through a cond, so a lock no cond ever waits on
/// has none: the word stays null until a first wait binds one, and every lock
/// bracket of such a lock reads that null and stops. Most hooked mutexes are
/// never paired with a cond at all, and the block is a cache line of its own,
/// so creating one per mutex would spend the larger part of the hook's memory
/// on locks that can never park anything. A run that can never requeue creates
/// none at all: nothing would ever park on them.
///
/// The block is only ever created by a thread holding the lock, because the
/// only path that creates one is a cond wait and a cond wait holds the mutex it
/// is about to release. The drain accounting rests on that: a broadcaster asks
/// whether an unlock of the bound lock is still coming by asking what this
/// thread holds, and a block that could appear under a lock this thread does
/// not hold would make that question unanswerable.
///
/// The lock holds the block through this, and dropping it retires the block:
/// the memory stays for as long as some cond is still bound to it, but nothing
/// is parked there again. The destroy leaves the retired block behind in the
/// word rather than emptying it, so a first binding racing the destroy loses
/// against a block that is already dead instead of publishing one that no
/// unlock would ever drain.
///
/// A lock that gains its block inside a critical section is not recorded as the
/// stage the holder holds, because the acquisition that took it read no block.
/// That is the case a broadcaster does not recognise the lock in, and it costs
/// one release handed out early, which the drain chain is defined to absorb.
pub struct CondStaging {
    block: AtomicPtr<CondStagingBlock>,
}

/// The block a destroyed lock publishes in place of its own.
static RETIRED_STAGING: CondStagingBlock = CondStagingBlock::retired();

/// The published value that stands for a lock whose destroy has already run.
#[inline(always)]
fn retired_staging() -> *mut CondStagingBlock {
    &raw const RETIRED_STAGING as *mut CondStagingBlock
}

/// The block a binding may use, which a retired lock has none of.
#[inline(always)]
fn usable_block(block: *mut CondStagingBlock) -> *const CondStagingBlock {
    if std::ptr::eq(block, retired_staging()) {
        return std::ptr::null();
    }

    block
}

impl Default for CondStaging {
    fn default() -> Self {
        Self::new()
    }
}

impl CondStaging {
    pub const fn new() -> Self {
        Self {
            block: AtomicPtr::new(std::ptr::null_mut()),
        }
    }

    /// The handle the lock brackets carry, which names no block until a cond
    /// has bound one.
    ///
    /// An unlock reads the handle out before it releases the lock, so the
    /// pointer it drains through is one it read while the lock still existed.
    /// A word left by a destroy names the retired block, which parks nothing
    /// and drains to nothing, so the brackets need no check of their own for
    /// the one case that cannot reach them in a correct program anyway.
    #[inline(always)]
    pub fn handle(&self) -> CondStagingRef {
        CondStagingRef {
            block: self.block.load(Ordering::Acquire),
        }
    }

    /// The handle a cond wait binds to, which creates the block if this is the
    /// first wait to reach the lock and the run can requeue at all.
    ///
    /// A cond and its mutex are only ever seen together here, so this is both
    /// the first point that can tell the lock will carry cond waiters and the
    /// only path that needs the block to exist.
    #[inline]
    pub fn binding_handle(&self) -> CondStagingRef {
        CondStagingRef {
            block: self.ensure_block(cv_requeue_possible()),
        }
    }

    /// The block, if some cond has bound to this lock and the lock still
    /// exists.
    #[inline(always)]
    fn block(&self) -> Option<&CondStagingBlock> {
        unsafe { usable_block(self.block.load(Ordering::Acquire)).as_ref() }
    }

    /// The block to bind to, created on the first binding.
    ///
    /// A block only ever exists to be parked on, so a run whose broadcasts can
    /// never requeue creates none: the binding then names nothing, and the cond
    /// wakes its waiters as it does with no binding at all.
    fn ensure_block(&self, requeue_possible: bool) -> *const CondStagingBlock {
        if !requeue_possible {
            return std::ptr::null();
        }

        let block = self.block.load(Ordering::Acquire);
        if !block.is_null() {
            return usable_block(block);
        }

        self.install_block()
    }

    /// Publishes a block for the lock, keeping whatever a racing first waiter
    /// published instead if it lost. The reference the loser created is given
    /// back here, so exactly one block is ever owned per lock.
    ///
    /// A destroy publishes the retired block through the same word, so a
    /// binding that loses to one is told there is no staging rather than
    /// installing a block into a lock that is already gone.
    #[cold]
    #[inline(never)]
    fn install_block(&self) -> *const CondStagingBlock {
        let candidate = Arc::into_raw(Arc::new(CondStagingBlock::new())).cast_mut();
        match self.block.compare_exchange(
            std::ptr::null_mut(),
            candidate,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => candidate,
            Err(published) => {
                unsafe { drop(Arc::from_raw(candidate.cast_const())) };
                usable_block(published)
            }
        }
    }

    /// Whether a cond has bound to this lock, which the backend tests read to
    /// check that a wait reached the staging at all.
    #[doc(hidden)]
    pub fn block_created(&self) -> bool {
        self.block().is_some()
    }

    /// Waiters parked on the stage that no release has claimed yet, which the
    /// staging tests account against the wakes they expect. A lock with no
    /// block has nothing parked on it and never had.
    #[doc(hidden)]
    pub fn staged_count(&self) -> u32 {
        self.block().map_or(0, CondStagingBlock::staged_count)
    }

    /// A reference of the caller's own to the block, for tests that account the
    /// references a binding takes and gives back.
    #[cfg(test)]
    fn block_arc(&self) -> Arc<CondStagingBlock> {
        let block = self.ensure_block(true);
        assert!(!block.is_null(), "the lock should still be able to bind");
        unsafe {
            Arc::increment_strong_count(block);
            Arc::from_raw(block)
        }
    }
}

impl Drop for CondStaging {
    /// Takes the block out of the word and puts the retired one in its place,
    /// in one exchange: a first binding running concurrently either published
    /// before this and is retired here, or finds the retired block and binds to
    /// nothing. Neither order leaves a live block behind a destroyed lock.
    fn drop(&mut self) {
        let block = self.block.swap(retired_staging(), Ordering::AcqRel);
        if usable_block(block).is_null() {
            return;
        }

        unsafe {
            (*block).retire();
            drop(Arc::from_raw(block.cast_const()));
        }
    }
}

thread_local! {
    /// The staging of the outermost lock this thread holds.
    ///
    /// A broadcaster reads this to decide whether an unlock of its own is still
    /// coming to drain the stage it just parked on. The lock is identified by
    /// its block, not by its class: classes are shared as soon as locks are
    /// created past the class limit, and two locks reading as one there would
    /// let a broadcaster leave the release to an unlock of the other lock,
    /// which never drains this one.
    ///
    /// The outermost hold that has a block to name is the one published, which
    /// is the outermost hold of all whenever it carries a stage. Locks with no
    /// staging pass the record on rather than claiming it, so a thread holding
    /// one of those outside a lock that does carry a stage publishes the inner
    /// block; that is the block a broadcast of theirs could park on, so the
    /// record still answers the question it is read for. A broadcaster holding
    /// an inner lock whose outer one also has a block reads the outer one, does
    /// not recognise the inner block, and releases one waiter early, which
    /// costs the pacing of that one release and nothing else.
    static HELD_STAGING: std::cell::Cell<*const CondStagingBlock> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

/// How a wait names the staging of the mutex it releases, and how the lock
/// brackets name the staging they hold and drain.
///
/// The handle is borrowed: it is only ever produced from a live lock. A wait
/// reads it while it still holds the mutex, and takes the cond's own reference
/// to the block before the wait can end; an unlock reads it before the release
/// that lets the mutex be destroyed. A cond captures the handle on its first
/// wait and keeps it; rebinding to a different mutex is unsupported and costs
/// the cond its staging rather than parking a waiter on a lock that waiter
/// never releases.
///
/// The handle names no block at all for a lock no cond has waited on, which is
/// what every bracket of such a lock reads. Each of the three operations below
/// is then the gate and a null pointer.
#[derive(Clone, Copy)]
pub struct CondStagingRef {
    block: *const CondStagingBlock,
}

impl CondStagingRef {
    /// A cond whose waits release a mutex with no staging: its broadcasts wake
    /// their waiters and nothing is ever parked.
    pub const fn none() -> Self {
        Self {
            block: std::ptr::null(),
        }
    }

    /// Records the lock this handle belongs to as the one this thread holds,
    /// which a nested acquisition leaves to the lock it entered first.
    ///
    /// The record is read by nothing but a broadcast deciding who releases what
    /// it parked, so a run that can never park anything keeps the thread-local
    /// out of the lock bracket entirely.
    #[inline(always)]
    pub fn publish_hold(&self) {
        if cv_requeue_possible() {
            self.publish_hold_outlined();
        }
    }

    #[inline(never)]
    fn publish_hold_outlined(&self) {
        if self.block.is_null() {
            return;
        }

        HELD_STAGING.with(|held| {
            if held.get().is_null() {
                held.set(self.block);
            }
        });
    }

    /// Gives the record up on the release of the lock that took it. An inner
    /// lock releasing first finds a record that is not its own and leaves it.
    ///
    /// Gated with the publication it undoes: a run that never published one has
    /// nothing to give up.
    #[inline(always)]
    pub fn clear_hold(&self) {
        if cv_requeue_possible() {
            self.clear_hold_outlined();
        }
    }

    #[inline(never)]
    fn clear_hold_outlined(&self) {
        if self.block.is_null() {
            return;
        }

        HELD_STAGING.with(|held| {
            if std::ptr::eq(held.get(), self.block) {
                held.set(std::ptr::null());
            }
        });
    }

    /// Releases one staged waiter through a handle the caller read while the
    /// lock still existed.
    ///
    /// A staged count can only be nonzero if requeueing was on at some broadcast
    /// of this process, and the switch is read once through a `OnceLock`, so off
    /// means off for the whole run; the per-thread override is the only other
    /// way to turn it on and the same gate accounts for it. With the gate shut
    /// the unlock tail therefore never follows the handle to a count that cannot
    /// be nonzero, which saves a dependent pointer load and a second cache line.
    #[inline(always)]
    pub fn drain_one(&self) {
        if !cv_requeue_possible() {
            return;
        }

        if let Some(block) = unsafe { self.block.as_ref() } {
            block.drain_one();
        }
    }
}

/// Whether the outermost lock this thread holds is the one that owns `staging`,
/// and so whether an unlock that drains it is still to come from this thread.
#[inline]
fn thread_holds_staging(staging: &CondStagingBlock) -> bool {
    HELD_STAGING.with(|held| std::ptr::eq(held.get(), staging))
}

/// The one word a cond publishes its staging in.
///
/// The word carries an owned reference to the block, taken when the binding is
/// published and released when the cond is destroyed. Publishing the whole
/// handle in a single atomic is what makes a losing racer harmless: it never
/// leaves half of one binding next to half of another, and the loser drops the
/// reference it took.
///
/// The low bit marks a cond that was waited on with two different mutexes. The
/// mark leaves the pointer in place for the destroy to release, and the
/// binding stops being usable, so blocks are addressed by pointer identity
/// with no sentinel allocation to compare against.
struct BoundStaging(AtomicUsize);

const CONFLICTING_BINDING: usize = 1;

impl BoundStaging {
    const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    /// The block this cond may park new waiters on, which a binding that was
    /// given up has none of.
    #[inline]
    fn parking_target(&self) -> Option<&CondStagingBlock> {
        let value = self.0.load(Ordering::Acquire);
        if value & CONFLICTING_BINDING != 0 {
            return None;
        }

        self.block_of(value)
    }

    /// The block this cond is bound to whatever the state of the binding.
    ///
    /// Giving a binding up only stops new waiters being parked. Whatever was
    /// staged before it is still owed a release, so the drain path reads
    /// through the mark rather than losing the block behind it.
    #[inline]
    fn block(&self) -> Option<&CondStagingBlock> {
        self.block_of(self.0.load(Ordering::Acquire))
    }

    #[inline]
    fn block_of(&self, value: usize) -> Option<&CondStagingBlock> {
        let block = value & !CONFLICTING_BINDING;
        if block == 0 {
            return None;
        }

        Some(unsafe { &*(block as *const CondStagingBlock) })
    }

    /// The published pointer whatever its state, which is what a later wait
    /// compares its own mutex against.
    #[inline]
    fn raw(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    /// Publishes the binding, taking the cond's own reference to the block
    /// first so that the block outlives the lock if it has to. A racer that
    /// published ahead of this one keeps its binding and this reference is
    /// dropped; a racer that published a different lock is the unsupported
    /// pairing the binding gives itself up over.
    fn publish(&self, block: *const CondStagingBlock) {
        debug_assert_eq!(
            block as usize & CONFLICTING_BINDING,
            0,
            "a cache-line aligned block leaves the low bit to the conflict mark"
        );

        unsafe { Arc::increment_strong_count(block) };
        if let Err(published) =
            self.0
                .compare_exchange(0, block as usize, Ordering::Release, Ordering::Acquire)
        {
            unsafe { Arc::decrement_strong_count(block) };
            if published & !CONFLICTING_BINDING != block as usize {
                record_cv_binding_conflict();
                self.mark_conflicting();
            }
        }
    }

    fn mark_conflicting(&self) {
        self.0.fetch_or(CONFLICTING_BINDING, Ordering::AcqRel);
    }
}

impl Drop for BoundStaging {
    fn drop(&mut self) {
        let block =
            (self.0.load(Ordering::Acquire) & !CONFLICTING_BINDING) as *const CondStagingBlock;
        if !block.is_null() {
            unsafe { Arc::decrement_strong_count(block) };
        }
    }
}

/// The futex state behind one condition variable: `seq` is the word waiters
/// block on, and `waiters` counts the sleepers a signal may hand a wakeup to.
///
/// `bound` holds the staging of the mutex the waits release, published as one
/// word so that a broadcast reads a whole binding or none of one.
pub struct CondState {
    seq: AtomicU32,
    waiters: AtomicU32,
    bound: BoundStaging,
}

impl Default for CondState {
    fn default() -> Self {
        Self::new()
    }
}

impl CondState {
    pub const fn new() -> Self {
        Self {
            seq: AtomicU32::new(0),
            waiters: AtomicU32::new(0),
            bound: BoundStaging::new(),
        }
    }

    /// Releases one waiter, if any is registered.
    pub fn signal(&self) {
        if self.take_waiter() {
            self.wake_one_waiter();
        }
    }

    /// Releases every registered waiter with a single sequence bump.
    pub fn broadcast(&self) {
        if self.waiters.swap(0, Ordering::AcqRel) != 0 {
            self.seq.fetch_add(1, Ordering::Release);
            self.release_broadcast();
        }
    }

    /// Hands the waiters the sequence bump was made for either to the mutex
    /// staging or, when there is none to park them on, straight to the run
    /// queue. A waiter released either way re-checks the sequence and enters
    /// the same re-acquisition, so the two differ only in when it runs.
    ///
    /// Only a wake-all reaches the staging: a wake-one takes the plain wake
    /// below whatever the switch says.
    #[inline]
    fn release_broadcast(&self) {
        if cv_requeue_enabled() && self.requeue_waiters() {
            return;
        }

        unsafe { futex_wake(&self.seq, libc::c_int::MAX) };
    }

    /// Wakes one waiter on the sequence word, which is how every wake-one
    /// release reaches its waiter whether the staging switch is on or off.
    ///
    /// Staging costs one serialisation hop per released waiter, parking it
    /// behind the unlock of whoever woke it instead of letting it run into the
    /// re-acquisition: a broadcast amortises that hop over a herd that would
    /// otherwise contend all at once, while a wake-one has nothing to amortise
    /// it over and pays it on every hand-off, which measures as a large loss on
    /// hand-off workloads and a large win on broadcast ones.
    #[inline]
    fn wake_one_waiter(&self) {
        unsafe { futex_wake(&self.seq, 1) };
    }

    /// Moves every waiter of the bump onto the bound lock's stage. Reports
    /// whether the move happened, so the caller can wake them instead when it
    /// did not.
    fn requeue_waiters(&self) -> bool {
        let Some(staging) = self.bound.parking_target() else {
            return false;
        };
        // A lock that no longer exists has no unlock left to release anything
        // parked on it, so the broadcast falls back to waking its waiters.
        if !staging.alive() {
            return false;
        }

        for _ in 0..REQUEUE_ATTEMPTS {
            // The kernel compares the sequence word against this value and
            // rejects the move if another waker bumped it in between.
            let expected = self.seq.load(Ordering::Relaxed);
            match unsafe {
                futex_cmp_requeue(
                    &self.seq,
                    libc::c_int::MAX,
                    &staging.words.0.stage,
                    expected,
                )
            } {
                Ok(moved) => {
                    staging.stage_waiters(moved);
                    // Kicked even when nothing moved: waiters parked by an
                    // earlier broadcast are still owed a release, and a
                    // broadcast is where a chain that lost its last release
                    // picks up again.
                    start_drain_chain(staging);
                    return true;
                }
                Err(errno) if errno == libc::EAGAIN => continue,
                Err(_) => break,
            }
        }

        record_cv_requeue_fallback();
        false
    }

    /// Binds the cond to the staging of the mutex its waits release.
    ///
    /// The handle is captured on the first wait because that is the only point
    /// where a cond and its mutex are seen together; a broadcast only ever sees
    /// the cond. A cond that has never waited stays unbound, and its broadcasts
    /// wake rather than park.
    fn bind_staging(&self, staging: CondStagingRef) {
        if staging.block.is_null() {
            return;
        }

        let bound = self.bound.raw();
        if bound != 0 {
            if bound == staging.block as usize || bound & CONFLICTING_BINDING != 0 {
                return;
            }

            // Waiting on one cond with a second mutex is allowed as long as
            // the first association has ended, and the two are the same thing
            // from here: nothing distinguishes a re-association from a genuine
            // overlap, and staging a waiter on a lock its wait never releases
            // would leave it there. The cond gives its binding up either way
            // and goes back to waking the waiters a broadcast releases, and the
            // count says how often the pacing was lost this way.
            record_cv_binding_conflict();
            self.bound.mark_conflicting();
            return;
        }

        self.bound.publish(staging.block);
    }

    /// Passes on a release this waiter consumed without its wait ending.
    ///
    /// A waiter released from the stage whose sequence has not moved goes back
    /// to sleep on the cond. The release it spent produces no unlock of its
    /// own, so a waiter parked behind it would be left waiting for a drain
    /// that nothing runs once the lock goes quiet. Handing the release on
    /// keeps every staged waiter with a release still coming.
    #[inline]
    fn hand_on_stale_release(&self) {
        if let Some(staging) = self.bound.block() {
            staging.drain_one();
        }
    }

    /// Takes one registration and breaks the sleep condition of whoever holds
    /// it, which is the whole of a release on the cond word.
    #[inline(always)]
    fn take_waiter(&self) -> bool {
        if !self.cancel_waiter() {
            return false;
        }

        self.seq.fetch_add(1, Ordering::Release);
        true
    }

    /// Takes one registration and leaves the sequence alone, for a waiter
    /// withdrawing its own rather than releasing somebody else's.
    #[inline(always)]
    fn cancel_waiter(&self) -> bool {
        claim_one(&self.waiters)
    }

    /// Withdraws the registration of a waiter whose deadline expired, and
    /// reports what the wait returns.
    ///
    /// A registration names no particular waiter, so the withdrawal is only
    /// this waiter's while no signal ran between the deadline and it: one that
    /// did consumed this waiter's registration, and the count now stands for a
    /// waiter that registered afterwards. Retiring that one would leave it
    /// asleep with nothing left to wake it, so the sequence is re-read and a
    /// wakeup handed on instead. The forwarding releases exactly one waiter and
    /// so takes the wake-one path a signal takes. The wait then reports
    /// success, which POSIX permits: a timed wait may always return early as a
    /// spurious wake, and the caller re-checks its predicate under the
    /// re-acquired mutex.
    #[inline(always)]
    fn retire_expired_waiter(&self, seq: u32) -> libc::c_int {
        if !self.cancel_waiter() {
            // A signal or a broadcast already took the registration, so the
            // wait ends as the wakeup it was given rather than as a timeout.
            return 0;
        }

        if self.seq.load(Ordering::Acquire) == seq {
            return libc::ETIMEDOUT;
        }

        self.seq.fetch_add(1, Ordering::Release);
        self.wake_one_waiter();
        0
    }
}

/// The drain chain runs off unlocks, so the waiters a broadcast just staged are
/// released only once somebody unlocks the lock. The broadcaster answers
/// whether such an unlock exists by asking about itself: a broadcaster holding
/// the very lock the waiters will re-acquire has one still to come and leaves
/// the release to it, and any other broadcaster releases the first waiter
/// itself.
///
/// The question is deliberately about this thread rather than about the lock
/// word. A shared probe of the lock would be a store-buffer race against the
/// unlock that frees it, which no ordering short of a fence on the unlock path
/// closes; the thread's own record of what it holds is state only this thread
/// writes, and the answer cannot be stale.
///
/// A broadcaster that does not recognise the lock releases one waiter early,
/// which is defined to be harmless: the waiter re-acquires the mutex as an
/// ordinary contender, exactly as it would have without any staging. Backends
/// that keep no lock bracket of their own always take that branch.
fn start_drain_chain(staging: &CondStagingBlock) {
    // A destroy that ran while this broadcast was parking its waiters may have
    // released the stage already, and only this side still sees them.
    if !staging.alive() {
        staging.release_all();
        return;
    }

    if thread_holds_staging(staging) {
        return;
    }

    if staging.release_staged_waiter() {
        record_cv_stranding_wake();
    }
}

/// The mutex a cond wait releases across its sleep: the lock class it belongs
/// to, whether the backend runs that class through admission at all, and the
/// staging a broadcast may park the waiter on.
#[derive(Clone, Copy)]
pub struct CondMutex {
    lock_id: u32,
    admission_scoped: bool,
    staging: CondStagingRef,
}

impl CondMutex {
    pub const fn new(lock_id: u32, admission_scoped: bool, staging: CondStagingRef) -> Self {
        Self {
            lock_id,
            admission_scoped,
            staging,
        }
    }
}

/// How the waiter should re-acquire the cond mutex once its wait ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CondRelock {
    /// Ordinary contended acquisition: the re-acquisition asks admission for a
    /// slot itself.
    Normal,
    /// A cond-reacquire hint may be waiting in the user word. Taking it enters
    /// the lock without asking admission again; otherwise the acquisition falls
    /// back to `Normal`.
    TakeHint,
    /// The wakeup was routed by the scheduler, which granted the admission
    /// token as it enqueued the waiter, so the re-acquisition carries the
    /// decision already.
    AlreadyAdmitted,
}

/// Releases `mutex`, blocks until the condition variable is signalled, and
/// re-acquires it through `relock` in the mode the wait protocol decided.
#[inline]
pub fn wait<U, R>(cond: &CondState, mutex: CondMutex, unlock: U, relock: R)
where
    U: FnOnce(),
    R: FnOnce(CondRelock),
{
    // An untimed wait never reports a deadline, so the expiry branch of the
    // shared protocol is unreachable and its return value is always zero.
    wait_inner(cond, mutex, unlock, relock, |seq| unsafe {
        futex_wait(&cond.seq, seq)
    });
}

/// Like `wait`, but gives up at `abstime` and then reports `ETIMEDOUT`.
#[inline]
pub fn timedwait<U, R>(
    cond: &CondState,
    mutex: CondMutex,
    abstime: &libc::timespec,
    unlock: U,
    relock: R,
) -> libc::c_int
where
    U: FnOnce(),
    R: FnOnce(CondRelock),
{
    wait_inner(cond, mutex, unlock, relock, |seq| unsafe {
        futex_wait_until_realtime(&cond.seq, seq, abstime)
    })
}

/// The wait protocol both entry points run, parameterised on the blocking call
/// that sleeps on the cond sequence and reports its errno.
///
/// A requeue carries the armed deadline with the sleep, so a wait moved onto the
/// staging still expires there. Expiring off the stage takes no claim with it
/// and leaves the staged count too high, which is the same absorbed over-count a
/// spurious wake leaves.
#[inline]
fn wait_inner<U, R, B>(
    cond: &CondState,
    mutex: CondMutex,
    unlock: U,
    relock: R,
    mut block: B,
) -> libc::c_int
where
    U: FnOnce(),
    R: FnOnce(CondRelock),
    B: FnMut(u32) -> libc::c_int,
{
    cond.bind_staging(mutex.staging);
    let seq = cond.seq.load(Ordering::Acquire);
    let mut ret = 0;
    cond.waiters.fetch_add(1, Ordering::AcqRel);
    unlock();
    let routed = publish_sleep_state(mutex);
    // The mutex is released across the sleep, so the sleep lands in the
    // released class's unlock-to-acquire gap. Bracketing it here covers both
    // relock modes, which are chosen only after the sleep ends.
    let cv_sleep_start = record_cv_sleep_start();
    let mut slept = false;
    // A broadcast may have moved this sleep onto the mutex staging, in which
    // case the wait resumes from there. The loop does not care where it woke up:
    // the sequence decides whether the wait is over, and a wake off the stage
    // always follows a bump. The sequence read that ends an iteration is the one
    // the next iteration tests, so a stale wake costs a single load.
    let mut current = cond.seq.load(Ordering::Acquire);
    while current == seq {
        let rc = block(seq);
        slept |= rc == 0 || rc == libc::EINTR || rc == libc::ETIMEDOUT;
        current = cond.seq.load(Ordering::Acquire);
        if rc == libc::ETIMEDOUT {
            if current == seq {
                ret = cond.retire_expired_waiter(seq);
            }
            break;
        }
        if current == seq {
            cond.hand_on_stale_release();
        }
    }
    retire_sleep_state(routed, slept);
    record_cv_sleep_end(cv_sleep_start);
    relock(relock_mode(routed, slept));
    ret
}

/// Publishes the state the waiter sleeps under, and reports whether the
/// scheduler will route the wakeup from it.
///
/// With routing on, the user word names the lock to re-acquire and carries the
/// cv-sleep flag, which is what the scheduler routes the wakeup from; the
/// cond-reacquire hint is published instead whenever that state cannot be taken,
/// so a waiter that still holds a managed lock keeps the behaviour it has with
/// routing off.
///
/// Only the routed case is reported. A published hint and no publication at all
/// are the same thing to the re-acquisition: it looks for a hint in the word
/// either way, and finding none is the fallback the unpublished case relies on.
#[inline]
fn publish_sleep_state(mutex: CondMutex) -> bool {
    if !mutex.admission_scoped {
        return false;
    }

    if cv_route_enabled() && admission::arm_cv_sleep_for_lock(mutex.lock_id).is_armed() {
        return true;
    }

    if cv_admission_hint_enabled()
        && admission::mark_cond_reacquire_pending_for_cond_mutex(mutex.lock_id)
    {
        record_cv_hint_published();
    }

    false
}

/// Retires the cv-sleep state as soon as the wait stops blocking: from here on
/// the thread is runnable and the scheduler must no longer read it as sleeping.
///
/// What replaces it is the pending state of the same class, so the re-acquisition
/// that follows is described as the contention it is for its whole duration; the
/// scheduler reads a bare class as a thread that has finished with the lock.
#[inline]
fn retire_sleep_state(routed: bool, slept: bool) {
    if routed {
        admission::retire_cv_sleep_to_pending(slept);
    }
}

/// A waiter that never blocked was never routed and never had a hint answered,
/// so it re-acquires as an ordinary contender.
#[inline]
fn relock_mode(routed: bool, slept: bool) -> CondRelock {
    if !slept {
        return CondRelock::Normal;
    }

    if routed {
        record_cv_route_relock();
        return CondRelock::AlreadyAdmitted;
    }

    CondRelock::TakeHint
}

/// Returns 0 when the wait completed, otherwise the errno reported by the
/// syscall, so callers classify the outcome without reading thread-local errno
/// themselves.
#[inline(always)]
pub(crate) unsafe fn futex_wait(addr: *const AtomicU32, expected: u32) -> libc::c_int {
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

/// Moves waiters from `seq` onto `stage` without waking any of them, and
/// reports how many moved or the errno the syscall raised.
///
/// The wake count is fixed at zero: the point of the move is that the waiters
/// stay parked until the lock releases them. `expected` is checked against the
/// current value of `seq` by the kernel, which reports `EAGAIN` when another
/// waker bumped it first.
///
/// Every operation on both words carries the private flag, matching the waits
/// that put the waiters on `seq`: the flag decides which key namespace the
/// futex is looked up in, so a mismatch would silently address a different
/// queue. Condition variables shared across processes are outside what this
/// module supports, as they already were.
#[inline(always)]
unsafe fn futex_cmp_requeue(
    seq: *const AtomicU32,
    requeue: libc::c_int,
    stage: *const AtomicU32,
    expected: u32,
) -> Result<u32, libc::c_int> {
    unsafe {
        let ret = libc::syscall(
            libc::SYS_futex,
            seq as *const u32,
            FUTEX_CMP_REQUEUE_PRIVATE,
            0 as libc::c_int,
            requeue,
            stage as *const u32,
            expected,
        );
        if ret < 0 {
            Err(*libc::__errno_location())
        } else {
            Ok(ret as u32)
        }
    }
}

#[inline(always)]
pub(crate) unsafe fn futex_wake(addr: *const AtomicU32, count: libc::c_int) -> libc::c_long {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            addr as *const u32,
            FUTEX_WAKE_PRIVATE,
            count,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    use super::{
        CondMutex, CondRelock, CondStaging, CondStagingBlock, CondStagingRef, CondState,
        announce_cv_requeue_override, force_cv_requeue_for_thread, force_cv_route_for_test,
        futex_wake, publish_sleep_state, relock_mode, retire_sleep_state, timedwait, wait,
    };
    use crate::admission;
    use crate::mutex_hook::cv_requeue_counters;
    use crate::test_support::{await_progress, deadline_in_millis, waking_thread};

    const HINT_LOCK_ID: u32 = 4;
    const ROUTE_LOCK_ID: u32 = 5;
    const SMOKE_LOCK_ID: u32 = 6;

    struct RouteGuard;

    impl Drop for RouteGuard {
        fn drop(&mut self) {
            force_cv_route_for_test(None);
            force_cv_requeue_for_thread(None);
            admission::reset_thread_depth_for_test();
            admission::reset_state();
        }
    }

    /// Starts a wait from the state a hooked unlock leaves behind: the word
    /// names the released class and no scope stays held.
    fn released_cond_mutex(lock_id: u32, route: bool) -> RouteGuard {
        released_cond_mutex_with(lock_id, route, false)
    }

    fn released_cond_mutex_with(lock_id: u32, route: bool, requeue: bool) -> RouteGuard {
        force_cv_route_for_test(Some(route));
        force_cv_requeue_for_thread(Some(requeue));
        admission::reset_thread_depth_for_test();
        admission::reset_state();

        let scope = admission::begin_lock_scope(lock_id);
        admission::mark_critical_section_entered_for_scope(scope);
        admission::finish_lock_scope(lock_id);
        RouteGuard
    }

    fn word_lock_id() -> u32 {
        admission::word_for_test() >> admission::USER_ADMISSION_LOCK_ID_SHIFT
    }

    #[test]
    fn routing_off_publishes_the_cond_reacquire_hint() {
        let _guard = released_cond_mutex(HINT_LOCK_ID, false);

        assert!(!publish_sleep_state(CondMutex::new(
            HINT_LOCK_ID,
            true,
            CondStagingRef::none()
        )));
        assert!(!admission::cv_sleep_set_for_test(HINT_LOCK_ID));

        let scope = admission::begin_lock_scope(HINT_LOCK_ID);
        assert!(admission::take_cond_reacquire_pending_for_scope(scope));
        admission::finish_lock_scope(HINT_LOCK_ID);
    }

    #[test]
    fn routing_on_publishes_cv_sleep_and_retires_it_after_the_wait() {
        let _guard = released_cond_mutex(ROUTE_LOCK_ID, true);

        let routed =
            publish_sleep_state(CondMutex::new(ROUTE_LOCK_ID, true, CondStagingRef::none()));
        assert!(routed);
        assert!(admission::cv_sleep_set_for_test(ROUTE_LOCK_ID));
        assert_eq!(word_lock_id(), admission::class_of(ROUTE_LOCK_ID));

        let scope = admission::begin_lock_scope(ROUTE_LOCK_ID);
        assert!(
            !admission::take_cond_reacquire_pending_for_scope(scope),
            "the routed state must not read as a cond-reacquire hint"
        );
        admission::finish_lock_scope(ROUTE_LOCK_ID);

        retire_sleep_state(routed, true);
        assert!(!admission::cv_sleep_set_for_test(ROUTE_LOCK_ID));
        assert!(
            admission::slow_path_pending_set_for_test(ROUTE_LOCK_ID),
            "the re-acquisition contends as a pending waiter for the class"
        );
        assert_eq!(word_lock_id(), admission::class_of(ROUTE_LOCK_ID));
    }

    #[test]
    fn a_held_lock_keeps_the_routing_off_publication() {
        let _guard = released_cond_mutex(ROUTE_LOCK_ID, true);

        let outer = admission::begin_lock_scope(ROUTE_LOCK_ID);
        admission::mark_critical_section_entered_for_scope(outer);
        admission::begin_lock_scope(HINT_LOCK_ID);

        assert!(!publish_sleep_state(CondMutex::new(
            HINT_LOCK_ID,
            true,
            CondStagingRef::none()
        )));
        assert!(!admission::cv_sleep_set_for_test(HINT_LOCK_ID));
        assert_eq!(
            word_lock_id(),
            admission::class_of(ROUTE_LOCK_ID),
            "the held lock keeps the word it owns"
        );

        admission::finish_lock_scope(HINT_LOCK_ID);
        admission::finish_lock_scope(ROUTE_LOCK_ID);
    }

    #[test]
    fn an_unscoped_backend_publishes_nothing() {
        let _guard = released_cond_mutex(ROUTE_LOCK_ID, true);

        assert!(!publish_sleep_state(CondMutex::new(
            ROUTE_LOCK_ID,
            false,
            CondStagingRef::none()
        )));
        assert!(!admission::cv_sleep_set_for_test(ROUTE_LOCK_ID));
    }

    #[test]
    fn only_a_routed_sleep_relocks_as_already_admitted() {
        assert_eq!(relock_mode(true, true), CondRelock::AlreadyAdmitted);
        assert_eq!(relock_mode(false, true), CondRelock::TakeHint);

        for routed in [true, false] {
            assert_eq!(relock_mode(routed, false), CondRelock::Normal);
        }
    }

    /// A signal that lands before the sleep leaves the futex wait immediately,
    /// which is the path that must not claim an admission token. The scheduler
    /// never routed such a wait, so both publications retire into the same
    /// state: the class it will re-acquire, pending, with the token it consumed
    /// before the wait still recorded.
    #[test]
    fn a_wait_that_never_sleeps_relocks_normally_without_a_token() {
        for (route, lock_id) in [(false, HINT_LOCK_ID), (true, ROUTE_LOCK_ID)] {
            let _guard = released_cond_mutex(lock_id, route);

            let cond = CondState::new();
            let mut observed = None;
            wait(
                &cond,
                CondMutex::new(lock_id, true, CondStagingRef::none()),
                // A signal that lands while the mutex is being released.
                || {
                    cond.seq.fetch_add(1, Ordering::Release);
                },
                |relock| observed = Some(relock),
            );

            assert_eq!(observed, Some(CondRelock::Normal));
            assert!(!admission::cv_sleep_set_for_test(lock_id));
            assert!(admission::slow_path_pending_set_for_test(lock_id));

            let scope = admission::begin_lock_scope(lock_id);
            assert!(
                admission::token_consumed_for_scope(scope),
                "an unrouted relock suppresses its fast path either way"
            );
            admission::finish_lock_scope(lock_id);
        }
    }

    /// A signal landing between the deadline check and the cancel consumes the
    /// timing-out waiter's registration, and the count then stands for a waiter
    /// that registered behind it. The interleaving cannot be produced by timing
    /// here, so the state the two atomics are left in at that instant is built
    /// directly and the cancel is asked with the sequence the check saw.
    #[test]
    fn a_stolen_registration_forwards_the_wakeup_instead_of_timing_out() {
        let cond = CondState::new();
        let expired_seq = cond.seq.load(Ordering::Acquire);

        cond.waiters.fetch_add(1, Ordering::AcqRel);
        assert!(cond.take_waiter(), "the signal takes the expiring waiter");
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        let new_waiter_seq = cond.seq.load(Ordering::Acquire);

        assert_eq!(cond.retire_expired_waiter(expired_seq), 0);
        assert_ne!(
            cond.seq.load(Ordering::Acquire),
            new_waiter_seq,
            "the waiter behind it has to see its sleep condition broken"
        );
    }

    #[test]
    fn an_expired_waiter_with_no_signal_reports_the_deadline() {
        let cond = CondState::new();
        let expired_seq = cond.seq.load(Ordering::Acquire);
        cond.waiters.fetch_add(1, Ordering::AcqRel);

        assert_eq!(cond.retire_expired_waiter(expired_seq), libc::ETIMEDOUT);
        assert_eq!(cond.waiters.load(Ordering::Acquire), 0);
        assert_eq!(cond.seq.load(Ordering::Acquire), expired_seq);
    }

    #[test]
    fn a_registration_a_broadcast_already_took_ends_the_wait_as_a_wakeup() {
        let cond = CondState::new();
        let expired_seq = cond.seq.load(Ordering::Acquire);
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();

        assert_eq!(cond.retire_expired_waiter(expired_seq), 0);
    }

    #[test]
    fn a_timed_wait_that_expires_reports_the_timeout() {
        let _guard = released_cond_mutex(ROUTE_LOCK_ID, true);

        let cond = CondState::new();
        let abstime = deadline_in_millis(1);

        let mut observed = None;
        let ret = timedwait(
            &cond,
            CondMutex::new(ROUTE_LOCK_ID, true, CondStagingRef::none()),
            &abstime,
            || {},
            |relock| observed = Some(relock),
        );

        assert_eq!(ret, libc::ETIMEDOUT);
        assert_eq!(observed, Some(CondRelock::AlreadyAdmitted));
        assert!(!admission::cv_sleep_set_for_test(ROUTE_LOCK_ID));
    }

    /// The word a cv-sleep publication for `lock_id` leaves behind, taken from
    /// the admission module rather than rebuilt from its bit layout.
    fn armed_cv_sleep_word(lock_id: u32) -> u32 {
        admission::reset_thread_depth_for_test();
        admission::reset_state();
        let armed = admission::arm_cv_sleep_for_lock(lock_id).armed_value();
        admission::reset_state();

        assert_ne!(armed, 0, "a published sleep state is never an empty word");
        armed
    }

    /// The scheduler reads the published state through the waiter's admission
    /// word while the waiter is off-CPU, so the state has to be in place before
    /// the wait blocks and gone again as soon as it stops blocking.
    ///
    /// Both ends are observed from where the protocol runs: the waiter hands out
    /// the address of its own word from inside the unlock bracket, the other
    /// thread reads that word while the waiter is asleep, and the relock bracket
    /// reports what the word carries before the mutex is taken again.
    #[test]
    fn the_sleep_state_brackets_the_futex_wait() {
        let shared = Shared::new();
        let word_addr = Arc::new(AtomicUsize::new(0));
        let parked = Arc::new(AtomicU32::new(0));

        let waiter = {
            let shared = Arc::clone(&shared);
            let word_addr = Arc::clone(&word_addr);
            let parked = Arc::clone(&parked);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex(SMOKE_LOCK_ID, true);
                let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);
                let mut observed = None;

                shared.mutex.lock();
                while shared.payload.load(Ordering::Acquire) == 0 {
                    wait(
                        &shared.cond,
                        mutex,
                        || {
                            shared.mutex.unlock();
                            assert!(
                                !admission::cv_sleep_set_for_test(SMOKE_LOCK_ID),
                                "nothing is published while the mutex is still being released"
                            );
                            word_addr
                                .store(admission::user_word_addr() as usize, Ordering::Release);
                            parked.fetch_add(1, Ordering::Release);
                        },
                        |relock| {
                            observed =
                                Some((relock, admission::cv_sleep_set_for_test(SMOKE_LOCK_ID)));
                            shared.mutex.lock();
                        },
                    );
                }
                shared.mutex.unlock();
                observed.expect("the waiter should have re-acquired the mutex")
            })
        };

        await_progress(&parked, 1, "the waiter reaching its sleep");
        let armed = armed_cv_sleep_word(SMOKE_LOCK_ID);
        let sleeper_word = word_addr.load(Ordering::Acquire) as *const u32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut published = false;
        while std::time::Instant::now() < deadline {
            if unsafe { sleeper_word.read_volatile() } == armed {
                published = true;
                break;
            }
            std::thread::yield_now();
        }

        // The state is published just before the futex call, so observing it
        // says the waiter is about to block rather than that it already has.
        // The release has to land on a waiter that is asleep for the relock to
        // describe a routed wakeup.
        std::thread::sleep(std::time::Duration::from_millis(50));
        shared.mutex.lock();
        shared.payload.store(1, Ordering::Release);
        shared.cond.signal();
        shared.mutex.unlock();

        let (relock, cv_sleep) = waiter.join().expect("the waiter should finish");
        assert!(
            published,
            "the sleep state should be readable while the waiter is blocked"
        );
        assert_eq!(relock, CondRelock::AlreadyAdmitted);
        assert!(
            !cv_sleep,
            "the sleep state is retired as soon as the wait stops blocking"
        );
    }

    /// Stands in for a hooked mutex: it keeps the same bracket the hook does,
    /// publishing the staging it holds on acquisition and draining it on the
    /// unlock tail, which is where the released waiters of a signal come from.
    struct SpinMutex {
        locked: AtomicBool,
        staging: CondStaging,
    }

    impl SpinMutex {
        /// Every lock here stands in for one a broadcast may park on, so the
        /// announcement that makes a stage creatable is part of creating one.
        /// It leaves the mode each test chooses for itself alone.
        fn new() -> Self {
            announce_cv_requeue_override();
            Self {
                locked: AtomicBool::new(false),
                staging: CondStaging::new(),
            }
        }

        fn lock(&self) {
            while self.locked.swap(true, Ordering::Acquire) {
                std::hint::spin_loop();
            }
            self.staging.handle().publish_hold();
        }

        fn unlock(&self) {
            let staging = self.staging.handle();
            staging.clear_hold();
            self.locked.store(false, Ordering::Release);
            staging.drain_one();
        }

        fn cond_mutex(&self, lock_id: u32) -> CondMutex {
            CondMutex::new(lock_id, true, self.staging.binding_handle())
        }

        /// The staging block of this lock, created as a first cond wait would
        /// create it, so that a test can stage waiters without one.
        fn block(&self) -> &CondStagingBlock {
            self.staging.binding_handle();
            self.staging
                .block()
                .expect("binding the staging creates the block")
        }
    }

    struct Shared {
        mutex: SpinMutex,
        cond: CondState,
        payload: AtomicU32,
    }

    impl Shared {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                mutex: SpinMutex::new(),
                cond: CondState::new(),
                payload: AtomicU32::new(0),
            })
        }
    }

    fn run_cond_handoff(route: bool, requeue: bool) -> CondRelock {
        let shared = Shared::new();

        let waiter = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex_with(SMOKE_LOCK_ID, route, requeue);
                let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);
                let mut observed = CondRelock::Normal;

                shared.mutex.lock();
                while shared.payload.load(Ordering::Acquire) == 0 {
                    wait(
                        &shared.cond,
                        mutex,
                        || shared.mutex.unlock(),
                        |relock| {
                            observed = relock;
                            shared.mutex.lock();
                        },
                    );
                }
                let payload = shared.payload.load(Ordering::Acquire);
                shared.mutex.unlock();
                (payload, observed)
            })
        };

        let _waker = waking_thread(requeue);
        std::thread::sleep(std::time::Duration::from_millis(50));
        shared.mutex.lock();
        shared.payload.store(7, Ordering::Release);
        shared.cond.signal();
        shared.mutex.unlock();

        let (payload, observed) = waiter.join().expect("the waiter should finish");
        assert_eq!(payload, 7);
        observed
    }

    /// The scheduler grants the wake its admission while the word still names
    /// the cond sleep, and reads that word again while the re-acquisition is
    /// contending. The class has to be published as pending for that whole
    /// window: a class with no flag reads as a thread done with the lock, and
    /// the grant would be taken back under the contention it was made for.
    #[test]
    fn a_routed_relock_contends_with_the_class_published_as_pending() {
        let shared = Shared::new();

        let waiter = {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex(SMOKE_LOCK_ID, true);
                let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);
                let mut observed = None;

                shared.mutex.lock();
                while shared.payload.load(Ordering::Acquire) == 0 {
                    wait(
                        &shared.cond,
                        mutex,
                        || shared.mutex.unlock(),
                        |relock| {
                            observed = Some((
                                relock,
                                admission::slow_path_pending_set_for_test(SMOKE_LOCK_ID),
                                admission::cv_sleep_set_for_test(SMOKE_LOCK_ID),
                            ));
                            shared.mutex.lock();
                        },
                    );
                }
                shared.mutex.unlock();
                observed.expect("the waiter should have re-acquired the mutex")
            })
        };

        std::thread::sleep(std::time::Duration::from_millis(50));
        shared.mutex.lock();
        shared.payload.store(3, Ordering::Release);
        shared.mutex.unlock();
        shared.cond.signal();

        let (relock, pending, cv_sleep) = waiter.join().expect("the waiter should finish");
        assert_eq!(relock, CondRelock::AlreadyAdmitted);
        assert!(pending, "the relock contends with the class pending");
        assert!(!cv_sleep, "the sleep state is retired before the relock");
    }

    /// A stage a waiter left without claiming a release keeps its count, so the
    /// next drains spend that count on wakes nobody is parked for. The only
    /// cost is those empty wakes: the count still reaches zero, and no waiter
    /// that is parked is ever left without a release.
    #[test]
    fn a_staged_count_left_too_high_costs_empty_wakes_and_nothing_else() {
        let staging = CondStaging::new();
        staging.ensure_block(true);
        let block = staging.block().expect("the binding creates the block");
        block.stage_waiters(2);
        assert_eq!(staging.staged_count(), 2);

        block.drain_one();
        block.drain_one();
        assert_eq!(staging.staged_count(), 0);

        block.drain_one();
        assert_eq!(staging.staged_count(), 0);
    }

    /// The unlock tail reads the staged count on every release, so the words
    /// keep a line to themselves rather than sharing the one the lock hands
    /// between its own waiters.
    #[test]
    fn staging_words_keep_their_own_cache_line() {
        assert_eq!(std::mem::align_of::<CondStagingBlock>(), 64);
        assert_eq!(std::mem::size_of::<CondStagingBlock>(), 64);
    }

    /// A lock only ever parks waiters that some cond hands it, so the block is
    /// worth its cache line only once a cond has waited on the lock. Ordinary
    /// acquisitions leave it uncreated however many of them run.
    #[test]
    fn a_lock_creates_its_staging_on_the_first_cond_binding() {
        let mutex = SpinMutex::new();
        let cond = CondState::new();
        let _guard = waking_thread(true);

        mutex.lock();
        mutex.unlock();
        assert!(
            mutex.staging.block().is_none(),
            "a lock no cond waits on never carries a stage"
        );

        cond.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);
        let block = mutex
            .staging
            .block()
            .expect("the first binding creates the block");

        let other = CondState::new();
        other.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);
        assert!(
            std::ptr::eq(
                mutex
                    .staging
                    .block()
                    .expect("the block outlives the first binding"),
                block
            ),
            "every cond of a lock binds to the one block the lock owns"
        );
        assert!(std::ptr::eq(
            cond.bound
                .parking_target()
                .expect("the first cond stays bound"),
            other
                .bound
                .parking_target()
                .expect("the second cond binds to the same lock")
        ));
    }

    /// A block exists to be parked on, so a run whose broadcasts can never
    /// requeue never creates one, however many conds wait on the lock.
    #[test]
    fn a_run_that_can_never_requeue_creates_no_staging() {
        let staging = CondStaging::new();

        assert!(staging.ensure_block(false).is_null());
        assert!(
            !staging.block_created(),
            "a wait that can never be parked leaves the lock without a stage"
        );

        assert!(!staging.ensure_block(true).is_null());
        assert!(staging.block_created());
    }

    /// A lock's destroy and a first binding on it can race. The destroy leaves
    /// the retired block in the word rather than emptying it, so the binding
    /// always loses: a block published after the destroy would take waiters
    /// that no unlock is left to release.
    #[test]
    fn a_binding_that_arrives_after_the_destroy_finds_no_staging() {
        announce_cv_requeue_override();
        let mut staging = std::mem::ManuallyDrop::new(CondStaging::new());
        // The destroy of the lock, with the word it leaves behind still
        // readable by the binding that lost to it.
        unsafe { std::ptr::drop_in_place(&mut *staging) };

        assert!(
            staging.ensure_block(true).is_null(),
            "a destroyed lock never gains a stage"
        );
        assert!(!staging.block_created());

        let cond = CondState::new();
        cond.bind_staging(staging.binding_handle());
        assert!(
            cond.bound.parking_target().is_none(),
            "a cond that binds after the destroy wakes its waiters instead"
        );
    }

    /// The block of a lock whose first cond wait happens inside a critical
    /// section is created while the lock is held, so the acquisition that took
    /// it recorded no stage and a broadcast from inside that same section does
    /// not recognise the lock it holds. The release it hands out early is the
    /// fallback the drain chain absorbs: nothing stays parked.
    #[test]
    fn a_stage_created_inside_the_critical_section_costs_one_early_release() {
        let mutex = SpinMutex::new();
        let cond = CondState::new();
        let _guard = waking_thread(true);

        mutex.lock();
        assert!(
            mutex.staging.block().is_none(),
            "the acquisition reads a lock with no stage"
        );

        cond.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);
        mutex
            .staging
            .block()
            .expect("the wait creates the stage under the held lock")
            .stage_waiters(1);

        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();
        assert_eq!(
            mutex.staging.staged_count(),
            0,
            "the broadcaster does not recognise the stage it gave the lock and releases the waiter itself"
        );

        mutex.unlock();
        assert_eq!(mutex.staging.staged_count(), 0);
    }

    /// The first waits of two conds on one lock can be concurrent, and the
    /// stage they park on has to be the same block: two blocks would leave the
    /// unlock draining one of them and the other's waiters unreleased.
    #[test]
    fn racing_first_bindings_converge_on_one_block() {
        const BINDERS: usize = 8;

        let mutex = Arc::new(SpinMutex::new());
        let barrier = Arc::new(std::sync::Barrier::new(BINDERS));

        let binders = (0..BINDERS)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let cond = CondState::new();
                    cond.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);
                    cond.bound
                        .parking_target()
                        .expect("every binder binds to the lock")
                        as *const CondStagingBlock as usize
                })
            })
            .collect::<Vec<_>>();

        let bound = binders
            .into_iter()
            .map(|binder| binder.join().expect("every binder should finish"))
            .collect::<Vec<_>>();

        let block = mutex.staging.block().expect("the lock has its block") as *const _ as usize;
        for observed in bound {
            assert_eq!(
                observed, block,
                "the losers of the install take the block that won"
            );
        }
        assert_eq!(
            Arc::strong_count(&mutex.staging.block_arc()),
            2,
            "the blocks the losers created are given back, leaving the lock's own"
        );
    }

    #[test]
    fn a_cond_keeps_the_staging_of_its_first_wait() {
        let mutex = SpinMutex::new();
        let cond = CondState::new();

        assert!(cond.bound.parking_target().is_none());
        cond.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);
        cond.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);

        let bound = cond
            .bound
            .parking_target()
            .expect("the wait should have bound");
        assert!(std::ptr::eq(bound, mutex.block()));
    }

    #[test]
    fn an_unbound_cond_wakes_without_reaching_a_staging() {
        let cond = CondState::new();
        cond.bind_staging(CondStagingRef::none());
        assert!(cond.bound.parking_target().is_none());

        let _guard = waking_thread(true);
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.signal();
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();
    }

    /// A cond used with a second mutex would otherwise park waiters on a lock
    /// they never release, so it gives up its staging and goes back to waking.
    #[test]
    fn a_cond_waited_on_with_two_mutexes_gives_up_its_staging() {
        let first = SpinMutex::new();
        let second = SpinMutex::new();
        let cond = CondState::new();

        cond.bind_staging(first.cond_mutex(SMOKE_LOCK_ID).staging);
        cond.bind_staging(second.cond_mutex(SMOKE_LOCK_ID).staging);
        assert!(cond.bound.parking_target().is_none());

        cond.bind_staging(first.cond_mutex(SMOKE_LOCK_ID).staging);
        assert!(
            cond.bound.parking_target().is_none(),
            "the conflict is not undone by a later wait"
        );

        let _guard = waking_thread(true);
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();
        assert_eq!(first.staging.staged_count(), 0);
        assert_eq!(second.staging.staged_count(), 0);
    }

    /// Re-associating a cond with another mutex once its first association has
    /// ended is allowed by POSIX and reads exactly like the unsupported
    /// overlap, so it costs the pacing rather than an assertion, and the count
    /// is what a run reports it by.
    #[test]
    fn a_second_mutex_costs_the_pacing_and_is_counted() {
        let first = SpinMutex::new();
        let second = SpinMutex::new();
        let cond = CondState::new();

        let measurement = crate::test_support::measure_debug_counters();
        measurement.enable();
        let before = cv_requeue_counters().binding_conflicts;
        cond.bind_staging(first.cond_mutex(SMOKE_LOCK_ID).staging);
        cond.bind_staging(second.cond_mutex(SMOKE_LOCK_ID).staging);
        let after = cv_requeue_counters().binding_conflicts;
        drop(measurement);

        assert_eq!(after, before + 1);
        assert!(cond.bound.parking_target().is_none());
    }

    /// The conflicting binding keeps the reference it published, so the block
    /// the first mutex owned is still there for the cond to release when it is
    /// destroyed rather than being dropped from under a concurrent signal.
    #[test]
    fn a_conflicting_binding_still_releases_its_reference() {
        let first = SpinMutex::new();
        let second = SpinMutex::new();
        let block = first.staging.block_arc();

        {
            let cond = CondState::new();
            cond.bind_staging(first.cond_mutex(SMOKE_LOCK_ID).staging);
            cond.bind_staging(second.cond_mutex(SMOKE_LOCK_ID).staging);
            assert_eq!(std::sync::Arc::strong_count(&block), 3);
        }

        assert_eq!(
            std::sync::Arc::strong_count(&block),
            2,
            "destroying the cond gives the block back to the lock that owns it"
        );
    }

    /// Giving a binding up stops new waiters being parked and nothing else.
    /// Whatever the cond staged before is still owed a release, so a waiter
    /// that wakes stale on it keeps handing its release on through the mark.
    #[test]
    fn a_conflicting_binding_still_hands_its_releases_on() {
        let first = SpinMutex::new();
        let second = SpinMutex::new();
        let cond = CondState::new();

        cond.bind_staging(first.cond_mutex(SMOKE_LOCK_ID).staging);
        first.block().stage_waiters(1);
        cond.bind_staging(second.cond_mutex(SMOKE_LOCK_ID).staging);
        assert!(
            cond.bound.parking_target().is_none(),
            "the binding no longer parks new waiters"
        );

        cond.hand_on_stale_release();
        assert_eq!(
            first.staging.staged_count(),
            0,
            "what was staged before the conflict still has its release handed on"
        );
    }

    /// Routing and requeueing decide different halves of a wakeup, so every
    /// combination has to hand a signal over.
    #[test]
    fn every_switch_combination_hands_a_signal_over() {
        for route in [false, true] {
            for requeue in [false, true] {
                let observed = run_cond_handoff(route, requeue);
                if route {
                    assert_ne!(observed, CondRelock::TakeHint);
                } else {
                    assert_ne!(observed, CondRelock::AlreadyAdmitted);
                }
            }
        }
    }

    /// Each side waits for the other's turn, so a wakeup that never arrives
    /// stops the round count where it stalled. The hand-off is a wake-one, so
    /// the switch being on changes nothing about how it is released.
    #[test]
    fn a_ping_pong_keeps_making_progress_with_the_switch_on() {
        const ROUNDS: u32 = 200;

        let shared = Shared::new();
        let rounds = Arc::new(AtomicU32::new(0));

        let consumer = {
            let shared = Arc::clone(&shared);
            let rounds = Arc::clone(&rounds);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex_with(SMOKE_LOCK_ID, false, true);
                let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);

                for _ in 0..ROUNDS {
                    shared.mutex.lock();
                    while shared.payload.load(Ordering::Acquire) == 0 {
                        wait(
                            &shared.cond,
                            mutex,
                            || shared.mutex.unlock(),
                            |_| shared.mutex.lock(),
                        );
                    }
                    shared.payload.store(0, Ordering::Release);
                    rounds.fetch_add(1, Ordering::Release);
                    shared.cond.signal();
                    shared.mutex.unlock();
                }
            })
        };

        let _waker = waking_thread(true);
        let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);
        for round in 0..ROUNDS {
            shared.mutex.lock();
            while shared.payload.load(Ordering::Acquire) != 0 {
                wait(
                    &shared.cond,
                    mutex,
                    || shared.mutex.unlock(),
                    |_| shared.mutex.lock(),
                );
            }
            shared.payload.store(1, Ordering::Release);
            shared.cond.signal();
            shared.mutex.unlock();
            await_progress(&rounds, round + 1, "the ping-pong");
        }

        consumer.join().expect("the consumer should finish");
        assert_eq!(rounds.load(Ordering::Acquire), ROUNDS);
        assert_eq!(
            shared.mutex.staging.staged_count(),
            0,
            "a hand-off of wake-ones never reaches the staging"
        );
    }

    /// Every signal here is issued under the mutex, which is where staging
    /// costs the most: the waiter would be parked behind the signaller's own
    /// unlock. A wake-one takes the plain wake instead, so the stage stays
    /// empty even while the signaller still holds the lock.
    #[test]
    fn a_signal_wakes_without_staging_with_the_switch_on() {
        let shared = Shared::new();
        let released = Arc::new(AtomicU32::new(0));

        let waiter = {
            let shared = Arc::clone(&shared);
            let released = Arc::clone(&released);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex_with(SMOKE_LOCK_ID, false, true);
                let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);

                shared.mutex.lock();
                while shared.payload.load(Ordering::Acquire) == 0 {
                    wait(
                        &shared.cond,
                        mutex,
                        || shared.mutex.unlock(),
                        |_| shared.mutex.lock(),
                    );
                }
                shared.mutex.unlock();
                released.fetch_add(1, Ordering::Release);
            })
        };

        let _waker = waking_thread(true);
        std::thread::sleep(std::time::Duration::from_millis(100));
        shared.mutex.lock();
        shared.payload.store(1, Ordering::Release);
        shared.cond.signal();
        // Read under the lock, where no drain of this stage can have run yet:
        // anything the signal parked would still be counted here.
        let staged = shared.mutex.staging.staged_count();
        shared.mutex.unlock();

        await_progress(&released, 1, "the signalled waiter");
        waiter.join().expect("the waiter should finish");
        assert_eq!(staged, 0, "a signal parks nothing on the stage");
        assert_eq!(shared.mutex.staging.staged_count(), 0);
    }

    /// A broadcast parks its waiters instead of making them all runnable, and
    /// the unlock chain then releases them one at a time. Scheduling cannot be
    /// asserted, but the staging can: it fills up at the broadcast and is back
    /// to zero once every waiter has been released.
    #[test]
    fn a_requeued_broadcast_releases_every_waiter_through_the_staging() {
        const WAITERS: u32 = 8;

        let shared = Shared::new();
        let released = Arc::new(AtomicU32::new(0));

        let waiters = (0..WAITERS)
            .map(|_| {
                let shared = Arc::clone(&shared);
                let released = Arc::clone(&released);
                std::thread::spawn(move || {
                    let _guard = released_cond_mutex_with(SMOKE_LOCK_ID, false, true);
                    let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);

                    shared.mutex.lock();
                    while shared.payload.load(Ordering::Acquire) == 0 {
                        wait(
                            &shared.cond,
                            mutex,
                            || shared.mutex.unlock(),
                            |_| shared.mutex.lock(),
                        );
                    }
                    released.fetch_add(1, Ordering::Release);
                    shared.mutex.unlock();
                })
            })
            .collect::<Vec<_>>();

        let _waker = waking_thread(true);
        std::thread::sleep(std::time::Duration::from_millis(100));
        shared.mutex.lock();
        shared.payload.store(1, Ordering::Release);
        shared.cond.broadcast();
        // The broadcast came from the thread holding the lock, so it left the
        // release to the unlock below and nothing has drained yet.
        let staged = shared.mutex.staging.staged_count();
        shared.mutex.unlock();

        await_progress(&released, WAITERS, "the broadcast");
        for waiter in waiters {
            waiter.join().expect("every waiter should finish");
        }

        assert!(staged > 0, "the broadcast should have parked its waiters");
        assert_eq!(
            shared.mutex.staging.staged_count(),
            0,
            "every staged waiter should have been released"
        );
    }

    /// The drain chain starts at an unlock, and a broadcaster that holds
    /// nothing has none coming. Without the probe that answers this the waiter
    /// would stay parked on the stage for good.
    #[test]
    fn a_broadcaster_holding_nothing_releases_the_waiter_it_staged() {
        let shared = Shared::new();
        let released = Arc::new(AtomicU32::new(0));

        let waiter = {
            let shared = Arc::clone(&shared);
            let released = Arc::clone(&released);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex_with(SMOKE_LOCK_ID, false, true);
                let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);

                shared.mutex.lock();
                while shared.payload.load(Ordering::Acquire) == 0 {
                    wait(
                        &shared.cond,
                        mutex,
                        || shared.mutex.unlock(),
                        |_| shared.mutex.lock(),
                    );
                }
                shared.mutex.unlock();
                released.fetch_add(1, Ordering::Release);
            })
        };

        let _waker = waking_thread(true);
        std::thread::sleep(std::time::Duration::from_millis(100));
        shared.mutex.lock();
        shared.payload.store(1, Ordering::Release);
        shared.mutex.unlock();
        // From here nothing else will unlock this mutex until the waiter runs.
        shared.cond.broadcast();

        await_progress(&released, 1, "the stranded waiter");
        waiter.join().expect("the waiter should finish");
        assert_eq!(shared.mutex.staging.staged_count(), 0);
    }

    /// A waiter the stage released whose sequence has not moved goes back to
    /// sleep, and the release it spent has to reach the waiter behind it.
    ///
    /// The interleaving is forced rather than raced for: the stage is given a
    /// release owed to somebody else, and the sleeping waiter is woken on the
    /// cond word with no signal behind it, which is exactly what a stale
    /// release looks like from inside the wait.
    #[test]
    fn a_stale_wake_hands_its_release_to_the_waiter_behind_it() {
        let shared = Shared::new();
        let parked = Arc::new(AtomicU32::new(0));

        let waiter = {
            let shared = Arc::clone(&shared);
            let parked = Arc::clone(&parked);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex_with(SMOKE_LOCK_ID, false, true);
                let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);

                shared.mutex.lock();
                while shared.payload.load(Ordering::Acquire) == 0 {
                    wait(
                        &shared.cond,
                        mutex,
                        || {
                            shared.mutex.unlock();
                            parked.fetch_add(1, Ordering::Release);
                        },
                        |_| shared.mutex.lock(),
                    );
                }
                shared.mutex.unlock();
            })
        };

        // Past the unlock, so the drain it ran cannot spend the release below.
        await_progress(&parked, 1, "the waiter reaching its sleep");
        shared.mutex.block().stage_waiters(1);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while shared.mutex.staging.staged_count() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the stale wake should have handed its release on"
            );
            unsafe { futex_wake(&shared.cond.seq, 1) };
            std::thread::yield_now();
        }

        shared.mutex.lock();
        shared.payload.store(1, Ordering::Release);
        shared.cond.signal();
        shared.mutex.unlock();

        waiter.join().expect("the waiter should finish");
    }

    /// A broadcast that moves nobody still kicks the drain chain: waiters an
    /// earlier broadcast parked are owed a release either way, and a broadcast
    /// is the moment a chain that lost one picks up again.
    #[test]
    fn a_broadcast_that_moves_nobody_still_kicks_the_drain_chain() {
        let mutex = SpinMutex::new();
        let cond = CondState::new();
        let _guard = waking_thread(true);

        cond.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);
        mutex.block().stage_waiters(1);

        // A registration with no sleeper behind it: the requeue finds nothing
        // to move, so the staged waiter is all this broadcast can release.
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();

        assert_eq!(
            mutex.staging.staged_count(),
            0,
            "a broadcast that staged nothing still releases what is already staged"
        );
    }

    /// A broadcaster holding the lock the waiters will re-acquire has an unlock
    /// of its own coming and leaves the release to it, which is the pacing the
    /// staging exists for.
    #[test]
    fn a_broadcast_from_inside_the_lock_leaves_the_release_to_the_unlock() {
        let mutex = SpinMutex::new();
        let cond = CondState::new();
        let _guard = waking_thread(true);

        cond.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);
        mutex.block().stage_waiters(1);

        mutex.lock();
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();
        assert_eq!(
            mutex.staging.staged_count(),
            1,
            "the release belongs to the unlock this broadcaster still owes"
        );

        mutex.unlock();
        assert_eq!(mutex.staging.staged_count(), 0);
    }

    /// Locks created past the class limit share one class, so a broadcaster can
    /// hold a lock whose class is the one the cond's mutex has without holding
    /// that mutex. The lock is recognised by its staging block rather than by
    /// its class, so the drain still runs: nothing else would ever unlock the
    /// cond's mutex and release the waiter parked on it.
    #[test]
    fn a_broadcaster_holding_a_class_mate_of_the_bound_lock_still_drains() {
        let bound = SpinMutex::new();
        let class_mate = SpinMutex::new();
        let cond = CondState::new();
        let _guard = waking_thread(true);

        cond.bind_staging(bound.cond_mutex(SMOKE_LOCK_ID).staging);
        bound.block().stage_waiters(1);

        // Both locks carry SMOKE_LOCK_ID, which is what folding does to them.
        class_mate.lock();
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();
        assert_eq!(
            bound.staging.staged_count(),
            0,
            "no unlock of the bound lock is coming, so the broadcast owes the release"
        );
        class_mate.unlock();
    }

    /// Destroying the mutex before the cond is allowed, and so is waking a cond
    /// nobody waits on. The binding keeps the staging block alive, so the
    /// wakeup reads a retired block instead of the freed lock.
    #[test]
    fn a_wakeup_after_the_mutex_is_destroyed_reaches_a_retired_block() {
        let cond = CondState::new();
        let block = {
            let mutex = SpinMutex::new();
            cond.bind_staging(mutex.cond_mutex(SMOKE_LOCK_ID).staging);
            let block = mutex.staging.block_arc();
            assert!(block.alive());
            block
        };

        assert!(!block.alive(), "destroying the mutex retires its staging");

        let _guard = waking_thread(true);
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.signal();
        cond.waiters.fetch_add(1, Ordering::AcqRel);
        cond.broadcast();

        assert_eq!(
            block.staged_count(),
            0,
            "a retired block never has a waiter parked on it"
        );
    }

    /// A mutex destroyed while waiters are parked on its stage has no unlock
    /// left to release them, so the destroy releases them itself.
    #[test]
    fn destroying_a_mutex_releases_what_is_staged_on_it() {
        let mutex = SpinMutex::new();
        let block = mutex.staging.block_arc();
        block.stage_waiters(2);

        drop(mutex);

        assert_eq!(block.staged_count(), 0);
    }

    /// A deadline that expires on the stage takes no release with it. The wait
    /// ends as the wakeup its broadcast made it, and the count it left behind
    /// is spent on the next drain.
    #[test]
    fn a_deadline_that_expires_on_the_stage_leaves_the_count_high() {
        let shared = Shared::new();
        let outcome = Arc::new(AtomicU32::new(u32::MAX));

        let waiter = {
            let shared = Arc::clone(&shared);
            let outcome = Arc::clone(&outcome);
            std::thread::spawn(move || {
                let _guard = released_cond_mutex_with(SMOKE_LOCK_ID, false, true);
                let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);
                let abstime = deadline_in_millis(100);

                shared.mutex.lock();
                let ret = timedwait(
                    &shared.cond,
                    mutex,
                    &abstime,
                    || shared.mutex.unlock(),
                    |_| shared.mutex.lock(),
                );
                shared.mutex.unlock();
                outcome.store(ret as u32, Ordering::Release);
            })
        };

        let _waker = waking_thread(true);
        std::thread::sleep(std::time::Duration::from_millis(30));
        shared.mutex.lock();
        shared.cond.broadcast();
        // The mutex stays held past the deadline, and the broadcast came from
        // the thread holding it, so the waiter can only leave the stage by
        // expiring on it.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            shared.mutex.staging.staged_count(),
            1,
            "the expired waiter should have left its count behind"
        );
        shared.mutex.unlock();

        waiter.join().expect("the waiter should finish");
        assert_eq!(
            outcome.load(Ordering::Acquire),
            0,
            "a woken wait reports the wakeup it was given"
        );
        assert_eq!(
            shared.mutex.staging.staged_count(),
            0,
            "the next drain spends the count on an empty wake"
        );
    }

    #[test]
    fn a_broadcast_releases_every_waiter() {
        let shared = Shared::new();

        let waiters = (0..4)
            .map(|_| {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || {
                    let _guard = released_cond_mutex(SMOKE_LOCK_ID, true);
                    let mutex = shared.mutex.cond_mutex(SMOKE_LOCK_ID);

                    shared.mutex.lock();
                    while shared.payload.load(Ordering::Acquire) == 0 {
                        wait(
                            &shared.cond,
                            mutex,
                            || shared.mutex.unlock(),
                            |_| shared.mutex.lock(),
                        );
                    }
                    shared.mutex.unlock();
                })
            })
            .collect::<Vec<_>>();

        std::thread::sleep(std::time::Duration::from_millis(50));
        shared.mutex.lock();
        shared.payload.store(1, Ordering::Release);
        shared.mutex.unlock();
        shared.cond.broadcast();

        for waiter in waiters {
            waiter.join().expect("every waiter should finish");
        }
    }
}
