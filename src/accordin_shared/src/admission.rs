use std::cell::Cell;
use std::sync::Mutex;
use std::sync::Once;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const IN_CRITICAL_SECTION: u32 = 1 << 0;
const SLOW_PATH_PENDING: u32 = 1 << 1;
const TOKEN_CONSUMED: u32 = 1 << 2;
/// Set by a thread immediately before it releases a managed lock to sleep on a
/// condition variable: the lock-class field then names the lock the thread will
/// re-acquire on wake. `SLOW_PATH_PENDING` and `IN_CRITICAL_SECTION` are never
/// set while this bit is.
///
/// Right after its futex wait returns the waiter retires the bit into
/// `SLOW_PATH_PENDING` for the same class rather than clearing it, so the
/// re-acquisition that follows describes itself as the contender it is; see
/// `retire_cv_sleep_to_pending`.
const CV_SLEEP: u32 = 1 << 3;
/// Everything an admission-disabled word update clears: the flags that describe
/// one in-flight acquisition rather than the lock's identity.
const TRANSIENT_FLAGS: u32 = SLOW_PATH_PENDING | IN_CRITICAL_SECTION | TOKEN_CONSUMED | CV_SLEEP;
pub const USER_ADMISSION_FLAG_MASK: u32 = 0xF;
pub const USER_ADMISSION_LOCK_ID_SHIFT: u32 = 4;
/// Must equal `MAX_LOCK_CLASSES` in the BPF `intf.h`: the scheduler loader
/// assigns a `[u32; CLASS_COUNT]` into the skeleton's per-class BSS array, so a
/// mismatch is a build error rather than a silent truncation.
pub const MAX_LOCK_CLASSES: u32 = 64;
pub const UNMANAGED_LOCK_ID: u32 = 0;
pub const DISABLE_ADMISSION_ENV: &str = "ACCORDIN_DISABLE_ADMISSION";
/// Turns the cross-class token preservation experiment on. Not prefixed by a
/// backend name: every hook library shares this admission word, and a run
/// compares the two arms by setting the one switch.
pub const PRESERVE_CROSS_CLASS_TOKEN_ENV: &str = "ACCORDIN_PRESERVE_CROSS_CLASS_TOKEN";
/// Holds a managed slow path back until the scheduler admits it, so that a
/// thread only ever publishes a queue node while it is on a CPU the scheduler
/// granted it. Default on; `0` turns it off for the comparison arm.
const ADMISSION_GATE_ENV: &str = "ACCORDIN_ADMISSION_GATE";
/// Longest a gated thread waits for a grant before it publishes unadmitted.
const ADMISSION_GATE_TIMEOUT_MS_ENV: &str = "ACCORDIN_ADMISSION_GATE_TIMEOUT_MS";
const ADMISSION_GATE_TIMEOUT_DEFAULT_MS: u32 = 1000;
/// Gate iterations between two clock reads. The wait is a yield loop, so the
/// timeout only has to be observed within a scheduling round rather than
/// exactly, and reading the clock once per iteration would cost more than the
/// iteration it guards.
const ADMISSION_GATE_TIMEOUT_CHECK_INTERVAL: u32 = 64;
const WIDTH_MERGE_ENV: &str = "ACCORDIN_WIDTH_MERGE";
const DUMP_DEPENDENCY_ENV: &str = "ACCORDIN_DUMP_DEPENDENCY";
const WIDTH_OVERFLOW_POLICY_ENV: &str = "ACCORDIN_WIDTH_OVERFLOW_POLICY";

/// Number of lock classes, i.e. the length of every per-class array on both
/// sides of the BPF boundary. Index 0 is the unmanaged sentinel.
pub const CLASS_COUNT: usize = MAX_LOCK_CLASSES as usize;

static NEXT_LOCK_ID: AtomicU32 = AtomicU32::new(1);
static NEXT_OVERFLOW_INDEX: AtomicU32 = AtomicU32::new(0);
static INACTIVE_ENQUEUE_SEQ_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
static INACTIVE_EMPTY_SEQ_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
/// The scheduler's per-CPU admission owner slots: slot `cpu` names the tid the
/// scheduler granted that CPU to, and a granted task always runs on the CPU its
/// grant names. Reading `slots[sched_getcpu()]` is therefore how a thread asks
/// whether it is admitted right now.
static CPU_OWNER_SLOTS_PTR: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
static CPU_OWNER_SLOTS_LEN: AtomicUsize = AtomicUsize::new(0);

static LOCK_CLASS_PARENT: [AtomicU8; CLASS_COUNT] = identity_lock_classes();
static LOCK_CLASS_MERGE_LOCK: Mutex<()> = Mutex::new(());
static CLASSES_DIRTY: AtomicBool = AtomicBool::new(false);
static TRANSITION_COUNTS: [[AtomicU32; CLASS_COUNT]; CLASS_COUNT] =
    [const { [const { AtomicU32::new(0) }; CLASS_COUNT] }; CLASS_COUNT];

#[cfg(test)]
thread_local! {
    static PRESERVE_CROSS_CLASS_TOKEN_FORCED: Cell<Option<bool>> = const { Cell::new(None) };
    static ADMISSION_GATE_FORCED: Cell<Option<bool>> = const { Cell::new(None) };
    static ADMISSION_GATE_TIMEOUT_FORCED: Cell<Option<Duration>> = const { Cell::new(None) };
}

thread_local! {
    static USER_ADMISSION_WORD: AtomicU32 = const { AtomicU32::new(0) };
    static THREAD_HELD_DEPTH: Cell<u32> = const { Cell::new(0) };
    static THREAD_OUTER_LOCK_ID: Cell<u32> = const { Cell::new(UNMANAGED_LOCK_ID) };
    static THREAD_PREV_OUTERMOST_CLASS: Cell<u32> = const { Cell::new(UNMANAGED_LOCK_ID) };
    /// A class this thread drew for a lock that never came into existence,
    /// kept for the next lock this thread creates.
    ///
    /// One slot is enough: a thread only ends up with an unused class by losing
    /// a race to install a lock's state, and it holds no other lock's state
    /// half-built while it does that.
    static PARKED_LOCK_CLASS: Cell<u32> = const { Cell::new(UNMANAGED_LOCK_ID) };
    /// This thread's kernel tid, which the gate compares an owner slot against.
    /// A tid never changes for the life of a thread and is never 0, so 0 stands
    /// for "not looked up yet".
    static CACHED_TID: Cell<u32> = const { Cell::new(0) };
    /// Whether this thread's admission word reached the scheduler's thread map.
    /// The scheduler cannot grant to a thread it cannot read, so a thread whose
    /// registration failed must never be held at the gate.
    static SCHEDULER_REGISTERED: Cell<bool> = const { Cell::new(false) };
    /// Set by a gate wait that expired, and cleared by the next grant this
    /// thread is observed to hold.
    ///
    /// An ejected scheduler is invisible from userspace: the owner slots stay
    /// mapped and readable, and nothing withdraws the pointer, so a wait that
    /// keeps expiring is the only signal that grants have stopped. Left ungated
    /// for that, every contended acquisition would pay the whole timeout. While
    /// this is set the gate probes once and lets the caller through, and only a
    /// grant actually observed puts the thread back under the gate.
    static GATE_DISARMED: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    static MERGE_FORCED: Cell<bool> = const { Cell::new(false) };
    static DUMP_FORCED: Cell<bool> = const { Cell::new(false) };
    static LOCK_CLASS_POLICY_FORCED: Cell<Option<LockClassPolicy>> = const { Cell::new(None) };
}

/// What a lock created past the class limit receives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockClassPolicy {
    /// An id shared with an earlier lock, exactly as a merged lock has.
    Fold,
    /// No id, so the lock stays outside admission control.
    Unmanaged,
}

#[derive(Clone, Copy)]
pub struct LockAdmissionScope {
    lock_id: u32,
    outer_managed: bool,
}

pub fn user_word_addr() -> *const u32 {
    USER_ADMISSION_WORD.with(|word| word as *const AtomicU32 as *const u32)
}

#[inline(always)]
fn admission_enabled() -> bool {
    static ADMISSION_ENABLED: OnceLock<bool> = OnceLock::new();

    *ADMISSION_ENABLED.get_or_init(|| !crate::env::env_flag(DISABLE_ADMISSION_ENV))
}

fn preserve_cross_class_token_env_enabled() -> bool {
    static PRESERVE_CROSS_CLASS_TOKEN: OnceLock<bool> = OnceLock::new();

    *PRESERVE_CROSS_CLASS_TOKEN.get_or_init(|| crate::env::env_flag(PRESERVE_CROSS_CLASS_TOKEN_ENV))
}

/// Whether a slow-path mark carries a consumed token across a change of lock
/// class. The arm is chosen per thread in tests so that a test picking one
/// never changes what concurrently running tests see.
#[cfg(test)]
#[inline(always)]
fn preserve_cross_class_token_enabled() -> bool {
    PRESERVE_CROSS_CLASS_TOKEN_FORCED
        .with(std::cell::Cell::get)
        .unwrap_or_else(preserve_cross_class_token_env_enabled)
}

#[cfg(not(test))]
#[inline(always)]
fn preserve_cross_class_token_enabled() -> bool {
    preserve_cross_class_token_env_enabled()
}

#[cfg(test)]
fn force_preserve_cross_class_token_for_test(forced: Option<bool>) {
    PRESERVE_CROSS_CLASS_TOKEN_FORCED.with(|preserve| preserve.set(forced));
}

fn gate_env_enabled() -> bool {
    static GATE_ENABLED: OnceLock<bool> = OnceLock::new();

    *GATE_ENABLED.get_or_init(|| crate::env::env_flag_default_on(ADMISSION_GATE_ENV))
}

/// Whether a managed slow path waits for its grant before it publishes. The arm
/// is chosen per thread in tests so that a test picking one never changes what
/// concurrently running tests see.
#[cfg(test)]
#[inline(always)]
fn gate_enabled() -> bool {
    ADMISSION_GATE_FORCED
        .with(std::cell::Cell::get)
        .unwrap_or_else(gate_env_enabled)
}

#[cfg(not(test))]
#[inline(always)]
fn gate_enabled() -> bool {
    gate_env_enabled()
}

#[cfg(test)]
fn force_gate_for_test(forced: Option<bool>) {
    ADMISSION_GATE_FORCED.with(|gate| gate.set(forced));
}

fn gate_timeout_env() -> Duration {
    static GATE_TIMEOUT: OnceLock<Duration> = OnceLock::new();

    *GATE_TIMEOUT.get_or_init(|| {
        Duration::from_millis(u64::from(crate::env::env_u32_clamped(
            ADMISSION_GATE_TIMEOUT_MS_ENV,
            ADMISSION_GATE_TIMEOUT_DEFAULT_MS,
            0,
            u32::MAX,
        )))
    })
}

#[cfg(test)]
#[inline(always)]
fn gate_timeout() -> Duration {
    ADMISSION_GATE_TIMEOUT_FORCED
        .with(std::cell::Cell::get)
        .unwrap_or_else(gate_timeout_env)
}

#[cfg(not(test))]
#[inline(always)]
fn gate_timeout() -> Duration {
    gate_timeout_env()
}

#[cfg(test)]
fn force_gate_timeout_for_test(forced: Option<Duration>) {
    ADMISSION_GATE_TIMEOUT_FORCED.with(|timeout| timeout.set(forced));
}

#[cfg(test)]
fn rearm_gate_for_test() {
    GATE_DISARMED.with(|disarmed| disarmed.set(false));
}

#[inline(always)]
fn managed_lock_id(lock_id: u32) -> bool {
    lock_id != UNMANAGED_LOCK_ID && lock_id < MAX_LOCK_CLASSES
}

const fn identity_lock_classes() -> [AtomicU8; CLASS_COUNT] {
    let mut parents = [const { AtomicU8::new(0) }; CLASS_COUNT];
    let mut id = 0;
    while id < CLASS_COUNT {
        parents[id] = AtomicU8::new(id as u8);
        id += 1;
    }
    parents
}

#[cfg(test)]
#[inline(always)]
fn merge_forced() -> bool {
    MERGE_FORCED.with(|forced| forced.get())
}

#[cfg(not(test))]
#[inline(always)]
fn merge_forced() -> bool {
    false
}

#[cfg(test)]
#[inline(always)]
fn dump_forced() -> bool {
    DUMP_FORCED.with(|forced| forced.get())
}

#[cfg(not(test))]
#[inline(always)]
fn dump_forced() -> bool {
    false
}

#[cfg(test)]
#[inline(always)]
fn forced_lock_class_policy() -> Option<LockClassPolicy> {
    LOCK_CLASS_POLICY_FORCED.with(|policy| policy.get())
}

#[cfg(not(test))]
#[inline(always)]
fn forced_lock_class_policy() -> Option<LockClassPolicy> {
    None
}

/// Anything other than `unmanaged` keeps the `fold` default, so a typo in the
/// environment does not silently drop locks out of admission control.
fn parse_lock_class_policy(value: Option<&str>) -> LockClassPolicy {
    match value.map(str::trim) {
        Some(value) if value.eq_ignore_ascii_case("unmanaged") => LockClassPolicy::Unmanaged,
        _ => LockClassPolicy::Fold,
    }
}

/// The overflow policy is a width-control policy: with the feature off there is
/// no width to share, so allocation stops at the class limit as it does without
/// any of this machinery.
fn lock_class_policy() -> LockClassPolicy {
    if let Some(forced) = forced_lock_class_policy() {
        return forced;
    }

    static POLICY: OnceLock<LockClassPolicy> = OnceLock::new();

    *POLICY.get_or_init(|| {
        if !crate::width_control::enabled() {
            return LockClassPolicy::Unmanaged;
        }

        parse_lock_class_policy(std::env::var(WIDTH_OVERFLOW_POLICY_ENV).ok().as_deref())
    })
}

/// Dependency merging and a statically configured width are mutually exclusive,
/// and the static configuration wins even against an explicit merge request.
/// A merge renames a lock's class, so an entry naming that class in the
/// operator's map stops describing anything the lock resolves to; and the fold
/// that reconciles a merged-away class with its canonical one belongs to the
/// controller tick, which a static width configuration replaces. Leaving the
/// two on together would let a merge silently retire a configured width with no
/// signal that it stopped applying.
fn merge_enabled_with(width_control: bool, fixed_widths: bool, requested: bool) -> bool {
    width_control && !fixed_widths && requested
}

#[inline(always)]
fn merge_enabled() -> bool {
    if merge_forced() {
        return true;
    }

    static MERGE_ENABLED: OnceLock<bool> = OnceLock::new();

    *MERGE_ENABLED.get_or_init(|| {
        merge_enabled_with(
            crate::width_control::enabled(),
            crate::width_control::fixed_widths_configured(),
            crate::env::env_flag_default_on(WIDTH_MERGE_ENV),
        )
    })
}

#[inline(always)]
fn dump_dependency_enabled() -> bool {
    if dump_forced() {
        return true;
    }

    static DUMP_DEPENDENCY: OnceLock<bool> = OnceLock::new();

    *DUMP_DEPENDENCY.get_or_init(|| crate::env::env_flag(DUMP_DEPENDENCY_ENV))
}

/// Walks parent pointers to the component root. A merge only ever points a root
/// at a strictly smaller id and never re-parents an entry afterwards, so the
/// walk is strictly decreasing and terminates even if a merge lands mid-walk.
#[inline(always)]
fn find_root(lock_id: u32) -> u32 {
    let mut root = lock_id;
    loop {
        let parent = LOCK_CLASS_PARENT[root as usize].load(Ordering::Relaxed) as u32;
        if parent == root {
            return root;
        }
        root = parent;
    }
}

fn union_lock_classes(a: u32, b: u32) {
    if !managed_lock_id(a) || !managed_lock_id(b) {
        return;
    }

    let _guard = LOCK_CLASS_MERGE_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    let root_a = find_root(a);
    let root_b = find_root(b);
    if root_a == root_b {
        return;
    }

    let (min_root, max_root) = if root_a < root_b {
        (root_a, root_b)
    } else {
        (root_b, root_a)
    };
    LOCK_CLASS_PARENT[max_root as usize].store(min_root as u8, Ordering::Relaxed);
    CLASSES_DIRTY.store(true, Ordering::Relaxed);
}

/// Scheduling class of a lock: the smallest lock id of its merged component.
///
/// This is the id published in the admission word, so BPF reads a merged
/// component as one identity: one shared inactive DSQ and one shared width,
/// with no mapping table of its own. A thread that still carries the id from
/// before a merge is harmless — that id is a valid class whose DSQ drains
/// normally, and the admission counters balance because BPF decrements the
/// class it recorded at grant time.
#[inline(always)]
pub fn class_of(lock_id: u32) -> u32 {
    if !managed_lock_id(lock_id) || !merge_enabled() {
        return lock_id;
    }

    find_root(lock_id)
}

/// Reports (and clears) whether classes were merged since the last call.
pub fn take_classes_dirty() -> bool {
    CLASSES_DIRTY.swap(false, Ordering::Relaxed)
}

#[inline(always)]
fn record_nesting_edge(inner: u32) {
    if !merge_enabled() {
        return;
    }

    let outer = THREAD_OUTER_LOCK_ID.with(|outer| outer.get());
    if outer == inner || !managed_lock_id(outer) {
        return;
    }

    // Lock-free reads first: only a genuinely new edge takes the merge mutex.
    if find_root(outer) == find_root(inner) {
        return;
    }

    union_lock_classes(outer, inner);
}

/// Counts sequential handoffs between outermost classes. Diagnostics only: a
/// transition is not a nesting edge and never merges classes.
#[inline(always)]
fn record_outermost_transition(lock_id: u32) {
    if !dump_dependency_enabled() {
        return;
    }

    let next = class_of(lock_id);
    if !managed_lock_id(next) {
        return;
    }

    let prev = THREAD_PREV_OUTERMOST_CLASS.with(|prev| prev.replace(next));
    if managed_lock_id(prev) {
        TRANSITION_COUNTS[prev as usize][next as usize].fetch_add(1, Ordering::Relaxed);
    }
}

pub fn dump_dependency_diagnostics() {
    if !dump_dependency_enabled() {
        return;
    }

    for lock_id in 1..MAX_LOCK_CLASSES {
        let class = find_root(lock_id);
        if class != lock_id {
            eprintln!("dependency_merge: lock={lock_id} class={class}");
        }
    }

    for (prev, row) in TRANSITION_COUNTS.iter().enumerate().skip(1) {
        for (next, cell) in row.iter().enumerate().skip(1) {
            let count = cell.load(Ordering::Relaxed);
            if count != 0 {
                eprintln!("dependency_transition: from={prev} to={next} count={count}");
            }
        }
    }
}

#[inline(always)]
fn flags(value: u32) -> u32 {
    value & USER_ADMISSION_FLAG_MASK
}

#[inline(always)]
fn lock_id_bits(value: u32) -> u32 {
    value & !USER_ADMISSION_FLAG_MASK
}

#[inline(always)]
fn user_lock_id_for_value(value: u32) -> u32 {
    lock_id_bits(value) >> USER_ADMISSION_LOCK_ID_SHIFT
}

#[inline(always)]
fn word_with_lock_id(lock_id: u32, flags: u32) -> u32 {
    (lock_id << USER_ADMISSION_LOCK_ID_SHIFT) | (flags & USER_ADMISSION_FLAG_MASK)
}

/// Word published while admission is disabled: the lock class stays visible and
/// every admission flag is dropped.
#[inline(always)]
fn cleared_flags_word(class: u32, value: u32) -> u32 {
    word_with_lock_id(class, flags(value) & !TRANSIENT_FLAGS)
}

#[inline(always)]
fn exit_word(class: u32, value: u32, enabled: bool) -> u32 {
    if enabled {
        word_with_lock_id(
            class,
            (flags(value) | TOKEN_CONSUMED) & !IN_CRITICAL_SECTION,
        )
    } else {
        cleared_flags_word(class, value)
    }
}

/// Hands out one id per lock while ids last, then reports every further lock as
/// unmanaged.
fn allocate_lock_id() -> u32 {
    loop {
        let current = NEXT_LOCK_ID.load(Ordering::Relaxed);
        if current >= MAX_LOCK_CLASSES {
            return UNMANAGED_LOCK_ID;
        }

        if NEXT_LOCK_ID
            .compare_exchange_weak(current, current + 1, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return current;
        }
    }
}

/// Scheduling class a lock is created with. The first `MAX_LOCK_CLASSES - 1`
/// locks each get an id to themselves; from there the overflow policy decides
/// whether the lock shares a class or drops out of admission control.
///
/// The id is drawn once, at creation, and the lock carries it unchanged for its
/// whole life: nothing here maps a lock back to an id, so no later allocation
/// can renumber a lock that already exists.
pub fn allocate_lock_class() -> u32 {
    // A class this thread already drew and did not use is spent before the
    // pool is touched again. It was counted and published when it was drawn,
    // so taking it here neither draws nor publishes anything.
    if let Some(parked) = take_parked_lock_class() {
        return parked;
    }

    let lock_id = allocate_lock_id();
    // The probe span follows the ids handed out. Publishing it here rather
    // than from the controller tick covers the fixed-width mode, where the
    // tick never runs.
    crate::width_control::publish_inactive_probe_span();
    if lock_id != UNMANAGED_LOCK_ID {
        return lock_id;
    }

    match lock_class_policy() {
        LockClassPolicy::Fold => fold_overflow_lock_id(),
        LockClassPolicy::Unmanaged => UNMANAGED_LOCK_ID,
    }
}

/// Keeps a class whose lock never came into existence for the next lock this
/// thread creates.
///
/// Locks are created by racing to publish their state, and every racer but one
/// frees the state it built. Without this the losers would each spend a class
/// out of a pool of `MAX_LOCK_CLASSES`, which a workload whose threads meet at
/// a barrier burns by construction: the first touch of a shared lock is as
/// many-way as the barrier is wide. The classes stay dense instead, and
/// `allocated_class_count()` keeps counting the locks that exist rather than
/// the attempts that were made to create them.
///
/// Only a class the pool still owns is worth keeping. Past the limit every id
/// is shared already, and handing a shared one on would bias the round-robin
/// the overflow policy deals them by.
pub fn release_unused_lock_class(lock_id: u32) {
    if !managed_lock_id(lock_id) || NEXT_LOCK_ID.load(Ordering::Relaxed) >= MAX_LOCK_CLASSES {
        return;
    }

    PARKED_LOCK_CLASS.with(|parked| {
        if parked.get() == UNMANAGED_LOCK_ID {
            parked.set(lock_id);
        }
    });
}

/// Spends the parked class, if this thread has one.
fn take_parked_lock_class() -> Option<u32> {
    PARKED_LOCK_CLASS.with(|parked| {
        let lock_id = parked.get();
        if lock_id == UNMANAGED_LOCK_ID {
            return None;
        }

        parked.set(UNMANAGED_LOCK_ID);
        Some(lock_id)
    })
}

/// Managed lock ids handed out so far. Ids are dense from 1 upward and an
/// overflowing lock folds onto an id already in use, so this is also the
/// highest managed id in play.
pub fn allocated_class_count() -> u32 {
    NEXT_LOCK_ID.load(Ordering::Relaxed).saturating_sub(1)
}

/// Deals overflowing locks round-robin over the managed ids. Folding reuses the
/// merge mechanism rather than adding one: the lock shares its id, and with it
/// one inactive DSQ and one width, with an earlier lock, so `class_of` and
/// everything downstream of it treat the two as the single class they are.
fn fold_overflow_lock_id() -> u32 {
    let overflow_index = NEXT_OVERFLOW_INDEX.fetch_add(1, Ordering::Relaxed);
    1 + overflow_index % (MAX_LOCK_CLASSES - 1)
}

#[doc(hidden)]
pub fn set_inactive_queue_seq_ptrs(enqueue_seq: *mut u32, empty_seq: *mut u32) {
    INACTIVE_ENQUEUE_SEQ_PTR.store(enqueue_seq, Ordering::Release);
    INACTIVE_EMPTY_SEQ_PTR.store(empty_seq, Ordering::Release);
}

/// Installs the scheduler's per-CPU owner slots. The length is published before
/// the pointer and withdrawn after it, so a reader that finds a pointer always
/// finds the length that belongs to it.
#[doc(hidden)]
pub fn set_cpu_owner_slots(ptr: *mut u32, len: usize) {
    if ptr.is_null() {
        CPU_OWNER_SLOTS_PTR.store(ptr, Ordering::Release);
        CPU_OWNER_SLOTS_LEN.store(len, Ordering::Release);
        return;
    }

    CPU_OWNER_SLOTS_LEN.store(len, Ordering::Release);
    CPU_OWNER_SLOTS_PTR.store(ptr, Ordering::Release);
}

/// Drops everything this thread believes about its own identity in the
/// scheduler. A fork keeps the owner slots mapped and the caches filled while
/// giving the child a tid of its own, so a child that kept them would compare
/// every slot against its parent's tid and never match.
extern "C" fn forget_scheduler_identity_after_fork() {
    CACHED_TID.with(|cached| cached.set(0));
    SCHEDULER_REGISTERED.with(|registered| registered.set(false));
    GATE_DISARMED.with(|disarmed| disarmed.set(false));
}

fn arm_fork_handler() {
    static FORK_HANDLER: Once = Once::new();

    FORK_HANDLER.call_once(|| {
        unsafe {
            libc::pthread_atfork(None, None, Some(forget_scheduler_identity_after_fork));
        };
    });
}

#[inline(always)]
fn cached_tid() -> u32 {
    CACHED_TID.with(|cached| {
        let tid = cached.get();
        if tid != 0 {
            return tid;
        }

        // Only the forking thread survives into the child, so a handler that
        // clears this cache is enough to cover every thread the child has.
        arm_fork_handler();
        let tid = crate::mutex_hook::current_tid();
        cached.set(tid);
        tid
    })
}

/// Records whether this thread's admission word reached the scheduler.
#[doc(hidden)]
pub fn set_scheduler_registered(registered: bool) {
    SCHEDULER_REGISTERED.with(|flag| flag.set(registered));
}

#[inline(always)]
fn scheduler_registered() -> bool {
    SCHEDULER_REGISTERED.with(std::cell::Cell::get)
}

/// Whether the scheduler currently holds a grant for this thread on the CPU it
/// is running on, or `None` when nothing could grant one here.
///
/// A thread the scheduler never took into its map is `None` for the same reason
/// an absent pointer is: it is outside what the scheduler decides about, so a
/// grant for it is never coming.
///
/// Both races this read can lose are safe. A migration between the CPU query and
/// the slot read reads another CPU's slot and costs one more yield; a read that
/// misses a concurrent release reports a grant that has just gone, which
/// degrades to the unadmitted publish that happened before the gate existed.
#[inline(always)]
fn admitted_on_current_cpu() -> Option<bool> {
    let slots = CPU_OWNER_SLOTS_PTR.load(Ordering::Acquire);
    if slots.is_null() || !scheduler_registered() {
        return None;
    }

    let len = CPU_OWNER_SLOTS_LEN.load(Ordering::Acquire);
    let cpu = unsafe { libc::sched_getcpu() };
    if cpu < 0 || cpu as usize >= len {
        return None;
    }

    let owner = unsafe { slots.add(cpu as usize).read_volatile() };
    Some(owner == cached_tid())
}

/// Whether the gate can hold a slow path: admission is in play, the gate is
/// switched on, and the scheduler has published the slots a grant would show up
/// in.
#[inline(always)]
fn gate_armed() -> bool {
    admission_enabled() && gate_enabled() && !CPU_OWNER_SLOTS_PTR.load(Ordering::Acquire).is_null()
}

/// How a gate wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GateOutcome {
    /// The gate is switched off, so the caller publishes as it did without it.
    Disabled,
    /// Nothing grants for this thread on this CPU, so a wait here could never
    /// end.
    Bypassed,
    Admitted,
    /// The grant never arrived. Publishing unadmitted is the liveness guard: a
    /// thread that waits forever holds up the lock it is queueing for as surely
    /// as an off-CPU queue head does.
    TimedOut,
    /// An earlier wait expired, so this thread only probed and went on. See
    /// `GATE_DISARMED`.
    Disarmed,
}

/// Holds a managed slow path until the scheduler admits this thread.
///
/// Taking the backend lock publishes this thread's node into the lock's queue,
/// and a thread the scheduler has not admitted may be sitting in an inactive
/// DSQ: its node would then hold the queue head off-CPU for as long as that DSQ
/// goes unserved. Waiting for the grant first keeps every published node on a
/// thread the scheduler is running.
///
/// Three things end a wait short of a grant, and all three are bounded by the
/// timeout and the disarm that follows it:
/// - a spinner that blocks involuntarily — a page fault, a signal — gives its
///   grant back at the stop and may be parked once when it wakes, which the
///   wake path and the running-hook grant retry recover from;
/// - a grant made for one CPU while the thread runs on another, which the
///   cpumask and offline races can leave behind;
/// - a scheduler that stopped granting altogether, which userspace cannot see
///   any other way.
///
/// Re-acquisitions that enter as `AlreadyAdmitted` carry their decision in the
/// user word rather than in an owner slot and never reach here, so they stay
/// outside the invariant this enforces.
fn wait_for_admission_grant() -> GateOutcome {
    if !admission_enabled() || !gate_enabled() {
        return GateOutcome::Disabled;
    }

    let disarmed = GATE_DISARMED.with(std::cell::Cell::get);
    let mut waiting_since = None;
    let mut iterations: u32 = 0;
    loop {
        match admitted_on_current_cpu() {
            None => {
                crate::mutex_hook::record_admission_gate_bypass();
                return GateOutcome::Bypassed;
            }
            Some(true) => {
                if disarmed {
                    GATE_DISARMED.with(|flag| flag.set(false));
                }
                return GateOutcome::Admitted;
            }
            Some(false) => {}
        }

        // One probe is all a disarmed thread pays, and a grant it happens to
        // see is what puts it back under the gate.
        if disarmed {
            return GateOutcome::Disarmed;
        }

        let since = *waiting_since.get_or_insert_with(Instant::now);
        iterations = iterations.wrapping_add(1);
        crate::mutex_hook::record_admission_gate_wait_loop();
        if iterations.is_multiple_of(ADMISSION_GATE_TIMEOUT_CHECK_INTERVAL)
            && since.elapsed() >= gate_timeout()
        {
            GATE_DISARMED.with(|flag| flag.set(true));
            crate::mutex_hook::record_admission_gate_timeout();
            return GateOutcome::TimedOut;
        }

        std::thread::yield_now();
    }
}

/// Prepares a marked slow path for the acquisition that follows it: the yield
/// that gives the scheduler a chance to act on the mark, the token release that
/// the yield makes safe, and the wait for the grant.
#[inline(always)]
pub fn wait_for_slow_path_admission_for_scope(scope: LockAdmissionScope) {
    std::thread::yield_now();
    clear_token_consumed_for_scope(scope);
    wait_for_admission_grant();
}

/// The same preparation for a backend that publishes one admission word for
/// every lock rather than one per lock class.
#[inline(always)]
pub fn wait_for_slow_path_admission() {
    std::thread::yield_now();
    clear_token_consumed();
    wait_for_admission_grant();
}

#[inline(always)]
fn inactive_queue_idle() -> Option<bool> {
    let enqueue_seq = INACTIVE_ENQUEUE_SEQ_PTR.load(Ordering::Acquire);
    let empty_seq = INACTIVE_EMPTY_SEQ_PTR.load(Ordering::Acquire);
    if enqueue_seq.is_null() || empty_seq.is_null() {
        return None;
    }

    let enqueue_seq = unsafe { enqueue_seq.read_volatile() };
    let empty_seq = unsafe { empty_seq.read_volatile() };
    Some(enqueue_seq == empty_seq)
}

#[inline(always)]
fn slow_path_yield_required(token_consumed: bool) -> bool {
    // The skip below publishes with the consumed token still in the word, and
    // that word is what the scheduler reclaims a slot from: the thread can be
    // parked with its node already in the queue, which is the state the gate
    // exists to rule out. So the skip only stands while the gate is not armed,
    // which also keeps the two arms comparable through the one switch.
    if gate_armed() {
        return true;
    }

    if token_consumed && inactive_queue_idle().is_some_and(|idle| idle) {
        return false;
    }

    true
}

#[inline(always)]
pub fn begin_lock_scope(lock_id: u32) -> LockAdmissionScope {
    THREAD_HELD_DEPTH.with(|depth| {
        let held = depth.get();
        depth.set(held.saturating_add(1));

        let managed = managed_lock_id(lock_id);
        if held == 0 {
            THREAD_OUTER_LOCK_ID.with(|outer| outer.set(lock_id));
            record_outermost_transition(lock_id);
        } else if managed {
            record_nesting_edge(lock_id);
        }

        LockAdmissionScope {
            lock_id,
            outer_managed: held == 0 && managed,
        }
    })
}

#[inline(always)]
pub fn mark_slow_path_pending_for_scope(scope: LockAdmissionScope) -> bool {
    if scope.outer_managed {
        mark_slow_path_pending_for_lock(scope.lock_id)
    } else {
        false
    }
}

#[inline(always)]
pub fn prepare_slow_path_admission() {
    let consumed = token_consumed();
    if mark_slow_path_pending() && slow_path_yield_required(consumed) {
        wait_for_slow_path_admission();
    }
}

#[inline(always)]
pub fn prepare_slow_path_admission_for_scope(scope: LockAdmissionScope) {
    let consumed = token_consumed_for_scope(scope);
    if mark_slow_path_pending_for_scope(scope) && slow_path_yield_required(consumed) {
        wait_for_slow_path_admission_for_scope(scope);
    }
}

#[inline(always)]
pub fn token_consumed_for_scope(scope: LockAdmissionScope) -> bool {
    scope.outer_managed && token_consumed_for_lock(scope.lock_id)
}

#[inline(always)]
pub fn clear_token_consumed_for_scope(scope: LockAdmissionScope) {
    if scope.outer_managed {
        clear_token_consumed_for_lock(scope.lock_id);
    }
}

#[inline(always)]
pub fn mark_critical_section_entered_for_scope(scope: LockAdmissionScope) {
    if scope.outer_managed {
        mark_critical_section_entered_for_lock(scope.lock_id);
    }
}

/// Publishes the critical-section exit for the lock that currently owns the
/// admission word, even when an inner scope is still held: condition variables
/// release the outer contended mutex while their internal mutex stays locked,
/// and leaving `IN_CRITICAL_SECTION` set there pins the CPU admission slot for
/// the whole sleep.
#[inline(always)]
pub fn finish_lock_scope(lock_id: u32) {
    THREAD_HELD_DEPTH.with(|depth| {
        let held = depth.get();
        if held == 0 {
            return;
        }

        if managed_lock_id(lock_id) {
            if held == 1 {
                mark_critical_section_exit_for_lock(lock_id);
            } else {
                mark_critical_section_exit_for_owned_lock(lock_id);
            }
        }
        if held == 1 {
            THREAD_OUTER_LOCK_ID.with(|outer| outer.set(UNMANAGED_LOCK_ID));
        }
        depth.set(held - 1);
    });
}

#[inline(always)]
pub fn mark_slow_path_pending() -> bool {
    mark_slow_path_pending_with_admission_enabled(admission_enabled())
}

#[inline(always)]
pub fn mark_slow_path_pending_for_lock(lock_id: u32) -> bool {
    mark_slow_path_pending_for_lock_with_admission_enabled(lock_id, admission_enabled())
}

#[inline(always)]
pub fn mark_cond_reacquire_pending_for_cond_mutex(lock_id: u32) -> bool {
    mark_cond_reacquire_pending_for_cond_mutex_with_admission_enabled(lock_id, admission_enabled())
}

/// Publishes the cv-sleep state and hands back the handle another thread needs
/// to retire it on this thread's behalf.
#[inline(always)]
pub fn arm_cv_sleep_for_lock(lock_id: u32) -> CvSleepArm {
    arm_cv_sleep_for_lock_with_admission_enabled(lock_id, admission_enabled())
}

/// Acknowledges a cond-reacquire hint published for this scope: the consumed
/// token is cleared in the same word access that matched it, so the caller can
/// block on the lock without asking admission again.
#[inline(always)]
pub fn take_cond_reacquire_pending_for_scope(scope: LockAdmissionScope) -> bool {
    if !scope.outer_managed {
        return false;
    }

    let class = class_of(scope.lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        if user_lock_id_for_value(value) != class
            || flags(value) & (SLOW_PATH_PENDING | TOKEN_CONSUMED)
                != SLOW_PATH_PENDING | TOKEN_CONSUMED
        {
            return false;
        }

        word.store(
            word_with_lock_id(class, flags(value) & !TOKEN_CONSUMED),
            Ordering::Relaxed,
        );
        true
    })
}

#[inline(always)]
pub fn token_consumed() -> bool {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        flags(value) & TOKEN_CONSUMED != 0
    })
}

#[inline(always)]
fn token_consumed_for_lock(lock_id: u32) -> bool {
    if !managed_lock_id(lock_id) {
        return false;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        user_lock_id_for_value(value) == class && flags(value) & TOKEN_CONSUMED != 0
    })
}

#[inline(always)]
fn mark_slow_path_pending_with_admission_enabled(enabled: bool) -> bool {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            value | SLOW_PATH_PENDING
        } else {
            value & !TRANSIENT_FLAGS
        };
        word.store(next, Ordering::Relaxed);
    });
    enabled
}

#[inline(always)]
fn mark_slow_path_pending_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) -> bool {
    if !managed_lock_id(lock_id) {
        return false;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            word_with_lock_id(
                class,
                preserved_pending_flags(class, value) | SLOW_PATH_PENDING,
            )
        } else {
            cleared_flags_word(class, value)
        };
        word.store(next, Ordering::Relaxed);
    });
    enabled
}

/// The flags a slow-path mark carries over from the word it replaces.
///
/// A mark naming the class the word already names keeps the consumed token: the
/// thread is contending for the class it was last admitted to, and the token it
/// holds is the one that acquisition will spend. A mark naming another class
/// drops it, which is what leaves the word showing `SLOW_PATH_PENDING` without
/// `TOKEN_CONSUMED` and lets the scheduler hand the per-CPU owner slot the old
/// class held to someone else.
///
/// Under `ACCORDIN_PRESERVE_CROSS_CLASS_TOKEN` the token crosses the class
/// change instead, so a run can measure what that release is worth. The switch
/// only widens the window in which the word claims a token: every caller of the
/// mark clears the token again once its yield has run, so the word a blocking
/// acquisition finally waits under is the same in both arms.
///
/// What the off arm still pays: a class change that carries a token reads the
/// switch to find it off, so it takes the `OnceLock` load on every such mark.
/// Only a class change with no token to preserve short-circuits ahead of the
/// read. Both arms also take the counters' enabled-flag load per mark.
#[inline(always)]
fn preserved_pending_flags(class: u32, value: u32) -> u32 {
    let token = flags(value) & TOKEN_CONSUMED;
    if user_lock_id_for_value(value) == class {
        crate::mutex_hook::record_admission_mark_same_class();
        return token;
    }

    crate::mutex_hook::record_admission_mark_cross_class();
    if token == 0 || !preserve_cross_class_token_enabled() {
        return 0;
    }

    crate::mutex_hook::record_admission_mark_cross_class_token_kept();
    token
}

/// Publishes the cond-reacquire hint only while the word still describes the
/// just-unlocked cond mutex and no managed scope stays held: any other state
/// belongs to an enclosing critical section that must keep owning the word.
#[inline(always)]
fn mark_cond_reacquire_pending_for_cond_mutex_with_admission_enabled(
    lock_id: u32,
    enabled: bool,
) -> bool {
    if !managed_lock_id(lock_id) || THREAD_HELD_DEPTH.with(|depth| depth.get()) != 0 {
        return false;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        if user_lock_id_for_value(value) != class {
            return false;
        }

        let next = if enabled {
            word_with_lock_id(
                class,
                (flags(value) | SLOW_PATH_PENDING | TOKEN_CONSUMED) & !IN_CRITICAL_SECTION,
            )
        } else {
            cleared_flags_word(class, value)
        };
        word.store(next, Ordering::Relaxed);
        enabled
    })
}

/// A published cv-sleep state, addressed so that a thread other than the
/// sleeper can take it back.
///
/// Admission words are thread-local, so the only way to reach a sleeper's word
/// is through the address the sleeper published; the value it published travels
/// with the address because the retirement is a compare-and-exchange against
/// exactly that value.
///
/// The handle borrows the sleeper's thread-local storage. It is valid for as
/// long as that thread is alive, which the wake protocol that uses it
/// guarantees: the handle is published before the sleep and is only read by the
/// thread that releases that sleep.
#[derive(Clone, Copy)]
pub struct CvSleepArm {
    word: *const AtomicU32,
    armed: u32,
}

impl CvSleepArm {
    /// A handle naming nothing, which every operation on it is a no-op for.
    pub const fn none() -> Self {
        Self {
            word: std::ptr::null(),
            armed: 0,
        }
    }

    /// Rebuilds a handle from the two values a sleeper published for its waker.
    #[inline(always)]
    pub fn from_parts(word_addr: usize, armed: u32) -> Self {
        Self {
            word: word_addr as *const AtomicU32,
            armed,
        }
    }

    #[inline(always)]
    pub fn is_armed(&self) -> bool {
        !self.word.is_null()
    }

    #[inline(always)]
    pub fn word_addr(&self) -> usize {
        self.word as usize
    }

    #[inline(always)]
    pub fn armed_value(&self) -> u32 {
        self.armed
    }

    /// Takes the published state back, and reports whether this call is the one
    /// that did it.
    ///
    /// What replaces it is the class with no flag, which is what a thread that
    /// has finished with a lock publishes and what the sleeper would leave
    /// behind itself; see `retire_cv_sleep_to_released`. Retiring from either
    /// side therefore lands on the same word, and neither side has to know
    /// which of them got there first.
    ///
    /// The exchange is what makes a cross-thread write to a thread-local word
    /// sound. A sleeper never writes its own word while it is inside the futex
    /// wait this state was published for, so the only writer that can race here
    /// is one that left the wait early; that one has already rewritten the word,
    /// the exchange sees a value other than the armed one and fails, and what it
    /// wrote stands. A failed retirement costs the routing of one wake and
    /// nothing else: the sleeper retires the state itself once its wait ends.
    #[inline(always)]
    pub fn retire(&self) -> bool {
        let Some(word) = (unsafe { self.word.as_ref() }) else {
            return false;
        };

        word.compare_exchange(
            self.armed,
            lock_id_bits(self.armed),
            Ordering::AcqRel,
            Ordering::Relaxed,
        )
        .is_ok()
    }
}

/// Publishes the cv-sleep state for the lock the waiter will re-acquire: the
/// word names that lock and carries no other flag, because the thread is about
/// to release the lock it holds and block outside any critical section.
///
/// Nothing is published while a managed scope stays held: that scope owns the
/// word, exactly as it does for the cond-reacquire hint.
#[inline(always)]
fn arm_cv_sleep_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) -> CvSleepArm {
    if !managed_lock_id(lock_id) || THREAD_HELD_DEPTH.with(|depth| depth.get()) != 0 {
        return CvSleepArm::none();
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        if !enabled {
            word.store(cleared_flags_word(class, value), Ordering::Relaxed);
            return CvSleepArm::none();
        }

        let armed = word_with_lock_id(class, CV_SLEEP);
        word.store(armed, Ordering::Relaxed);
        CvSleepArm {
            word: word as *const AtomicU32,
            armed,
        }
    })
}

#[inline(always)]
pub fn mark_critical_section_entered() {
    mark_critical_section_entered_with_admission_enabled(admission_enabled());
}

#[inline(always)]
pub fn mark_critical_section_entered_for_lock(lock_id: u32) {
    mark_critical_section_entered_for_lock_with_admission_enabled(lock_id, admission_enabled());
}

#[inline(always)]
fn mark_critical_section_entered_with_admission_enabled(enabled: bool) {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            (value | IN_CRITICAL_SECTION) & !(SLOW_PATH_PENDING | TOKEN_CONSUMED)
        } else {
            value & !TRANSIENT_FLAGS
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
fn mark_critical_section_entered_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) {
    if !managed_lock_id(lock_id) {
        return;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            word_with_lock_id(
                class,
                (flags(value) | IN_CRITICAL_SECTION) & !(SLOW_PATH_PENDING | TOKEN_CONSUMED),
            )
        } else {
            cleared_flags_word(class, value)
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn mark_critical_section_exit() {
    mark_critical_section_exit_with_admission_enabled(admission_enabled());
}

#[inline(always)]
pub fn mark_critical_section_exit_for_lock(lock_id: u32) {
    mark_critical_section_exit_for_lock_with_admission_enabled(lock_id, admission_enabled());
}

#[inline(always)]
fn mark_critical_section_exit_with_admission_enabled(enabled: bool) {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        let next = if enabled {
            (value | TOKEN_CONSUMED) & !IN_CRITICAL_SECTION
        } else {
            value & !TRANSIENT_FLAGS
        };
        word.store(next, Ordering::Relaxed);
    });
}

#[inline(always)]
fn mark_critical_section_exit_for_lock_with_admission_enabled(lock_id: u32, enabled: bool) {
    if !managed_lock_id(lock_id) {
        return;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(exit_word(class, value, enabled), Ordering::Relaxed);
    });
}

/// Out-of-order unlock: the exit word is published only for the lock that owns
/// the admission word, which is the outermost one this thread entered.
///
/// Ownership is decided on lock identity rather than on the class in the word,
/// because a merge makes an inner lock share the outer lock's class: comparing
/// classes would accept an ordinary nested release as if it were the outer
/// one and drop `IN_CRITICAL_SECTION` while the outer lock is still held.
#[inline(always)]
fn mark_critical_section_exit_for_owned_lock(lock_id: u32) {
    if THREAD_OUTER_LOCK_ID.with(|outer| outer.get()) != lock_id {
        return;
    }

    let enabled = admission_enabled();
    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(exit_word(class, value, enabled), Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn clear_token_consumed() {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(value & !TOKEN_CONSUMED, Ordering::Relaxed);
    });
}

/// Retires the cv-sleep state once the futex wait has returned, into the state
/// the re-acquisition that follows contends under: the same lock class with
/// `SLOW_PATH_PENDING`, which is what every other contender for that class
/// publishes.
///
/// Dropping the flag instead would leave the class alone in the word, which
/// describes a thread that is done with the lock: the scheduler would take back
/// the admission it granted the wake, and the re-acquisition would then have no
/// way to ask for another one. The pending state is cleared where every other
/// contender clears it, on entering the critical section.
///
/// `slept` distinguishes a waiter the scheduler could have routed from one whose
/// wait never blocked. The latter was not routed at all, so it re-acquires as it
/// would with routing off, where the hint it published leaves the consumed token
/// in the word and the re-acquisition suppresses its fast path because of it.
#[inline(always)]
pub fn retire_cv_sleep_to_pending(slept: bool) {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        if flags(value) & CV_SLEEP == 0 {
            return;
        }

        let retired = if slept {
            SLOW_PATH_PENDING
        } else {
            SLOW_PATH_PENDING | TOKEN_CONSUMED
        };
        word.store(
            word_with_lock_id(user_lock_id_for_value(value), retired),
            Ordering::Relaxed,
        );
    });
}

/// Retires the cv-sleep state of a waiter that is done with the lock it named
/// instead of about to re-acquire it, which is what a wake carrying the work's
/// result rather than the lock leaves behind.
///
/// The class stays in the word with no flag, the state every ordinary release
/// publishes: the scheduler reads it as a thread that has finished with the
/// lock, so a grant made for a re-acquisition that never happens is taken back
/// at the next stopping rather than held for the rest of the wake.
///
/// Reports whether there was a state left to retire, which is how a waiter
/// learns that its waker did not take it back before the wake.
#[inline(always)]
pub fn retire_cv_sleep_to_released() -> bool {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        if flags(value) & CV_SLEEP == 0 {
            return false;
        }

        word.store(lock_id_bits(value), Ordering::Relaxed);
        true
    })
}

#[inline(always)]
fn clear_token_consumed_for_lock(lock_id: u32) {
    if !managed_lock_id(lock_id) {
        return;
    }

    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(
            word_with_lock_id(class, flags(value) & !TOKEN_CONSUMED),
            Ordering::Relaxed,
        );
    });
}

#[inline(always)]
pub fn reset_state() {
    USER_ADMISSION_WORD.with(|word| {
        word.store(0, Ordering::Relaxed);
    });
}

#[inline(always)]
pub fn reset_transient_state() {
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        word.store(lock_id_bits(value), Ordering::Relaxed);
    });
}

#[doc(hidden)]
pub fn word_for_test() -> u32 {
    USER_ADMISSION_WORD.with(|word| word.load(Ordering::Relaxed))
}

/// The admission flags this thread publishes for `lock_id`, or none at all when
/// the word describes a different class: a flag set for another class says
/// nothing about this one.
#[doc(hidden)]
pub fn admission_flags_for_test(lock_id: u32) -> u32 {
    let class = class_of(lock_id);
    USER_ADMISSION_WORD.with(|word| {
        let value = word.load(Ordering::Relaxed);
        if user_lock_id_for_value(value) == class {
            flags(value)
        } else {
            0
        }
    })
}

#[doc(hidden)]
pub fn slow_path_pending_set_for_test(lock_id: u32) -> bool {
    admission_flags_for_test(lock_id) & SLOW_PATH_PENDING != 0
}

#[doc(hidden)]
pub fn cv_sleep_set_for_test(lock_id: u32) -> bool {
    admission_flags_for_test(lock_id) & CV_SLEEP != 0
}

#[doc(hidden)]
pub fn reset_thread_depth_for_test() {
    THREAD_HELD_DEPTH.with(|depth| depth.set(0));
    THREAD_OUTER_LOCK_ID.with(|outer| outer.set(UNMANAGED_LOCK_ID));
}

#[doc(hidden)]
pub fn reset_lock_id_allocator_for_test() {
    NEXT_LOCK_ID.store(1, Ordering::Relaxed);
    NEXT_OVERFLOW_INDEX.store(0, Ordering::Relaxed);
    // A class parked from before the reset belongs to the pool that was just
    // thrown away, so the next allocation draws rather than spends it.
    PARKED_LOCK_CLASS.with(|parked| parked.set(UNMANAGED_LOCK_ID));
}

/// Merging and dump gating are forced per thread so that a test never changes
/// what concurrently running tests observe.
#[cfg(test)]
fn force_merge_for_test(forced: bool) {
    MERGE_FORCED.with(|merge| merge.set(forced));
}

#[cfg(test)]
fn force_dump_for_test(forced: bool) {
    DUMP_FORCED.with(|dump| dump.set(forced));
}

#[cfg(test)]
fn force_lock_class_policy_for_test(forced: Option<LockClassPolicy>) {
    LOCK_CLASS_POLICY_FORCED.with(|policy| policy.set(forced));
}

#[cfg(test)]
fn reset_union_find_for_tests() {
    for (lock_id, parent) in LOCK_CLASS_PARENT.iter().enumerate() {
        parent.store(lock_id as u8, Ordering::Relaxed);
    }
    CLASSES_DIRTY.store(false, Ordering::Relaxed);
}

#[cfg(test)]
fn reset_transitions_for_tests() {
    for row in TRANSITION_COUNTS.iter() {
        for cell in row.iter() {
            cell.store(0, Ordering::Relaxed);
        }
    }
    THREAD_PREV_OUTERMOST_CLASS.with(|prev| prev.set(UNMANAGED_LOCK_ID));
}

#[cfg(test)]
fn transition_count_for_test(prev: u32, next: u32) -> u32 {
    TRANSITION_COUNTS[prev as usize][next as usize].load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    use crate::test_support::DebugCounterMeasurement;

    use super::GATE_DISARMED;
    use super::{
        CV_SLEEP, CvSleepArm, GateOutcome, IN_CRITICAL_SECTION, LockClassPolicy, MAX_LOCK_CLASSES,
        SLOW_PATH_PENDING, TOKEN_CONSUMED, UNMANAGED_LOCK_ID, USER_ADMISSION_FLAG_MASK,
        USER_ADMISSION_LOCK_ID_SHIFT, allocate_lock_class, allocate_lock_id, allocated_class_count,
        arm_cv_sleep_for_lock, arm_cv_sleep_for_lock_with_admission_enabled, begin_lock_scope,
        class_of, clear_token_consumed_for_scope, cleared_flags_word, exit_word, finish_lock_scope,
        flags, force_dump_for_test, force_gate_for_test, force_gate_timeout_for_test,
        force_lock_class_policy_for_test, force_merge_for_test,
        force_preserve_cross_class_token_for_test, lock_class_policy, managed_lock_id,
        mark_cond_reacquire_pending_for_cond_mutex,
        mark_cond_reacquire_pending_for_cond_mutex_with_admission_enabled,
        mark_critical_section_entered, mark_critical_section_entered_for_scope,
        mark_critical_section_entered_with_admission_enabled, mark_critical_section_exit,
        mark_critical_section_exit_with_admission_enabled, mark_slow_path_pending,
        mark_slow_path_pending_for_scope, mark_slow_path_pending_with_admission_enabled,
        merge_enabled_with, parse_lock_class_policy, rearm_gate_for_test,
        release_unused_lock_class, reset_lock_id_allocator_for_test, reset_state,
        reset_thread_depth_for_test, reset_transient_state, reset_transitions_for_tests,
        reset_union_find_for_tests, retire_cv_sleep_to_pending, retire_cv_sleep_to_released,
        set_cpu_owner_slots, set_inactive_queue_seq_ptrs, set_scheduler_registered,
        slow_path_yield_required, take_classes_dirty, take_cond_reacquire_pending_for_scope,
        token_consumed_for_scope, transition_count_for_test, union_lock_classes,
        user_lock_id_for_value, user_word_addr, wait_for_admission_grant, word_for_test,
        word_with_lock_id,
    };
    use super::{cached_tid, cv_sleep_set_for_test};
    use super::{forget_scheduler_identity_after_fork, scheduler_registered};

    static SHARED_SCHEDULER_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static DEPENDENCY_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static ALLOCATOR_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());
    static mut TEST_INACTIVE_ENQUEUE_SEQ: u32 = 0;
    static mut TEST_INACTIVE_EMPTY_SEQ: u32 = 0;

    /// An owner value no thread can carry: tids are bounded well below this, so
    /// a slot holding it never names anyone.
    const NO_OWNER_TID: u32 = u32::MAX;

    /// The pointer channels the scheduler loader installs, plus the gate arms
    /// that depend on them. Every one of them is process-wide, so a test that
    /// fakes any of them takes this guard and holds it while they stay installed.
    struct SharedSchedulerStateTestGuard {
        _guard: MutexGuard<'static, ()>,
        cpu_owner_slots: Option<Box<[u32]>>,
    }

    impl SharedSchedulerStateTestGuard {
        fn install_inactive_queue(&mut self, enqueue_seq: u32, empty_seq: u32) -> &mut Self {
            unsafe {
                TEST_INACTIVE_ENQUEUE_SEQ = enqueue_seq;
                TEST_INACTIVE_EMPTY_SEQ = empty_seq;
            }
            set_inactive_queue_seq_ptrs(
                &raw mut TEST_INACTIVE_ENQUEUE_SEQ,
                &raw mut TEST_INACTIVE_EMPTY_SEQ,
            );
            self
        }

        /// Publishes one slot per configured CPU, all naming `owner`, so the
        /// verdict does not depend on which CPU the test thread runs on. The
        /// calling thread also stands in for a registered one, which is the
        /// other half of what makes a grant reachable.
        fn install_cpu_owner_slots(&mut self, owner: u32) -> &mut Self {
            set_scheduler_registered(true);
            // Withdraw before the previous allocation is freed, so no reader can
            // reach a slot that has already gone.
            set_cpu_owner_slots(std::ptr::null_mut(), 0);
            let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) }.max(1) as usize;
            let slots = self
                .cpu_owner_slots
                .insert(vec![owner; cpus].into_boxed_slice());
            set_cpu_owner_slots(slots.as_mut_ptr(), slots.len());
            self
        }

        fn cpu_owner_slots_addr(&self) -> usize {
            self.cpu_owner_slots
                .as_ref()
                .map_or(0, |slots| slots.as_ptr() as usize)
        }

        fn cpu_owner_slots_len(&self) -> usize {
            self.cpu_owner_slots.as_ref().map_or(0, |slots| slots.len())
        }

        fn force_gate(&mut self, enabled: bool) -> &mut Self {
            force_gate_for_test(Some(enabled));
            self
        }

        fn force_gate_timeout(&mut self, timeout: Duration) -> &mut Self {
            force_gate_timeout_for_test(Some(timeout));
            self
        }

        fn unregister_from_scheduler(&mut self) -> &mut Self {
            set_scheduler_registered(false);
            self
        }
    }

    impl Drop for SharedSchedulerStateTestGuard {
        fn drop(&mut self) {
            set_inactive_queue_seq_ptrs(std::ptr::null_mut(), std::ptr::null_mut());
            set_cpu_owner_slots(std::ptr::null_mut(), 0);
            set_scheduler_registered(false);
            rearm_gate_for_test();
            force_gate_for_test(None);
            force_gate_timeout_for_test(None);
        }
    }

    fn isolate_shared_scheduler_state() -> SharedSchedulerStateTestGuard {
        let guard = SHARED_SCHEDULER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        SharedSchedulerStateTestGuard {
            _guard: guard,
            cpu_owner_slots: None,
        }
    }

    fn enable_debug_counters_for_test() -> DebugCounterMeasurement {
        let measurement = crate::test_support::measure_debug_counters();
        measurement.enable();
        measurement
    }

    struct DependencyStateTestGuard {
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for DependencyStateTestGuard {
        fn drop(&mut self) {
            force_merge_for_test(false);
            force_dump_for_test(false);
            reset_union_find_for_tests();
            reset_transitions_for_tests();
            reset_thread_depth_for_test();
        }
    }

    /// The per-thread state a test starts from and hands back: the admission
    /// word, the nesting depth and the cross-class token arm. All three are
    /// thread-local, so a test that takes this guard is isolated without
    /// serializing against anything.
    struct ThreadStateTestGuard;

    impl Drop for ThreadStateTestGuard {
        fn drop(&mut self) {
            force_preserve_cross_class_token_for_test(None);
            reset_thread_depth_for_test();
            reset_state();
        }
    }

    fn isolate_thread_state() -> ThreadStateTestGuard {
        reset_state();
        reset_thread_depth_for_test();
        force_preserve_cross_class_token_for_test(Some(false));
        ThreadStateTestGuard
    }

    /// Starts a test from a clean thread state with the cross-class token
    /// experiment pinned to one arm, so the run's environment never decides
    /// which transition the test observes.
    fn isolate_thread_state_preserving_cross_class_token(preserve: bool) -> ThreadStateTestGuard {
        let guard = isolate_thread_state();
        force_preserve_cross_class_token_for_test(Some(preserve));
        guard
    }

    struct AllocatorStateTestGuard {
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for AllocatorStateTestGuard {
        fn drop(&mut self) {
            force_lock_class_policy_for_test(None);
            reset_lock_id_allocator_for_test();
        }
    }

    /// Serializes the tests that draw from the process-wide id allocator and
    /// starts them from a known state. The policy is forced on this thread
    /// only, so tests running in parallel still see the configured one.
    fn isolate_allocator_state(policy: Option<LockClassPolicy>) -> AllocatorStateTestGuard {
        let guard = ALLOCATOR_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_lock_id_allocator_for_test();
        force_lock_class_policy_for_test(policy);
        AllocatorStateTestGuard { _guard: guard }
    }

    fn allocate_lock_classes(count: usize) -> Vec<u32> {
        (0..count).map(|_| allocate_lock_class()).collect()
    }

    fn classes_of(lock_ids: &[u32]) -> Vec<u32> {
        lock_ids.iter().map(|lock_id| class_of(*lock_id)).collect()
    }

    /// Serializes the tests that mutate the process-wide union-find and
    /// transition matrix, and starts them from a known state. Merging is forced
    /// on this thread only, so tests running in parallel still see it off.
    fn isolate_dependency_state() -> DependencyStateTestGuard {
        let guard = DEPENDENCY_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        reset_union_find_for_tests();
        reset_transitions_for_tests();
        reset_thread_depth_for_test();
        force_merge_for_test(true);
        force_dump_for_test(false);
        DependencyStateTestGuard { _guard: guard }
    }

    fn word_for(lock_id: u32, flags: u32) -> u32 {
        (lock_id << USER_ADMISSION_LOCK_ID_SHIFT) | flags
    }

    fn install_inactive_queue_state(
        enqueue_seq: u32,
        empty_seq: u32,
    ) -> SharedSchedulerStateTestGuard {
        let mut state = isolate_shared_scheduler_state();
        state.install_inactive_queue(enqueue_seq, empty_seq);
        state
    }

    #[test]
    fn admission_word_helpers_track_bit_transitions() {
        reset_state();

        mark_slow_path_pending();
        assert_eq!(word_for_test(), SLOW_PATH_PENDING);

        mark_critical_section_entered();
        assert_eq!(word_for_test(), IN_CRITICAL_SECTION);

        mark_slow_path_pending();
        assert_eq!(word_for_test(), IN_CRITICAL_SECTION | SLOW_PATH_PENDING);

        mark_critical_section_exit();
        assert_eq!(word_for_test(), SLOW_PATH_PENDING | TOKEN_CONSUMED);

        mark_critical_section_entered();
        mark_critical_section_exit();
        assert_eq!(word_for_test(), TOKEN_CONSUMED);
    }

    #[test]
    fn measurement_reset_clears_transient_state() {
        reset_state();

        mark_slow_path_pending();
        mark_critical_section_entered();

        reset_transient_state();

        assert_eq!(word_for_test(), 0);

        reset_state();
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn disabled_admission_does_not_set_admission_bits() {
        reset_state();

        mark_slow_path_pending_with_admission_enabled(false);
        assert_eq!(word_for_test(), 0);

        mark_critical_section_entered_with_admission_enabled(false);
        assert_eq!(word_for_test(), 0);

        mark_critical_section_exit_with_admission_enabled(false);
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn slow_path_pending_reports_whether_admission_policy_is_enabled() {
        reset_state();

        assert!(mark_slow_path_pending_with_admission_enabled(true));
        assert_eq!(word_for_test(), SLOW_PATH_PENDING);

        reset_state();

        assert!(!mark_slow_path_pending_with_admission_enabled(false));
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn explicit_lock_scope_encodes_lock_id_and_preserves_it_on_exit() {
        let _thread_state = isolate_thread_state();

        let scope = begin_lock_scope(3);
        assert!(mark_slow_path_pending_for_scope(scope));
        assert_eq!(word_for_test(), word_for(3, SLOW_PATH_PENDING));

        mark_critical_section_entered_for_scope(scope);
        assert_eq!(word_for_test(), word_for(3, IN_CRITICAL_SECTION));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(3, TOKEN_CONSUMED));

        reset_state();
        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn unmanaged_scope_does_not_write_admission_bits() {
        let _thread_state = isolate_thread_state();

        let scope = begin_lock_scope(UNMANAGED_LOCK_ID);
        assert!(!mark_slow_path_pending_for_scope(scope));
        mark_critical_section_entered_for_scope(scope);
        finish_lock_scope(UNMANAGED_LOCK_ID);

        assert_eq!(word_for_test(), 0);
    }

    #[test]
    fn nested_managed_lock_does_not_replace_outer_lock_id() {
        let _thread_state = isolate_thread_state();

        let outer = begin_lock_scope(2);
        assert!(mark_slow_path_pending_for_scope(outer));
        mark_critical_section_entered_for_scope(outer);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        let inner = begin_lock_scope(3);
        assert!(!mark_slow_path_pending_for_scope(inner));
        mark_critical_section_entered_for_scope(inner);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));
    }

    /// Nesting merges the inner lock into the outer lock's class, so the class
    /// the word carries stops distinguishing the two. Releasing the inner lock
    /// must still leave the outer critical section published: dropping
    /// `IN_CRITICAL_SECTION` here lets the scheduler reclaim the admission slot
    /// while the outer lock is held.
    #[test]
    fn a_merged_inner_unlock_keeps_the_outer_critical_section_published() {
        let _guard = isolate_dependency_state();
        reset_state();

        let outer = begin_lock_scope(2);
        assert!(mark_slow_path_pending_for_scope(outer));
        mark_critical_section_entered_for_scope(outer);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        let inner = begin_lock_scope(3);
        assert_eq!(class_of(3), 2, "nesting merges the inner lock");
        assert!(!mark_slow_path_pending_for_scope(inner));
        mark_critical_section_entered_for_scope(inner);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));
    }

    /// The out-of-order release the owned-lock branch exists for still has to
    /// work once the two locks share a class.
    #[test]
    fn a_merged_out_of_order_unlock_still_publishes_the_outer_exit_word() {
        let _guard = isolate_dependency_state();
        reset_state();

        let outer = begin_lock_scope(2);
        mark_critical_section_entered_for_scope(outer);

        let inner = begin_lock_scope(3);
        assert_eq!(class_of(3), 2, "nesting merges the inner lock");
        assert!(!mark_slow_path_pending_for_scope(inner));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));
    }

    #[test]
    fn out_of_order_unlock_publishes_the_exit_word_for_the_outer_lock() {
        let _thread_state = isolate_thread_state();

        let outer = begin_lock_scope(2);
        assert!(mark_slow_path_pending_for_scope(outer));
        mark_critical_section_entered_for_scope(outer);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        let inner = begin_lock_scope(MAX_LOCK_CLASSES);
        assert!(!mark_slow_path_pending_for_scope(inner));
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));

        finish_lock_scope(MAX_LOCK_CLASSES);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));
    }

    #[test]
    fn out_of_order_unlock_with_a_managed_inner_lock_publishes_both_exits() {
        let _thread_state = isolate_thread_state();

        let outer = begin_lock_scope(2);
        mark_critical_section_entered_for_scope(outer);

        let inner = begin_lock_scope(3);
        assert!(!mark_slow_path_pending_for_scope(inner));

        finish_lock_scope(2);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));

        finish_lock_scope(3);
        assert_eq!(word_for_test(), word_for(3, TOKEN_CONSUMED));
    }

    #[test]
    fn next_slow_path_after_consuming_token_exposes_consumed_flag_until_yield() {
        let _thread_state = isolate_thread_state();

        let first = begin_lock_scope(4);
        assert!(mark_slow_path_pending_for_scope(first));
        mark_critical_section_entered_for_scope(first);
        finish_lock_scope(4);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        let second = begin_lock_scope(4);
        assert!(token_consumed_for_scope(second));
        assert!(mark_slow_path_pending_for_scope(second));
        assert_eq!(
            word_for_test(),
            word_for(4, TOKEN_CONSUMED | SLOW_PATH_PENDING)
        );

        clear_token_consumed_for_scope(second);
        assert!(!token_consumed_for_scope(second));
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));
    }

    #[test]
    fn slow_path_for_different_lock_drops_consumed_token() {
        let _thread_state = isolate_thread_state();

        let first = begin_lock_scope(4);
        assert!(mark_slow_path_pending_for_scope(first));
        mark_critical_section_entered_for_scope(first);
        finish_lock_scope(4);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        let second = begin_lock_scope(5);
        assert!(!token_consumed_for_scope(second));
        assert!(mark_slow_path_pending_for_scope(second));

        assert_eq!(word_for_test(), word_for(5, SLOW_PATH_PENDING));
    }

    /// Leaves the word holding a consumed token for `lock_id` and hands back
    /// the scope a caller can go on marking with.
    fn consume_token_for_test(lock_id: u32) {
        let scope = begin_lock_scope(lock_id);
        assert!(mark_slow_path_pending_for_scope(scope));
        mark_critical_section_entered_for_scope(scope);
        finish_lock_scope(lock_id);
        assert_eq!(word_for_test(), word_for(class_of(lock_id), TOKEN_CONSUMED));
    }

    #[test]
    fn slow_path_for_different_lock_keeps_consumed_token_under_the_switch() {
        let _thread_state = isolate_thread_state_preserving_cross_class_token(true);

        consume_token_for_test(4);

        let second = begin_lock_scope(5);
        assert!(!token_consumed_for_scope(second));
        assert!(mark_slow_path_pending_for_scope(second));

        assert_eq!(
            word_for_test(),
            word_for(5, SLOW_PATH_PENDING | TOKEN_CONSUMED)
        );

        clear_token_consumed_for_scope(second);
        assert_eq!(word_for_test(), word_for(5, SLOW_PATH_PENDING));
    }

    /// A class change with nothing to preserve must land on the same word in
    /// both arms: the switch only ever carries a token that was already there.
    #[test]
    fn slow_path_for_different_lock_without_a_token_is_arm_independent() {
        for preserve in [false, true] {
            let _thread_state = isolate_thread_state_preserving_cross_class_token(preserve);

            let first = begin_lock_scope(4);
            assert!(mark_slow_path_pending_for_scope(first));
            finish_lock_scope(4);
            // Releasing the class is what leaves the token behind, so the
            // no-token state has to be reached by dropping it again.
            clear_token_consumed_for_scope(first);
            assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

            let second = begin_lock_scope(5);
            assert!(mark_slow_path_pending_for_scope(second));

            assert_eq!(word_for_test(), word_for(5, SLOW_PATH_PENDING));
        }
    }

    /// A mark for the class the word already names is outside what the switch
    /// decides, so both arms keep the token exactly as the same-class rule
    /// always did.
    #[test]
    fn slow_path_for_the_same_lock_keeps_the_token_in_both_arms() {
        for preserve in [false, true] {
            let _thread_state = isolate_thread_state_preserving_cross_class_token(preserve);

            consume_token_for_test(4);

            let second = begin_lock_scope(4);
            assert!(token_consumed_for_scope(second));
            assert!(mark_slow_path_pending_for_scope(second));

            assert_eq!(
                word_for_test(),
                word_for(4, SLOW_PATH_PENDING | TOKEN_CONSUMED)
            );
        }
    }

    /// The next acquisition of the class the switch marked has to reach the
    /// same word as the unswitched run once the mark's yield has cleared the
    /// token, which is what every caller of the mark does.
    #[test]
    fn cleared_cross_class_token_rejoins_the_unswitched_transition() {
        for preserve in [false, true] {
            let _thread_state = isolate_thread_state_preserving_cross_class_token(preserve);

            consume_token_for_test(4);

            let second = begin_lock_scope(5);
            assert!(mark_slow_path_pending_for_scope(second));
            clear_token_consumed_for_scope(second);
            assert_eq!(word_for_test(), word_for(5, SLOW_PATH_PENDING));

            mark_critical_section_entered_for_scope(second);
            assert_eq!(word_for_test(), word_for(5, IN_CRITICAL_SECTION));

            finish_lock_scope(5);
            assert_eq!(word_for_test(), word_for(5, TOKEN_CONSUMED));

            let third = begin_lock_scope(5);
            assert!(token_consumed_for_scope(third));
            finish_lock_scope(5);
        }
    }

    /// The counters are process-wide and every test that marks a slow path
    /// bumps them, so the deltas are lower bounds rather than equalities.
    #[test]
    fn admission_mark_counters_separate_the_class_transitions() {
        let _thread_state = isolate_thread_state_preserving_cross_class_token(true);

        let _counters = enable_debug_counters_for_test();
        let baseline = crate::mutex_hook::admission_mark_counters();

        consume_token_for_test(4);
        let same = begin_lock_scope(4);
        assert!(mark_slow_path_pending_for_scope(same));
        finish_lock_scope(4);

        let cross = begin_lock_scope(5);
        assert!(mark_slow_path_pending_for_scope(cross));
        finish_lock_scope(5);

        let counters = crate::mutex_hook::admission_mark_counters();

        assert!(counters.pending_same_class > baseline.pending_same_class);
        assert!(counters.pending_cross_class > baseline.pending_cross_class);
        assert!(counters.pending_cross_class_token_kept > baseline.pending_cross_class_token_kept);
    }

    fn release_managed_lock_for_test(lock_id: u32) {
        let scope = begin_lock_scope(lock_id);
        mark_critical_section_entered_for_scope(scope);
        finish_lock_scope(lock_id);
    }

    #[test]
    fn cond_reacquire_hint_drives_the_full_relock_transition() {
        let _thread_state = isolate_thread_state();

        release_managed_lock_for_test(4);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        assert!(mark_cond_reacquire_pending_for_cond_mutex(4));
        assert_eq!(
            word_for_test(),
            word_for(4, SLOW_PATH_PENDING | TOKEN_CONSUMED)
        );

        let relock = begin_lock_scope(4);
        assert!(take_cond_reacquire_pending_for_scope(relock));
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

        assert!(!take_cond_reacquire_pending_for_scope(relock));
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

        mark_critical_section_entered_for_scope(relock);
        assert_eq!(word_for_test(), word_for(4, IN_CRITICAL_SECTION));
    }

    #[test]
    fn cond_reacquire_hint_is_skipped_for_another_lock_id() {
        let _thread_state = isolate_thread_state();

        release_managed_lock_for_test(4);

        assert!(!mark_cond_reacquire_pending_for_cond_mutex(5));
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        let other = begin_lock_scope(5);
        assert!(!take_cond_reacquire_pending_for_scope(other));
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));
    }

    #[test]
    fn cond_reacquire_hint_is_skipped_while_an_outer_lock_stays_held() {
        let _thread_state = isolate_thread_state();

        let outer = begin_lock_scope(2);
        mark_critical_section_entered_for_scope(outer);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        assert!(!mark_cond_reacquire_pending_for_cond_mutex(3));
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
    }

    #[test]
    fn disabled_admission_cond_reacquire_hint_leaves_flags_clear() {
        let _thread_state = isolate_thread_state();

        release_managed_lock_for_test(4);

        assert!(!mark_cond_reacquire_pending_for_cond_mutex_with_admission_enabled(4, false));
        assert_eq!(word_for_test(), word_for(4, 0));
    }

    /// The handle is only usable if it names exactly what was published: the
    /// retirement is an exchange against that value, so an armed value that
    /// does not match the word retires nothing.
    #[test]
    fn an_arm_handle_names_the_word_and_the_exact_value_published() {
        let _thread_state = isolate_thread_state();

        let arm = arm_cv_sleep_for_lock(4);

        assert!(arm.is_armed());
        assert_eq!(arm.word_addr(), user_word_addr() as usize);
        assert_eq!(arm.armed_value(), word_for(class_of(4), CV_SLEEP));
        assert_eq!(word_for_test(), arm.armed_value());

        assert!(arm.retire());
        assert_eq!(
            word_for_test(),
            word_for(class_of(4), 0),
            "a retirement from either side lands on the released word"
        );
    }

    /// Only the first retirement takes the state, so a second waker cannot
    /// clear a state the owner published again afterwards.
    #[test]
    fn a_second_retirement_of_the_same_arm_reports_no_state_to_take() {
        let _thread_state = isolate_thread_state();

        let arm = arm_cv_sleep_for_lock(4);
        assert!(arm.retire());
        assert!(!arm.retire());
    }

    /// The owner is allowed to leave the sleep early and rewrite its own word.
    /// Losing the exchange against that write is the tolerated outcome: the
    /// state the owner published stands and only the routing of one wake is
    /// lost.
    #[test]
    fn a_retirement_that_loses_the_exchange_leaves_the_owners_word_alone() {
        let _thread_state = isolate_thread_state();

        let arm = arm_cv_sleep_for_lock(4);
        retire_cv_sleep_to_pending(true);
        let rewritten = word_for_test();
        assert_eq!(rewritten, word_for(class_of(4), SLOW_PATH_PENDING));

        assert!(!arm.retire());
        assert_eq!(word_for_test(), rewritten);
    }

    #[test]
    fn an_arm_handle_round_trips_through_its_parts() {
        let _thread_state = isolate_thread_state();

        let arm = arm_cv_sleep_for_lock(4);
        let rebuilt = CvSleepArm::from_parts(arm.word_addr(), arm.armed_value());

        assert!(rebuilt.is_armed());
        assert!(rebuilt.retire());
        assert_eq!(word_for_test(), word_for(class_of(4), 0));
    }

    /// Either side may be the one that retires a published sleep, and neither
    /// knows which got there first, so both have to leave the same word behind.
    #[test]
    fn both_sides_of_a_retirement_leave_the_same_word() {
        let _thread_state = isolate_thread_state();

        let arm = arm_cv_sleep_for_lock(4);
        assert!(arm.retire());
        let by_the_waker = word_for_test();

        reset_state();
        arm_cv_sleep_for_lock(4);
        retire_cv_sleep_to_released();

        assert_eq!(word_for_test(), by_the_waker);
    }

    #[test]
    fn an_unarmed_handle_retires_nothing() {
        let none = CvSleepArm::none();

        assert!(!none.is_armed());
        assert_eq!(none.word_addr(), 0);
        assert!(!none.retire());
    }

    /// A class the backend does not manage, a scope that still owns the word,
    /// and admission turned off each leave the waiter unrouted, so none of them
    /// may hand out a handle a waker would act on.
    #[test]
    fn nothing_is_armed_when_the_sleep_state_is_not_published() {
        let _thread_state = isolate_thread_state();
        assert!(!arm_cv_sleep_for_lock(UNMANAGED_LOCK_ID).is_armed());

        let outer = begin_lock_scope(2);
        mark_critical_section_entered_for_scope(outer);
        assert!(!arm_cv_sleep_for_lock(3).is_armed());
        finish_lock_scope(2);

        reset_state();
        reset_thread_depth_for_test();
        assert!(!arm_cv_sleep_for_lock_with_admission_enabled(4, false).is_armed());
        assert_eq!(word_for_test(), word_for(class_of(4), 0));
    }

    /// A waiter that returns without re-acquiring the lock is done with it, and
    /// the class alone is what a thread done with a lock publishes.
    #[test]
    fn a_released_retirement_leaves_the_class_with_no_flag() {
        let _thread_state = isolate_thread_state();

        arm_cv_sleep_for_lock(4);
        retire_cv_sleep_to_released();

        assert_eq!(word_for_test(), word_for(class_of(4), 0));
        assert!(!cv_sleep_set_for_test(4));

        // Nothing is published any more, so a second retirement is inert.
        retire_cv_sleep_to_released();
        assert_eq!(word_for_test(), word_for(class_of(4), 0));
    }

    #[test]
    fn a_lock_class_survives_a_word_roundtrip_next_to_every_flag() {
        for class in [1, MAX_LOCK_CLASSES - 1] {
            for flag_bits in [
                0,
                IN_CRITICAL_SECTION,
                SLOW_PATH_PENDING,
                TOKEN_CONSUMED,
                CV_SLEEP,
                SLOW_PATH_PENDING | TOKEN_CONSUMED,
                CV_SLEEP | TOKEN_CONSUMED,
            ] {
                let word = word_with_lock_id(class, flag_bits);
                assert_eq!(user_lock_id_for_value(word), class, "class {class}");
                assert_eq!(flags(word), flag_bits, "class {class}");
            }
        }
    }

    #[test]
    fn the_class_field_and_the_flag_field_stay_disjoint() {
        let highest_class = MAX_LOCK_CLASSES - 1;

        assert_eq!(CV_SLEEP & !USER_ADMISSION_FLAG_MASK, 0);
        assert_eq!(
            (highest_class << USER_ADMISSION_LOCK_ID_SHIFT) & USER_ADMISSION_FLAG_MASK,
            0
        );

        // A class alone decodes with no flags set ...
        assert_eq!(flags(word_with_lock_id(highest_class, 0)), 0);
        // ... and a flag alone decodes as the unmanaged class.
        assert_eq!(user_lock_id_for_value(CV_SLEEP), UNMANAGED_LOCK_ID);
    }

    #[test]
    fn exit_and_cleared_words_keep_the_transition_they_had() {
        let held = word_with_lock_id(7, IN_CRITICAL_SECTION | SLOW_PATH_PENDING);

        assert_eq!(
            exit_word(7, held, true),
            word_with_lock_id(7, SLOW_PATH_PENDING | TOKEN_CONSUMED)
        );
        assert_eq!(exit_word(7, held, false), word_with_lock_id(7, 0));
        assert_eq!(
            cleared_flags_word(7, word_with_lock_id(7, TOKEN_CONSUMED)),
            word_with_lock_id(7, 0)
        );
        // Cv-sleep degrades with the other in-flight flags.
        assert_eq!(
            cleared_flags_word(7, word_with_lock_id(7, CV_SLEEP)),
            word_with_lock_id(7, 0)
        );
    }

    #[test]
    fn cv_sleep_names_the_lock_the_waiter_will_reacquire() {
        let _thread_state = isolate_thread_state();

        release_managed_lock_for_test(4);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        arm_cv_sleep_for_lock(4);
        assert_eq!(word_for_test(), word_for(4, CV_SLEEP));

        retire_cv_sleep_to_pending(true);
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

        // The lock slept on and the lock re-acquired need not be the one the
        // word already carries.
        arm_cv_sleep_for_lock(5);
        assert_eq!(word_for_test(), word_for(5, CV_SLEEP));
    }

    /// The retire hands the class to the re-acquisition as an ordinary
    /// contender: the class it named survives, the sleep state is gone, and the
    /// two are never published together.
    #[test]
    fn retiring_cv_sleep_publishes_the_pending_state_for_the_same_class() {
        let _thread_state = isolate_thread_state();

        for class in [1, 4, MAX_LOCK_CLASSES - 1] {
            for slept in [true, false] {
                reset_state();
                arm_cv_sleep_for_lock(class);
                assert_eq!(word_for_test(), word_for(class, CV_SLEEP));

                retire_cv_sleep_to_pending(slept);

                let word = word_for_test();
                assert_eq!(user_lock_id_for_value(word), class, "class {class}");
                assert_eq!(flags(word) & CV_SLEEP, 0, "class {class}");
                assert_ne!(flags(word) & SLOW_PATH_PENDING, 0, "class {class}");
                // A waiter that never blocked was never routed, so it carries
                // the consumed token the routing-off relock carries.
                assert_eq!(
                    flags(word) & TOKEN_CONSUMED != 0,
                    !slept,
                    "class {class}, slept {slept}"
                );
            }
        }
    }

    /// The retire owns the whole flag field, so a word that never carried the
    /// sleep state is not turned into a pending one by it.
    #[test]
    fn retiring_without_a_published_cv_sleep_leaves_the_word_alone() {
        let _thread_state = isolate_thread_state();

        release_managed_lock_for_test(4);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        retire_cv_sleep_to_pending(true);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));
    }

    #[test]
    fn cv_sleep_is_skipped_while_a_lock_stays_held() {
        let _thread_state = isolate_thread_state();

        let outer = begin_lock_scope(2);
        mark_critical_section_entered_for_scope(outer);

        arm_cv_sleep_for_lock(3);
        assert_eq!(word_for_test(), word_for(2, IN_CRITICAL_SECTION));

        finish_lock_scope(2);
        arm_cv_sleep_for_lock(UNMANAGED_LOCK_ID);
        assert_eq!(word_for_test(), word_for(2, TOKEN_CONSUMED));
    }

    #[test]
    fn disabled_admission_cv_sleep_leaves_flags_clear() {
        let _thread_state = isolate_thread_state();

        release_managed_lock_for_test(4);

        arm_cv_sleep_for_lock_with_admission_enabled(4, false);
        assert_eq!(word_for_test(), word_for(4, 0));
    }

    #[test]
    fn slow_path_without_consumed_token_still_yields_to_request_admission() {
        let _queue_state = install_inactive_queue_state(7, 7);

        assert!(slow_path_yield_required(false));
    }

    #[test]
    fn consumed_token_reuse_skips_yield_when_inactive_queue_is_idle() {
        let _queue_state = install_inactive_queue_state(7, 7);

        assert!(!slow_path_yield_required(true));
    }

    #[test]
    fn consumed_token_reuse_yields_when_inactive_queue_has_pending_work() {
        let _queue_state = install_inactive_queue_state(8, 7);

        assert!(slow_path_yield_required(true));
    }

    /// How long a gate test waits for the other side of a hand-off before it
    /// calls the wait stalled rather than hanging.
    const GATE_PROGRESS_TIMEOUT: Duration = Duration::from_secs(20);

    /// A timeout no gate test can reach, so that a slow machine never turns a
    /// wait into a timeout escape.
    const GATE_TIMEOUT_OUT_OF_REACH: Duration = Duration::from_secs(600);

    /// The gate holds the caller until the slot for the CPU it runs on names it,
    /// which is the whole invariant: nothing is published before the grant.
    #[test]
    fn the_gate_holds_a_slow_path_until_a_grant_names_this_thread() {
        let mut state = isolate_shared_scheduler_state();
        state
            .force_gate(true)
            .force_gate_timeout(GATE_TIMEOUT_OUT_OF_REACH)
            .install_cpu_owner_slots(NO_OWNER_TID);

        // The wait-loop counter is the hand-off: the granter writes the slot
        // only once the waiter has been round the loop, so the test covers the
        // wait rather than the first probe.
        let _counters = enable_debug_counters_for_test();
        let spins_before = crate::mutex_hook::admission_gate_counters().gate_wait_loops;

        let tid = crate::mutex_hook::current_tid();
        let slots = state.cpu_owner_slots_addr();
        let len = state.cpu_owner_slots_len();
        let granter = std::thread::spawn(move || {
            let deadline = Instant::now() + GATE_PROGRESS_TIMEOUT;
            while crate::mutex_hook::admission_gate_counters().gate_wait_loops == spins_before {
                assert!(Instant::now() < deadline, "the gate never entered its wait");
                std::thread::yield_now();
            }

            let slots = slots as *mut u32;
            for slot in 0..len {
                unsafe { slots.add(slot).write_volatile(tid) };
            }
        });

        assert_eq!(wait_for_admission_grant(), GateOutcome::Admitted);
        granter.join().expect("granter thread");
    }

    /// A thread the scheduler never took into its map is never granted, so the
    /// gate has to let it through rather than hold it for a decision that is
    /// not coming.
    #[test]
    fn the_gate_lets_an_unregistered_thread_through() {
        let mut state = isolate_shared_scheduler_state();
        let tid = crate::mutex_hook::current_tid();
        state
            .force_gate(true)
            .install_cpu_owner_slots(tid)
            .unregister_from_scheduler();

        assert_eq!(wait_for_admission_grant(), GateOutcome::Bypassed);
    }

    /// A fork keeps the owner slots mapped and every cache filled while giving
    /// the child a tid of its own, so the child has to be made to look itself
    /// up again rather than compare slots against its parent.
    #[test]
    fn a_fork_child_forgets_the_identity_it_inherited() {
        let mut state = isolate_shared_scheduler_state();
        state
            .force_gate(true)
            .force_gate_timeout(Duration::ZERO)
            .install_cpu_owner_slots(NO_OWNER_TID);

        let tid = cached_tid();
        assert_eq!(wait_for_admission_grant(), GateOutcome::TimedOut);

        forget_scheduler_identity_after_fork();

        assert!(!scheduler_registered());
        assert!(!GATE_DISARMED.with(std::cell::Cell::get));
        assert_eq!(cached_tid(), tid, "the same thread looks up the same tid");
    }

    /// A grant already in place is the common case and costs no wait at all.
    #[test]
    fn the_gate_returns_at_once_when_the_grant_is_already_held() {
        let mut state = isolate_shared_scheduler_state();
        let tid = crate::mutex_hook::current_tid();
        state.force_gate(true).install_cpu_owner_slots(tid);

        assert_eq!(wait_for_admission_grant(), GateOutcome::Admitted);
    }

    /// With no scheduler behind the slots nothing would ever grant, so the gate
    /// has to let the caller through instead of waiting for something that
    /// cannot happen.
    #[test]
    fn the_gate_lets_a_slow_path_through_when_no_scheduler_published_slots() {
        let mut state = isolate_shared_scheduler_state();
        state.force_gate(true);

        let _counters = enable_debug_counters_for_test();
        let baseline = crate::mutex_hook::admission_gate_counters();

        assert_eq!(wait_for_admission_grant(), GateOutcome::Bypassed);

        let counters = crate::mutex_hook::admission_gate_counters();
        assert!(counters.gate_bypass_ungrantable > baseline.gate_bypass_ungrantable);
    }

    /// The switch has to leave the caller on exactly the path it took before the
    /// gate existed, which is what makes the two arms comparable.
    #[test]
    fn a_disabled_gate_never_waits_even_with_slots_published() {
        let mut state = isolate_shared_scheduler_state();
        state
            .force_gate(false)
            .install_cpu_owner_slots(NO_OWNER_TID);

        assert_eq!(wait_for_admission_grant(), GateOutcome::Disabled);
    }

    /// The escape a starved waiter takes: it publishes unadmitted rather than
    /// waiting out a grant that is not coming.
    #[test]
    fn the_gate_publishes_unadmitted_once_the_wait_times_out() {
        let mut state = isolate_shared_scheduler_state();
        state
            .force_gate(true)
            .force_gate_timeout(Duration::ZERO)
            .install_cpu_owner_slots(NO_OWNER_TID);

        let _counters = enable_debug_counters_for_test();
        let baseline = crate::mutex_hook::admission_gate_counters();

        assert_eq!(wait_for_admission_grant(), GateOutcome::TimedOut);

        let counters = crate::mutex_hook::admission_gate_counters();
        assert!(counters.gate_timeouts > baseline.gate_timeouts);
        assert!(counters.gate_wait_loops > baseline.gate_wait_loops);
    }

    /// An ejected scheduler leaves its owner slots mapped and readable, so a
    /// wait that keeps expiring is all userspace ever learns about it. One
    /// expiry therefore takes the thread out of the wait for good, and only a
    /// grant it actually observes puts it back.
    #[test]
    fn a_timed_out_wait_disarms_the_gate_until_a_grant_is_observed() {
        let mut state = isolate_shared_scheduler_state();
        state
            .force_gate(true)
            .force_gate_timeout(Duration::ZERO)
            .install_cpu_owner_slots(NO_OWNER_TID);

        assert_eq!(wait_for_admission_grant(), GateOutcome::TimedOut);
        assert_eq!(
            wait_for_admission_grant(),
            GateOutcome::Disarmed,
            "a second wait would cost another timeout"
        );

        state.install_cpu_owner_slots(crate::mutex_hook::current_tid());
        assert_eq!(
            wait_for_admission_grant(),
            GateOutcome::Admitted,
            "the probe a disarmed thread still takes is what re-arms it"
        );
        assert_eq!(wait_for_admission_grant(), GateOutcome::Admitted);

        // Back under the gate, an unowned slot waits again rather than passing.
        state
            .force_gate_timeout(GATE_TIMEOUT_OUT_OF_REACH)
            .install_cpu_owner_slots(NO_OWNER_TID);
        assert!(
            !GATE_DISARMED.with(std::cell::Cell::get),
            "the grant observed above rearmed the gate"
        );
    }

    /// The idle-relock skip publishes with the consumed token still in the word,
    /// which is the state a thread can be parked out of with its node already in
    /// the queue. An armed gate therefore has to outrank the skip.
    #[test]
    fn an_armed_gate_outranks_the_idle_relock_skip() {
        let mut state = isolate_shared_scheduler_state();
        state.install_inactive_queue(7, 7).force_gate(false);

        assert!(!slow_path_yield_required(true));

        state.force_gate(true);
        assert!(
            !slow_path_yield_required(true),
            "the gate is not armed until the slots are published"
        );

        state.install_cpu_owner_slots(NO_OWNER_TID);
        assert!(slow_path_yield_required(true));

        state.force_gate(false);
        assert!(!slow_path_yield_required(true));
    }

    #[test]
    fn lock_id_allocator_exhausts_at_configured_class_limit() {
        let _allocator = isolate_allocator_state(None);

        assert_eq!(MAX_LOCK_CLASSES, 64);
        for expected in 1..MAX_LOCK_CLASSES {
            assert_eq!(allocate_lock_id(), expected);
        }

        assert_eq!(allocate_lock_id(), UNMANAGED_LOCK_ID);
        assert_eq!(allocate_lock_id(), UNMANAGED_LOCK_ID);
    }

    #[test]
    fn overflow_policy_folds_unless_the_environment_asks_otherwise() {
        assert_eq!(parse_lock_class_policy(None), LockClassPolicy::Fold);
        assert_eq!(parse_lock_class_policy(Some("fold")), LockClassPolicy::Fold);
        assert_eq!(
            parse_lock_class_policy(Some(" UNMANAGED ")),
            LockClassPolicy::Unmanaged
        );
        // An unrecognized value keeps the default rather than dropping locks.
        assert_eq!(
            parse_lock_class_policy(Some("no-such-policy")),
            LockClassPolicy::Fold
        );
    }

    #[test]
    fn allocation_stops_at_the_class_limit_while_width_control_is_off() {
        let _allocator = isolate_allocator_state(None);

        // With no override the policy comes from the environment, and width
        // control is off in the test process.
        assert_eq!(lock_class_policy(), LockClassPolicy::Unmanaged);

        for expected in 1..MAX_LOCK_CLASSES {
            assert_eq!(allocate_lock_class(), expected);
        }

        assert_eq!(allocate_lock_class(), UNMANAGED_LOCK_ID);
        assert_eq!(allocate_lock_class(), UNMANAGED_LOCK_ID);
    }

    #[test]
    fn unmanaged_policy_leaves_overflowing_locks_outside_admission() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Unmanaged));

        let managed: Vec<u32> = (1..MAX_LOCK_CLASSES).collect();
        let ids = allocate_lock_classes(managed.len() + 4);

        assert_eq!(&ids[..managed.len()], &managed[..]);
        assert!(
            ids[managed.len()..]
                .iter()
                .all(|id| *id == UNMANAGED_LOCK_ID)
        );
    }

    #[test]
    fn fold_policy_deals_overflowing_locks_round_robin_over_the_managed_ids() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let managed: Vec<u32> = (1..MAX_LOCK_CLASSES).collect();
        let ids = allocate_lock_classes(3 * managed.len());

        assert_eq!(&ids[..managed.len()], &managed[..]);
        // The first locks past the limit fold onto the lowest ids in order.
        assert_eq!(&ids[managed.len()..managed.len() + 5], &[1, 2, 3, 4, 5]);
        // A full round of overflow lands on every managed id exactly once.
        assert_eq!(&ids[managed.len()..2 * managed.len()], &managed[..]);
        assert_eq!(&ids[2 * managed.len()..], &managed[..]);
    }

    #[test]
    fn allocated_class_count_tracks_the_ids_handed_out() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        assert_eq!(allocated_class_count(), 0);
        allocate_lock_classes(2);
        assert_eq!(allocated_class_count(), 2);

        // Ids run out at the class limit and folded overflow locks reuse them,
        // so the count stops at the managed space.
        allocate_lock_classes(MAX_LOCK_CLASSES as usize + 8);
        assert_eq!(allocated_class_count(), MAX_LOCK_CLASSES - 1);
    }

    /// A lock that never came into existence gives its class back, and the next
    /// lock this thread creates spends it instead of drawing another. The pool
    /// therefore pays for the locks that exist rather than for the attempts at
    /// creating them, which is what keeps a many-way first touch from eating
    /// the classes a barrier-wide workload needs.
    #[test]
    fn a_class_given_back_unused_is_spent_by_the_next_lock() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let first = allocate_lock_class();
        let unused = allocate_lock_class();
        assert_ne!(first, unused);
        assert_eq!(allocated_class_count(), 2);

        release_unused_lock_class(unused);
        assert_eq!(
            allocated_class_count(),
            2,
            "giving a class back neither draws nor unpublishes one"
        );

        assert_eq!(
            allocate_lock_class(),
            unused,
            "the next lock takes the class that was given back"
        );
        assert_eq!(
            allocated_class_count(),
            2,
            "spending the class that was given back draws nothing new"
        );

        assert_ne!(allocate_lock_class(), unused);
        assert_eq!(allocated_class_count(), 3);
    }

    /// Only one class is ever kept, and only a class the pool still owns.
    /// Everything past the limit is shared already, so keeping one would bias
    /// the round-robin the overflow policy deals them by rather than save
    /// anything.
    #[test]
    fn only_one_owned_class_is_ever_kept_back() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let first = allocate_lock_class();
        let second = allocate_lock_class();
        release_unused_lock_class(first);
        release_unused_lock_class(second);
        assert_eq!(allocate_lock_class(), first);
        assert_ne!(
            allocate_lock_class(),
            second,
            "the second class given back was dropped rather than queued"
        );

        release_unused_lock_class(UNMANAGED_LOCK_ID);
        let before_unmanaged = allocate_lock_class();
        assert_ne!(before_unmanaged, UNMANAGED_LOCK_ID);

        allocate_lock_classes(MAX_LOCK_CLASSES as usize);
        let folded = allocate_lock_class();
        release_unused_lock_class(folded);
        assert_ne!(
            allocate_lock_class(),
            folded,
            "a shared class is not kept back, so the fold keeps dealing in turn"
        );
    }

    #[test]
    fn fold_mapping_repeats_identically_from_a_fresh_allocator() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let first = allocate_lock_classes(40);
        reset_lock_id_allocator_for_test();
        let second = allocate_lock_classes(40);

        assert_eq!(first, second);
    }

    #[test]
    fn an_allocated_lock_class_does_not_move_as_more_locks_appear() {
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let early = allocate_lock_classes(MAX_LOCK_CLASSES as usize + 3);
        let early_classes = classes_of(&early);

        allocate_lock_classes(40);

        assert_eq!(classes_of(&early), early_classes);
        assert!(early.iter().all(|lock_id| managed_lock_id(*lock_id)));
    }

    #[test]
    fn folded_locks_resolve_through_the_class_of_the_id_they_share() {
        let _dependency = isolate_dependency_state();
        let _allocator = isolate_allocator_state(Some(LockClassPolicy::Fold));

        let managed: Vec<u32> = (1..MAX_LOCK_CLASSES).collect();
        let ids = allocate_lock_classes(managed.len() + 4);
        let folded_onto_three = ids[managed.len() + 2];
        let folded_onto_four = ids[managed.len() + 3];
        assert_eq!(folded_onto_three, 3);
        assert_eq!(folded_onto_four, 4);

        // A folded lock is one of its class, so a merge of that class carries
        // it along with the lock it folded onto.
        union_lock_classes(4, 3);

        for lock_id in [3, 4, folded_onto_three, folded_onto_four] {
            assert_eq!(class_of(lock_id), 3, "lock {lock_id}");
        }

        // Later overflow does not disturb the classes already resolved.
        allocate_lock_classes(managed.len());
        for lock_id in [3, 4, folded_onto_three, folded_onto_four] {
            assert_eq!(class_of(lock_id), 3, "lock {lock_id}");
        }
    }

    #[test]
    fn merged_component_is_named_by_its_smallest_lock_id() {
        let _state = isolate_dependency_state();

        union_lock_classes(5, 2);
        assert_eq!(class_of(5), 2);
        assert_eq!(class_of(2), 2);

        union_lock_classes(11, 7);
        assert_eq!(class_of(7), 7);
        assert_eq!(class_of(11), 7);
    }

    #[test]
    fn merges_are_sticky_and_reported_once_as_dirty() {
        let _state = isolate_dependency_state();

        assert!(!take_classes_dirty());

        union_lock_classes(2, 3);
        assert!(take_classes_dirty());
        assert!(!take_classes_dirty());

        union_lock_classes(3, 5);
        assert!(take_classes_dirty());
        assert_eq!(class_of(2), 2);
        assert_eq!(class_of(3), 2);
        assert_eq!(class_of(5), 2);

        union_lock_classes(5, 2);
        assert!(!take_classes_dirty());
        assert_eq!(class_of(5), 2);
    }

    #[test]
    fn find_converges_through_chained_parent_pointers() {
        let _state = isolate_dependency_state();

        union_lock_classes(15, 14);
        union_lock_classes(14, 12);
        union_lock_classes(12, 3);

        for lock_id in [3, 12, 14, 15] {
            assert_eq!(class_of(lock_id), 3, "lock {lock_id}");
        }
    }

    #[test]
    fn class_of_is_identity_for_unmanaged_and_out_of_range_ids() {
        let _state = isolate_dependency_state();

        assert_eq!(class_of(UNMANAGED_LOCK_ID), UNMANAGED_LOCK_ID);
        assert_eq!(class_of(MAX_LOCK_CLASSES), MAX_LOCK_CLASSES);
        assert_eq!(class_of(99), 99);
    }

    #[test]
    fn class_of_is_identity_while_merging_is_disabled() {
        let _state = isolate_dependency_state();

        union_lock_classes(2, 3);
        assert_eq!(class_of(3), 2);

        force_merge_for_test(false);
        assert_eq!(class_of(3), 3);

        force_merge_for_test(true);
        assert_eq!(class_of(3), 2);
    }

    /// Resolves the merge gate from raw width settings the way the process
    /// resolves it from the environment. Merging is requested throughout, which
    /// is what an explicit `ACCORDIN_WIDTH_MERGE=1` amounts to.
    fn merge_enabled_for_settings(fixed: Option<&str>, class_map: Option<&str>) -> bool {
        const WIDTH_CONTROL: bool = true;
        const REQUESTED: bool = true;

        merge_enabled_with(
            WIDTH_CONTROL,
            crate::width_control::fixed_widths_present_with(WIDTH_CONTROL, fixed, class_map),
            REQUESTED,
        )
    }

    #[test]
    fn a_fixed_width_takes_class_merging_out_of_play() {
        assert!(!merge_enabled_for_settings(Some("4"), None));
    }

    #[test]
    fn a_width_class_map_takes_class_merging_out_of_play() {
        assert!(!merge_enabled_for_settings(None, Some("3:5")));
        assert!(!merge_enabled_for_settings(Some("4"), Some("3:5")));
    }

    #[test]
    fn merging_follows_the_request_while_no_static_width_is_configured() {
        assert!(merge_enabled_for_settings(None, None));
        assert!(merge_enabled_with(true, false, true));
        assert!(!merge_enabled_with(true, false, false));
    }

    #[test]
    fn static_widths_do_not_reach_merging_while_width_control_is_off() {
        // Without the feature there is no width to configure, so neither
        // variable counts as one ...
        assert!(!crate::width_control::fixed_widths_present_with(
            false,
            Some("4"),
            Some("3:5")
        ));
        // ... and merging is off either way, exactly as it is without any of
        // this machinery.
        assert!(!merge_enabled_with(false, false, true));
        assert!(!merge_enabled_with(false, true, true));
    }

    #[test]
    fn only_nested_acquires_of_distinct_managed_locks_merge_classes() {
        let _state = isolate_dependency_state();

        begin_lock_scope(2);
        begin_lock_scope(3);
        finish_lock_scope(3);
        finish_lock_scope(2);
        assert_eq!(class_of(3), 2);
        assert_eq!(class_of(2), 2);

        begin_lock_scope(6);
        finish_lock_scope(6);
        begin_lock_scope(7);
        finish_lock_scope(7);
        assert_eq!(class_of(6), 6);
        assert_eq!(class_of(7), 7);

        begin_lock_scope(UNMANAGED_LOCK_ID);
        begin_lock_scope(9);
        finish_lock_scope(9);
        finish_lock_scope(UNMANAGED_LOCK_ID);
        assert_eq!(class_of(9), 9);

        begin_lock_scope(10);
        begin_lock_scope(UNMANAGED_LOCK_ID);
        finish_lock_scope(UNMANAGED_LOCK_ID);
        finish_lock_scope(10);
        assert_eq!(class_of(10), 10);
    }

    #[test]
    fn admission_word_publishes_the_class_of_a_merged_lock() {
        let _state = isolate_dependency_state();
        reset_state();

        union_lock_classes(9, 4);
        assert_eq!(class_of(9), 4);

        let scope = begin_lock_scope(9);
        assert!(mark_slow_path_pending_for_scope(scope));
        assert_eq!(word_for_test(), word_for(4, SLOW_PATH_PENDING));

        mark_critical_section_entered_for_scope(scope);
        assert_eq!(word_for_test(), word_for(4, IN_CRITICAL_SECTION));

        finish_lock_scope(9);
        assert_eq!(word_for_test(), word_for(4, TOKEN_CONSUMED));

        // The consumed token belongs to the class, so the sibling lock of the
        // same component reuses it instead of dropping it.
        let sibling = begin_lock_scope(4);
        assert!(token_consumed_for_scope(sibling));
        finish_lock_scope(4);

        reset_state();
    }

    #[test]
    fn nested_reacquire_of_the_same_lock_creates_no_edge() {
        let _state = isolate_dependency_state();

        begin_lock_scope(4);
        begin_lock_scope(4);
        finish_lock_scope(4);
        finish_lock_scope(4);

        assert_eq!(class_of(4), 4);
        assert!(!take_classes_dirty());
    }

    #[test]
    fn transition_counts_follow_outermost_class_handoffs() {
        let _state = isolate_dependency_state();
        force_dump_for_test(true);

        begin_lock_scope(2);
        finish_lock_scope(2);
        begin_lock_scope(3);
        finish_lock_scope(3);
        begin_lock_scope(2);
        finish_lock_scope(2);

        assert_eq!(transition_count_for_test(2, 3), 1);
        assert_eq!(transition_count_for_test(3, 2), 1);
        assert_eq!(transition_count_for_test(2, 2), 0);

        begin_lock_scope(5);
        begin_lock_scope(6);
        finish_lock_scope(6);
        finish_lock_scope(5);

        assert_eq!(transition_count_for_test(2, 5), 1);
        assert_eq!(transition_count_for_test(5, 6), 0);

        // Lock 6 was merged into class 5 by the nesting above, so its next
        // outermost acquire is counted against the class, not the lock.
        begin_lock_scope(6);
        finish_lock_scope(6);

        assert_eq!(class_of(6), 5);
        assert_eq!(transition_count_for_test(5, 5), 1);
        assert_eq!(transition_count_for_test(5, 6), 0);
    }
}
